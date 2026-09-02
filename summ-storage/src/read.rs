//! The read path: an open blob, a byte range, and a chunked stream.
//!
//! R2 settled the shape of this and the answer was "do the simple thing well".
//! Measured on an 8-vCPU box at line rate, moving bytes from page cache to
//! socket costs 11-15 % of the machine with a naive 4 KiB `ReaderStream`, 5-11 %
//! at 64 KiB, **2-5 % at 1 MiB**, and 2-2.4 % with true `sendfile`. So chunk
//! size is worth 3-5x and zero-copy is worth about 1 % - bought at the price of
//! fighting hyper for the socket, breaking under TLS, and blocking the reactor
//! on page-cache misses. Hence: `pread` in `spawn_blocking`, 1 MiB chunks,
//! `Bytes` handed to hyper's `writev`. No `sendfile`, no `mmap`, no io_uring
//! runtime.
//!
//! `pread` rather than seek-then-read for three reasons: no seek syscall, no
//! cursor state to keep, and the offset arithmetic a range needs is the same
//! arithmetic the chunking already does.

use std::fs::File;
use std::future::Future;
use std::io;
use std::os::unix::fs::FileExt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use tokio::task::JoinHandle;

/// A `Range` header value, parsed but not yet resolved against a size.
///
/// Parsing the header is the HTTP layer's job; deciding what it means is not,
/// because that needs the blob's length. The conformance suite exercises six
/// cases against a 2048-byte blob and all six fall out of
/// [`ByteRange::resolve`].
///
/// containerd only ever sends [`ByteRange::From`] - `bytes=N-` to resume an
/// interrupted layer fetch - so that is the case to care about most, not the
/// one to treat as an afterthought.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// `bytes=<start>-<end>`, inclusive at both ends.
    Inclusive { start: u64, end: u64 },
    /// `bytes=<start>-`, open ended.
    From { start: u64 },
    /// `bytes=-<len>`, the last `len` bytes.
    Suffix { len: u64 },
}

impl ByteRange {
    /// Resolve against a known blob size.
    ///
    /// `None` means unsatisfiable, which the caller turns into a `416`. Note
    /// that an over-long *end* is clamped rather than rejected (`bytes=2000-5000`
    /// on a 2048-byte blob is a `206` of 48 bytes), while an over-long *start*
    /// is unsatisfiable - RFC 9110 §14.1.1, and both are tested by the suite.
    pub fn resolve(self, size: u64) -> Option<ResolvedRange> {
        match self {
            ByteRange::Inclusive { start, end } => {
                if start > end || start >= size {
                    return None;
                }
                Some(ResolvedRange {
                    start,
                    end: end.min(size - 1),
                })
            }
            ByteRange::From { start } => {
                if start >= size {
                    return None;
                }
                Some(ResolvedRange {
                    start,
                    end: size - 1,
                })
            }
            ByteRange::Suffix { len } => {
                // A zero-length suffix is unsatisfiable, and so is any suffix of
                // an empty blob.
                if len == 0 || size == 0 {
                    return None;
                }
                Some(ResolvedRange {
                    start: size.saturating_sub(len),
                    end: size - 1,
                })
            }
        }
    }
}

/// A range known to lie inside a specific blob.
///
/// Constructed only by [`ByteRange::resolve`] or [`Blob::resolve`], so a
/// `ResolvedRange` in hand means the `416` decision has already been made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedRange {
    start: u64,
    end: u64,
}

impl ResolvedRange {
    pub fn start(self) -> u64 {
        self.start
    }

    /// Inclusive, matching the `Content-Range: bytes <start>-<end>/<total>` form
    /// the download path uses. (The *upload* path's `Content-Range` is a
    /// different grammar entirely - a bare `0-1023` with no `bytes ` prefix -
    /// which is why neither is parsed in this crate.)
    pub fn end(self) -> u64 {
        self.end
    }

    /// The `Content-Length` of the `206`.
    pub fn len(self) -> u64 {
        self.end - self.start + 1
    }

    /// Always false: a resolved range covers at least one byte, because an
    /// empty range is unsatisfiable and resolves to `None`.
    pub fn is_empty(self) -> bool {
        false
    }
}

/// An open blob. Holds the file descriptor; carries the size so the caller can
/// build `Content-Length` and `Content-Range` without a second `stat`.
pub struct Blob {
    file: Arc<File>,
    size: u64,
    chunk_size: usize,
}

impl Blob {
    pub(crate) fn new(file: File, size: u64, chunk_size: usize) -> Self {
        Self {
            file: Arc::new(file),
            size,
            chunk_size,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Resolve a `Range` against this blob. `None` is a `416`.
    pub fn resolve(&self, range: ByteRange) -> Option<ResolvedRange> {
        range.resolve(self.size)
    }

    /// Stream the whole blob.
    pub fn stream(self) -> BlobStream {
        let size = self.size;
        self.stream_from(0, size)
    }

    /// Stream one resolved range.
    pub fn stream_range(self, range: ResolvedRange) -> BlobStream {
        // Clamp defensively: a `ResolvedRange` is only constructible against a
        // size, but nothing stops a caller resolving against one blob and
        // streaming another. Reading past EOF would otherwise abort the
        // response mid-body.
        let start = range.start.min(self.size);
        let end = range.end.min(self.size.saturating_sub(1));
        let len = if self.size == 0 || start > end {
            0
        } else {
            end - start + 1
        };
        self.stream_from(start, len)
    }

    fn stream_from(self, offset: u64, len: u64) -> BlobStream {
        BlobStream {
            file: self.file,
            next_offset: offset,
            end_offset: offset + len,
            chunk_size: self.chunk_size,
            inflight: None,
        }
    }
}

/// A blob body, delivered as `Bytes` for hyper to `writev`.
///
/// One read is always in flight ahead of the chunk being yielded, so the socket
/// is fed while the next `pread` runs. Raise
/// `hyper::server::conn::http1::Builder::max_buf_size` to a few MiB or hyper
/// will refuse to queue the prefetched chunk and the pipeline collapses back to
/// read-then-write.
///
/// **Aborted reads.** containerd 2.1+ requests `bytes=N-`, reads 8 MiB, and
/// kills the connection; Bottlerocket ships that on by default. Dropping this
/// stream cancels nothing already issued - a `spawn_blocking` task cannot be
/// interrupted - so the wasted work is bounded by exactly one chunk, and the
/// descriptor is released as soon as that read returns, because the blocking
/// task holds the only other `Arc<File>`. That is the reason to prefetch exactly
/// one chunk and not a window of them.
pub struct BlobStream {
    file: Arc<File>,
    /// Next byte to issue a read for.
    next_offset: u64,
    /// One past the last byte of the slice being served.
    end_offset: u64,
    chunk_size: usize,
    /// The prefetched read, with the length it will produce.
    inflight: Option<(JoinHandle<io::Result<Bytes>>, u64)>,
}

impl BlobStream {
    /// Bytes this stream has yet to yield - the `Content-Length` of the
    /// response, before anything has been consumed.
    pub fn len(&self) -> u64 {
        let queued = self.inflight.as_ref().map_or(0, |(_, len)| *len);
        (self.end_offset - self.next_offset) + queued
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn spawn_next(&mut self) {
        let remaining = self.end_offset - self.next_offset;
        if remaining == 0 {
            return;
        }
        let len = remaining.min(self.chunk_size as u64) as usize;
        let offset = self.next_offset;
        self.next_offset += len as u64;
        let file = Arc::clone(&self.file);
        let handle = tokio::task::spawn_blocking(move || read_exact_at(&file, offset, len));
        self.inflight = Some((handle, len as u64));
    }
}

impl Stream for BlobStream {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.inflight.is_none() {
            // Lazy so that constructing a stream needs no runtime; a no-op once
            // the last chunk has been issued, which is what ends the stream.
            this.spawn_next();
        }
        let handle = match this.inflight.as_mut() {
            Some((h, _)) => h,
            None => return Poll::Ready(None),
        };

        match Pin::new(handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(joined) => {
                this.inflight = None;
                match joined {
                    // Abort, do not apologise. The stream ends here rather than
                    // skipping to the chunk after the one that failed: a
                    // consumer that logged the error and kept polling would
                    // otherwise send a body with a hole in it, turning a
                    // retryable read failure into a digest mismatch on the
                    // client.
                    Err(e) => {
                        this.next_offset = this.end_offset;
                        Poll::Ready(Some(Err(io::Error::other(e))))
                    }
                    Ok(Err(e)) => {
                        this.next_offset = this.end_offset;
                        Poll::Ready(Some(Err(e)))
                    }
                    Ok(Ok(chunk)) => {
                        // Issue the next read *before* yielding this one, so the
                        // disk and the socket are busy at the same time.
                        this.spawn_next();
                        Poll::Ready(Some(Ok(chunk)))
                    }
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let chunk = self.chunk_size as u64;
        let total = self.len();
        let chunks = total.div_ceil(chunk.max(1));
        let chunks = usize::try_from(chunks).unwrap_or(usize::MAX);
        (chunks, Some(chunks))
    }
}

/// `pread` until the chunk is full.
///
/// A short read here means the blob shrank underneath us, which can only happen
/// if something outside this crate truncated it. Report it as an error and let
/// the connection tear down: appending anything, or padding, converts a
/// retryable short read into a digest mismatch on the client.
fn read_exact_at(file: &File, offset: u64, len: usize) -> io::Result<Bytes> {
    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    while filled < len {
        match file.read_at(&mut buf[filled..], offset + filled as u64) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "blob truncated while it was being served",
                ))
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The six cases the conformance suite runs against a 2048-byte blob,
    // verbatim from R1 §12.
    #[test]
    fn conformance_range_cases() {
        let size = 2048;
        let r = ByteRange::Inclusive {
            start: 500,
            end: 1499,
        }
        .resolve(size)
        .expect("500-1499");
        assert_eq!((r.start(), r.end(), r.len()), (500, 1499, 1000));

        let r = ByteRange::From { start: 500 }.resolve(size).expect("500-");
        assert_eq!((r.start(), r.end(), r.len()), (500, 2047, 1548));

        let r = ByteRange::Suffix { len: 500 }.resolve(size).expect("-500");
        assert_eq!((r.start(), r.end(), r.len()), (1548, 2047, 500));

        let r = ByteRange::Inclusive {
            start: 2000,
            end: 5000,
        }
        .resolve(size)
        .expect("2000-5000 clamps to EOF");
        assert_eq!((r.start(), r.end(), r.len()), (2000, 2047, 48));

        assert_eq!(
            ByteRange::Inclusive { start: 500, end: 0 }.resolve(size),
            None,
            "start > end must be a 416"
        );
        assert_eq!(
            ByteRange::Inclusive {
                start: 5000,
                end: 10000
            }
            .resolve(size),
            None,
            "start past EOF must be a 416"
        );
    }

    #[test]
    fn degenerate_ranges() {
        assert_eq!(ByteRange::From { start: 0 }.resolve(0), None);
        assert_eq!(ByteRange::Suffix { len: 0 }.resolve(2048), None);
        // A suffix longer than the blob is the whole blob, not an error.
        let r = ByteRange::Suffix { len: 9999 }
            .resolve(2048)
            .expect("-9999");
        assert_eq!((r.start(), r.end()), (0, 2047));
        // A single byte at the last offset.
        let r = ByteRange::Inclusive {
            start: 2047,
            end: 2047,
        }
        .resolve(2048)
        .expect("last byte");
        assert_eq!(r.len(), 1);
    }
}
