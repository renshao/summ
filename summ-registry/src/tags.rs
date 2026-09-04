//! Tags: set, get, delete, and the name-ordered list.

use summ_core::{
    keys, Digest, ManifestRecord, ReferrerRecord, RepoId, TagEvent, TagEventKind, TagRecord,
    Timestamp,
};
use summ_meta::WriteBatch;

use crate::codec::{decode, decompress_body, encode};
use crate::cosign;
use crate::error::{RegistryError, Result};
use crate::manifest;
use crate::reference::validate_tag;
use crate::registry::{Planned, Registry};

/// One page of tag names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagList {
    pub tags: Vec<String>,
    /// Pass back as `start_after` - and as the spec's `?last=` - to continue.
    /// `None` means the list is exhausted, which is how a handler decides
    /// whether to emit a `Link` header.
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagSet {
    /// What the tag pointed at before, if it pointed anywhere.
    pub displaced: Option<Digest>,
}

/// Everything a tag write needs to know about its target.
///
/// Carried explicitly rather than re-read inside the batch builder so that a
/// push, which already has all of it parsed, does not pay for a second lookup.
pub(crate) struct TagTarget {
    pub digest: Digest,
    pub media_type: String,
    pub size: u64,
    /// The descriptor to write if this tag turns out to name a legacy artifact
    /// subject. Built even when it is not used: it is four cloned fields.
    pub referrer: ReferrerRecord,
}

impl Registry {
    pub(crate) fn tag_record(&self, repo: RepoId, tag: &str) -> Result<Option<TagRecord>> {
        match self.engine().get(&keys::tag(repo, tag))? {
            Some(raw) => Ok(Some(decode(&raw, "TagRecord")?)),
            None => Ok(None),
        }
    }

    pub fn get_tag(&self, repo: &str, tag: &str) -> Result<Option<TagRecord>> {
        let repo_id = self.require_repo(repo)?;
        self.tag_record(repo_id, tag)
    }

    pub fn set_tag(
        &self,
        repo: &str,
        tag: &str,
        digest: &Digest,
        now: Timestamp,
    ) -> Result<TagSet> {
        let planned = self.plan_set_tag(repo, tag, digest, now)?;
        self.engine().apply(&planned.batch)?;
        Ok(planned.outcome)
    }

    /// Point a tag at a manifest that is already stored.
    ///
    /// The manifest must exist: the history event denormalises its descriptor,
    /// and a tag pointing at nothing is a 404 waiting to happen.
    pub fn plan_set_tag(
        &self,
        repo: &str,
        tag: &str,
        digest: &Digest,
        now: Timestamp,
    ) -> Result<Planned<TagSet>> {
        validate_tag(tag)?;
        let repo_id = self.require_repo(repo)?;
        let record = self.manifest_record(repo_id, digest)?.ok_or_else(|| {
            RegistryError::ManifestUnknown {
                repo: repo.to_string(),
                reference: digest.to_string(),
            }
        })?;
        let target = self.tag_target(repo_id, digest, tag, record)?;

        let mut batch = WriteBatch::new();
        let displaced = self.stage_set_tag(&mut batch, repo_id, tag, &target, now)?;
        Ok(Planned {
            outcome: TagSet { displaced },
            batch,
        })
    }

    /// Stage `T`, `G`, the `H`/`J` history pair, and any synthesised referrer
    /// edge, into a batch that may already contain a manifest push.
    ///
    /// The history events go in *this* batch and never through a separate
    /// path. A dropped pull count is a rounding error; a dropped history record
    /// is a hole in an audit trail.
    pub(crate) fn stage_set_tag(
        &self,
        batch: &mut WriteBatch,
        repo: RepoId,
        tag: &str,
        target: &TagTarget,
        now: Timestamp,
    ) -> Result<Option<Digest>> {
        validate_tag(tag)?;
        let artifact_subject = cosign::subject_of_artifact_tag(tag);
        let previous = self.tag_record(repo, tag)?.map(|r| r.digest);

        // Repointing must retract the old reverse edge. An orphaned `G` makes
        // an untagged manifest look tagged, and purge - which keys entirely off
        // "is it tagged?" - would then never reclaim it.
        if let Some(old) = previous.filter(|old| *old != target.digest) {
            batch.delete(keys::manifest_tag(repo, &old, tag));
            if let Some(subject) = artifact_subject {
                batch.delete(keys::referrer(repo, &subject, &old));
            }
        }

        batch.put(
            keys::tag(repo, tag),
            encode(&TagRecord {
                digest: target.digest,
                tagged_at: now.secs(),
            })?,
        );
        batch.set(keys::manifest_tag(repo, &target.digest, tag));

        let event = TagEvent {
            event: TagEventKind::Created,
            media_type: target.media_type.clone(),
            size: target.size,
        };
        let encoded = encode(&event)?;
        batch.put(
            keys::tag_history(repo, tag, now, &target.digest),
            encoded.clone(),
        );
        batch.put(
            keys::manifest_tag_history(repo, &target.digest, now, tag),
            encoded,
        );

        // A legacy cosign artifact names its subject in the tag rather than in
        // a `subject` field. Without this edge the signature outlives its
        // subject forever with its layers pinned by `R`, invisible to purge.
        if let Some(subject) = artifact_subject {
            batch.put(
                keys::referrer(repo, &subject, &target.digest),
                encode(&target.referrer)?,
            );
        }

        Ok(previous)
    }

    pub fn delete_tag(&self, repo: &str, tag: &str, now: Timestamp) -> Result<Digest> {
        let planned = self.plan_delete_tag(repo, tag, now)?;
        self.engine().apply(&planned.batch)?;
        Ok(planned.outcome)
    }

    /// Remove a tag, leaving the manifest reachable by digest.
    ///
    /// The `Deleted` event carries the digest the tag was displaced from, which
    /// only `T` knows at this moment - after the batch commits, nothing does.
    pub fn plan_delete_tag(
        &self,
        repo: &str,
        tag: &str,
        now: Timestamp,
    ) -> Result<Planned<Digest>> {
        let repo_id = self.require_repo(repo)?;
        let record =
            self.tag_record(repo_id, tag)?
                .ok_or_else(|| RegistryError::ManifestUnknown {
                    repo: repo.to_string(),
                    reference: tag.to_string(),
                })?;

        let manifest = self.manifest_record(repo_id, &record.digest)?;
        let mut batch = WriteBatch::new();
        self.stage_delete_tag(
            &mut batch,
            repo_id,
            tag,
            &record.digest,
            manifest.as_ref(),
            now,
        )?;
        Ok(Planned {
            outcome: record.digest,
            batch,
        })
    }

    pub(crate) fn stage_delete_tag(
        &self,
        batch: &mut WriteBatch,
        repo: RepoId,
        tag: &str,
        digest: &Digest,
        manifest: Option<&ManifestRecord>,
        now: Timestamp,
    ) -> Result<()> {
        batch.delete(keys::tag(repo, tag));
        batch.delete(keys::manifest_tag(repo, digest, tag));
        if let Some(subject) = cosign::subject_of_artifact_tag(tag) {
            batch.delete(keys::referrer(repo, &subject, digest));
        }

        // The descriptor is denormalised into the event because history must
        // stay queryable after the manifest is gone, at which point `M` cannot
        // supply it. When the manifest has already been dropped in this same
        // batch, fall back to what is left rather than dropping the event.
        let event = TagEvent {
            event: TagEventKind::Deleted,
            media_type: manifest
                .map(|m| m.media_type.clone())
                .unwrap_or_else(|| manifest::DEFAULT_MEDIA_TYPE.to_string()),
            size: manifest.map_or(0, |m| m.size),
        };
        let encoded = encode(&event)?;
        batch.put(keys::tag_history(repo, tag, now, digest), encoded.clone());
        batch.put(keys::manifest_tag_history(repo, digest, now, tag), encoded);
        Ok(())
    }

    /// `GET /v2/<name>/tags/list`, name-ordered.
    ///
    /// The order is a MUST in the spec and it is free here: `T <repo> <tag>` is
    /// byte-ordered and the engine scans it in key order, so there is nothing
    /// to sort and nothing to buffer. `start_after` is the spec's `?last=`, a
    /// tag name rather than an index, and results begin strictly after it.
    pub fn list_tags(
        &self,
        repo: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> Result<TagList> {
        let repo_id = self.require_repo(repo)?;
        let prefix = keys::tags_in_repo(repo_id);
        let cursor = start_after.map(|t| keys::tag(repo_id, t));
        let page = self.engine().scan_keys(&prefix, cursor.as_deref(), limit)?;

        let mut tags = Vec::with_capacity(page.keys.len());
        for key in &page.keys {
            tags.push(
                keys::parse_tag_suffix(key)
                    .ok_or_else(|| RegistryError::corrupt("tag key"))?
                    .to_string(),
            );
        }
        Ok(TagList {
            next: next_tag(page.next.as_deref())?,
            tags,
        })
    }

    /// Which tags point at one manifest, from the `G` reverse index. An empty
    /// first page means the manifest is untagged and therefore purgeable.
    pub fn tags_of_manifest(
        &self,
        repo: &str,
        digest: &Digest,
        start_after: Option<&str>,
        limit: usize,
    ) -> Result<TagList> {
        let repo_id = self.require_repo(repo)?;
        let prefix = keys::tags_of_manifest(repo_id, digest);
        let cursor = start_after.map(|t| keys::manifest_tag(repo_id, digest, t));
        let page = self.engine().scan_keys(&prefix, cursor.as_deref(), limit)?;

        let mut tags = Vec::with_capacity(page.keys.len());
        for key in &page.keys {
            tags.push(
                keys::parse_manifest_tag_suffix(key, digest)
                    .ok_or_else(|| RegistryError::corrupt("manifest-tag key"))?
                    .to_string(),
            );
        }
        let next = match page.next.as_deref() {
            Some(key) => Some(
                keys::parse_manifest_tag_suffix(key, digest)
                    .ok_or_else(|| RegistryError::corrupt("manifest-tag cursor"))?
                    .to_string(),
            ),
            None => None,
        };
        Ok(TagList { tags, next })
    }

    /// Build a [`TagTarget`] from what is already stored.
    ///
    /// The `artifactType` a referrers response must report is not
    /// `ManifestRecord::artifact_type`: for an image manifest with no explicit
    /// `artifactType` it is the *config descriptor's* media type, which the
    /// record does not hold. Re-reading `B` recovers it, and that read is taken
    /// only when the tag actually names an artifact subject - never on an
    /// ordinary tag write.
    fn tag_target(
        &self,
        repo: RepoId,
        digest: &Digest,
        tag: &str,
        record: ManifestRecord,
    ) -> Result<TagTarget> {
        let artifact_type = if cosign::subject_of_artifact_tag(tag).is_some() {
            match self.engine().get(&keys::manifest_body(repo, digest))? {
                Some(stored) => {
                    let body = decompress_body(&stored)?;
                    manifest::parse(&body, Some(&record.media_type))?.referrer_artifact_type
                }
                None => record.artifact_type.clone(),
            }
        } else {
            record.artifact_type.clone()
        };

        Ok(TagTarget {
            digest: *digest,
            media_type: record.media_type.clone(),
            size: record.size,
            referrer: ReferrerRecord {
                media_type: record.media_type,
                artifact_type,
                size: record.size,
                annotations: record.annotations,
            },
        })
    }
}

fn next_tag(cursor: Option<&[u8]>) -> Result<Option<String>> {
    match cursor {
        Some(key) => Ok(Some(
            keys::parse_tag_suffix(key)
                .ok_or_else(|| RegistryError::corrupt("tag cursor"))?
                .to_string(),
        )),
        None => Ok(None),
    }
}
