//! The filesystem blob store: content-addressed `digest -> bytes`, and nothing
//! else.
//!
//! # What this deliberately is not
//!
//! distribution's on-disk layout is two things fused together: a
//! content-addressed blob store, and a set of *link files* whose paths encode
//! repo-to-blob membership, tag-to-manifest, and manifest revisions. Those exist
//! because distribution has no metadata database - it encodes relationships as
//! filesystem paths so it can run against S3 alone. summ has RocksDB, where
//! those relationships already live as `P`, `R`, `T` and `G` keys. Reproducing
//! them here would mean two sources of truth that can silently diverge, and it
//! is precisely what makes distribution's GC a full storage-tree walk and its
//! catalog a recursive directory listing.
//!
//! So: **no directory structure in this crate carries meaning.** The fan-out
//! path is a hash of the content and can be recomputed from the digest alone;
//! deleting the whole tree loses bytes, never relationships. Nothing here reads
//! a directory to answer a question.
//!
//! There is also **no `StorageDriver` trait**. distribution's Reader/Writer
//! abstraction is the cautionary tale - it forces buffering that S3 does not
//! need. The trait comes in Phase 5, shaped by what a measured filesystem driver
//! and a real S3 driver turn out to have in common. What this API does do is
//! avoid the shapes that are known to be wrong on S3: there is no `move` (see
//! [`BlobStore::commit_upload`]), no "open a writer and stream into it", and no
//! operation that requires listing.
//!
//! # Layout
//!
//! ```text
//! <root>/blobs/<algo>/ab/cd/ef/<full-hex>     the file *is* the blob
//! <root>/uploads/<id>                         staging for an in-progress upload
//! ```
//!
//! Three levels of two hex characters, not distribution's
//! `blobs/sha256/ab/<full-hex>/data`. Two hex characters gives 256 first-level
//! buckets - roughly 400K subdirectories in each at 10^8 blobs - and the
//! per-blob directory doubles the inode count in order to hold exactly one file.
//! Three levels gives 16.7M buckets, about six blobs per directory at the same
//! scale. On S3 the prefix depth is irrelevant, but one layout across drivers
//! costs nothing.
//!
//! # Ordering rule
//!
//! **Blob bytes land and are fsynced before the metadata batch commits.** A blob
//! with no metadata is harmless garbage that purge reclaims; metadata
//! referencing a missing blob is corruption that surfaces as a failed pull.
//! [`BlobStore::commit_upload`] fsyncs the data *and* the containing directory
//! before it returns, so a caller that commits its `WriteBatch` afterwards gets
//! that ordering for free.
//!
//! # Hashing
//!
//! The hasher advances as bytes are written and is never re-run over a stored
//! blob. zot's S3 path costs three full passes over every layer - complete the
//! multipart upload, re-read the whole object to hash it, then copy it to its
//! final key - and that is a named anti-pattern here. Committing compares the
//! accumulated digest against the client's; a mismatch is rejected and no blob
//! is created.
//!
//! The in-progress hasher state serialises to bytes ([`Upload::hasher_state`])
//! so an interrupted chunked upload can resume on *any* process, which is what
//! stops chunked uploads becoming an HA constraint. Persisting it is the
//! metadata layer's job - it belongs in `UploadSession.hasher_state` - and this
//! crate only produces and consumes the bytes.
//!
//! # Error mapping
//!
//! [`summ_core::SummError`] has no variant per failure mode, so the two errors
//! the HTTP layer must distinguish are carried on distinct variants:
//!
//! | Failure | Variant | HTTP |
//! |---|---|---|
//! | Chunk does not start at the committed offset | [`SummError::InvalidData`] | `416` |
//! | Accumulated digest != the client's `?digest=` | [`SummError::InvalidDigest`] | `400 DIGEST_INVALID` |
//! | Blob or upload absent | [`SummError::NotFound`] | `404` |
//! | Anything from the filesystem | [`SummError::Storage`] | `500` |
//!
//! `InvalidData` is used *only* for the offset case on the append path, so
//! matching on the variant is enough; no message parsing is required.
//!
//! # Platform
//!
//! Unix only. The read and write paths use `pread`/`pwrite`
//! (`std::os::unix::fs::FileExt`) rather than seek-then-read: no cursor state,
//! one syscall instead of two, and the same `File` can serve two ranges
//! concurrently without a lock.

mod algorithm;
mod hasher;
mod paths;
mod read;
mod store;
mod upload;

pub use algorithm::DigestAlgorithm;
pub use read::{Blob, BlobStream, ByteRange, ResolvedRange};
pub use store::{BlobStore, DEFAULT_READ_CHUNK_SIZE, MIN_READ_CHUNK_SIZE};
pub use upload::{Upload, UploadId};
