//! Deletion, per `research/R1` §11.
//!
//! The two spec deletes must be *visible* the instant they return: the
//! conformance suite issues a `HEAD` immediately after each `202 Accepted` and
//! requires a `404`, with no retry and no grace period. One `WriteBatch` gives
//! that for free.
//!
//! Dropping a whole repository cannot be one batch, because the work is
//! unbounded - ten million manifests in one repository is the scale target -
//! and it splits along the grain of the key schema. Releasing the *name* is
//! O(1) and synchronous; everything keyed by the repo id is swept afterwards.
//! See [`Registry::delete_repository`].
//!
//! Nothing here touches blob bytes. `DELETE /v2/<name>/blobs/<digest>` removes
//! the blob's membership of a repository; whether the bytes are reclaimed is
//! purge's business, and because a blob is only servable when `R` or `P` says
//! so, bytes lingering after the edges are gone are invisible.

use summ_core::{keys, DeadRepo, Digest, ManifestRecord, RepoId, Timestamp};
use summ_meta::WriteBatch;

use crate::codec::{decode, encode};
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

// --- repositories ------------------------------------------------------

/// A repository whose name has been released but whose keys have not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTombstoned {
    /// The id the name was interned under. Never reused, which is what makes
    /// sweeping it safe while the registry keeps serving.
    pub id: RepoId,
    pub name: String,
}

/// An outstanding sweep, read back off the `D` range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadRepoEntry {
    pub id: RepoId,
    pub name: String,
    pub dropped_at: u64,
}

/// One bounded step of a sweep: the `R` edges of up to `limit` manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepStep {
    /// Manifests visited in this step.
    pub manifests: usize,
    /// Where to resume. `None` means every manifest has been visited and
    /// [`Registry::finish_repo_sweep`] is what is left.
    pub next: Option<Vec<u8>>,
}

impl Registry {
    /// Release a repository's name, and record that its keys are still there.
    ///
    /// This is the whole of what a caller waits for, and it is deliberately
    /// O(1): dropping the `n`/`i` mapping is what makes the repository
    /// disappear from `_catalog`, from the discovery API and from the UI, since
    /// all three page over `n`. Everything keyed by the id is swept afterwards
    /// by [`Registry::sweep_repo_refs`] and [`Registry::finish_repo_sweep`].
    ///
    /// **The id is never reused.** Ids come from a monotonic counter, so a
    /// repository recreated under the same name gets a fresh one and cannot
    /// collide with keys the sweeper has not reached yet. That is the entire
    /// safety argument for sweeping in the background: a straggler under a dead
    /// id is unreachable garbage rather than another repository's data, so the
    /// sweep needs no lock and no coordination with concurrent pushes.
    ///
    /// An upload in flight to this repository is *not* blocked, for the same
    /// reason. It commits into the dead id and leaves an orphan `P`, which the
    /// sweep drops if it is still running and purge reclaims if it is not.
    pub fn delete_repository(&self, repo: &str, now: Timestamp) -> Result<RepoTombstoned> {
        let planned = self.plan_repo_tombstone(repo, now)?;
        self.engine().apply(&planned.batch)?;
        // After the batch, never before: until the mapping is actually gone,
        // evicting it only forces the next lookup to read it back.
        //
        // A push of the same name racing this will have interned a *new* id
        // between the two lines, and this then evicts that fresh entry instead.
        // Harmless - it is a cache, and the next lookup re-reads the mapping we
        // did not touch. The alternative, leaving the old entry in place, is
        // not: it resolves the name to an id that is being swept.
        self.forget_repo(&planned.outcome.name, planned.outcome.id);
        Ok(planned.outcome)
    }

    pub fn plan_repo_tombstone(
        &self,
        repo: &str,
        now: Timestamp,
    ) -> Result<Planned<RepoTombstoned>> {
        let repo_id = self.require_repo(repo)?;
        let mut batch = WriteBatch::new();
        batch.delete(keys::repo_by_name(repo));
        batch.delete(keys::repo_by_id(repo_id));
        batch.put(
            keys::dead_repo(repo_id),
            encode(&DeadRepo {
                name: repo.to_string(),
                dropped_at: now.secs(),
            })?,
        );
        Ok(Planned {
            outcome: RepoTombstoned {
                id: repo_id,
                name: repo.to_string(),
            },
            batch,
        })
    }

    /// Sweeps that have not finished, oldest id first.
    ///
    /// Cursor-paged like everything else, though in practice this range holds
    /// one entry per delete that has not finished sweeping and is usually
    /// empty.
    pub fn dead_repos(
        &self,
        start_after: Option<RepoId>,
        limit: usize,
    ) -> Result<Vec<DeadRepoEntry>> {
        let cursor = start_after.map(keys::dead_repo);
        let page = self
            .engine()
            .scan(&keys::dead_repos(), cursor.as_deref(), limit)?;
        let mut out = Vec::with_capacity(page.entries.len());
        for (key, raw) in &page.entries {
            let id = keys::parse_dead_repo_id(key)
                .ok_or_else(|| RegistryError::corrupt("dead repo key"))?;
            let record: DeadRepo = decode(raw, "DeadRepo")?;
            out.push(DeadRepoEntry {
                id,
                name: record.name,
                dropped_at: record.dropped_at,
            });
        }
        Ok(out)
    }

    /// Retract the blob-reference edges of one page of a dead repo's manifests.
    ///
    /// This is the one part of dropping a repository that cannot be a prefix
    /// delete. `R <digest> <repo> <manifest>` is keyed by the *blob* first, so
    /// a repository's edges are scattered across the whole `R` range and the
    /// only way to find them is through the `M` records that name the blobs -
    /// which is why this step runs strictly before
    /// [`Registry::finish_repo_sweep`] drops `M`. Reversing the two would leave
    /// every one of those edges unreachable, and each one is a blob that purge
    /// would then decline to reclaim for ever.
    ///
    /// Unlike the rest of this crate there is no `plan_` form. The batch is
    /// derived from a scan of a range this same call is emptying, so it is not
    /// something a caller can fold into another operation; and a repository's
    /// edge set is unbounded, so the whole sweep is a sequence of batches
    /// rather than one. Each step is still atomic, and re-running one is a
    /// no-op, so a sweep interrupted anywhere resumes by starting over.
    pub fn sweep_repo_refs(
        &self,
        repo: RepoId,
        cursor: Option<&[u8]>,
        limit: usize,
    ) -> Result<SweepStep> {
        let page = self
            .engine()
            .scan(&keys::manifests_in_repo(repo), cursor, limit)?;

        let mut batch = WriteBatch::new();
        for (_, raw) in &page.entries {
            let record: ManifestRecord = decode(raw, "ManifestRecord")?;
            // Exactly what `plan_manifest_delete` retracts, and for the same
            // reason a foreign layer is in the list without ever having had an
            // edge: deleting a key that is not there costs nothing.
            for blob in &record.layers {
                batch.delete(keys::blob_ref(blob, repo, &record.digest));
            }
        }
        self.engine().apply(&batch)?;

        Ok(SweepStep {
            manifests: page.entries.len(),
            next: page.next,
        })
    }

    /// Drop every range keyed by the dead repo's id, and the `D` record with
    /// them.
    ///
    /// One batch, because all of it is `DeletePrefix` - the ranges are all
    /// `<type> <repo> ...`, which is what the key schema buys here. Only `R`
    /// is not, and [`Registry::sweep_repo_refs`] must have run first.
    ///
    /// What is deliberately left: `L` and the blob bytes. Both are global and
    /// shared, so whether this repository was the last user of a layer is
    /// purge's question, not this one's.
    pub fn finish_repo_sweep(&self, repo: RepoId) -> Result<()> {
        let planned = self.plan_repo_sweep_finish(repo);
        self.engine().apply(&planned.batch)?;
        Ok(())
    }

    pub fn plan_repo_sweep_finish(&self, repo: RepoId) -> Planned<()> {
        let mut batch = WriteBatch::new();
        for prefix in [
            keys::manifests_in_repo(repo),
            keys::manifest_bodies_in_repo(repo),
            keys::tags_in_repo(repo),
            keys::manifest_tags_in_repo(repo),
            keys::blobs_in_repo(repo),
            keys::children_in_repo(repo),
            keys::referrers_in_repo(repo),
            keys::tag_history_in_repo(repo),
            keys::manifest_tag_history_in_repo(repo),
            keys::counters_in_repo_scope(keys::SCOPE_MANIFEST, repo),
            keys::counters_in_repo_scope(keys::SCOPE_TAG, repo),
            keys::counters_in_repo_scope(keys::SCOPE_REPO, repo),
        ] {
            batch.delete_prefix(prefix);
        }
        batch.delete(keys::dead_repo(repo));
        Planned { outcome: (), batch }
    }
}
