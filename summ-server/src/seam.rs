//! The seam between the HTTP layer and everything below it.
//!
//! `summ-registry` (manifest and tag operations over `WriteBatch`) and
//! `summ-storage` (the content-addressed blob store) are separate crates built
//! separately. This trait is what the handlers actually call, so the HTTP layer
//! is complete and testable before either exists, and so wiring them up later
//! is an implementation of one trait rather than a rewrite of the handlers.
//!
//! Three rules shaped it, and they are worth keeping if it grows:
//!
//! - **Every method is a single registry operation**, not a step in one. There
//!   is no `begin`/`commit` pair, because a push must land as one `WriteBatch`
//!   and a seam that let the HTTP layer sequence writes would make that
//!   impossible to guarantee.
//! - **Failures are expressed in spec terms** ([`OpsError`]), not storage
//!   terms. The layer below knows that an offset did not match; only the layer
//!   above knows that means `416 BLOB_UPLOAD_INVALID`.
//! - **Identifiers are minted above and passed down.** `create_upload` takes
//!   the id rather than returning one, because a `WriteBatch` must contain no
//!   engine-minted values if it is to mean the same thing when replayed on a
//!   replica.
//!
//! Pagination is a page plus a `more` flag rather than an opaque cursor. The
//! flag is the whole reason `Link` can be emitted only when a further page
//! genuinely exists - the reference implementation cannot tell, so it emits
//! `Link` on the final page too and every client pays for one wasted request.

use std::collections::BTreeMap;

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use summ_core::Digest;

use crate::range::ByteRange;
use crate::reference::Reference;

/// One page of an ordered scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Whether a key exists beyond this page. Determined by peeking one past
    /// the limit, never by "the page was full".
    pub more: bool,
}

impl<T> Page<T> {
    pub fn empty() -> Self {
        Page {
            items: Vec::new(),
            more: false,
        }
    }
}

/// Everything a `HEAD` on a manifest needs, which is also everything a `GET`
/// needs besides the bytes.
///
/// `media_type` is the `Content-Type` the client pushed, stored verbatim. The
/// spec requires the response `Content-Type` to match the manifest's own
/// `mediaType` field, and the suite asserts it exactly, so it is never
/// synthesised or normalised here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestStat {
    pub digest: Digest,
    pub media_type: String,
    pub size: u64,
}

/// What a manifest `PUT` reports back to the HTTP layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestPut {
    /// Digest of the bytes as received. `Docker-Content-Digest` is this value,
    /// under the algorithm the client used - summ never rehashes under a
    /// different algorithm, so the spec's differing-algorithm escape hatch
    /// never applies.
    pub digest: Digest,
    /// The manifest's `subject`, if it had one. Drives `OCI-Subject`.
    pub subject: Option<Digest>,
    /// Tags applied from `?tag=` parameters. Drives `OCI-Tag`.
    pub tags: Vec<String>,
}

/// A blob body ready to be written to the socket.
///
/// `body` is an `axum::body::Body` rather than `Bytes` precisely so the real
/// implementation can stream: containerd 2.1+ chunked fetch opens `bytes=N-`,
/// reads 8 MiB and kills the connection, so buffering a whole blob to answer a
/// range is pathological.
pub struct BlobRead {
    /// Full size of the blob, for `Content-Range`'s `/<total>`.
    pub total_size: u64,
    /// The window actually being served. `None` means the whole blob.
    pub window: Option<ByteRange>,
    pub body: Body,
}

/// One entry of a referrers image index.
///
/// `artifact_type` is resolved at push time, not here: for an image manifest it
/// falls back to the config descriptor's `mediaType`, and for an index it is
/// omitted entirely when absent. Resolving it on write is what keeps the
/// referrers endpoint a pure ordered scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub media_type: String,
    pub digest: Digest,
    pub size: u64,
    pub artifact_type: Option<String>,
    pub annotations: BTreeMap<String, String>,
}

/// A referrers response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referrers {
    pub manifests: Vec<Descriptor>,
    /// Whether the `?artifactType=` filter was actually applied. Drives
    /// `OCI-Filters-Applied`, which must be sent only when the filter is exact
    /// - the suite then verifies no descriptor of another type is present.
    pub filter_applied: bool,
    /// Referrer digest to resume after, or `None` when the scan is exhausted.
    ///
    /// This is deliberately not a [`Page`]: `more` there is "another item
    /// exists", and here the cursor advances over the *scanned* edges rather
    /// than the returned ones, because the `artifactType` filter is applied
    /// inside the scan. So `manifests` can be short - even empty - with `next`
    /// still set, and the `Link` header is driven by this field alone. Never
    /// by whether the page came back full: that would end the walk on the
    /// first page whose matches happened not to fill it.
    pub next: Option<Digest>,
}

/// A blob body arriving on a push.
///
/// Carried as a stream rather than as `Bytes` because the alternative is a
/// buffer the size of a layer: a layer is routinely gigabytes, and a handful of
/// concurrent pushes at that size is an out-of-memory kill rather than a slow
/// registry. The implementation writes frames through as they arrive, so a push
/// costs one frame of memory regardless of the blob.
///
/// The two limits travel with the body because only the consumer can enforce
/// them: `declared` is not known to be wrong until the last frame has arrived,
/// and `limit` is not known to be exceeded until it is - except in the one case
/// where the client declares a length above it, which is a `413` before a byte
/// is written.
pub struct UploadBody {
    pub body: Body,
    /// The client's `Content-Length`, when it sent one. `None` is a streamed
    /// upload with no declared length, which the spec permits.
    pub declared: Option<u64>,
    /// Hard ceiling on the bytes this one request may carry, or `None` for
    /// none. See [`ServerConfig::max_upload_bytes`](crate::ServerConfig).
    pub limit: Option<u64>,
}

impl UploadBody {
    /// Collect the whole body, enforcing both limits.
    ///
    /// For an implementation that has nowhere to stream *to*. The streaming
    /// implementation must not use this - the point of the type is that it need
    /// not.
    pub async fn collect(self) -> OpsResult<Bytes> {
        let limit = self.limit.unwrap_or(u64::MAX);
        if let Some(declared) = self.declared {
            if declared > limit {
                return Err(OpsError::BodyTooLarge { limit });
            }
        }
        let ceiling = usize::try_from(limit).unwrap_or(usize::MAX);
        let bytes = axum::body::to_bytes(self.body, ceiling)
            .await
            .map_err(|_| OpsError::BodyTooLarge { limit })?;
        if let Some(declared) = self.declared {
            if declared != bytes.len() as u64 {
                return Err(OpsError::SizeMismatch {
                    declared,
                    actual: bytes.len() as u64,
                });
            }
        }
        Ok(bytes)
    }
}

// ---- discovery beyond the spec ------------------------------------------
//
// Nothing below is in the Distribution Spec. `_catalog` was removed before
// v1.0.0 and nothing standard answers "what is in this registry", so the
// shapes here are ours to choose - which also means nothing external
// validates them, and they carry their own tests.

/// How far a bounded count scans before it reports a floor instead of a total.
///
/// The scale target is 10M manifests in one repository, so "count the
/// manifests" is not an operation that may run to completion on a request
/// thread. It runs to this ceiling and then stops, and the caller is told which
/// happened. A UI renders the difference as `10,000+`.
///
/// The alternative - a stored counter - is the read-modify-write on the push
/// path that the whole key schema exists to avoid.
pub const COUNT_CEILING: u64 = 10_000;

/// How many tags one manifest row reports.
///
/// The `G` range is a fan-in: a manifest may carry thousands of tags, and a
/// list of them is not what a list row is for. The first few are the useful
/// signal; the rest are on the manifest's own page.
pub const TAGS_PER_MANIFEST: usize = 8;

/// A count that may have stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    pub count: u64,
    /// `false` when the scan hit [`COUNT_CEILING`], which makes `count` a floor
    /// rather than a total. Reported rather than hidden: a number that is
    /// silently wrong above a threshold is worse than no number.
    pub complete: bool,
}

impl Tally {
    pub fn exact(count: u64) -> Self {
        Tally {
            count,
            complete: true,
        }
    }
}

/// One row of the repository list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSummary {
    pub name: String,
    pub tags: Tally,
    pub manifests: Tally,
}

/// Everything a repository's own page shows above its lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoDetail {
    pub name: String,
    pub tags: Tally,
    pub manifests: Tally,
    pub blobs: Tally,
    /// Summed over the blobs `blobs` counted, so a floor whenever
    /// `blobs.complete` is `false`.
    pub size_bytes: u64,
}

/// A manifest as the discovery API describes it - the stored record, with the
/// two things a row needs that the record does not hold: the platforms it
/// covers, and the tags pointing at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestInfo {
    pub digest: Digest,
    pub media_type: String,
    /// Size of the manifest document itself.
    pub size: u64,
    /// Bytes of the blobs this manifest directly references.
    ///
    /// The config counts: it is a blob, it gets an `R` edge like any other, and
    /// a total that agreed with the UI but not with the edge set would be a
    /// number nobody could reconcile. Not recursive either - an index reports
    /// zero here, because its weight is in its children.
    pub blob_size: u64,
    pub artifact_type: Option<String>,
    pub subject: Option<Digest>,
    pub pushed_at: u64,
    /// `os/arch[/variant]`, once per platform: the manifest's own for an image,
    /// its children's for an index.
    pub platforms: Vec<String>,
    /// Count of those same blobs: config plus layers, matching `blob_size`.
    pub blobs: u64,
    /// Child manifests, for an index. Zero for an image manifest.
    pub children: u64,
    /// Tags pointing at this manifest, at most [`TAGS_PER_MANIFEST`] of them.
    pub tags: Vec<String>,
    pub annotations: BTreeMap<String, String>,
}

/// A tag with the manifest it resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagInfo {
    pub name: String,
    pub digest: Digest,
    pub tagged_at: u64,
    /// `None` when `T` names a manifest `M` does not have. That is corruption
    /// rather than a miss, but a list page is the wrong place to fail: the row
    /// says what it knows and the rest of the page still renders.
    pub manifest: Option<ManifestInfo>,
}

/// Failures the layers below can report, in the vocabulary of the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpsError {
    RepoUnknown,
    ManifestUnknown,
    BlobUnknown,
    UploadUnknown,
    /// A chunk did not begin at the committed offset. `current` is the offset
    /// the session is actually at, which the `416` path leaves untouched.
    OffsetMismatch {
        current: u64,
    },
    /// The bytes did not hash to the digest the client named.
    DigestMismatch,
    /// The body did not carry the number of bytes the client declared.
    ///
    /// Detected while consuming the body rather than before it, which is the
    /// price of not buffering. The session's *recorded* offset is still
    /// untouched - a rejected request never commits one - so the client's
    /// recovery is unchanged: ask for the offset, resume from it.
    SizeMismatch {
        declared: u64,
        actual: u64,
    },
    /// The body exceeded the per-request ceiling.
    BodyTooLarge {
        limit: u64,
    },
    /// The body ended early or the connection failed part-way through. A
    /// client's problem rather than the registry's, so not an `Internal`.
    BodyIncomplete(String),
    /// The manifest could not be parsed well enough to index it. Note this is
    /// *not* schema validation: manifests carrying fields outside the OCI
    /// schema must round-trip, and referenced blobs are deliberately not
    /// required to exist.
    ManifestInvalid(String),
    /// The manifest named a layer or a child manifest this repository does not
    /// have. Distinct from [`OpsError::ManifestInvalid`] because the spec gives
    /// it its own code, `MANIFEST_BLOB_UNKNOWN`, and a client can act on the
    /// difference: the document is well-formed and the fix is to push the blob,
    /// not to rewrite the manifest.
    ///
    /// Only an implementation that validates references can raise it. The check
    /// is optional per spec - see `RegistryOptions::validate_references` - so an
    /// implementation that skips it simply never returns this.
    ManifestBlobUnknown {
        digest: Digest,
    },
    Internal(String),
}

pub type OpsResult<T> = Result<T, OpsError>;

/// The one trait the handlers depend on.
#[async_trait]
pub trait Registry: Send + Sync + 'static {
    // ---- discovery -------------------------------------------------------

    /// Repository names in byte order, starting strictly after `last`.
    async fn repositories(&self, last: Option<&str>, limit: usize) -> OpsResult<Page<String>>;

    /// Tags in byte order, starting strictly after `last`.
    /// `Err(OpsError::RepoUnknown)` when the repository does not exist.
    async fn tags(&self, name: &str, last: Option<&str>, limit: usize) -> OpsResult<Page<String>>;

    // ---- manifests -------------------------------------------------------

    /// Metadata only. `HEAD` is a first-class path, never a `GET` with the body
    /// thrown away: four of the five serial steps in a cold containerd pull are
    /// metadata lookups, and this is the first of them.
    async fn stat_manifest(&self, name: &str, reference: &Reference) -> OpsResult<ManifestStat>;

    /// The manifest exactly as pushed. The bytes are never re-serialised - the
    /// digest is over them and the suite byte-compares.
    async fn get_manifest(
        &self,
        name: &str,
        reference: &Reference,
    ) -> OpsResult<(ManifestStat, Bytes)>;

    /// `tags` carries any `?tag=` parameters, already validated against the tag
    /// grammar.
    async fn put_manifest(
        &self,
        name: &str,
        reference: &Reference,
        content_type: &str,
        tags: &[String],
        body: Bytes,
    ) -> OpsResult<ManifestPut>;

    /// Deleting by digest MUST cascade to every tag pointing at it; deleting by
    /// tag MUST leave the manifest reachable by digest.
    async fn delete_manifest(&self, name: &str, reference: &Reference) -> OpsResult<()>;

    // ---- blobs -----------------------------------------------------------

    /// Size of a blob servable under `name`. Membership is per repository: a
    /// blob present elsewhere in the registry is not servable here.
    async fn stat_blob(&self, name: &str, digest: &Digest) -> OpsResult<u64>;

    async fn get_blob(
        &self,
        name: &str,
        digest: &Digest,
        window: Option<ByteRange>,
    ) -> OpsResult<BlobRead>;

    /// Removes the blob's membership of `name` only. Deleting from one
    /// repository must not affect another that has the same blob mounted.
    async fn delete_blob(&self, name: &str, digest: &Digest) -> OpsResult<()>;

    /// Cross-repository mount. `Ok(true)` mounted, `Ok(false)` refused - a
    /// refusal is not an error, it means "answer `202` and take a normal
    /// upload". `from` is optional: the spec allows anonymous mount, and with a
    /// registry-wide blob record "is this blob present anywhere" is one lookup.
    async fn mount_blob(&self, name: &str, digest: &Digest, from: Option<&str>) -> OpsResult<bool>;

    // ---- uploads ---------------------------------------------------------

    /// `algorithm` comes from `?digest-algorithm=` and selects which hasher the
    /// session carries; it defaults to `sha256`.
    async fn create_upload(&self, name: &str, id: &str, algorithm: &str) -> OpsResult<()>;

    /// Committed byte count. Backs both the `416` check and the end-13 status
    /// `GET`, and must not have any side effect - the spec requires a rejected
    /// chunk to leave the session byte-identical.
    async fn upload_offset(&self, name: &str, id: &str) -> OpsResult<u64>;

    /// Append at `expected_offset`, returning the new committed offset.
    ///
    /// The offset is re-checked here as well as in the handler. The handler
    /// checks it because that is where the `Content-Range` grammar lives; the
    /// implementation checks it because only it can do so atomically with the
    /// write.
    async fn append_upload(
        &self,
        name: &str,
        id: &str,
        expected_offset: u64,
        body: UploadBody,
    ) -> OpsResult<u64>;

    /// Append an optional final chunk, verify the whole-blob digest, and commit.
    ///
    /// Verification happens only here: a `PATCH` cannot verify a whole-blob
    /// digest because it has not seen the end. On mismatch nothing is
    /// committed.
    async fn finish_upload(
        &self,
        name: &str,
        id: &str,
        expected_offset: u64,
        body: UploadBody,
        digest: &Digest,
    ) -> OpsResult<()>;

    async fn cancel_upload(&self, name: &str, id: &str) -> OpsResult<()>;

    /// Push a whole blob in one request (end-4b, the single-POST flow).
    async fn put_blob(&self, name: &str, digest: &Digest, body: UploadBody) -> OpsResult<()>;

    // ---- referrers -------------------------------------------------------

    /// Manifests in `name` whose `subject` is `subject`.
    ///
    /// An unknown subject is **not** an error: once the referrers API is on it
    /// MUST NOT return `404`, so this returns an empty list. A subject that
    /// does not resolve to a stored manifest is normal - the spec requires a
    /// manifest with a dangling `subject` to be accepted and listed.
    ///
    /// `last` is a referrer digest to resume strictly after, and `limit` bounds
    /// the edges *scanned*, not the descriptors returned. The endpoint has no
    /// pagination in the spec, only the rule that `Link` MUST be sent when the
    /// list does not fit in one response, so the page is bounded here and the
    /// handler turns [`Referrers::next`] into that header.
    async fn referrers(
        &self,
        name: &str,
        subject: &Digest,
        artifact_type: Option<&str>,
        last: Option<&Digest>,
        limit: usize,
    ) -> OpsResult<Referrers>;

    // ---- discovery beyond the spec ---------------------------------------

    /// Repository names with their tag and manifest counts, in name order.
    ///
    /// `prefix` narrows the scan to names beginning with it, and `""` is the
    /// whole registry. It is a *scan* prefix, not a filter: `n <name>` is the
    /// name appended to one type byte, so this costs one seek and a walk of the
    /// matching run.
    ///
    /// Counting is bounded per repository - see [`COUNT_CEILING`] - so the cost
    /// of a page is bounded by `limit * CEILING` key reads and not by the size
    /// of the registry.
    async fn repository_summaries(
        &self,
        prefix: &str,
        last: Option<&str>,
        limit: usize,
    ) -> OpsResult<Page<RepoSummary>>;

    /// Counts and size for one repository.
    async fn repository_detail(&self, name: &str) -> OpsResult<RepoDetail>;

    /// Tags in name order, each resolved to the manifest it points at.
    async fn tag_details(
        &self,
        name: &str,
        last: Option<&str>,
        limit: usize,
    ) -> OpsResult<Page<TagInfo>>;

    /// Manifests in digest order, each with the tags pointing at it.
    async fn manifest_details(
        &self,
        name: &str,
        last: Option<&Digest>,
        limit: usize,
    ) -> OpsResult<Page<ManifestInfo>>;

    /// One manifest, or [`OpsError::ManifestUnknown`].
    async fn manifest_detail(&self, name: &str, reference: &Reference) -> OpsResult<ManifestInfo>;
}
