//! The real [`Registry`] implementation: `summ-registry` over `summ-meta`, with
//! `summ-storage` holding the bytes.
//!
//! This is the whole of package K. The four crates were built concurrently
//! against a fixed key schema, which is why [`seam::Registry`] exists at all;
//! this module is the one place where they meet, and it is deliberately thin -
//! it translates, it does not decide. Every spec decision already lives above
//! it in the handlers, and every schema decision below it in the ops layer.
//!
//! Three rules it exists to enforce, none of which either layer could enforce
//! alone:
//!
//! - **Bytes land before metadata.** Every write path here fsyncs the blob
//!   through [`BlobStore`] and only then applies a [`WriteBatch`]. An orphan
//!   blob is garbage that purge reclaims; metadata naming a blob that is not
//!   there is corruption that surfaces as a failed pull, days later, to
//!   somebody else. A manifest push obeys the same order for its own document,
//!   which is why it plans its batch and applies it as two steps rather than
//!   one. See [`Backend::archive_manifest`].
//! - **A pull streams.** `get_blob` hands back a [`BlobStream`] over 1 MiB
//!   `pread`s, never a buffered body. containerd 2.1+ opens `bytes=N-`, reads
//!   8 MiB and drops the connection, so buffering a 900 MB layer to answer it
//!   is the pathological case, not the rare one.
//! - **Failures arrive in spec vocabulary.** [`OpsError`] is the whole
//!   contract; `RegistryError` and `SummError` stop here. That is what keeps
//!   the handlers testable against `memory` and the ops layer testable without
//!   a server.
//!
//! [`WriteBatch`]: summ_meta::WriteBatch
//! [`BlobStream`]: summ_storage::BlobStream
//! [`seam::Registry`]: crate::seam::Registry

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use futures_util::StreamExt;
use summ_core::{Digest, ManifestRecord, Platform, SummError};
use summ_meta::{MetaEngine, RedbEngine, RocksEngine};
use summ_registry::error::RegistryError;
use summ_registry::{Reference as OpsReference, Registry as Ops, RegistryOptions, UploadKey};
use summ_storage::{BlobStore, DigestAlgorithm, UploadId};

use crate::range::ByteRange;
use crate::reference::Reference;
use crate::seam::{
    BlobRead, Descriptor, ManifestInfo, ManifestPut, ManifestStat, OpsError, OpsResult, Page,
    Referrers, Registry, RepoDetail, RepoSummary, TagInfo, Tally, UploadBody, COUNT_CEILING,
    TAGS_PER_MANIFEST,
};

/// Which metadata engine `serve` opens.
///
/// RocksDB is the v1 decision. redb is not a fallback plan - it is the second
/// implementation that keeps [`MetaEngine`] honest, and being able to run the
/// whole binary on it is a stronger check than running the trait's own tests
/// against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Engine {
    Rocks,
    Redb,
}

/// How many keys one turn of a bounded count reads.
///
/// A count runs to [`COUNT_CEILING`] in steps of this, rather than asking the
/// engine for the ceiling in one call: the engine materialises the keys of a
/// page, so a single 10,000-key request would allocate ten thousand keys to
/// discard all of them. A step is one seek and a sequential block walk, which
/// is the cheap shape.
const COUNT_STEP: usize = 500;

/// `os/arch` or `os/arch/variant`. The variant is part of the identity - a
/// `linux/arm64` and a `linux/arm64/v8` image are different images - so it is
/// rendered rather than dropped.
fn platform_label(platform: &Platform) -> String {
    match &platform.variant {
        Some(variant) => format!("{}/{}/{}", platform.os, platform.arch, variant),
        None => format!("{}/{}", platform.os, platform.arch),
    }
}

/// Walk a prefix in [`COUNT_STEP`] steps until it ends or [`COUNT_CEILING`]
/// does, whichever comes first.
///
/// `step` is handed the cursor and returns `(counted, next)`. The ceiling is
/// checked *after* adding a page rather than before requesting one, so a repo
/// with exactly the ceiling's worth of keys reports a complete count instead of
/// a floor that happens to be right.
fn count_to_ceiling<C, F>(mut step: F) -> summ_registry::Result<Tally>
where
    F: FnMut(Option<&C>) -> summ_registry::Result<(u64, Option<C>)>,
{
    let mut count = 0u64;
    let mut cursor: Option<C> = None;
    loop {
        let (added, next) = step(cursor.as_ref())?;
        count += added;
        match next {
            None => return Ok(Tally::exact(count)),
            Some(_) if count >= COUNT_CEILING => {
                return Ok(Tally {
                    count,
                    complete: false,
                })
            }
            Some(next) => cursor = Some(next),
        }
    }
}

fn count_tags(ops: &Ops, repo: &str) -> summ_registry::Result<Tally> {
    count_to_ceiling(|cursor: Option<&String>| {
        let page = ops.count_tags(repo, cursor.map(String::as_str), COUNT_STEP)?;
        Ok((page.tags, page.next))
    })
}

fn count_manifests(ops: &Ops, repo: &str) -> summ_registry::Result<Tally> {
    count_to_ceiling(|cursor: Option<&Digest>| {
        let page = ops.count_manifests(repo, cursor, COUNT_STEP)?;
        Ok((page.manifests, page.next))
    })
}

/// Blob count and byte total, folded to the same ceiling.
///
/// `repo_usage` is paged for the same reason everything else is: there is no
/// stored total, deliberately, because keeping one would be a read-modify-write
/// on the push path.
fn count_usage(ops: &Ops, repo: &str) -> summ_registry::Result<(Tally, u64)> {
    let mut bytes = 0u64;
    let blobs = count_to_ceiling(|cursor: Option<&Digest>| {
        let page = ops.repo_usage(repo, cursor, COUNT_STEP)?;
        bytes = bytes.saturating_add(page.bytes);
        Ok((page.blobs, page.next))
    })?;
    Ok((blobs, bytes))
}

/// A stored record plus the two things a list row needs that it does not hold.
fn manifest_info(
    ops: &Ops,
    repo: &str,
    record: ManifestRecord,
) -> summ_registry::Result<ManifestInfo> {
    // An image manifest carries its own platform; an index carries none and
    // its children carry theirs. Deduplicated because an index may list the
    // same platform twice - an attestation manifest alongside an image, say.
    let mut platforms: Vec<String> = record.platform.iter().map(platform_label).collect();
    for child in &record.children {
        if let Some(label) = child.platform.as_ref().map(platform_label) {
            if !platforms.contains(&label) {
                platforms.push(label);
            }
        }
    }
    let tags = ops
        .tags_of_manifest(repo, &record.digest, None, TAGS_PER_MANIFEST)?
        .tags;

    Ok(ManifestInfo {
        digest: record.digest,
        media_type: record.media_type,
        size: record.size,
        blob_size: record.total_layer_size,
        artifact_type: record.artifact_type,
        subject: record.subject,
        pushed_at: record.pushed_at,
        platforms,
        blobs: record.layers.len() as u64,
        children: record.children.len() as u64,
        tags,
        annotations: record.annotations,
    })
}

pub struct Backend {
    ops: Arc<Ops>,
    blobs: BlobStore,
}

impl Backend {
    /// Open a registry rooted at `data_dir`: `meta/` for the engine, `blobs/`
    /// for committed content and `uploads/` for content still arriving.
    ///
    /// The blob store is rooted at `data_dir` rather than at a subdirectory of
    /// it because it owns both of those names, and an upload is committed by
    /// renaming across them - which is only atomic while they share a
    /// filesystem.
    ///
    /// Opening stamps the schema version on a fresh store and refuses one
    /// written by a newer build. That check is cheap here and impossible to
    /// retrofit: a populated store with no version marker cannot be told apart
    /// from one written before versioning existed.
    pub fn open(data_dir: &Path, engine: Engine, options: RegistryOptions) -> Result<Self, String> {
        let meta_dir = data_dir.join("meta");
        std::fs::create_dir_all(&meta_dir).map_err(|e| format!("creating {meta_dir:?}: {e}"))?;

        let migrations = summ_meta::Migrations::new();
        let engine: Arc<dyn MetaEngine> = match engine {
            Engine::Rocks => Arc::new(
                summ_meta::version::open(
                    RocksEngine::open(&meta_dir).map_err(|e| format!("opening RocksDB: {e}"))?,
                    &migrations,
                )
                .map_err(|e| format!("opening metadata store: {e}"))?,
            ),
            Engine::Redb => Arc::new(
                summ_meta::version::open(
                    RedbEngine::open(meta_dir.join("summ.redb"))
                        .map_err(|e| format!("opening redb: {e}"))?,
                    &migrations,
                )
                .map_err(|e| format!("opening metadata store: {e}"))?,
            ),
        };

        let blobs = BlobStore::open(data_dir).map_err(|e| format!("opening blob store: {e}"))?;
        Ok(Backend {
            ops: Arc::new(Ops::with_options(engine, options)),
            blobs,
        })
    }

    /// Unix seconds, read here and passed down.
    ///
    /// The ops layer never reads a clock: a `WriteBatch` carrying an
    /// apply-time timestamp would mean something different on a replica than it
    /// did here, and the batch is the future WAL. So the clock is read exactly
    /// once per request, at the top, and the value travels with the operation.
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Run a blocking metadata operation off the reactor.
    ///
    /// Everything in `summ-registry` is synchronous, and the writes among it
    /// reach RocksDB's WAL, so calling them inline would park a tokio worker on
    /// an fsync. Reads are left inline deliberately - they are overwhelmingly
    /// block-cache hits measured in microseconds, and a `spawn_blocking` round
    /// trip would cost more than the lookup it protects. Phase 3 is where that
    /// assumption gets measured rather than asserted.
    async fn write<T, F>(&self, f: F) -> OpsResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Ops) -> summ_registry::Result<T> + Send + 'static,
    {
        self.offload(f).await
    }

    /// Run a bounded *scan* off the reactor.
    ///
    /// The read policy above is that a point lookup is cheaper than a
    /// `spawn_blocking` round trip, and it is. A discovery fold is the case it
    /// was not written for: counting a repository's manifests walks up to
    /// [`COUNT_CEILING`] keys, and a page of summaries does that once per
    /// repository, so it is milliseconds of CPU rather than microseconds. That
    /// belongs off the reactor whichever way the point-lookup bet turns out.
    async fn scan<T, F>(&self, f: F) -> OpsResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Ops) -> summ_registry::Result<T> + Send + 'static,
    {
        self.offload(f).await
    }

    async fn offload<T, F>(&self, f: F) -> OpsResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Ops) -> summ_registry::Result<T> + Send + 'static,
    {
        let ops = Arc::clone(&self.ops);
        tokio::task::spawn_blocking(move || f(&ops))
            .await
            .map_err(|e| OpsError::Internal(format!("metadata task failed: {e}")))?
            .map_err(ops_error)
    }
}

/// `RegistryError` in the vocabulary of the spec.
///
/// The mapping is deliberately lossy in one direction only: several distinct
/// storage conditions collapse onto one spec code, and none of them leaks a
/// storage concept upward. `NameUnknown` becoming `RepoUnknown` is the case
/// worth naming - the handlers turn it into `NAME_UNKNOWN`, which is what the
/// suite checks.
fn ops_error(e: RegistryError) -> OpsError {
    match e {
        RegistryError::NameUnknown { .. } => OpsError::RepoUnknown,
        RegistryError::ManifestUnknown { .. } => OpsError::ManifestUnknown,
        RegistryError::BlobUnknown { .. } => OpsError::BlobUnknown,
        RegistryError::DigestInvalid { .. } => OpsError::DigestMismatch,
        RegistryError::TagInvalid { tag, reason } => {
            OpsError::ManifestInvalid(format!("tag {tag:?}: {reason}"))
        }
        RegistryError::ManifestInvalid { reason } => OpsError::ManifestInvalid(reason),
        RegistryError::ManifestBlobUnknown { digest, .. } => {
            OpsError::ManifestBlobUnknown { digest }
        }
        RegistryError::Meta(e) => OpsError::Internal(e.to_string()),
    }
}

/// `SummError` from the blob store, likewise.
///
/// `InvalidDigest` is the one that matters: it is the commit-time verdict on
/// bytes the client claimed a digest for, and it must reach the client as a
/// `400 DIGEST_INVALID` rather than as a 500.
fn storage_error(e: SummError) -> OpsError {
    match e {
        SummError::NotFound => OpsError::BlobUnknown,
        SummError::InvalidDigest(_) => OpsError::DigestMismatch,
        SummError::InvalidData(m) | SummError::Storage(m) => OpsError::Internal(m),
    }
}

/// Write a request body through to the staging file, frame by frame.
///
/// This is the reason [`UploadBody`] exists. Buffering a layer to append it
/// would make a push cost as much memory as the blob is large, and the default
/// ceiling is 1 GiB - so a few concurrent pushes of a big image would be an
/// out-of-memory kill rather than a slow registry. Here the cost is one frame,
/// whatever the size of the blob.
///
/// Both limits are checked as the bytes arrive, because neither can be checked
/// before. The ceiling is enforced *before* the offending frame is written, so
/// an over-long body cannot fill a disk on its way to being rejected.
async fn drain_into(upload: &mut summ_storage::Upload, body: UploadBody) -> OpsResult<u64> {
    let UploadBody {
        body,
        declared,
        limit,
    } = body;
    let mut stream = body.into_data_stream();
    let mut written = 0u64;

    while let Some(frame) = stream.next().await {
        let chunk = frame.map_err(|e| OpsError::BodyIncomplete(e.to_string()))?;
        if chunk.is_empty() {
            continue;
        }
        written = written.saturating_add(chunk.len() as u64);
        if written > limit {
            return Err(OpsError::BodyTooLarge { limit });
        }
        // Each frame lands at wherever the previous one ended. The caller has
        // already checked the *session's* offset against the client's claim;
        // this is only the running position inside one body.
        let at = upload.offset();
        upload.append(at, chunk).await.map_err(storage_error)?;
    }

    if let Some(declared) = declared {
        if declared != written {
            return Err(OpsError::SizeMismatch {
                declared,
                actual: written,
            });
        }
    }
    Ok(written)
}

fn upload_key(id: &str) -> OpsResult<UploadKey> {
    Ops::parse_upload_id(id).map_err(|_| OpsError::UploadUnknown)
}

fn upload_id(id: &str) -> OpsResult<UploadId> {
    UploadId::new(id).map_err(|_| OpsError::UploadUnknown)
}

/// The server's [`Reference`] in the ops layer's own type.
///
/// Two types for one concept looks like duplication and is not: the HTTP one is
/// parsed from a path segment and carries the `:`-means-digest rule that decides
/// `DIGEST_INVALID` against `MANIFEST_UNKNOWN`, and the ops one is a storage
/// key. Neither crate should depend on the other's, which is exactly what the
/// seam is for.
fn as_ops_reference(reference: &Reference) -> OpsReference {
    match reference {
        Reference::Tag(t) => OpsReference::Tag(t.clone()),
        Reference::Digest(d) => OpsReference::Digest(*d),
    }
}

#[async_trait]
impl Registry for Backend {
    // ---- discovery -------------------------------------------------------

    async fn repositories(&self, last: Option<&str>, limit: usize) -> OpsResult<Page<String>> {
        let page = self.ops.list_repos(last, limit).map_err(ops_error)?;
        Ok(Page {
            more: page.next.is_some(),
            items: page.repos,
        })
    }

    async fn tags(&self, name: &str, last: Option<&str>, limit: usize) -> OpsResult<Page<String>> {
        let page = self.ops.list_tags(name, last, limit).map_err(ops_error)?;
        Ok(Page {
            more: page.next.is_some(),
            items: page.tags,
        })
    }

    // ---- manifests -------------------------------------------------------

    async fn stat_manifest(&self, name: &str, reference: &Reference) -> OpsResult<ManifestStat> {
        let head = self
            .ops
            .head_manifest(name, &as_ops_reference(reference))
            .map_err(ops_error)?
            .ok_or(OpsError::ManifestUnknown)?;
        Ok(ManifestStat {
            digest: head.digest,
            media_type: head.media_type,
            size: head.size,
        })
    }

    async fn get_manifest(
        &self,
        name: &str,
        reference: &Reference,
    ) -> OpsResult<(ManifestStat, Bytes)> {
        let stored = match reference {
            Reference::Tag(tag) => self.ops.get_manifest_by_tag(name, tag),
            Reference::Digest(digest) => self.ops.get_manifest_by_digest(name, digest),
        }
        .map_err(ops_error)?
        .ok_or(OpsError::ManifestUnknown)?;

        Ok((
            ManifestStat {
                digest: stored.digest,
                media_type: stored.media_type,
                size: stored.body.len() as u64,
            },
            Bytes::from(stored.body),
        ))
    }

    async fn put_manifest(
        &self,
        name: &str,
        reference: &Reference,
        content_type: &str,
        tags: &[String],
        body: Bytes,
    ) -> OpsResult<ManifestPut> {
        let now = self.now();
        let name = name.to_string();
        let reference = as_ops_reference(reference);
        let content_type = content_type.to_string();
        let tags = tags.to_vec();
        let echo = tags.clone();
        let document = body.clone();

        // Planned, not applied. Planning is where the body is parsed, the
        // reference checked against it and the digest computed, so it is also
        // the only place the digest is known before anything has been written.
        let planned = self
            .write(move |ops| {
                let req = summ_registry::ManifestPut {
                    repo: &name,
                    reference: &reference,
                    body: &body,
                    content_type: Some(&content_type),
                    now,
                };
                // The manifest, every edge it implies, and every tag it lands
                // under, in one batch. A push is atomic or it is a manifest
                // that resolves by digest under a tag that does not exist.
                ops.plan_manifest_put_tagged(&req, &tags)
            })
            .await?;

        // Then the archive copy, fsynced - see `archive_manifest`. It goes here
        // and not after the batch for the same reason a layer does: the batch
        // is the commit point, and everything the store will need must already
        // be on disk when it lands.
        self.archive_manifest(&planned.outcome.digest, document)
            .await?;

        let outcome = self
            .write(move |ops| {
                ops.engine().apply(&planned.batch)?;
                Ok(planned.outcome)
            })
            .await?;

        Ok(ManifestPut {
            digest: outcome.digest,
            subject: outcome.subject,
            tags: echo,
        })
    }

    async fn delete_manifest(&self, name: &str, reference: &Reference) -> OpsResult<()> {
        let now = self.now();
        let name = name.to_string();
        let reference = as_ops_reference(reference);
        self.write(move |ops| {
            match &reference {
                // A tag delete leaves the manifest reachable by digest, so it
                // is a tag operation and not a manifest one.
                OpsReference::Tag(tag) => {
                    ops.delete_tag(&name, tag, now)?;
                }
                // A digest delete cascades to every tag pointing at it, which
                // `plan_manifest_delete` stages into the same batch.
                OpsReference::Digest(digest) => {
                    ops.delete_manifest(&name, digest, now)?;
                }
            }
            Ok(())
        })
        .await
    }

    // ---- blobs -----------------------------------------------------------

    async fn stat_blob(&self, name: &str, digest: &Digest) -> OpsResult<u64> {
        // Repository membership first, and always: `L` alone says the bytes
        // exist somewhere in the registry, which is not permission to serve
        // them under this name.
        let record = self
            .ops
            .servable_blob(name, digest)
            .map_err(|_| OpsError::BlobUnknown)?
            .ok_or(OpsError::BlobUnknown)?;
        Ok(record.size)
    }

    async fn get_blob(
        &self,
        name: &str,
        digest: &Digest,
        window: Option<ByteRange>,
    ) -> OpsResult<BlobRead> {
        if !self
            .ops
            .blob_is_servable(name, digest)
            .map_err(|_| OpsError::BlobUnknown)?
        {
            return Err(OpsError::BlobUnknown);
        }

        let blob = self.blobs.open_blob(digest).await.map_err(storage_error)?;
        // The file's own length, not `L`'s: the range arithmetic has to agree
        // with the descriptor the read is actually issued against, and the
        // store is content-addressed so the two can only differ if something
        // is already wrong.
        let total_size = blob.size();

        let stream = match window {
            Some(range) => {
                let resolved = blob
                    .resolve(summ_storage::ByteRange::Inclusive {
                        start: range.start,
                        end: range.end,
                    })
                    // The handler resolved this window against `stat_blob`
                    // before asking, so an unsatisfiable range here means the
                    // two sizes disagree - corruption, not a client error.
                    .ok_or_else(|| {
                        OpsError::Internal(format!(
                            "range {}-{} outside blob {digest} of {total_size} bytes",
                            range.start, range.end
                        ))
                    })?;
                blob.stream_range(resolved)
            }
            None => blob.stream(),
        };

        Ok(BlobRead {
            total_size,
            window,
            body: Body::from_stream(stream),
        })
    }

    async fn delete_blob(&self, name: &str, digest: &Digest) -> OpsResult<()> {
        let name = name.to_string();
        let digest = *digest;
        self.write(move |ops| {
            if !ops.blob_is_servable(&name, &digest)? {
                return Err(RegistryError::BlobUnknown {
                    repo: name.clone(),
                    digest,
                });
            }
            ops.delete_blob_reference(&name, &digest)?;
            Ok(())
        })
        .await?;
        // The bytes stay. They may be shared with another repository, and
        // deciding that they are not is purge's job, not a DELETE handler's -
        // which is why this endpoint drops membership and nothing else.
        Ok(())
    }

    async fn mount_blob(&self, name: &str, digest: &Digest, from: Option<&str>) -> OpsResult<bool> {
        let now = self.now();
        let name = name.to_string();
        let from = from.map(str::to_string);
        let digest = *digest;
        self.write(move |ops| {
            let size = match &from {
                // Named source: the source repo must itself have been entitled
                // to the blob. Mounting out of a repo that could not serve it
                // would launder the content across a boundary.
                Some(from) => {
                    if !ops.blob_is_servable(from, &digest)? {
                        return Ok(None);
                    }
                    ops.blob_metadata(&digest)?.map(|r| r.size)
                }
                // Anonymous mount, which the spec permits: the question is
                // only whether the content exists at all, and `L` answers it
                // in one lookup.
                None => ops.blob_metadata(&digest)?.map(|r| r.size),
            };
            let Some(size) = size else {
                return Ok(None);
            };
            // Mounting is one `P` edge under the target name. Nothing is
            // copied, because content is addressed by digest and already
            // there.
            ops.commit_blob(&name, &digest, size, now)?;
            Ok(Some(()))
        })
        .await
        .map(|mounted| mounted.is_some())
    }

    // ---- uploads ---------------------------------------------------------

    async fn create_upload(&self, name: &str, id: &str, algorithm: &str) -> OpsResult<()> {
        let key = upload_key(id)?;
        let algo = DigestAlgorithm::from_name(algorithm)
            .map_err(|e| OpsError::ManifestInvalid(e.to_string()))?;

        // Staging file first, session record second. The reverse order would
        // leave a session pointing at a file that does not exist, which the
        // resume path cannot tell from a truncated one.
        self.blobs
            .create_upload(&upload_id(id)?, algo)
            .await
            .map_err(storage_error)?;

        let now = self.now();
        let name = name.to_string();
        let algorithm = algorithm.to_string();
        self.write(move |ops| {
            ops.create_upload(&name, &key, &algorithm, now)?;
            Ok(())
        })
        .await
    }

    async fn upload_offset(&self, name: &str, id: &str) -> OpsResult<u64> {
        let key = upload_key(id)?;
        let session = self
            .ops
            .get_upload_in(name, &key)
            .map_err(ops_error)?
            .ok_or(OpsError::UploadUnknown)?;
        Ok(session.offset)
    }

    async fn append_upload(
        &self,
        name: &str,
        id: &str,
        expected_offset: u64,
        body: UploadBody,
    ) -> OpsResult<u64> {
        let key = upload_key(id)?;
        let mut session = self
            .ops
            .get_upload_in(name, &key)
            .map_err(ops_error)?
            .ok_or(OpsError::UploadUnknown)?;

        // Checked before the file is touched. The spec requires a rejected
        // chunk to leave the session byte-identical, because the client
        // recovers by asking for the offset and retrying from it.
        if session.offset != expected_offset {
            return Err(OpsError::OffsetMismatch {
                current: session.offset,
            });
        }

        let mut upload = self.resume(id, &session).await?;
        // If this fails part-way the staging file is left long and the session
        // record is not written, so the recorded offset is unchanged and the
        // next resume truncates the excess. That is the same recovery a crash
        // would get, which is why a half-arrived body needs no special case.
        drain_into(&mut upload, body).await?;

        let now = self.now();
        session.offset = upload.offset();
        session.updated_at = now;
        session.hasher_state = Some(upload.hasher_state().map_err(storage_error)?);
        let offset = session.offset;

        // Bytes are on disk before the offset that describes them is
        // committed. A crash between the two leaves the staging file long,
        // which `resume_upload` truncates; the reverse would leave it short,
        // which it cannot repair.
        self.write(move |ops| {
            ops.save_upload(&key, &session)?;
            Ok(())
        })
        .await?;
        Ok(offset)
    }

    async fn finish_upload(
        &self,
        name: &str,
        id: &str,
        expected_offset: u64,
        body: UploadBody,
        digest: &Digest,
    ) -> OpsResult<()> {
        let key = upload_key(id)?;
        let session = self
            .ops
            .get_upload_in(name, &key)
            .map_err(ops_error)?
            .ok_or(OpsError::UploadUnknown)?;

        if session.offset != expected_offset {
            return Err(OpsError::OffsetMismatch {
                current: session.offset,
            });
        }
        let mut upload = self.resume(id, &session).await?;

        // The session was opened under one algorithm and the client has now
        // closed it with a digest in another. `?digest-algorithm=` is a SHOULD
        // (end-4c) and no client in the conformance suite sends it, so a
        // sha512 push arrives on a session hashing sha256 - and rejecting it
        // would be blaming the client for content that is perfectly good.
        // Rehash the staged bytes instead; this is a no-op on every push whose
        // algorithm already matches, and it happens before the closing chunk
        // so the hasher simply carries on from `offset`.
        self.blobs
            .rehash_upload(&mut upload, DigestAlgorithm::of(digest))
            .await
            .map_err(storage_error)?;

        drain_into(&mut upload, body).await?;

        // Commit fsyncs the bytes *and* the containing directory before it
        // returns, so the batch below is genuinely the commit point. On a
        // digest mismatch nothing is created and the session survives, which
        // is what lets the client retry rather than start over.
        let size = self
            .blobs
            .commit_upload(upload, digest)
            .await
            .map_err(storage_error)?;

        let now = self.now();
        let name = name.to_string();
        let digest = *digest;
        self.write(move |ops| {
            // One batch: the blob's `L`/`P` records and the retirement of the
            // session. Two batches would leave a window in which the blob is
            // servable but its upload could still be resumed onto.
            let planned = ops.plan_blob_commit(&name, &digest, size, now)?;
            let mut batch = planned.batch;
            batch.ops.extend(ops.plan_delete_upload(&key).batch.ops);
            ops.engine().apply(&batch)?;
            Ok(())
        })
        .await
    }

    async fn cancel_upload(&self, name: &str, id: &str) -> OpsResult<()> {
        let key = upload_key(id)?;
        self.ops
            .get_upload_in(name, &key)
            .map_err(ops_error)?
            .ok_or(OpsError::UploadUnknown)?;

        // Session record first this time: it is what makes the upload
        // findable, and an orphaned staging file is garbage rather than a
        // dangling reference.
        self.write(move |ops| {
            ops.delete_upload(&key)?;
            Ok(())
        })
        .await?;
        self.blobs
            .cancel_upload(&upload_id(id)?)
            .await
            .map_err(storage_error)
    }

    async fn put_blob(&self, name: &str, digest: &Digest, body: UploadBody) -> OpsResult<()> {
        // A staging id, not a batch value. The determinism rule is about what
        // goes *into* a `WriteBatch` - this name never does; it exists for as
        // long as it takes to rename the file to its digest.
        let id = UploadId::new(format!("single-{}", uuid::Uuid::new_v4()))
            .map_err(|e| OpsError::Internal(e.to_string()))?;
        let algo = DigestAlgorithm::of(digest);

        let mut upload = self
            .blobs
            .create_upload(&id, algo)
            .await
            .map_err(storage_error)?;
        let commit = async {
            drain_into(&mut upload, body).await?;
            self.blobs
                .commit_upload(upload, digest)
                .await
                .map_err(storage_error)
        }
        .await;

        let size = match commit {
            Ok(size) => size,
            Err(e) => {
                // Nothing above will ever ask about this id again, so a failed
                // single-shot push must not leave its bytes staged forever.
                let _ = self.blobs.cancel_upload(&id).await;
                return Err(e);
            }
        };

        let now = self.now();
        let name = name.to_string();
        let digest = *digest;
        self.write(move |ops| {
            ops.commit_blob(&name, &digest, size, now)?;
            Ok(())
        })
        .await
    }

    // ---- referrers -------------------------------------------------------

    /// Off the reactor, like the discovery folds and unlike the point lookups.
    ///
    /// A page here is a prefix scan plus one postcard decode per edge, which is
    /// the shape [`Backend::scan`] exists for: the inline-read bet is that a
    /// block-cache hit beats a `spawn_blocking` round trip, and it is a bet
    /// about a single lookup, not about a thousand of them.
    async fn referrers(
        &self,
        name: &str,
        subject: &Digest,
        artifact_type: Option<&str>,
        last: Option<&Digest>,
        limit: usize,
    ) -> OpsResult<Referrers> {
        let name = name.to_owned();
        let subject = *subject;
        let artifact_type = artifact_type.map(str::to_owned);
        let last = last.copied();
        self.scan(move |ops| {
            let list = ops.referrers(
                &name,
                &subject,
                artifact_type.as_deref(),
                last.as_ref(),
                limit,
            )?;
            Ok(Referrers {
                manifests: list
                    .entries
                    .into_iter()
                    .map(|entry| Descriptor {
                        media_type: entry.record.media_type,
                        digest: entry.digest,
                        size: entry.record.size,
                        artifact_type: entry.record.artifact_type,
                        annotations: entry.record.annotations,
                    })
                    .collect(),
                filter_applied: list.filter_applied,
                next: list.next,
            })
        })
        .await
    }

    // ---- discovery beyond the spec ---------------------------------------

    async fn repository_summaries(
        &self,
        prefix: &str,
        last: Option<&str>,
        limit: usize,
    ) -> OpsResult<Page<RepoSummary>> {
        let prefix = prefix.to_string();
        let last = last.map(str::to_string);
        self.scan(move |ops| {
            let page = ops.search_repos(&prefix, last.as_deref(), limit)?;
            let mut items = Vec::with_capacity(page.repos.len());
            for name in page.repos {
                items.push(RepoSummary {
                    tags: count_tags(ops, &name)?,
                    manifests: count_manifests(ops, &name)?,
                    name,
                });
            }
            Ok(Page {
                items,
                more: page.next.is_some(),
            })
        })
        .await
    }

    async fn repository_detail(&self, name: &str) -> OpsResult<RepoDetail> {
        let name = name.to_string();
        self.scan(move |ops| {
            // Not `repo_exists` first: every fold below already resolves the
            // name through the interner and raises `NameUnknown` if it cannot,
            // so a separate existence check would only be a fourth lookup
            // saying what the first one already said.
            let tags = count_tags(ops, &name)?;
            let manifests = count_manifests(ops, &name)?;
            let (blobs, size_bytes) = count_usage(ops, &name)?;
            Ok(RepoDetail {
                name,
                tags,
                manifests,
                blobs,
                size_bytes,
            })
        })
        .await
    }

    async fn tag_details(
        &self,
        name: &str,
        last: Option<&str>,
        limit: usize,
    ) -> OpsResult<Page<TagInfo>> {
        let name = name.to_string();
        let last = last.map(str::to_string);
        self.scan(move |ops| {
            let page = ops.list_tags(&name, last.as_deref(), limit)?;
            let mut items = Vec::with_capacity(page.tags.len());
            for tag in page.tags {
                // `T` then `M`, the same two point lookups a `HEAD` makes. The
                // list is already bounded, so this is bounded with it.
                let Some(record) = ops.get_tag(&name, &tag)? else {
                    // The key was in the scan and gone by the lookup: a delete
                    // landed between the two. Skipping is the honest answer -
                    // the tag no longer exists.
                    continue;
                };
                let manifest = match ops.get_manifest_record(&name, &record.digest)? {
                    Some(m) => Some(manifest_info(ops, &name, m)?),
                    None => None,
                };
                items.push(TagInfo {
                    name: tag,
                    digest: record.digest,
                    tagged_at: record.tagged_at,
                    manifest,
                });
            }
            Ok(Page {
                items,
                more: page.next.is_some(),
            })
        })
        .await
    }

    async fn manifest_details(
        &self,
        name: &str,
        last: Option<&Digest>,
        limit: usize,
    ) -> OpsResult<Page<ManifestInfo>> {
        let name = name.to_string();
        let last = last.copied();
        self.scan(move |ops| {
            let page = ops.list_manifests(&name, last.as_ref(), limit)?;
            let mut items = Vec::with_capacity(page.manifests.len());
            for record in page.manifests {
                items.push(manifest_info(ops, &name, record)?);
            }
            Ok(Page {
                items,
                more: page.next.is_some(),
            })
        })
        .await
    }

    async fn manifest_detail(&self, name: &str, reference: &Reference) -> OpsResult<ManifestInfo> {
        let name = name.to_string();
        let reference = as_ops_reference(reference);
        self.scan(move |ops| {
            let digest = match &reference {
                OpsReference::Digest(digest) => *digest,
                OpsReference::Tag(tag) => match ops.get_tag(&name, tag)? {
                    Some(record) => record.digest,
                    None => {
                        return Err(RegistryError::ManifestUnknown {
                            repo: name.clone(),
                            reference: tag.clone(),
                        })
                    }
                },
            };
            match ops.get_manifest_record(&name, &digest)? {
                Some(record) => manifest_info(ops, &name, record),
                None => Err(RegistryError::ManifestUnknown {
                    repo: name.clone(),
                    reference: digest.to_string(),
                }),
            }
        })
        .await
    }
}

impl Backend {
    /// Write a copy of a manifest document into the blob store, under its own
    /// digest.
    ///
    /// The first mitigation for Risk 0: manifest bytes otherwise live only
    /// under `B <repo> <digest>`, so a lost metadata store leaves a disk of
    /// blobs that nothing on it can identify - no way to tell a config from a
    /// layer, no way to tell which layers belong together, and no way to name
    /// any of it. A manifest is content-addressed by construction, so the copy
    /// is `digest -> bytes` and needs no new concept: the corpus becomes
    /// self-describing, because every manifest is discoverable in it and every
    /// manifest names its config and layers.
    ///
    /// Four decisions, all of which the next change here has to keep:
    ///
    /// - **No `L` or `P` record, deliberately.** Those are what make bytes
    ///   servable through `GET /v2/<name>/blobs/<digest>`, and a manifest is
    ///   not a blob of its repository: publishing one as though it were is a
    ///   client-visible change nothing asked for, and it would put manifest
    ///   bytes into the blob count and byte total `P` is folded for. The
    ///   consequence is that the copy has no metadata at all, so **purge must
    ///   retain a blob whose digest appears as a manifest digest.** `M` is the
    ///   record that keeps it - no second one is invented here, because the
    ///   blob store holds bytes and not relationships.
    /// - **The delete path does not remove it**, exactly as `delete_blob` does
    ///   not remove a layer's bytes. `M` is repo-scoped and the store is
    ///   global, so a manifest deleted from one repository may still be named
    ///   by another's `M`; deciding that nothing names it is purge's job and
    ///   needs the whole sweep to decide it.
    /// - **A failure here fails the push.** The copy is redundant - `B` is
    ///   still the read path - so this cannot corrupt anything, and a warning
    ///   would be tempting. But the state it would leave is metadata with no
    ///   copy, silently, which is the exact state the mitigation exists to
    ///   prevent and which nobody discovers until they are already recovering.
    ///   One small file on a filesystem the push has just written layers to
    ///   fails when the disk does, and then the push should fail too.
    /// - **A re-push is a no-op.** Content-addressed: if the digest is present
    ///   the bytes are already right, and they are complete, because a blob
    ///   only ever appears by a rename of a sealed and fsynced staging file.
    async fn archive_manifest(&self, digest: &Digest, body: Bytes) -> OpsResult<()> {
        if self.blobs.contains(digest).await.map_err(storage_error)? {
            return Ok(());
        }

        // A staging id, like the one `put_blob` mints, and for the same reason:
        // it never enters a `WriteBatch`, and it lives only as long as it takes
        // to rename the file to its digest.
        let id = UploadId::new(format!("manifest-{}", uuid::Uuid::new_v4()))
            .map_err(|e| OpsError::Internal(e.to_string()))?;
        let mut upload = self
            .blobs
            .create_upload(&id, DigestAlgorithm::of(digest))
            .await
            .map_err(storage_error)?;

        let commit = async {
            upload.append(0, body).await.map_err(storage_error)?;
            // The digest is recomputed from the bytes as they are written and
            // checked against the one planning derived from the same bytes, so
            // the copy cannot land under a name that is not its own.
            self.blobs
                .commit_upload(upload, digest)
                .await
                .map_err(storage_error)
        }
        .await;

        if let Err(e) = commit {
            // Bounded by `max_manifest_bytes`, but nothing above will ever ask
            // about this id again, so leaving it staged would leak it forever.
            let _ = self.blobs.cancel_upload(&id).await;
            return Err(e);
        }
        Ok(())
    }

    /// Reopen a session's staging file with its hasher rehydrated.
    ///
    /// Done per chunk rather than by keeping the handle in a map, and that is
    /// the point: the resume path is then the *only* path, so it is exercised
    /// by every ordinary upload instead of only by the rare crash. It also
    /// means a chunked upload can continue on any process, which is what keeps
    /// chunked uploads from becoming an HA constraint. The cost is one `open`
    /// and a 104-byte hasher restore per chunk, against a chunk that is
    /// megabytes.
    async fn resume(
        &self,
        id: &str,
        session: &summ_core::UploadSession,
    ) -> OpsResult<summ_storage::Upload> {
        let id = upload_id(id)?;
        let algo = DigestAlgorithm::from_name(&session.algorithm)
            .map_err(|e| OpsError::Internal(e.to_string()))?;
        // `None` state means nothing has been appended yet; the store starts a
        // fresh hasher, and refuses to do so at a non-zero offset.
        self.blobs
            .resume_upload(&id, algo, session.offset, session.hasher_state.as_deref())
            .await
            .map_err(storage_error)
    }
}
