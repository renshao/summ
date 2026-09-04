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

/// Hours in a day, and therefore the width of every array in
/// [`CounterBucket`].
pub const HOURS_PER_DAY: usize = 24;

/// `A <scope> <...> <day> <shard>` - one day's counters for one subject,
/// broken down by hour.
///
/// Absolute values, not deltas: the aggregation worker accumulates increments
/// in memory and writes the current value each flush, so the batch stays a
/// plain `Put` with deterministic content and no read-modify-write appears on
/// the write path.
///
/// **Per hour, UTC, and there is no stored day total.** The day figure is the
/// sum of the array, which is 24 additions and cannot disagree with the parts;
/// a cached total beside them would be a second source of truth for one number.
/// The hours are what make the numbers honest for a reader who is not in UTC:
/// the day bucket is fixed at write time and must never be re-bucketed, or the
/// same wall changes shape depending on who is looking at it, but an hourly
/// breakdown can be *re-summed* into any zone, and it answers "when in the day
/// does this get pulled" as a fold over the same scan.
///
/// A struct rather than a bare array because a second metric must not mean a
/// second key range. The arrays are fixed-width, so this is bounded fan-out and
/// not a value that grows with the registry.
///
/// **The hours arrived after the scalars, and cost no schema bump.** Adding a
/// field to a stored record is normally a migration gated on
/// [`SCHEMA_VERSION`], because postcard is not self-describing and will not
/// decode a record written before the change. This one was free: the `A` range
/// had key builders, a value type and a prefix group but no writer in any
/// build that ever shipped, so no store on disk can hold one of these. It was
/// the last moment that was true - after the first `A` key exists, changing
/// this shape is a migration like any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterBucket {
    /// `GET /v2/<name>/manifests/<ref>` per hour. `HEAD` is not a pull.
    pub manifest_pulls: [u32; HOURS_PER_DAY],
    /// `GET /v2/<name>/blobs/<digest>` per hour, repo scope only.
    pub blob_pulls: [u32; HOURS_PER_DAY],
    /// Blob bytes actually written to the socket, per hour. A client that
    /// aborts mid-stream contributes what it received, not what it asked for.
    pub bytes_out: [u64; HOURS_PER_DAY],
}

impl Default for CounterBucket {
    fn default() -> Self {
        CounterBucket {
            manifest_pulls: [0; HOURS_PER_DAY],
            blob_pulls: [0; HOURS_PER_DAY],
            bytes_out: [0; HOURS_PER_DAY],
        }
    }
}

impl CounterBucket {
    /// Add one hour's increments, saturating.
    ///
    /// Saturating rather than wrapping because a counter that has run out of
    /// range should stop being useful, not start being wrong. `hour` outside
    /// `0..24` is ignored - it can only come from a caller that computed it
    /// wrongly, and a panic on the flush path would take the worker down.
    pub fn add(&mut self, hour: usize, manifest_pulls: u64, blob_pulls: u64, bytes_out: u64) {
        if hour >= HOURS_PER_DAY {
            return;
        }
        self.manifest_pulls[hour] =
            self.manifest_pulls[hour].saturating_add(manifest_pulls.min(u32::MAX as u64) as u32);
        self.blob_pulls[hour] =
            self.blob_pulls[hour].saturating_add(blob_pulls.min(u32::MAX as u64) as u32);
        self.bytes_out[hour] = self.bytes_out[hour].saturating_add(bytes_out);
    }

    /// The whole day, summed. There is no stored total; this is it.
    pub fn manifest_pulls_total(&self) -> u64 {
        self.manifest_pulls.iter().map(|&n| n as u64).sum()
    }

    pub fn blob_pulls_total(&self) -> u64 {
        self.blob_pulls.iter().map(|&n| n as u64).sum()
    }

    pub fn bytes_out_total(&self) -> u64 {
        self.bytes_out.iter().sum()
    }

    /// Whether anything at all was recorded. A bucket that folds to this is not
    /// worth a row in a response.
    pub fn is_empty(&self) -> bool {
        self.manifest_pulls_total() == 0
            && self.blob_pulls_total() == 0
            && self.bytes_out_total() == 0
    }
}
