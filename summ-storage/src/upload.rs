//! Staged uploads.
//!
//! An upload is a staging file plus a running hasher. It is identified by an
//! opaque id the *caller* mints - never this crate, and never the metadata
//! engine - so that a `WriteBatch` describing the upload means the same thing
//! wherever it is replayed.
//!
//! Session bookkeeping (started/updated timestamps, expiry, the repo it belongs
//! to) is not here. distribution keeps `_uploads/<id>/startedat` as a file so it
//! can expire abandoned uploads; summ has `UploadSession` under a `U` key, which
//! is cheaper to scan and already transactional with everything else. What this
//! crate owns is the bytes and the hash, and it hands the hash state back for
//! the caller to store.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use bytes::Bytes;
use summ_core::{Result, SummError};

use crate::algorithm::DigestAlgorithm;
use crate::hasher::Hasher;

/// An opaque upload identifier, validated so it can safely be a filename.
///
/// The id is a path component, so it must not be able to escape the uploads
/// directory. Restricting it to an unreserved subset of URL-safe characters
/// makes traversal impossible by construction rather than by sanitising - and
/// keeps the value usable verbatim in the `Location` header.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UploadId(String);

impl UploadId {
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        let ok = !id.is_empty()
            && id.len() <= 128
            && id != "."
            && id != ".."
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'));
        if ok {
            Ok(UploadId(id))
        } else {
            Err(SummError::InvalidData(format!(
                "malformed upload id {id:?}"
            )))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UploadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// State that has to cross into `spawn_blocking` and come back.
struct Staged {
    file: File,
    hasher: Hasher,
}

/// An in-progress upload: append-only, hashed on the way in.
pub struct Upload {
    id: UploadId,
    path: PathBuf,
    algorithm: DigestAlgorithm,
    offset: u64,
    staged: Option<Staged>,
}

/// Hand-written so the hasher state cannot reach a log line. It is `hazmat`,
/// and a derived `Debug` on the enum holding it would be one `tracing` call away
/// from printing it.
impl std::fmt::Debug for Upload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upload")
            .field("id", &self.id)
            .field("path", &self.path)
            .field("algorithm", &self.algorithm)
            .field("offset", &self.offset)
            .finish_non_exhaustive()
    }
}

impl Upload {
    pub(crate) fn open(
        id: UploadId,
        path: PathBuf,
        algorithm: DigestAlgorithm,
        offset: u64,
        hasher: Hasher,
        file: File,
    ) -> Self {
        Upload {
            id,
            path,
            algorithm,
            offset,
            staged: Some(Staged { file, hasher }),
        }
    }

    pub fn id(&self) -> &UploadId {
        &self.id
    }

    pub fn algorithm(&self) -> DigestAlgorithm {
        self.algorithm
    }

    /// Bytes committed so far. The next chunk must start exactly here; this is
    /// the value the `Range: 0-<offset-1>` response header is built from.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The hasher state at [`Upload::offset`], for `UploadSession.hasher_state`.
    ///
    /// This is `sha2`'s `hazmat` serialisation. It goes straight into the
    /// metadata record and nowhere else - it must not be logged, returned to a
    /// client, or persisted anywhere a blob's bytes are not already trusted.
    ///
    /// Store it in the same batch that stores the new offset. The two together
    /// are the resume point; either alone is useless.
    pub fn hasher_state(&self) -> Result<Vec<u8>> {
        Ok(self.staged()?.hasher.serialize_state())
    }

    fn staged(&self) -> Result<&Staged> {
        self.staged.as_ref().ok_or_else(Self::poisoned)
    }

    fn poisoned() -> SummError {
        SummError::Storage(
            "upload handle is unusable: a previous append did not return its state".into(),
        )
    }

    /// Append `chunk` at `offset`.
    ///
    /// `offset` is the client's claim about where this chunk starts, from the
    /// `Content-Range` of a chunked `PATCH`. It must equal [`Upload::offset`];
    /// anything else is [`SummError::InvalidData`], which the caller turns into
    /// a `416`.
    ///
    /// The check happens **before the file is touched**, because the spec
    /// requires a rejected chunk to leave the session byte-identical - the
    /// client recovers by `GET`ting the upload status and retrying from the
    /// offset it reports. A writer that appended first and validated afterwards
    /// would corrupt the upload on every out-of-order chunk.
    pub async fn append(&mut self, offset: u64, chunk: Bytes) -> Result<u64> {
        if offset != self.offset {
            return Err(SummError::InvalidData(format!(
                "out-of-order chunk: upload {} is at offset {}, chunk starts at {offset}",
                self.id, self.offset
            )));
        }
        if chunk.is_empty() {
            return Ok(self.offset);
        }

        let mut staged = self.staged.take().ok_or_else(Self::poisoned)?;
        let at = self.offset;
        let (staged, written) = tokio::task::spawn_blocking(move || {
            // Bytes first, hasher second: a failed write must not advance the
            // hash, or the digest would describe bytes that are not on disk.
            let result = write_all_at(&staged.file, at, &chunk);
            if result.is_ok() {
                staged.hasher.update(&chunk);
            }
            (staged, result.map(|_| chunk.len() as u64))
        })
        .await
        .map_err(|e| SummError::Storage(format!("upload append task failed: {e}")))?;
        self.staged = Some(staged);

        let written = written.map_err(|e| {
            SummError::Storage(format!("writing to upload {}: {e}", self.path.display()))
        })?;
        self.offset += written;
        Ok(self.offset)
    }

    /// Flush and fsync the staged bytes, then finalise the hash.
    ///
    /// Used by [`crate::BlobStore::commit_upload`], which is where the fsync
    /// ordering rule is enforced.
    pub(crate) async fn seal(self) -> Result<(PathBuf, summ_core::Digest, u64)> {
        let staged = self.staged.ok_or_else(Self::poisoned)?;
        let path = self.path;
        let offset = self.offset;
        let digest = tokio::task::spawn_blocking(move || {
            staged.file.sync_all().map(|()| staged.hasher.finalize())
        })
        .await
        .map_err(|e| SummError::Storage(format!("upload seal task failed: {e}")))?
        .map_err(|e| SummError::Storage(format!("fsyncing upload {}: {e}", path.display())))?;
        Ok((path, digest, offset))
    }
}

/// `pwrite` until the chunk is fully written.
///
/// No fsync per chunk. Durability of the finished blob comes from the fsync in
/// `commit_upload`, and paying for one per chunk would slow the push path for a
/// case - machine crash mid-upload - that the client already has to handle by
/// restarting the upload.
fn write_all_at(file: &File, offset: u64, mut buf: &[u8]) -> std::io::Result<()> {
    let mut at = offset;
    while !buf.is_empty() {
        match file.write_at(buf, at) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "short write to upload staging file",
                ))
            }
            Ok(n) => {
                buf = &buf[n..];
                at += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub(crate) fn open_staging(path: &PathBuf, create_new: bool) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    if create_new {
        opts.create_new(true);
    }
    opts.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => SummError::NotFound,
        _ => SummError::Storage(format!("opening upload {}: {e}", path.display())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_ids_cannot_escape_the_uploads_directory() {
        assert!(UploadId::new("2f1c9a4e-0000-4000-8000-000000000001").is_ok());
        assert!(UploadId::new("..").is_err());
        assert!(UploadId::new(".").is_err());
        assert!(UploadId::new("../../etc/passwd").is_err());
        assert!(UploadId::new("a/b").is_err());
        assert!(UploadId::new("").is_err());
        assert!(UploadId::new("x".repeat(129)).is_err());
    }
}
