//! Values stored against the keys in [`crate::keys`].
//!
//! Records hold only bounded, fan-out data. A manifest lists its own layers and
//! children because both are small and fixed at push time. Anything with
//! unbounded fan-in - which manifests reference a blob, which tags point at a
//! manifest - lives in its own key range instead, so no value grows with the
//! size of the registry.
//!
//! Several records denormalise a descriptor that could in principle be looked
//! up elsewhere. That is deliberate and bounded: `ReferrerRecord` copies the
//! referrer's own descriptor because the referrers response is an image index
//! that cannot be built without it, and `TagEvent` copies one because tag
//! history must stay queryable after the manifest it names has been deleted.
//! Both are one descriptor per edge, not a set that grows.
//!
//! postcard is not self-describing, so a record written before a field was
//! added will not decode afterwards. Adding a field is therefore a migration,
//! gated on [`crate::keys::db_version`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;

pub type RepoId = u32;

/// Schema version written to [`crate::keys::db_version`] on store creation.
///
/// Bump when a stored record's layout changes, and add a migration for the
/// step. A store whose version is greater than this must be refused rather
/// than opened: a newer summ may have written records this build cannot decode.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    /// Deliberately not `skip_serializing_if`. postcard is not
    /// self-describing, so a skipped field is not "absent" on the wire - it is
    /// simply missing, and the decoder reads the following field's bytes
    /// instead. An absent variant is the common case (`linux/amd64` has none),
    /// so with that attribute every ordinary multi-arch index wrote an `M`
    /// record that could not be read back. See tests/postcard_roundtrip.rs.
    #[serde(default)]
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
    /// OCI 1.1 `artifactType`, needed to answer a filtered referrers query.
    #[serde(default)]
    pub artifact_type: Option<String>,
    /// Manifest annotations. `BTreeMap` so the encoding is order-stable.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// Unix seconds at push. Supplied by the caller, never minted at apply
    /// time, so a batch means the same thing wherever it is replayed.
    #[serde(default)]
    pub pushed_at: u64,
}

/// `L <digest>` - global blob metadata. Content is deduplicated registry-wide,
/// so this record says nothing about who may pull it; see `P` and `R`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRecord {
    pub size: u64,
}

/// `P <repo> <digest>` - a repo's blob set, including blobs uploaded but not
/// yet referenced by any manifest.
///
/// `added_at` is the grace clock: an unreferenced blob is only reclaimable once
/// it has been sitting here longer than the grace period, which is what stops
/// purge racing a push between its blob uploads and its manifest PUT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoBlobRecord {
    pub size: u64,
    pub added_at: u64,
}

/// `T <repo> <tag>` - the tag's current target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRecord {
    pub digest: Digest,
    pub tagged_at: u64,
}

/// `F <repo> <subject> <referrer>` - one referrer's own descriptor.
///
/// The referrers response is an image index whose entries require
/// `artifactType` and `annotations`, and `?artifactType=` filters on them, so
/// the edge cannot be valueless: the endpoint would otherwise need a point
/// lookup per referrer to build its response. With this it is a single ordered
/// prefix scan with the filter applied during it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferrerRecord {
    pub media_type: String,
    #[serde(default)]
    pub artifact_type: Option<String>,
    pub size: u64,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
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
    /// Digest algorithm this upload is being hashed under, from
    /// `?digest-algorithm=` or the default. Stored as the spec's name
    /// (`"sha256"` / `"sha512"`).
    pub algorithm: String,
    /// Serialised hasher state at `offset`, so a resumed chunked upload need
    /// not rehash from zero - 104 bytes for sha256.
    ///
    /// It lives here rather than on the storage driver so an interrupted
    /// upload can resume on any process, which is what keeps chunked uploads
    /// from becoming an HA constraint.
    ///
    /// This is `sha2`'s `hazmat` serialisation and is not to be exposed outside
    /// the metadata store.
    #[serde(default)]
    pub hasher_state: Option<Vec<u8>>,
}

/// Whether a tag event created or removed the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TagEventKind {
    Created,
    Deleted,
}

/// `H <repo> <tag> 0x00 <!ts> <digest>` and `J <repo> <digest> <!ts> <tag>`.
///
/// Written in the same batch as the tag mutation itself, never through the
/// analytics queue: a dropped history record is a hole in an audit trail,
/// where a dropped pull count is a rounding error.
///
/// The descriptor is denormalised because history must remain queryable after
/// the manifest is deleted, at which point `M <repo> <digest>` is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagEvent {
    pub event: TagEventKind,
    pub media_type: String,
    pub size: u64,
}

/// `A <scope> <...> <day> <shard>` - one day's counters for one subject.
///
/// Absolute values, not deltas: the aggregation worker holds the running total
/// in memory and writes the current value each flush, so the batch stays a
/// plain `Put` with deterministic content and no read-modify-write appears on
/// the write path.
///
/// A struct rather than a bare `u64` because a second metric must not mean a
/// second key range.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterBucket {
    pub manifest_pulls: u64,
    pub blob_pulls: u64,
    pub bytes_out: u64,
}
