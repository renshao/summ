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
use summ_core::Digest;

use crate::range::ByteRange;
use crate::reference::Reference;
use crate::seam::{
    BlobRead, Descriptor, ManifestPut, ManifestStat, OpsError, OpsResult, Page, Referrers,
    Registry, UploadBody,
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
}

#[derive(Debug)]
struct Upload {
    repo: String,
    algorithm: String,
    buffer: Vec<u8>,
}

#[derive(Debug, Default)]
struct State {
    repos: BTreeMap<String, Repo>,
    uploads: BTreeMap<String, Upload>,
}

#[derive(Debug, Default)]
pub struct MemoryRegistry {
    state: Mutex<State>,
}

impl MemoryRegistry {
    pub fn new() -> Self {
        Self::default()
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

/// Hash under the same algorithm the client named. summ never rehashes content
/// under a different algorithm, which is what lets `Docker-Content-Digest`
/// always echo the client's digest exactly.
fn hash_like(bytes: &[u8], like: &Digest) -> Digest {
    match like {
        Digest::Sha256(_) => sha256(bytes),
        Digest::Sha512(_) => sha512(bytes),
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
        if let Reference::Tag(tag) = reference {
            repo.tags.insert(tag.clone(), digest);
        }
        for tag in tags {
            repo.tags.insert(tag.clone(), digest);
        }

        Ok(ManifestPut {
            digest,
            subject,
            tags: tags.to_vec(),
        })
    }

    async fn delete_manifest(&self, name: &str, reference: &Reference) -> OpsResult<()> {
        let mut state = self.lock();
        let repo = state.repos.get_mut(name).ok_or(OpsError::RepoUnknown)?;
        match reference {
            // Deleting a tag leaves the manifest reachable by digest.
            Reference::Tag(tag) => {
                repo.tags.remove(tag).ok_or(OpsError::ManifestUnknown)?;
            }
            // Deleting by digest cascades to every tag pointing at it.
            Reference::Digest(digest) => {
                repo.manifests
                    .remove(digest)
                    .ok_or(OpsError::ManifestUnknown)?;
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

    async fn create_upload(&self, name: &str, id: &str, algorithm: &str) -> OpsResult<()> {
        let mut state = self.lock();
        state.repos.entry(name.to_owned()).or_default();
        state.uploads.insert(
            id.to_owned(),
            Upload {
                repo: name.to_owned(),
                algorithm: algorithm.to_owned(),
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
        // `algorithm` is what the session was opened with; a `?digest=` naming
        // a different one is a client error rather than a silent rehash.
        let expected_algorithm = digest.algorithm();
        if upload.algorithm != expected_algorithm {
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

    async fn referrers(
        &self,
        name: &str,
        subject: &Digest,
        artifact_type: Option<&str>,
    ) -> OpsResult<Referrers> {
        let state = self.lock();
        // An unknown repository is an empty list, not an error: the endpoint
        // must never 404 once it is enabled.
        let Some(repo) = state.repos.get(name) else {
            return Ok(Referrers {
                manifests: Vec::new(),
                filter_applied: artifact_type.is_some(),
            });
        };
        let manifests = repo
            .manifests
            .iter()
            .filter(|(_, m)| m.subject.as_ref() == Some(subject))
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
        })
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
