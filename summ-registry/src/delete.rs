//! Deletion, per `research/R1` §11.
//!
//! Two operations, and both must be *visible* the instant they return: the
//! conformance suite issues a `HEAD` immediately after each `202 Accepted` and
//! requires a `404`, with no retry and no grace period. One `WriteBatch` gives
//! that for free.
//!
//! Nothing here touches blob bytes. `DELETE /v2/<name>/blobs/<digest>` removes
//! the blob's membership of a repository; whether the bytes are reclaimed is
//! purge's business, and because a blob is only servable when `R` or `P` says
//! so, bytes lingering after the edges are gone are invisible.

use summ_core::{keys, Digest, Timestamp};
use summ_meta::WriteBatch;

use crate::error::{RegistryError, Result};
use crate::registry::{Planned, Registry};

/// Page size for draining a manifest's own edge ranges. These are fan-in sets
/// of one manifest - its tags, the indexes listing it - not registry-wide
/// scans, so they are small; the paging is there so a pathological case costs
/// memory linear in the page rather than in the set.
const DRAIN_PAGE: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestDeleted {
    pub digest: Digest,
    /// Tags that pointed at this manifest and have gone with it. The spec
    /// requires the cascade: after a delete by digest, "a GET to
    /// `/v2/<name>/manifests/<digest>` and any tag pointing to that digest will
    /// return a 404".
    pub removed_tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRefDeleted {
    pub digest: Digest,
    /// Whether the blob was in the repo's own blob set (`P`).
    pub was_member: bool,
    /// Reference edges from manifests in this repo that were dropped with it.
    pub references_removed: usize,
}

impl Registry {
    pub fn delete_manifest(
        &self,
        repo: &str,
        digest: &Digest,
        now: Timestamp,
    ) -> Result<ManifestDeleted> {
        let planned = self.plan_manifest_delete(repo, digest, now)?;
        self.engine().apply(&planned.batch)?;
        Ok(planned.outcome)
    }

    /// Remove a manifest and every edge that named it.
    ///
    /// `M`, `B`, one `R` per referenced blob, the `S` edges in both directions,
    /// the `F` edge to its subject, and every `T`/`G` pair pointing at it.
    /// After this the manifest is purgeable: nothing is left that would make it
    /// look referenced.
    ///
    /// The `R` and `S` edges are point-deleted rather than swept with
    /// `DeletePrefix`. A single manifest's edge set is on the order of ten
    /// keys, so a prefix delete would be strictly more work - and, being
    /// dependent on what is in the store at apply time rather than on the batch
    /// alone, it is the one op that does not replay cleanly out of order.
    ///
    /// What is deliberately *not* deleted: `F <repo> <this> <*>`, the referrers
    /// pointing *at* this manifest. Those referrers still exist as manifests,
    /// and the spec permits a subject to dangle. Nor is `P`, which is blob
    /// membership and outlives any single manifest.
    pub fn plan_manifest_delete(
        &self,
        repo: &str,
        digest: &Digest,
        now: Timestamp,
    ) -> Result<Planned<ManifestDeleted>> {
        let repo_id = self.require_repo(repo)?;
        let record = self.manifest_record(repo_id, digest)?.ok_or_else(|| {
            RegistryError::ManifestUnknown {
                repo: repo.to_string(),
                reference: digest.to_string(),
            }
        })?;

        let mut batch = WriteBatch::new();
        batch.delete(keys::manifest(repo_id, digest));
        batch.delete(keys::manifest_body(repo_id, digest));

        for blob in &record.layers {
            batch.delete(keys::blob_ref(blob, repo_id, digest));
        }
        for child in &record.children {
            batch.delete(keys::child_parent(repo_id, &child.digest, digest));
        }
        if let Some(subject) = record.subject {
            batch.delete(keys::referrer(repo_id, &subject, digest));
        }

        // The other direction of `S`: indexes that list *this* manifest as a
        // child. Leaving them would make `parents_of` report an edge to a
        // manifest that no longer exists.
        self.drain(&keys::parents_of(repo_id, digest), |key| {
            batch.delete(key.to_vec());
            Ok(())
        })?;

        let mut removed_tags = Vec::new();
        self.drain(&keys::tags_of_manifest(repo_id, digest), |key| {
            let tag = keys::parse_manifest_tag_suffix(key, digest)
                .ok_or_else(|| RegistryError::corrupt("manifest-tag key"))?;
            removed_tags.push(tag.to_string());
            Ok(())
        })?;
        for tag in &removed_tags {
            self.stage_delete_tag(&mut batch, repo_id, tag, digest, Some(&record), now)?;
        }

        Ok(Planned {
            outcome: ManifestDeleted {
                digest: *digest,
                removed_tags,
            },
            batch,
        })
    }

    pub fn delete_blob_reference(&self, repo: &str, digest: &Digest) -> Result<BlobRefDeleted> {
        let planned = self.plan_blob_reference_delete(repo, digest)?;
        self.engine().apply(&planned.batch)?;
        Ok(planned.outcome)
    }

    /// `DELETE /v2/<name>/blobs/<digest>` - drop the blob's membership of one
    /// repository.
    ///
    /// Both halves of the servability predicate have to go, not just `P`:
    /// leaving an `R` edge behind would keep the blob servable under this name
    /// and fail the suite's immediate `HEAD`-after-delete check. In the suite's
    /// own ordering the manifests are already gone by this point, so there is
    /// usually nothing to drop; a client deleting out of order gets the
    /// documented post-condition anyway.
    pub fn plan_blob_reference_delete(
        &self,
        repo: &str,
        digest: &Digest,
    ) -> Result<Planned<BlobRefDeleted>> {
        let repo_id = self.require_repo(repo)?;
        let was_member = self
            .engine()
            .exists_prefix(&keys::repo_blob(repo_id, digest))?;

        let mut batch = WriteBatch::new();
        let mut references_removed = 0usize;
        self.drain(&keys::blob_refs_in_repo(digest, repo_id), |key| {
            references_removed += 1;
            batch.delete(key.to_vec());
            Ok(())
        })?;

        if !was_member && references_removed == 0 {
            return Err(RegistryError::BlobUnknown {
                repo: repo.to_string(),
                digest: *digest,
            });
        }
        if was_member {
            batch.delete(keys::repo_blob(repo_id, digest));
        }

        Ok(Planned {
            outcome: BlobRefDeleted {
                digest: *digest,
                was_member,
                references_removed,
            },
            batch,
        })
    }

    /// Walk every key under `prefix`, a page at a time.
    ///
    /// Internal only, and only over a single object's own edges. The
    /// no-unbounded-list rule is about what this crate exposes; a delete that
    /// cascaded over only the first page of its tags would simply be wrong.
    fn drain(&self, prefix: &[u8], mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            let page = self
                .engine()
                .scan_keys(prefix, cursor.as_deref(), DRAIN_PAGE)?;
            for key in &page.keys {
                f(key)?;
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(()),
            }
        }
    }
}
