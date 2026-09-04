//! An in-memory implementation of [`Registry`].
//!
//! It exists so the HTTP layer can be driven end to end before `summ-registry`
//! and `summ-storage` land, and it is what `summ serve` currently runs on. It
//! is deliberately the *simplest* implementation that is semantically correct -
//! `BTreeMap`s under one mutex, whole blobs in `Vec<u8>` - because its job is to
//! prove the wire behaviour, not the storage design. Everything about it that
//! would be wrong at scale (a global lock, unbounded memory, fan-in held as a
//! scan) is wrong in ways the real implementation must not copy.
//!
//! It does honour the semantics that the HTTP layer's correctness depends on,
//! and those are worth listing because they are the contract the real
//! implementation inherits:
//!
//! - Blob membership is **per repository**. A blob present in one repository is
//!   not servable from another until it is mounted or pushed there.
//! - Deleting a manifest by digest **cascades to every tag** pointing at it;
//!   deleting a tag leaves the manifest reachable by digest.
//! - Manifest bytes are stored and returned **byte-exact**, never
//!   re-serialised.
//! - A `subject` naming a manifest that does not exist is **accepted**, and the
//!   referrer is listed.
//! - Deletes are visible immediately, not eventually.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use sha2::{Digest as _, Sha256, Sha512};
use summ_core::{CounterBucket, Digest};

use crate::counters::{Recorded, Subject};
use crate::range::ByteRange;
use crate::reference::Reference;
use crate::seam::{
    BlobRead, Descriptor, HistoryCursor, ManifestInfo, ManifestPut, ManifestStat, OpsError,
    OpsResult, Page, PullCountDay, PullCountScope, Referrers, Registry, RepoDetail, RepoSummary,
    TagEventInfo, TagInfo, Tally, UploadBody, TAGS_PER_MANIFEST,
};

#[derive(Debug, Clone)]
struct StoredManifest {
    media_type: String,
    body: Bytes,
    subject: Option<Digest>,
    artifact_type: Option<String>,
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct Repo {
    tags: BTreeMap<String, Digest>,
    manifests: BTreeMap<Digest, StoredManifest>,
    blobs: BTreeMap<Digest, Vec<u8>>,
    /// Tag events, oldest first, appended on every tag mutation.
    ///
    /// The real store indexes the same events twice (`H` by tag, `J` by
    /// digest) and pages each with a seek. Here it is one vector filtered on
    /// read, which is the usual bargain in this module: semantically
    /// identical, structurally nothing like it. Reversed on read, because the
    /// API is newest first.
    history: Vec<TagEventInfo>,
    /// `(scope, day) -> bucket`, standing in for the `A` range.
    ///
    /// Keyed and folded exactly as the real one is, because this is the half
    /// of pull counting that has semantics worth a second implementation: the
    /// three scopes are separate series maintained on write, a day is broken
    /// down by hour, and a flush adds to what is there rather than replacing
    /// it. What it is not is the accumulator - that lives above the seam in
    /// `crate::counters` and is the same object whichever registry is behind
    /// it.
    counters: BTreeMap<(CounterScope, u16), CounterBucket>,
}

/// The memory store's spelling of `summ_registry::CountSubject`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CounterScope {
    Manifest(Digest),
    Tag(String),
    Repo,
}

#[derive(Debug)]
struct Upload {
    repo: String,
    buffer: Vec<u8>,
}

#[derive(Debug, Default)]
struct State {
    repos: BTreeMap<String, Repo>,
    uploads: BTreeMap<String, Upload>,
    /// Stands in for the clock. Tests need a total order over events they
    /// create in the same instant, and a counter gives one without making them
    /// sleep. It is unix milliseconds in the real implementation.
    clock: u64,
}

/// Hand out the next instant.
///
/// Taken as `&mut u64` rather than as a method on [`State`] so it can be held
/// alongside a `&mut Repo`: the two are disjoint fields, which the borrow
/// checker only sees once the guard has been reborrowed as a `&mut State`.
fn tick(clock: &mut u64) -> u64 {
    *clock += 1;
    *clock
}

/// Append a `Created` or `Deleted` event, mirroring what `stage_set_tag` and
/// `stage_delete_tag` write into one `WriteBatch` with the mutation itself.
fn record_event(repo: &mut Repo, at: u64, tag: &str, digest: Digest, deleted: bool) {
    let (media_type, size) = repo
        .manifests
        .get(&digest)
        .map(|m| (m.media_type.clone(), m.body.len() as u64))
        .unwrap_or_default();
    repo.history.push(TagEventInfo {
        at,
        tag: tag.to_owned(),
        digest,
        deleted,
        media_type,
        size,
    });
}

#[derive(Debug, Default)]
pub struct MemoryRegistry {
    state: Mutex<State>,
}

impl MemoryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a drained accumulator, as the flush task does against the real
    /// store.
    ///
    /// The memory registry has no flush task of its own - it is not what
    /// `summ serve` runs - so this is the seam at which a test hands it an
    /// interval's worth of counts.
    pub fn apply_pull_counts(&self, drained: Vec<Recorded>) {
        let mut state = self.lock();
        for row in drained {
            // A repository the store does not have is skipped, exactly as the
            // real flush skips one it cannot resolve to an interned id: a
            // counter must not resurrect a name in the catalog.
            let Some(repo) = state.repos.get_mut(&row.repo) else {
                continue;
            };
            let scope = match row.subject {
                Subject::Manifest(digest) => CounterScope::Manifest(digest),
                Subject::Tag(tag) => CounterScope::Tag(tag),
                Subject::Repo => CounterScope::Repo,
            };
            repo.counters.entry((scope, row.day)).or_default().add(
                row.hour,
                row.manifest_pulls,
                row.blob_pulls,
                row.bytes_out,
            );
        }
    }

    /// Seed a blob without going through the upload flow, for tests that are
    /// about reading rather than writing.
    pub fn seed_blob(&self, repo: &str, bytes: &[u8]) -> Digest {
        let digest = sha256(bytes);
        let mut state = self.lock();
        state
            .repos
            .entry(repo.to_owned())
            .or_default()
            .blobs
            .insert(digest, bytes.to_vec());
        digest
    }

    /// Seed a manifest and optionally tag it.
    pub fn seed_manifest(
        &self,
        repo: &str,
        tag: Option<&str>,
        media_type: &str,
        body: &[u8],
    ) -> Digest {
        let digest = sha256(body);
        let mut state = self.lock();
        let state = &mut *state;
        let clock = &mut state.clock;
        let entry = state.repos.entry(repo.to_owned()).or_default();
        entry.manifests.insert(
            digest,
            StoredManifest {
                media_type: media_type.to_owned(),
                body: Bytes::copy_from_slice(body),
                subject: None,
                artifact_type: None,
                annotations: BTreeMap::new(),
            },
        );
        if let Some(tag) = tag {
            entry.tags.insert(tag.to_owned(), digest);
            let at = tick(clock);
            record_event(entry, at, tag, digest, false);
        }
        digest
    }

    /// Attach a `subject` to an already-seeded manifest, for referrers tests.
    pub fn seed_subject(
        &self,
        repo: &str,
        digest: &Digest,
        subject: Digest,
        artifact_type: Option<&str>,
    ) {
        let mut state = self.lock();
        if let Some(manifest) = state
            .repos
            .get_mut(repo)
            .and_then(|r| r.manifests.get_mut(digest))
        {
            manifest.subject = Some(subject);
            manifest.artifact_type = artifact_type.map(str::to_owned);
        }
    }

    /// A poisoned mutex means a previous test panicked while holding it; the
    /// data is still structurally valid, so recovering keeps one failure from
    /// cascading into every later assertion.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn sha256(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&out);
    Digest::Sha256(raw)
}

fn sha512(bytes: &[u8]) -> Digest {
    let mut hasher = Sha512::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut raw = [0u8; 64];
    raw.copy_from_slice(&out);
    Digest::Sha512(raw)
}

/// Hash under the algorithm the client's `?digest=` named, whatever the session
/// was opened with. That is what lets `Docker-Content-Digest` echo the client's
/// digest exactly, and it is the cheap equivalent of the real backend rehashing
/// its staged bytes when the closing digest disagrees with the opening hint.
fn hash_like(bytes: &[u8], like: &Digest) -> Digest {
    match like {
        Digest::Sha256(_) => sha256(bytes),
        Digest::Sha512(_) => sha512(bytes),
    }
}

/// Describe a stored manifest the way the discovery API does.
///
/// The real backend reads a `ManifestRecord` written at push time; there is no
/// such record here, so the shape is recovered from the body. It is the same
/// answer by a slower route, which is what makes this a second implementation
/// of the seam rather than a stub of it - a discovery test that passes here and
/// fails against the backend is a real disagreement.
fn describe(repo: &Repo, digest: &Digest, stored: &StoredManifest) -> ManifestInfo {
    let parsed: serde_json::Value = serde_json::from_slice(&stored.body).unwrap_or_default();

    let mut platforms = Vec::new();
    let mut push_platform = |value: &serde_json::Value| {
        let os = value.get("os").and_then(|v| v.as_str());
        let arch = value.get("architecture").and_then(|v| v.as_str());
        if let (Some(os), Some(arch)) = (os, arch) {
            let label = match value.get("variant").and_then(|v| v.as_str()) {
                Some(variant) => format!("{os}/{arch}/{variant}"),
                None => format!("{os}/{arch}"),
            };
            if !platforms.contains(&label) {
                platforms.push(label);
            }
        }
    };

    let children = parsed
        .get("manifests")
        .and_then(|v| v.as_array())
        .map(|entries| {
            for entry in entries {
                if let Some(platform) = entry.get("platform") {
                    push_platform(platform);
                }
            }
            entries.len() as u64
        })
        .unwrap_or(0);

    // Config *and* layers, in that order, which is the set the backend records
    // and the set the push writes an `R` edge for. Counting only `layers` here
    // would make the two implementations quietly disagree by one blob.
    let referenced: Vec<&serde_json::Value> = parsed
        .get("config")
        .into_iter()
        .chain(
            parsed
                .get("layers")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten(),
        )
        .collect();
    let blobs = referenced.len() as u64;
    // Foreign layers - the ones carrying `urls` - are counted but not sized.
    // The real backend does the same: the manifest genuinely references them,
    // so they belong in the count, but their bytes live on somebody else's CDN
    // and adding them would inflate every repository size that mentions a
    // Windows base layer.
    let blob_size = referenced
        .iter()
        .filter(|e| {
            e.get("urls")
                .and_then(serde_json::Value::as_array)
                .is_none_or(|u| u.is_empty())
        })
        .filter_map(|e| e.get("size").and_then(serde_json::Value::as_u64))
        .sum();

    // The reverse of `T`, which the real store keeps as its own `G` range and
    // this one has to find by looking.
    let tags: Vec<String> = repo
        .tags
        .iter()
        .filter(|(_, d)| *d == digest)
        .map(|(tag, _)| tag.clone())
        .take(TAGS_PER_MANIFEST)
        .collect();

    ManifestInfo {
        digest: *digest,
        media_type: stored.media_type.clone(),
        size: stored.body.len() as u64,
        blob_size,
        artifact_type: stored.artifact_type.clone(),
        subject: stored.subject,
        // No push clock here. The backend stamps `pushed_at` from the request
        // and this store has no request to stamp from, so it reports the one
        // honest value rather than inventing one.
        pushed_at: 0,
        platforms,
        blobs,
        children,
        tags,
        // From the body, not from `stored`: the backend parses these out at
        // push time and `seed_manifest` never sets them, so reading the field
        // would make this implementation quietly weaker than the one it exists
        // to check.
        annotations: parsed
            .get("annotations")
            .and_then(|v| v.as_object())
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_else(|| stored.annotations.clone()),
    }
}

/// One page of an ordered iterator, with `more` decided by peeking one past the
/// limit rather than by "the page came back full".
fn paginate<T: Ord + Clone>(
    all: impl Iterator<Item = T>,
    last: Option<&T>,
    limit: usize,
) -> Page<T> {
    let mut items: Vec<T> = all
        .filter(|item| last.is_none_or(|last| item > last))
        .take(limit + 1)
        .collect();
    let more = items.len() > limit;
    items.truncate(limit);
    Page { items, more }
}

impl State {
    fn repo(&self, name: &str) -> OpsResult<&Repo> {
        self.repos.get(name).ok_or(OpsError::RepoUnknown)
    }

    fn resolve(&self, repo: &Repo, reference: &Reference) -> OpsResult<Digest> {
        match reference {
            Reference::Digest(digest) => repo
                .manifests
                .contains_key(digest)
                .then_some(*digest)
                .ok_or(OpsError::ManifestUnknown),
            Reference::Tag(tag) => repo.tags.get(tag).copied().ok_or(OpsError::ManifestUnknown),
        }
    }
}

#[async_trait]
impl Registry for MemoryRegistry {
    async fn repositories(&self, last: Option<&str>, limit: usize) -> OpsResult<Page<String>> {
        let state = self.lock();
        Ok(paginate(
            state.repos.keys().cloned(),
            last.map(str::to_owned).as_ref(),
            limit,
        ))
    }

    async fn tags(&self, name: &str, last: Option<&str>, limit: usize) -> OpsResult<Page<String>> {
        let state = self.lock();
        let repo = state.repo(name)?;
        Ok(paginate(
            repo.tags.keys().cloned(),
            last.map(str::to_owned).as_ref(),
            limit,
        ))
    }

    async fn stat_manifest(&self, name: &str, reference: &Reference) -> OpsResult<ManifestStat> {
        let state = self.lock();
        let repo = state.repo(name)?;
        let digest = state.resolve(repo, reference)?;
        let manifest = repo
            .manifests
            .get(&digest)
            .ok_or(OpsError::ManifestUnknown)?;
        Ok(ManifestStat {
            digest,
            media_type: manifest.media_type.clone(),
            size: manifest.body.len() as u64,
        })
    }

    async fn get_manifest(
        &self,
        name: &str,
        reference: &Reference,
    ) -> OpsResult<(ManifestStat, Bytes)> {
        let state = self.lock();
        let repo = state.repo(name)?;
        let digest = state.resolve(repo, reference)?;
        let manifest = repo
            .manifests
            .get(&digest)
            .ok_or(OpsError::ManifestUnknown)?;
        Ok((
            ManifestStat {
                digest,
                media_type: manifest.media_type.clone(),
                size: manifest.body.len() as u64,
            },
            manifest.body.clone(),
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
        // Push creates the repository; there is no separate "create repo" call
        // in the spec.
        let digest = match reference {
            Reference::Digest(claimed) => {
                let actual = hash_like(&body, claimed);
                if actual != *claimed {
                    return Err(OpsError::DigestMismatch);
                }
                actual
            }
            Reference::Tag(_) => sha256(&body),
        };

        // Parsed permissively: fields outside the OCI schema must round-trip,
        // and referenced blobs are deliberately not required to exist - the
        // spec makes that validation optional and it would break concurrent
        // layer-and-manifest pushes.
        let parsed: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
        let subject = parsed
            .as_ref()
            .and_then(|v| v.get("subject"))
            .and_then(|s| s.get("digest"))
            .and_then(|d| d.as_str())
            .and_then(|d| d.parse::<Digest>().ok());
        let artifact_type = parsed
            .as_ref()
            .and_then(|v| v.get("artifactType"))
            .and_then(|a| a.as_str())
            .map(str::to_owned)
            // Image manifests fall back to the config descriptor's media type;
            // an index does not, and omits the field entirely.
            .or_else(|| {
                parsed
                    .as_ref()
                    .and_then(|v| v.get("config"))
                    .and_then(|c| c.get("mediaType"))
                    .and_then(|m| m.as_str())
                    .map(str::to_owned)
            });
        let annotations = parsed
            .as_ref()
            .and_then(|v| v.get("annotations"))
            .and_then(|a| a.as_object())
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                    .collect()
            })
            .unwrap_or_default();

        let mut state = self.lock();
        let state = &mut *state;
        let clock = &mut state.clock;
        let repo = state.repos.entry(name.to_owned()).or_default();
        repo.manifests.insert(
            digest,
            StoredManifest {
                media_type: content_type.to_owned(),
                body,
                subject,
                artifact_type,
                annotations,
            },
        );
        let mut written: Vec<&String> = Vec::new();
        if let Reference::Tag(tag) = reference {
            written.push(tag);
        }
        written.extend(tags);
        for tag in written {
            repo.tags.insert(tag.clone(), digest);
            let at = tick(clock);
            record_event(repo, at, tag, digest, false);
        }

        Ok(ManifestPut {
            digest,
            subject,
            tags: tags.to_vec(),
        })
    }

    async fn delete_manifest(&self, name: &str, reference: &Reference) -> OpsResult<()> {
        let mut state = self.lock();
        let state = &mut *state;
        let clock = &mut state.clock;
        let repo = state.repos.get_mut(name).ok_or(OpsError::RepoUnknown)?;
        match reference {
            // Deleting a tag leaves the manifest reachable by digest.
            Reference::Tag(tag) => {
                let digest = repo.tags.remove(tag).ok_or(OpsError::ManifestUnknown)?;
                let at = tick(clock);
                record_event(repo, at, tag, digest, true);
            }
            // Deleting by digest cascades to every tag pointing at it.
            Reference::Digest(digest) => {
                // The events are written before the manifest is dropped: they
                // denormalise its media type and size, which is the whole
                // reason history outlives it.
                let dropped: Vec<String> = repo
                    .tags
                    .iter()
                    .filter(|(_, target)| *target == digest)
                    .map(|(tag, _)| tag.clone())
                    .collect();
                if !repo.manifests.contains_key(digest) {
                    return Err(OpsError::ManifestUnknown);
                }
                for tag in &dropped {
                    let at = tick(clock);
                    record_event(repo, at, tag, *digest, true);
                }
                repo.manifests.remove(digest);
                repo.tags.retain(|_, target| target != digest);
            }
        }
        Ok(())
    }

    async fn stat_blob(&self, name: &str, digest: &Digest) -> OpsResult<u64> {
        let state = self.lock();
        let repo = state.repo(name).map_err(|_| OpsError::BlobUnknown)?;
        repo.blobs
            .get(digest)
            .map(|b| b.len() as u64)
            .ok_or(OpsError::BlobUnknown)
    }

    async fn get_blob(
        &self,
        name: &str,
        digest: &Digest,
        window: Option<ByteRange>,
    ) -> OpsResult<BlobRead> {
        let state = self.lock();
        let repo = state.repo(name).map_err(|_| OpsError::BlobUnknown)?;
        let bytes = repo.blobs.get(digest).ok_or(OpsError::BlobUnknown)?;
        let total_size = bytes.len() as u64;
        let slice = match window {
            Some(range) => {
                let start = range.start.min(total_size) as usize;
                let end = (range.end + 1).min(total_size) as usize;
                bytes[start..end].to_vec()
            }
            None => bytes.clone(),
        };
        Ok(BlobRead {
            total_size,
            window,
            body: Body::from(slice),
        })
    }

    async fn delete_blob(&self, name: &str, digest: &Digest) -> OpsResult<()> {
        let mut state = self.lock();
        let repo = state.repos.get_mut(name).ok_or(OpsError::BlobUnknown)?;
        repo.blobs.remove(digest).ok_or(OpsError::BlobUnknown)?;
        Ok(())
    }

    async fn mount_blob(&self, name: &str, digest: &Digest, from: Option<&str>) -> OpsResult<bool> {
        let mut state = self.lock();
        // With `from`, look only there; without it, anywhere - the spec permits
        // anonymous mount and it is the cheapest push path there is.
        let source = match from {
            Some(from) => state.repos.get(from).and_then(|r| r.blobs.get(digest)),
            None => state.repos.values().find_map(|repo| repo.blobs.get(digest)),
        };
        let Some(bytes) = source.cloned() else {
            return Ok(false);
        };
        state
            .repos
            .entry(name.to_owned())
            .or_default()
            .blobs
            .insert(*digest, bytes);
        Ok(true)
    }

    /// `algorithm` is not kept. The session it names is only ever closed by a
    /// `?digest=`, and that digest names the algorithm the content is verified
    /// under - here by hashing the buffer, in the real backend by rehashing the
    /// staged bytes when the two differ. Storing the opening hint would give
    /// this implementation a decision the real one does not make.
    async fn create_upload(&self, name: &str, id: &str, _algorithm: &str) -> OpsResult<()> {
        let mut state = self.lock();
        state.repos.entry(name.to_owned()).or_default();
        state.uploads.insert(
            id.to_owned(),
            Upload {
                repo: name.to_owned(),
                buffer: Vec::new(),
            },
        );
        Ok(())
    }

    async fn upload_offset(&self, name: &str, id: &str) -> OpsResult<u64> {
        let state = self.lock();
        let upload = state.uploads.get(id).ok_or(OpsError::UploadUnknown)?;
        if upload.repo != name {
            return Err(OpsError::UploadUnknown);
        }
        Ok(upload.buffer.len() as u64)
    }

    async fn append_upload(
        &self,
        name: &str,
        id: &str,
        expected_offset: u64,
        body: UploadBody,
    ) -> OpsResult<u64> {
        // Collected, because there is nowhere to stream to: this
        // implementation's whole storage is a `Vec<u8>`. The real one must not
        // do this, which is why the collecting is here rather than in the
        // handler.
        let chunk = body.collect().await?;
        let mut state = self.lock();
        let upload = state.uploads.get_mut(id).ok_or(OpsError::UploadUnknown)?;
        if upload.repo != name {
            return Err(OpsError::UploadUnknown);
        }
        let current = upload.buffer.len() as u64;
        if current != expected_offset {
            return Err(OpsError::OffsetMismatch { current });
        }
        upload.buffer.extend_from_slice(&chunk);
        Ok(upload.buffer.len() as u64)
    }

    async fn finish_upload(
        &self,
        name: &str,
        id: &str,
        expected_offset: u64,
        body: UploadBody,
        digest: &Digest,
    ) -> OpsResult<()> {
        let chunk = body.collect().await?;
        let mut state = self.lock();
        let upload = state.uploads.get(id).ok_or(OpsError::UploadUnknown)?;
        if upload.repo != name {
            return Err(OpsError::UploadUnknown);
        }
        let current = upload.buffer.len() as u64;
        if current != expected_offset {
            return Err(OpsError::OffsetMismatch { current });
        }
        let mut bytes = upload.buffer.clone();
        bytes.extend_from_slice(&chunk);

        // Verification happens here and only here. On failure nothing commits,
        // and the session survives so the client can retry.
        if hash_like(&bytes, digest) != *digest {
            return Err(OpsError::DigestMismatch);
        }
        state.uploads.remove(id);
        state
            .repos
            .entry(name.to_owned())
            .or_default()
            .blobs
            .insert(*digest, bytes);
        Ok(())
    }

    async fn cancel_upload(&self, name: &str, id: &str) -> OpsResult<()> {
        let mut state = self.lock();
        match state.uploads.get(id) {
            Some(upload) if upload.repo == name => {
                state.uploads.remove(id);
                Ok(())
            }
            _ => Err(OpsError::UploadUnknown),
        }
    }

    async fn put_blob(&self, name: &str, digest: &Digest, body: UploadBody) -> OpsResult<()> {
        let body = body.collect().await?;
        if hash_like(&body, digest) != *digest {
            return Err(OpsError::DigestMismatch);
        }
        let mut state = self.lock();
        state
            .repos
            .entry(name.to_owned())
            .or_default()
            .blobs
            .insert(*digest, body.to_vec());
        Ok(())
    }

    /// The `F` scan, over a `BTreeMap` instead of a key range.
    ///
    /// `Digest`'s derived ordering is the same as its encoded key ordering -
    /// sha256 before sha512, then raw bytes - so iterating the map visits
    /// referrers in exactly the order the real engine's prefix scan does, and
    /// a cursor means the same thing on both sides of the seam.
    ///
    /// `limit` bounds the edges *scanned*, and the `artifactType` filter is
    /// applied after that bound rather than before it. Filtering first would be
    /// the easier code and the wrong contract: it would let one request walk
    /// every edge in the repository whenever the requested type is rare.
    async fn referrers(
        &self,
        name: &str,
        subject: &Digest,
        artifact_type: Option<&str>,
        last: Option<&Digest>,
        limit: usize,
    ) -> OpsResult<Referrers> {
        let state = self.lock();
        // An unknown repository is an empty list, not an error: the endpoint
        // must never 404 once it is enabled.
        let Some(repo) = state.repos.get(name) else {
            return Ok(Referrers {
                manifests: Vec::new(),
                filter_applied: artifact_type.is_some(),
                next: None,
            });
        };

        let mut scanned = repo
            .manifests
            .iter()
            .filter(|(digest, m)| m.subject.as_ref() == Some(subject) && Some(*digest) > last)
            .take(limit + 1);

        let mut page: Vec<(&Digest, &StoredManifest)> = Vec::new();
        let mut next = None;
        for entry in scanned.by_ref().take(limit) {
            page.push(entry);
        }
        // Peeking one past the limit is what makes `next` mean "there is more"
        // rather than "the page was full".
        if scanned.next().is_some() {
            next = page.last().map(|(digest, _)| **digest);
        }

        let manifests = page
            .into_iter()
            .filter(|(_, m)| match artifact_type {
                Some(wanted) => m.artifact_type.as_deref() == Some(wanted),
                None => true,
            })
            .map(|(digest, m)| Descriptor {
                media_type: m.media_type.clone(),
                digest: *digest,
                size: m.body.len() as u64,
                artifact_type: m.artifact_type.clone(),
                annotations: m.annotations.clone(),
            })
            .collect();
        Ok(Referrers {
            manifests,
            filter_applied: artifact_type.is_some(),
            next,
        })
    }

    // ---- discovery beyond the spec ---------------------------------------

    async fn repository_summaries(
        &self,
        prefix: &str,
        last: Option<&str>,
        limit: usize,
    ) -> OpsResult<Page<RepoSummary>> {
        let state = self.lock();
        let names = paginate(
            state
                .repos
                .keys()
                .filter(|name| name.starts_with(prefix))
                .cloned(),
            last.map(str::to_owned).as_ref(),
            limit,
        );
        // Counts are exact here because the whole registry fits in a map. The
        // ceiling the real backend stops at is a property of scanning ten
        // million keys, not of the contract.
        let items = names
            .items
            .into_iter()
            .map(|name| {
                let repo = &state.repos[&name];
                RepoSummary {
                    name: name.clone(),
                    tags: Tally::exact(repo.tags.len() as u64),
                    manifests: Tally::exact(repo.manifests.len() as u64),
                }
            })
            .collect();
        Ok(Page {
            items,
            more: names.more,
        })
    }

    async fn repository_detail(&self, name: &str) -> OpsResult<RepoDetail> {
        let state = self.lock();
        let repo = state.repo(name)?;
        Ok(RepoDetail {
            name: name.to_owned(),
            tags: Tally::exact(repo.tags.len() as u64),
            manifests: Tally::exact(repo.manifests.len() as u64),
            blobs: Tally::exact(repo.blobs.len() as u64),
            size_bytes: repo.blobs.values().map(|b| b.len() as u64).sum(),
        })
    }

    async fn tag_details(
        &self,
        name: &str,
        last: Option<&str>,
        limit: usize,
    ) -> OpsResult<Page<TagInfo>> {
        let state = self.lock();
        let repo = state.repo(name)?;
        let page = paginate(
            repo.tags.keys().cloned(),
            last.map(str::to_owned).as_ref(),
            limit,
        );
        let items = page
            .items
            .into_iter()
            .map(|tag| {
                let digest = repo.tags[&tag];
                TagInfo {
                    name: tag,
                    digest,
                    tagged_at: 0,
                    manifest: repo
                        .manifests
                        .get(&digest)
                        .map(|m| describe(repo, &digest, m)),
                }
            })
            .collect();
        Ok(Page {
            items,
            more: page.more,
        })
    }

    async fn manifest_details(
        &self,
        name: &str,
        last: Option<&Digest>,
        limit: usize,
    ) -> OpsResult<Page<ManifestInfo>> {
        let state = self.lock();
        let repo = state.repo(name)?;
        let page = paginate(repo.manifests.keys().copied(), last, limit);
        let items = page
            .items
            .iter()
            .map(|digest| describe(repo, digest, &repo.manifests[digest]))
            .collect();
        Ok(Page {
            items,
            more: page.more,
        })
    }

    async fn manifest_detail(&self, name: &str, reference: &Reference) -> OpsResult<ManifestInfo> {
        let state = self.lock();
        let repo = state.repo(name)?;
        let digest = state.resolve(repo, reference)?;
        let stored = repo
            .manifests
            .get(&digest)
            .ok_or(OpsError::ManifestUnknown)?;
        Ok(describe(repo, &digest, stored))
    }

    async fn tag_history(
        &self,
        name: &str,
        reference: &Reference,
        before: Option<u64>,
        last: Option<&str>,
        limit: usize,
    ) -> OpsResult<(Vec<TagEventInfo>, Option<HistoryCursor>)> {
        let state = self.lock();
        // An unknown repository is an empty page, not an error: history outlives
        // what it describes, so the real store cannot tell "never existed" from
        // "gone" either.
        let Some(repo) = state.repos.get(name) else {
            return Ok((Vec::new(), None));
        };

        // Newest first, which the real implementation gets from a complemented
        // timestamp in the key rather than from reversing.
        let mut rows: Vec<TagEventInfo> = repo
            .history
            .iter()
            .rev()
            .filter(|e| match reference {
                Reference::Tag(tag) => e.tag == *tag,
                Reference::Digest(digest) => e.digest == *digest,
            })
            .filter(|e| match (before, last) {
                // `before` alone is strictly-before. With `last` it resumes
                // exactly, which matters when events share an instant.
                //
                // Ties break *ascending* on the other key component even though
                // the instants descend, because that is the byte order of the
                // real key: `<!ts> <digest>`. The clock here never repeats, so
                // nothing observes it - it is written this way so the two
                // implementations cannot disagree if that ever changes.
                (Some(at), Some(last)) => {
                    e.at < at || (e.at == at && key_of(reference, e).as_str() > last)
                }
                (Some(at), None) => e.at < at,
                (None, _) => true,
            })
            .cloned()
            .collect();

        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = more.then(|| rows.last()).flatten().map(|e| HistoryCursor {
            before: e.at,
            last: key_of(reference, e),
        });
        Ok((rows, next))
    }

    async fn pull_counts(
        &self,
        name: &str,
        scope: &PullCountScope,
        from_day: u16,
        days: u16,
    ) -> OpsResult<Vec<PullCountDay>> {
        if days == 0 {
            return Ok(Vec::new());
        }
        let state = self.lock();
        // An unknown repository is an empty series, never an error - counts
        // outlive what they describe.
        let Some(repo) = state.repos.get(name) else {
            return Ok(Vec::new());
        };
        let wanted = match scope {
            PullCountScope::Repository => CounterScope::Repo,
            PullCountScope::Tag(tag) => CounterScope::Tag(tag.clone()),
            PullCountScope::Manifest(digest) => CounterScope::Manifest(*digest),
        };
        let last = from_day.saturating_add(days.saturating_sub(1));
        Ok(repo
            .counters
            .iter()
            .filter(|((scope, day), _)| *scope == wanted && *day >= from_day && *day <= last)
            .map(|((_, day), bucket)| PullCountDay {
                day: *day,
                bucket: *bucket,
            })
            .collect())
    }
}

/// The tiebreaker within one instant: the other half of the key from whichever
/// side the caller asked.
fn key_of(reference: &Reference, event: &TagEventInfo) -> String {
    match reference {
        Reference::Tag(_) => event.digest.to_string(),
        Reference::Digest(_) => event.tag.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_peeks_rather_than_guessing_from_a_full_page() {
        let items = ["a", "b", "c"].map(str::to_owned);
        let page = paginate(items.iter().cloned(), None, 3);
        assert_eq!(page.items.len(), 3);
        assert!(
            !page.more,
            "a page that exactly consumes the range has nothing after it"
        );

        let page = paginate(items.iter().cloned(), None, 2);
        assert!(page.more);

        let page = paginate(items.iter().cloned(), Some(&"a".to_owned()), 5);
        assert_eq!(page.items, vec!["b".to_owned(), "c".to_owned()]);
        assert!(!page.more);
    }
}
