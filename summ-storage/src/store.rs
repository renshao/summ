//! The store itself.

use std::path::{Path, PathBuf};

use summ_core::{Digest, Result, SummError};

use crate::algorithm::DigestAlgorithm;
use crate::hasher::Hasher;
use crate::paths::{blob_path, create_dir_durable, fsync_dir, upload_path, BLOBS_DIR, UPLOADS_DIR};
use crate::read::Blob;
use crate::upload::{open_staging, Upload, UploadId};

/// R2 measured this as the whole ball game: 4 KiB costs 11-15 % of an 8-vCPU box
/// at line rate, 64 KiB 5-11 %, 1 MiB 2-5 %. Every Rust file server surveyed
/// defaults to 4-64 KiB, which is 3-5x too small.
pub const DEFAULT_READ_CHUNK_SIZE: usize = 1024 * 1024;

/// Below this the per-chunk fixed cost (a measured ~5 µs `spawn_blocking` round
/// trip) starts to dominate, so a smaller configured value is raised to it
/// rather than honoured.
pub const MIN_READ_CHUNK_SIZE: usize = 256 * 1024;

/// A content-addressed blob store on a local filesystem.
///
/// Cheap to clone conceptually - it holds a path and a number - but it is also
/// fine to share behind an `Arc`; every method takes `&self` and holds no
/// mutable state.
pub struct BlobStore {
    root: PathBuf,
    read_chunk_size: usize,
}

impl BlobStore {
    /// Open (creating if needed) a store rooted at `root`.
    ///
    /// The uploads directory has to share a filesystem with the blobs
    /// directory, because committing is a rename and a rename does not cross
    /// mount points. Keeping both under one root is what guarantees that.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for dir in [root.join(BLOBS_DIR), root.join(UPLOADS_DIR)] {
            std::fs::create_dir_all(&dir)
                .map_err(|e| SummError::Storage(format!("creating {}: {e}", dir.display())))?;
        }
        fsync_dir(&root)
            .map_err(|e| SummError::Storage(format!("fsyncing {}: {e}", root.display())))?;
        Ok(BlobStore {
            root,
            read_chunk_size: DEFAULT_READ_CHUNK_SIZE,
        })
    }

    /// Override the read chunk size. Values below [`MIN_READ_CHUNK_SIZE`] are
    /// raised to it.
    pub fn with_read_chunk_size(mut self, size: usize) -> Self {
        self.read_chunk_size = size.max(MIN_READ_CHUNK_SIZE);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ---------------------------------------------------------------- reads

    /// Size of a blob, or `None` if it is not here.
    ///
    /// A `stat`, not an open: this backs `HEAD /v2/<name>/blobs/<digest>`, which
    /// must never degrade into "GET and discard the body".
    pub async fn stat(&self, digest: &Digest) -> Result<Option<u64>> {
        let (_, path) = blob_path(&self.root, digest);
        match tokio::fs::metadata(&path).await {
            Ok(md) => Ok(Some(md.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SummError::Storage(format!("stat {}: {e}", path.display()))),
        }
    }

    /// Whether a blob is present. Existence only - it says nothing about which
    /// repositories may serve it, which is a question only `P` and `R` answer.
    pub async fn contains(&self, digest: &Digest) -> Result<bool> {
        Ok(self.stat(digest).await?.is_some())
    }

    /// Open a blob for reading. [`SummError::NotFound`] if it is absent.
    pub async fn open_blob(&self, digest: &Digest) -> Result<Blob> {
        let (_, path) = blob_path(&self.root, digest);
        let chunk_size = self.read_chunk_size;
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&path).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => SummError::NotFound,
                _ => SummError::Storage(format!("opening blob {}: {e}", path.display())),
            })?;
            let size = file
                .metadata()
                .map_err(|e| SummError::Storage(format!("stat blob {}: {e}", path.display())))?
                .len();
            Ok(Blob::new(file, size, chunk_size))
        })
        .await
        .map_err(|e| SummError::Storage(format!("blob open task failed: {e}")))?
    }

    /// Remove a blob. `false` if it was already gone.
    ///
    /// Purge needs this. The now-possibly-empty fan-out directories are left
    /// behind deliberately: removing one races a concurrent commit into the same
    /// bucket, and 16.7M empty directories cost an inode each and nothing else.
    pub async fn delete_blob(&self, digest: &Digest) -> Result<bool> {
        let (_, path) = blob_path(&self.root, digest);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(SummError::Storage(format!(
                "deleting blob {}: {e}",
                path.display()
            ))),
        }
    }

    // -------------------------------------------------------------- uploads

    /// Begin a new upload under a caller-supplied id.
    ///
    /// Fails if the id is already in use, rather than truncating: an id
    /// collision means the caller's id generation is broken, and silently
    /// discarding somebody else's in-flight upload is the worse outcome.
    pub async fn create_upload(&self, id: &UploadId, algorithm: DigestAlgorithm) -> Result<Upload> {
        let path = upload_path(&self.root, id.as_str());
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            let file = open_staging(&path, true)?;
            Ok(Upload::open(
                id,
                path,
                algorithm,
                0,
                Hasher::new(algorithm),
                file,
            ))
        })
        .await
        .map_err(|e| SummError::Storage(format!("upload create task failed: {e}")))?
    }

    /// Reopen an upload from its persisted `UploadSession`.
    ///
    /// `offset` and `hasher_state` are the pair stored under the `U` key; they
    /// must come from the same batch. Because they were committed *after* the
    /// bytes were written, the staging file can legitimately be longer than
    /// `offset` - a crash between the write and the metadata commit - so it is
    /// truncated back to the recorded offset. It can never legitimately be
    /// shorter, and a short file is reported rather than papered over: resuming
    /// on top of a hole would produce a blob whose digest mismatch looks like
    /// the client's fault.
    ///
    /// This is what makes chunked uploads survivable across processes, and
    /// therefore not an HA constraint.
    pub async fn resume_upload(
        &self,
        id: &UploadId,
        algorithm: DigestAlgorithm,
        offset: u64,
        hasher_state: &[u8],
    ) -> Result<Upload> {
        let path = upload_path(&self.root, id.as_str());
        let id = id.clone();
        let hasher = Hasher::restore(algorithm, hasher_state)?;
        tokio::task::spawn_blocking(move || {
            let file = open_staging(&path, false)?;
            let len = file
                .metadata()
                .map_err(|e| SummError::Storage(format!("stat upload {}: {e}", path.display())))?
                .len();
            if len < offset {
                return Err(SummError::Storage(format!(
                    "upload {} is {len} bytes but its session records offset {offset}",
                    path.display()
                )));
            }
            if len > offset {
                file.set_len(offset).map_err(|e| {
                    SummError::Storage(format!("truncating upload {}: {e}", path.display()))
                })?;
            }
            Ok(Upload::open(id, path, algorithm, offset, hasher, file))
        })
        .await
        .map_err(|e| SummError::Storage(format!("upload resume task failed: {e}")))?
    }

    /// Commit an upload as `expected`, returning the blob's size.
    ///
    /// **Not `move`.** distribution's driver trait exposes `Move`, which on S3
    /// is a lie: there is no rename, so it degrades to copy-then-delete, and
    /// copying a multi-gigabyte layer in order to commit it is pathological.
    /// "Commit this upload as this digest" is implementable natively everywhere:
    /// a rename here, a multipart completion straight at the final key on S3.
    /// That is the single most important thing to take from distribution, and it
    /// is a lesson by counter-example.
    ///
    /// The digest is the one accumulated while the bytes were written; the blob
    /// is never re-read to verify it. A mismatch is
    /// [`SummError::InvalidDigest`] (`400 DIGEST_INVALID`) and creates no blob.
    /// The staging file is left in place so the caller can still answer an
    /// upload-status `GET` and can cancel the session explicitly; drop it with
    /// [`BlobStore::cancel_upload`].
    ///
    /// On success the bytes are fsynced *and* the containing directory is
    /// fsynced before this returns, so a caller that commits its `WriteBatch`
    /// afterwards satisfies the ordering rule: blob bytes land first, metadata
    /// is the commit point.
    pub async fn commit_upload(&self, upload: Upload, expected: &Digest) -> Result<u64> {
        let (staging, actual, size) = upload.seal().await?;

        if actual != *expected {
            return Err(SummError::InvalidDigest(format!(
                "digest mismatch: client declared {expected}, content hashes to {actual}"
            )));
        }

        let (dir, final_path) = blob_path(&self.root, expected);
        let blobs_root = self.root.join(BLOBS_DIR);
        tokio::task::spawn_blocking(move || {
            create_dir_durable(&dir, &blobs_root)
                .map_err(|e| SummError::Storage(format!("creating {}: {e}", dir.display())))?;
            // Atomic, and the same filesystem by construction. Renaming over an
            // existing blob is harmless: the store is content-addressed, so the
            // bytes are identical, and a reader holding the old file keeps
            // reading it through its own descriptor.
            std::fs::rename(&staging, &final_path).map_err(|e| {
                SummError::Storage(format!(
                    "committing {} as {}: {e}",
                    staging.display(),
                    final_path.display()
                ))
            })?;
            // Without this the rename itself can be lost on a crash, and a lost
            // rename under committed metadata is corruption rather than garbage.
            fsync_dir(&dir)
                .map_err(|e| SummError::Storage(format!("fsyncing {}: {e}", dir.display())))
        })
        .await
        .map_err(|e| SummError::Storage(format!("upload commit task failed: {e}")))??;

        Ok(size)
    }

    /// Discard an upload's staged bytes. Idempotent.
    pub async fn cancel_upload(&self, id: &UploadId) -> Result<()> {
        let path = upload_path(&self.root, id.as_str());
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SummError::Storage(format!(
                "cancelling upload {}: {e}",
                path.display()
            ))),
        }
    }
}
