//! Values stored against the keys in [`crate::keys`].
//!
//! Records hold only bounded, fan-out data. A manifest lists its own layers and
//! children because both are small and fixed at push time. Anything with
//! unbounded fan-in - which manifests reference a blob, which tags point at a
//! manifest - lives in its own key range instead, so no value grows with the
//! size of the registry.

use serde::{Deserialize, Serialize};

use crate::digest::Digest;

pub type RepoId = u32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildRef {
    pub digest: Digest,
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRef {
    pub repo: RepoId,
    pub digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRecord {
    pub repo: RepoId,
    pub digest: Digest,
    pub media_type: String,
    /// Size of the manifest document itself, for the `Content-Length` on HEAD.
    pub size: u64,
    /// Sum of this manifest's own layer sizes. Not recursive: an index's total
    /// is computed by walking children, deduplicating shared layers.
    pub total_layer_size: u64,
    pub platform: Option<Platform>,
    /// Layers plus config - the blobs this manifest directly references.
    pub layers: Vec<Digest>,
    /// Per-platform manifests, for an index. Empty for an image manifest.
    #[serde(default)]
    pub children: Vec<ChildRef>,
    /// OCI 1.1 subject, if this manifest refers to another.
    #[serde(default)]
    pub subject: Option<Digest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRecord {
    pub size: u64,
}

/// An in-progress chunked blob upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadSession {
    pub repo: RepoId,
    /// Bytes committed so far; the next chunk must start here.
    pub offset: u64,
    /// Unix seconds. Purge uses this to expire abandoned uploads, and to avoid
    /// deleting a blob that an active upload is about to reference.
    pub started_at: u64,
    pub updated_at: u64,
}
