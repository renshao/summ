//! The registry facade: manifest push, pull, existence, and blob authorisation.

use std::sync::Arc;

use sha2::{Digest as _, Sha256, Sha512};
use summ_core::{
    keys, BlobRecord, Digest, ManifestRecord, ReferrerRecord, RepoBlobRecord, RepoId, Timestamp,
};
use summ_meta::{MetaEngine, RepoInterner, WriteBatch};

use crate::codec::{compress_body, decode, decompress_body, encode};
use crate::error::{RegistryError, Result};
use crate::manifest::{self, ParsedManifest};
use crate::reference::Reference;
use crate::tags::TagTarget;

/// A built but unapplied mutation, and the result applying it will produce.
///
/// Every mutating operation is available in this form so that callers can fold
/// several into one batch - a push that sets more than one tag, say - and still
/// commit exactly once. The plain form of each operation is `plan` followed by
/// [`MetaEngine::apply`], and nothing else.
#[derive(Debug, Clone)]
pub struct Planned<T> {
    pub outcome: T,
    pub batch: WriteBatch,
}

#[derive(Debug, Clone)]
pub struct RegistryOptions {
    /// Reject a push whose blobs or child manifests are not already present in
    /// the repository.
    ///
    /// The spec makes this optional ("A registry MAY reject a manifest ...
    /// with descriptors in other fields that reference a manifest or blob that
    /// does not exist"), and R1 recommends leaving it off: it costs N point
    /// lookups per push and it makes a concurrent layer-and-manifest push
    /// fail. It defaults on here because a registry that silently accepts a
    /// manifest it cannot serve trades a push-time 400 for a pull-time 404,
    /// and the pull-time failure is the one nobody can diagnose. Turn it off
    /// for the conformance suite's sparse data sets, which push exactly that.
    ///
    /// A dangling `subject` is never affected: the spec *requires* it to be
    /// accepted so that a referrer and its subject may be pushed in either
    /// order.
    pub validate_references: bool,

    /// Largest manifest document accepted. zot uses 4 MiB and so do we; a
    /// manifest is a descriptor list, and one larger than this is an attack or
    /// a bug.
    pub max_manifest_bytes: usize,
}

impl Default for RegistryOptions {
    fn default() -> Self {
        Self {
            validate_references: true,
            max_manifest_bytes: 4 * 1024 * 1024,
        }
    }
}

pub struct Registry {
    engine: Arc<dyn MetaEngine>,
    interner: RepoInterner,
    options: RegistryOptions,
}

/// What `HEAD /v2/<name>/manifests/<ref>` needs, and nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestHead {
    pub digest: Digest,
    pub media_type: String,
    pub size: u64,
}

/// A manifest and the bytes exactly as they were pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredManifest {
    pub digest: Digest,
    pub media_type: String,
    pub body: Vec<u8>,
}

/// `PUT /v2/<name>/manifests/<reference>`.
#[derive(Debug, Clone)]
pub struct ManifestPut<'a> {
    pub repo: &'a str,
    pub reference: &'a Reference,
    /// The document as received. Stored byte-exact, because the digest is over
    /// these bytes.
    pub body: &'a [u8],
    /// `Content-Type` of the push, with any parameters already stripped.
    pub content_type: Option<&'a str>,
    /// Unix seconds, supplied by the caller so the batch carries no clock read.
    pub now: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    pub digest: Digest,
    pub media_type: String,
    pub size: u64,
    /// Echo in `OCI-Subject` when set: the spec makes that header a MUST for a
    /// registry that supports the referrers API.
    pub subject: Option<Digest>,
    /// The tag this push set, if the reference was one.
    pub tag: Option<String>,
    /// Digest the tag pointed at before this push, if it moved.
    pub displaced: Option<Digest>,
}

impl Registry {
    pub fn new(engine: Arc<dyn MetaEngine>) -> Self {
        Self::with_options(engine, RegistryOptions::default())
    }

    pub fn with_options(engine: Arc<dyn MetaEngine>, options: RegistryOptions) -> Self {
        Self {
            engine,
            interner: RepoInterner::default(),
            options,
        }
    }

    pub fn engine(&self) -> &dyn MetaEngine {
        &*self.engine
    }

    pub fn options(&self) -> &RegistryOptions {
        &self.options
    }

    // --- repositories ---------------------------------------------------

    /// Resolve a repo name to its interned id without creating it.
    ///
    /// A read must never intern: allocating an id on a `GET` would make every
    /// 404 leave a repository behind, and `_catalog` would fill with names
    /// nobody ever pushed.
    pub fn lookup_repo(&self, name: &str) -> Result<Option<RepoId>> {
        Ok(self.interner.lookup(&*self.engine, name)?)
    }

    pub(crate) fn require_repo(&self, name: &str) -> Result<RepoId> {
        self.lookup_repo(name)?
            .ok_or_else(|| RegistryError::NameUnknown {
                repo: name.to_string(),
            })
    }

    pub(crate) fn intern_repo(&self, name: &str) -> Result<RepoId> {
        Ok(self.interner.intern(&*self.engine, name)?)
    }

    pub(crate) fn repo_name(&self, id: RepoId) -> Result<String> {
        self.interner
            .resolve(&*self.engine, id)?
            .ok_or_else(|| RegistryError::corrupt("repo id with no name"))
    }

    pub fn repo_exists(&self, name: &str) -> Result<bool> {
        Ok(self.lookup_repo(name)?.is_some())
    }

    // --- blob authorisation ---------------------------------------------

    /// Whether this repo may serve this blob.
    ///
    /// A blob is servable under a repo only if some manifest in that repo
    /// references it (`R <digest> <repo>` is non-empty) or it was uploaded
    /// there (`P <repo> <digest>` exists). Existence of `L` alone is *not*
    /// enough and must never be used: blob content is deduplicated
    /// registry-wide, so serving on `L` would let any repo name pull any
    /// private layer in the store by digest.
    ///
    /// The same predicate is what a push validates its layers against, which is
    /// deliberate: a manifest may only reference blobs the repo could already
    /// serve.
    pub fn blob_is_servable(&self, repo: &str, digest: &Digest) -> Result<bool> {
        match self.lookup_repo(repo)? {
            Some(id) => self.blob_is_servable_id(id, digest),
            None => Ok(false),
        }
    }

    pub(crate) fn blob_is_servable_id(&self, repo: RepoId, digest: &Digest) -> Result<bool> {
        if self
            .engine
            .exists_prefix(&keys::blob_refs_in_repo(digest, repo))?
        {
            return Ok(true);
        }
        Ok(self.engine.exists_prefix(&keys::repo_blob(repo, digest))?)
    }

    /// Blob metadata, but only if this repo is allowed to serve it. `None`
    /// covers both "no such blob" and "not this repo's blob", which are the
    /// same 404 to a client and must not be distinguishable.
    pub fn servable_blob(&self, repo: &str, digest: &Digest) -> Result<Option<BlobRecord>> {
        if !self.blob_is_servable(repo, digest)? {
            return Ok(None);
        }
        self.blob_record(digest)
    }

    /// Record a blob as present in a repository: `L` for the content and `P`
    /// for the membership.
    ///
    /// Called once the bytes have landed and been fsynced, never before - a
    /// blob with no metadata is garbage that purge reclaims, while metadata
    /// pointing at bytes that are not there is corruption that surfaces as a
    /// failed pull.
    ///
    /// This is also cross-repository mount: mounting is nothing more than
    /// adding a `P` edge under the target name, once the caller has checked
    /// with [`Registry::blob_is_servable`] that the *source* repo was entitled
    /// to the blob in the first place.
    pub fn commit_blob(
        &self,
        repo: &str,
        digest: &Digest,
        size: u64,
        now: Timestamp,
    ) -> Result<()> {
        let planned = self.plan_blob_commit(repo, digest, size, now)?;
        self.engine.apply(&planned.batch)?;
        Ok(())
    }

    pub fn plan_blob_commit(
        &self,
        repo: &str,
        digest: &Digest,
        size: u64,
        now: Timestamp,
    ) -> Result<Planned<()>> {
        let repo_id = self.intern_repo(repo)?;
        let mut batch = WriteBatch::new();
        batch.put(keys::blob(digest), encode(&BlobRecord { size })?);
        // `added_at` is left alone if the blob is already a member: it is the
        // grace clock for reclaiming an unreferenced blob, and re-uploading
        // must not restart it indefinitely.
        if !self
            .engine
            .exists_prefix(&keys::repo_blob(repo_id, digest))?
        {
            batch.put(
                keys::repo_blob(repo_id, digest),
                encode(&RepoBlobRecord {
                    size,
                    added_at: now.secs(),
                })?,
            );
        }
        Ok(Planned { outcome: (), batch })
    }

    /// The global `L` record, ignoring repository membership entirely.
    ///
    /// **Never gate serving on this.** Blob content is deduplicated
    /// registry-wide, so `L` says only that the bytes exist somewhere; using it
    /// to answer a `GET` would let any repository name pull any layer in the
    /// store by digest. [`Registry::servable_blob`] is the predicate for that,
    /// and it is not this one.
    ///
    /// It exists for the two callers that legitimately ask a registry-wide
    /// question: anonymous cross-repository mount, where the spec's whole point
    /// is that the client need not know where the blob already lives, and
    /// purge, which reasons about content rather than about names.
    pub fn blob_metadata(&self, digest: &Digest) -> Result<Option<BlobRecord>> {
        self.blob_record(digest)
    }

    pub(crate) fn blob_record(&self, digest: &Digest) -> Result<Option<BlobRecord>> {
        match self.engine.get(&keys::blob(digest))? {
            Some(raw) => Ok(Some(decode(&raw, "BlobRecord")?)),
            None => Ok(None),
        }
    }

    // --- manifest reads -------------------------------------------------

    pub(crate) fn manifest_record(
        &self,
        repo: RepoId,
        digest: &Digest,
    ) -> Result<Option<ManifestRecord>> {
        match self.engine.get(&keys::manifest(repo, digest))? {
            Some(raw) => Ok(Some(decode(&raw, "ManifestRecord")?)),
            None => Ok(None),
        }
    }

    /// Resolve a reference to a digest without reading the manifest.
    pub(crate) fn resolve(&self, repo: RepoId, reference: &Reference) -> Result<Option<Digest>> {
        match reference {
            Reference::Digest(d) => Ok(Some(*d)),
            Reference::Tag(t) => Ok(self.tag_record(repo, t)?.map(|r| r.digest)),
        }
    }

    /// `HEAD /v2/<name>/manifests/<reference>`, as a first-class path.
    ///
    /// Two point lookups for a tag - `T` then `M` - and one for a digest. The
    /// body is never touched. This matters more than it looks: four of the five
    /// serial steps in a cold containerd pull are metadata lookups and their
    /// latencies add, so implementing this as "get the manifest and throw the
    /// body away" would put a `B` read and a zstd decompression on the critical
    /// path of every pull for nothing.
    pub fn head_manifest(&self, repo: &str, reference: &Reference) -> Result<Option<ManifestHead>> {
        let repo_id = self.require_repo(repo)?;
        let Some(digest) = self.resolve(repo_id, reference)? else {
            return Ok(None);
        };
        Ok(self
            .manifest_record(repo_id, &digest)?
            .map(|r| ManifestHead {
                digest,
                media_type: r.media_type,
                size: r.size,
            }))
    }

    pub fn get_manifest_by_tag(&self, repo: &str, tag: &str) -> Result<Option<StoredManifest>> {
        let repo_id = self.require_repo(repo)?;
        let Some(record) = self.tag_record(repo_id, tag)? else {
            return Ok(None);
        };
        self.stored_manifest(repo_id, &record.digest)
    }

    pub fn get_manifest_by_digest(
        &self,
        repo: &str,
        digest: &Digest,
    ) -> Result<Option<StoredManifest>> {
        let repo_id = self.require_repo(repo)?;
        self.stored_manifest(repo_id, digest)
    }

    /// The full record, for the discovery API rather than for a pull.
    pub fn get_manifest_record(
        &self,
        repo: &str,
        digest: &Digest,
    ) -> Result<Option<ManifestRecord>> {
        let repo_id = self.require_repo(repo)?;
        self.manifest_record(repo_id, digest)
    }

    fn stored_manifest(&self, repo: RepoId, digest: &Digest) -> Result<Option<StoredManifest>> {
        let Some(record) = self.manifest_record(repo, digest)? else {
            return Ok(None);
        };
        // `M` without `B` is corruption, not a miss: they are written in one
        // batch and deleted in one batch.
        let stored = self
            .engine
            .get(&keys::manifest_body(repo, digest))?
            .ok_or_else(|| RegistryError::corrupt("manifest record with no body"))?;
        Ok(Some(StoredManifest {
            digest: *digest,
            media_type: record.media_type,
            body: decompress_body(&stored)?,
        }))
    }

    // --- manifest push ---------------------------------------------------

    pub fn put_manifest(&self, req: &ManifestPut<'_>) -> Result<PushOutcome> {
        let planned = self.plan_manifest_put(req)?;
        self.engine.apply(&planned.batch)?;
        Ok(planned.outcome)
    }

    /// Build the batch a manifest push commits.
    ///
    /// One batch touches `M`, `B`, `L`/`R`/`P` for every referenced blob, `S`
    /// for an index's children, `F` for a subject, and - when the reference is
    /// a tag - `T`, `G` and the `H`/`J` history pair. Atomicity across all of
    /// them is the point: a half-applied push is a manifest that resolves but
    /// cannot be served, or a layer pinned by an edge to a manifest that does
    /// not exist.
    ///
    /// Interning the repo name is the one write this does outside the returned
    /// batch. It has to be: no key for the repo can be encoded until its id
    /// exists. It is idempotent and independently atomic.
    pub fn plan_manifest_put(&self, req: &ManifestPut<'_>) -> Result<Planned<PushOutcome>> {
        self.plan_manifest_put_tagged(req, &[])
    }

    /// A push that also applies the `?tag=` parameters of end-7b.
    ///
    /// The extra tags cannot be a second call to
    /// [`Registry::plan_set_tag`]: that one requires the manifest to be stored
    /// already, and here it exists only inside the batch being built. Staging
    /// them from the parsed body instead is what keeps the whole push - the
    /// manifest, its edges, and every tag it lands under - a single atomic
    /// batch. Applying the manifest first and the tags afterwards would leave a
    /// crash window in which the digest resolves but the tag the client was
    /// told about does not.
    pub fn plan_manifest_put_tagged(
        &self,
        req: &ManifestPut<'_>,
        extra_tags: &[String],
    ) -> Result<Planned<PushOutcome>> {
        if req.body.len() > self.options.max_manifest_bytes {
            return Err(RegistryError::invalid(format!(
                "manifest is {} bytes, limit is {}",
                req.body.len(),
                self.options.max_manifest_bytes
            )));
        }

        let digest = digest_body(req.body, req.reference.as_digest());
        if let Some(claimed) = req.reference.as_digest() {
            if *claimed != digest {
                return Err(RegistryError::DigestInvalid {
                    reason: format!("body digests to {digest}, not the {claimed} it was pushed as"),
                });
            }
        }

        let parsed = manifest::parse(req.body, req.content_type)?;
        let repo_id = self.intern_repo(req.repo)?;
        let mut batch = WriteBatch::new();

        self.stage_blobs(&mut batch, req.repo, repo_id, &digest, &parsed, req.now)?;
        self.stage_children(&mut batch, req.repo, repo_id, &digest, &parsed)?;

        let record = ManifestRecord {
            repo: repo_id,
            digest,
            media_type: parsed.media_type.clone(),
            size: req.body.len() as u64,
            total_layer_size: parsed.total_layer_size(),
            // An image manifest carries no platform of its own - it is in the
            // config blob, which this layer does not read. An index supplies
            // its children's platforms through `ChildRef` instead.
            platform: None,
            layers: parsed.blobs.iter().map(|b| b.digest).collect(),
            children: parsed.children.clone(),
            subject: parsed.subject,
            artifact_type: parsed.artifact_type.clone(),
            annotations: parsed.annotations.clone(),
            pushed_at: req.now.secs(),
        };
        batch.put(keys::manifest(repo_id, &digest), encode(&record)?);
        batch.put(
            keys::manifest_body(repo_id, &digest),
            compress_body(req.body)?,
        );

        let referrer = ReferrerRecord {
            media_type: parsed.media_type.clone(),
            artifact_type: parsed.referrer_artifact_type.clone(),
            size: record.size,
            annotations: parsed.annotations.clone(),
        };

        // A subject that does not resolve is explicitly legal - the spec
        // requires a registry to accept a referrer pushed before its subject -
        // so this edge is written with no existence check at all.
        if let Some(subject) = parsed.subject {
            batch.put(
                keys::referrer(repo_id, &subject, &digest),
                encode(&referrer)?,
            );
        }

        let mut displaced = None;
        if req.reference.as_tag().is_some() || !extra_tags.is_empty() {
            let target = TagTarget {
                digest,
                media_type: parsed.media_type.clone(),
                size: record.size,
                referrer,
            };
            // The reference's own tag first, so it is the one whose displaced
            // digest is reported; a `?tag=` repeating it stages identical keys
            // and is skipped rather than written twice.
            let mut applied: Vec<&str> = Vec::with_capacity(extra_tags.len() + 1);
            for tag in req
                .reference
                .as_tag()
                .into_iter()
                .chain(extra_tags.iter().map(String::as_str))
            {
                if applied.contains(&tag) {
                    continue;
                }
                let previous = self.stage_set_tag(&mut batch, repo_id, tag, &target, req.now)?;
                if applied.is_empty() {
                    displaced = previous;
                }
                applied.push(tag);
            }
        }

        Ok(Planned {
            outcome: PushOutcome {
                digest,
                media_type: parsed.media_type,
                size: record.size,
                subject: parsed.subject,
                tag: req.reference.as_tag().map(str::to_string),
                displaced,
            },
            batch,
        })
    }

    /// `L`, `P` and `R` for every blob the manifest references.
    fn stage_blobs(
        &self,
        batch: &mut WriteBatch,
        repo_name: &str,
        repo: RepoId,
        manifest_digest: &Digest,
        parsed: &ParsedManifest,
        now: Timestamp,
    ) -> Result<()> {
        for desc in &parsed.blobs {
            let known = self.blob_record(&desc.digest)?;
            let in_repo = self.blob_is_servable_id(repo, &desc.digest)?;

            // A foreign layer names its content's real home in `urls`, and the
            // spec does not expect a registry to hold it. Requiring the blob
            // would reject every Windows base image; recording `L`, `P` or `R`
            // for it would be worse - those keys are what make a blob servable,
            // so the edges would advertise bytes that are not on disk and turn
            // a pull into a 500. Absent and foreign means no validation and no
            // edges. Present anyway - a client is free to push one - and it is
            // an ordinary blob from here on.
            if desc.foreign && !(known.is_some() && in_repo) {
                continue;
            }

            if self.options.validate_references && !(known.is_some() && in_repo) {
                return Err(RegistryError::ManifestBlobUnknown {
                    repo: repo_name.to_string(),
                    digest: desc.digest,
                });
            }

            // A present `L` was written from the bytes that actually arrived,
            // so it outranks the descriptor, which is client-supplied and may
            // simply be wrong.
            let size = known.map_or(desc.size, |r| r.size);
            if known.is_none() {
                batch.put(keys::blob(&desc.digest), encode(&BlobRecord { size })?);
            }
            // Only when absent: `added_at` is the grace clock for reclaiming an
            // uploaded-but-unreferenced blob, and rewriting it would restart
            // that clock on every push touching the layer.
            if !self
                .engine
                .exists_prefix(&keys::repo_blob(repo, &desc.digest))?
            {
                batch.put(
                    keys::repo_blob(repo, &desc.digest),
                    encode(&RepoBlobRecord {
                        size,
                        added_at: now.secs(),
                    })?,
                );
            }
            batch.set(keys::blob_ref(&desc.digest, repo, manifest_digest));
        }
        Ok(())
    }

    /// `S <repo> <child> <parent>` for each entry of an index.
    fn stage_children(
        &self,
        batch: &mut WriteBatch,
        repo_name: &str,
        repo: RepoId,
        manifest_digest: &Digest,
        parsed: &ParsedManifest,
    ) -> Result<()> {
        for child in &parsed.children {
            if self.options.validate_references
                && !self
                    .engine
                    .exists_prefix(&keys::manifest(repo, &child.digest))?
            {
                return Err(RegistryError::ManifestBlobUnknown {
                    repo: repo_name.to_string(),
                    digest: child.digest,
                });
            }
            batch.set(keys::child_parent(repo, &child.digest, manifest_digest));
        }
        Ok(())
    }
}

/// Digest the pushed bytes.
///
/// The algorithm follows the reference when the push named a digest, so a
/// sha512 push is checked as sha512; otherwise sha256, which the spec mandates
/// and every client uses.
fn digest_body(body: &[u8], like: Option<&Digest>) -> Digest {
    match like {
        Some(Digest::Sha512(_)) => {
            let out = Sha512::digest(body);
            let mut raw = [0u8; 64];
            raw.copy_from_slice(&out);
            Digest::Sha512(raw)
        }
        _ => {
            let out = Sha256::digest(body);
            let mut raw = [0u8; 32];
            raw.copy_from_slice(&out);
            Digest::Sha256(raw)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_digest_follows_the_reference_algorithm() {
        let body = b"{}";
        assert!(matches!(digest_body(body, None), Digest::Sha256(_)));
        assert!(matches!(
            digest_body(body, Some(&Digest::Sha512([0; 64]))),
            Digest::Sha512(_)
        ));
        // sha256 of "{}" - the OCI empty descriptor, a value worth pinning.
        assert_eq!(
            digest_body(body, None).to_string(),
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }
}
