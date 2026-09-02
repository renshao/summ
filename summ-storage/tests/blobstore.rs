//! Integration tests against a real filesystem.
//!
//! These deliberately go through `tempfile::TempDir` rather than any in-memory
//! fake: the properties under test are fsync ordering, atomic rename, path
//! layout and descriptor lifetime, none of which a fake would exercise.

use bytes::Bytes;
use futures_util::StreamExt;
use sha2::{Digest as _, Sha256, Sha512};
use summ_core::{Digest, SummError};
use summ_storage::{
    BlobStore, ByteRange, DigestAlgorithm, UploadId, DEFAULT_READ_CHUNK_SIZE, MIN_READ_CHUNK_SIZE,
};
use tempfile::TempDir;

fn store() -> (TempDir, BlobStore) {
    let dir = TempDir::new().expect("tempdir");
    let store = BlobStore::open(dir.path()).expect("open store");
    (dir, store)
}

fn sha256_of(bytes: &[u8]) -> Digest {
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&Sha256::digest(bytes));
    Digest::Sha256(raw)
}

fn sha512_of(bytes: &[u8]) -> Digest {
    let mut raw = [0u8; 64];
    raw.copy_from_slice(&Sha512::digest(bytes));
    Digest::Sha512(raw)
}

fn upload_id(s: &str) -> UploadId {
    UploadId::new(s).expect("valid upload id")
}

/// Push a whole blob in one append, the monolithic PUT flow.
async fn put(store: &BlobStore, id: &str, algorithm: DigestAlgorithm, body: &[u8]) -> Digest {
    let id = upload_id(id);
    let mut upload = store
        .create_upload(&id, algorithm)
        .await
        .expect("create upload");
    upload
        .append(0, Bytes::copy_from_slice(body))
        .await
        .expect("append");
    let digest = match algorithm {
        DigestAlgorithm::Sha256 => sha256_of(body),
        DigestAlgorithm::Sha512 => sha512_of(body),
    };
    let size = store
        .commit_upload(upload, &digest)
        .await
        .expect("commit upload");
    assert_eq!(size, body.len() as u64);
    digest
}

async fn read_all(store: &BlobStore, digest: &Digest) -> Vec<u8> {
    let blob = store.open_blob(digest).await.expect("open blob");
    let mut stream = blob.stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

// --------------------------------------------------------------- round trips

#[tokio::test]
async fn round_trips_sha256_and_sha512() {
    let (_dir, store) = store();

    for algorithm in [DigestAlgorithm::Sha256, DigestAlgorithm::Sha512] {
        let body = format!("a layer hashed under {algorithm}").into_bytes();
        let digest = put(&store, algorithm.as_str(), algorithm, &body).await;

        assert_eq!(digest.algorithm(), algorithm.as_str());
        assert_eq!(
            store.stat(&digest).await.expect("stat"),
            Some(body.len() as u64)
        );
        assert!(store.contains(&digest).await.expect("contains"));
        assert_eq!(read_all(&store, &digest).await, body);
    }
}

#[tokio::test]
async fn the_on_disk_layout_is_three_levels_of_two_hex_chars() {
    let (_dir, store) = store();
    let digest = put(&store, "layout", DigestAlgorithm::Sha256, b"hello").await;

    let expected = blob_file(&store, &digest);
    assert!(expected.is_file(), "blob not at {}", expected.display());
    // The file *is* the blob: no per-blob directory holding a `data` file, which
    // is what doubles the inode count in distribution's layout.
    assert!(!expected.join("data").exists());
}

#[tokio::test]
async fn an_empty_blob_round_trips() {
    // The conformance suite pushes and pulls a zero-byte blob by default.
    let (_dir, store) = store();
    let digest = put(&store, "empty", DigestAlgorithm::Sha256, b"").await;
    assert_eq!(
        digest.to_string(),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(store.stat(&digest).await.expect("stat"), Some(0));
    assert_eq!(read_all(&store, &digest).await, Vec::<u8>::new());
}

#[tokio::test]
async fn streams_a_blob_larger_than_one_chunk() {
    let (_dir, store) = store();
    // Two and a half default chunks, so the prefetch-one-ahead path runs and the
    // final short chunk is exercised.
    let body: Vec<u8> = (0..DEFAULT_READ_CHUNK_SIZE * 5 / 2)
        .map(|i| (i % 251) as u8)
        .collect();
    let digest = put(&store, "big", DigestAlgorithm::Sha256, &body).await;

    let blob = store.open_blob(&digest).await.expect("open");
    let mut stream = blob.stream();
    assert_eq!(stream.len(), body.len() as u64);

    let mut chunks = 0;
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("chunk");
        assert!(chunk.len() <= DEFAULT_READ_CHUNK_SIZE);
        chunks += 1;
        out.extend_from_slice(&chunk);
    }
    assert_eq!(chunks, 3);
    assert_eq!(out, body);
}

#[tokio::test]
async fn missing_blobs_are_not_found() {
    let (_dir, store) = store();
    let digest = sha256_of(b"never pushed");
    assert_eq!(store.stat(&digest).await.expect("stat"), None);
    assert!(!store.contains(&digest).await.expect("contains"));
    assert!(matches!(
        store.open_blob(&digest).await,
        Err(SummError::NotFound)
    ));
}

#[tokio::test]
async fn deleting_a_blob_is_idempotent() {
    let (_dir, store) = store();
    let digest = put(&store, "doomed", DigestAlgorithm::Sha256, b"purge me").await;
    assert!(store.delete_blob(&digest).await.expect("first delete"));
    assert!(!store.delete_blob(&digest).await.expect("second delete"));
    assert!(!store.contains(&digest).await.expect("contains"));
}

// ------------------------------------------------------------------- uploads

#[tokio::test]
async fn a_chunked_upload_commits_across_several_appends() {
    let (_dir, store) = store();
    let chunks: [&[u8]; 4] = [
        b"the quick ",
        b"brown fox ",
        b"jumps over ",
        b"the lazy dog",
    ];
    let whole: Vec<u8> = chunks.concat();

    let id = upload_id("chunked");
    let mut upload = store
        .create_upload(&id, DigestAlgorithm::Sha256)
        .await
        .expect("create");

    let mut offset = 0u64;
    for chunk in chunks {
        offset = upload
            .append(offset, Bytes::from_static(chunk))
            .await
            .expect("append");
        assert_eq!(offset, upload.offset());
    }
    assert_eq!(offset, whole.len() as u64);

    let digest = sha256_of(&whole);
    assert_eq!(
        store.commit_upload(upload, &digest).await.expect("commit"),
        whole.len() as u64
    );
    assert_eq!(read_all(&store, &digest).await, whole);
}

#[tokio::test]
async fn an_out_of_order_append_is_rejected_and_leaves_the_session_untouched() {
    let (_dir, store) = store();
    let id = upload_id("outoforder");
    let mut upload = store
        .create_upload(&id, DigestAlgorithm::Sha256)
        .await
        .expect("create");

    upload
        .append(0, Bytes::from_static(b"first half "))
        .await
        .expect("append");
    let state_before = upload.hasher_state().expect("state");
    let offset_before = upload.offset();

    // A chunk starting past the committed offset: the caller turns this into a
    // 416, and the spec requires the session to be byte-identical afterwards.
    let err = upload
        .append(offset_before + 1, Bytes::from_static(b"second half"))
        .await
        .expect_err("must reject");
    assert!(matches!(err, SummError::InvalidData(_)), "got {err:?}");

    // ...and a chunk starting before it, i.e. a retried chunk.
    let err = upload
        .append(0, Bytes::from_static(b"second half"))
        .await
        .expect_err("must reject");
    assert!(matches!(err, SummError::InvalidData(_)), "got {err:?}");

    assert_eq!(upload.offset(), offset_before);
    assert_eq!(upload.hasher_state().expect("state"), state_before);

    // The session still works from the right offset.
    upload
        .append(offset_before, Bytes::from_static(b"second half"))
        .await
        .expect("append at the right offset");
    let digest = sha256_of(b"first half second half");
    store.commit_upload(upload, &digest).await.expect("commit");
    assert_eq!(read_all(&store, &digest).await, b"first half second half");
}

#[tokio::test]
async fn a_digest_mismatch_is_rejected_and_creates_no_blob() {
    let (_dir, store) = store();
    let id = upload_id("liar");
    let mut upload = store
        .create_upload(&id, DigestAlgorithm::Sha256)
        .await
        .expect("create");
    upload
        .append(0, Bytes::from_static(b"the actual bytes"))
        .await
        .expect("append");

    let claimed = sha256_of(b"what the client claimed");
    let err = store
        .commit_upload(upload, &claimed)
        .await
        .expect_err("must reject");
    assert!(matches!(err, SummError::InvalidDigest(_)), "got {err:?}");

    assert!(!store.contains(&claimed).await.expect("contains claimed"));
    assert!(!store
        .contains(&sha256_of(b"the actual bytes"))
        .await
        .expect("contains actual"));

    // The staging file survives so the caller can still answer an upload-status
    // GET; cancelling is explicit.
    store.cancel_upload(&id).await.expect("cancel");
    store
        .cancel_upload(&id)
        .await
        .expect("cancel is idempotent");
}

#[tokio::test]
async fn committing_a_blob_that_already_exists_is_fine() {
    let (_dir, store) = store();
    let body = b"deduplicated across repos";
    let first = put(&store, "dedup-a", DigestAlgorithm::Sha256, body).await;
    let second = put(&store, "dedup-b", DigestAlgorithm::Sha256, body).await;
    assert_eq!(first, second);
    assert_eq!(read_all(&store, &first).await, body);
}

#[tokio::test]
async fn an_upload_id_cannot_be_reused_while_it_is_live() {
    let (_dir, store) = store();
    let id = upload_id("taken");
    let _upload = store
        .create_upload(&id, DigestAlgorithm::Sha256)
        .await
        .expect("create");
    assert!(store
        .create_upload(&id, DigestAlgorithm::Sha256)
        .await
        .is_err());
}

#[tokio::test]
async fn resuming_an_unknown_upload_is_not_found() {
    let (_dir, store) = store();
    let state = {
        let id = upload_id("scratch");
        let upload = store
            .create_upload(&id, DigestAlgorithm::Sha256)
            .await
            .expect("create");
        let state = upload.hasher_state().expect("state");
        store.cancel_upload(&id).await.expect("cancel");
        state
    };
    let err = store
        .resume_upload(&upload_id("ghost"), DigestAlgorithm::Sha256, 0, &state)
        .await
        .expect_err("must fail");
    assert!(matches!(err, SummError::NotFound), "got {err:?}");
}

// ---------------------------------------------------------- resumable hashing

#[tokio::test]
async fn hasher_state_survives_a_process_restart() {
    for algorithm in [DigestAlgorithm::Sha256, DigestAlgorithm::Sha512] {
        let dir = TempDir::new().expect("tempdir");
        let id = upload_id("resumable");
        let head: &[u8] = b"the first chunk, uploaded before the crash; ";
        let tail: &[u8] = b"the second chunk, uploaded by a different process";
        let whole: Vec<u8> = [head, tail].concat();

        // Process one: append the head, persist offset + hasher state, exit.
        let (offset, state) = {
            let store = BlobStore::open(dir.path()).expect("open");
            let mut upload = store.create_upload(&id, algorithm).await.expect("create");
            let offset = upload
                .append(0, Bytes::copy_from_slice(head))
                .await
                .expect("append");
            (offset, upload.hasher_state().expect("state"))
        };
        assert_eq!(offset, head.len() as u64);
        // Small enough to live inside the `U` key's `UploadSession` record,
        // which is the whole reason it is not a file on the storage driver.
        assert!(state.len() <= 256, "hasher state is {} bytes", state.len());

        // Process two: rehydrate from exactly what the metadata store holds.
        let store = BlobStore::open(dir.path()).expect("reopen");
        let mut upload = store
            .resume_upload(&id, algorithm, offset, &state)
            .await
            .expect("resume");
        assert_eq!(upload.offset(), offset);
        upload
            .append(offset, Bytes::copy_from_slice(tail))
            .await
            .expect("append tail");

        let expected = match algorithm {
            DigestAlgorithm::Sha256 => sha256_of(&whole),
            DigestAlgorithm::Sha512 => sha512_of(&whole),
        };
        store
            .commit_upload(upload, &expected)
            .await
            .expect("the resumed digest must equal the uninterrupted one");
        assert_eq!(read_all(&store, &expected).await, whole);
    }
}

#[tokio::test]
async fn resuming_truncates_bytes_written_past_the_recorded_offset() {
    // A crash between writing a chunk and committing the metadata batch leaves
    // the staging file longer than the session's offset. Those bytes are not in
    // the hasher, so they must go.
    let (_dir, store) = store();
    let id = upload_id("torn");
    let head: &[u8] = b"committed bytes ";

    let (offset, state) = {
        let mut upload = store
            .create_upload(&id, DigestAlgorithm::Sha256)
            .await
            .expect("create");
        let offset = upload
            .append(0, Bytes::copy_from_slice(head))
            .await
            .expect("append");
        let state = upload.hasher_state().expect("state");
        // The chunk whose metadata never landed.
        upload
            .append(offset, Bytes::from_static(b"UNCOMMITTED"))
            .await
            .expect("append");
        (offset, state)
    };

    let mut upload = store
        .resume_upload(&id, DigestAlgorithm::Sha256, offset, &state)
        .await
        .expect("resume");
    upload
        .append(offset, Bytes::from_static(b"and the rest"))
        .await
        .expect("append");

    let expected = sha256_of(b"committed bytes and the rest");
    store
        .commit_upload(upload, &expected)
        .await
        .expect("commit");
    assert_eq!(
        read_all(&store, &expected).await,
        b"committed bytes and the rest"
    );
}

#[tokio::test]
async fn resuming_a_short_staging_file_is_an_error_not_a_hole() {
    let (_dir, store) = store();
    let id = upload_id("short");
    let mut upload = store
        .create_upload(&id, DigestAlgorithm::Sha256)
        .await
        .expect("create");
    upload
        .append(0, Bytes::from_static(b"only eight"))
        .await
        .expect("append");
    let state = upload.hasher_state().expect("state");
    drop(upload);

    let err = store
        .resume_upload(&id, DigestAlgorithm::Sha256, 9_999, &state)
        .await
        .expect_err("must fail");
    assert!(matches!(err, SummError::Storage(_)), "got {err:?}");
}

// -------------------------------------------------------------------- ranges

/// The six cases the conformance suite runs against a 2048-byte blob, end to
/// end through the read path rather than only through the range arithmetic.
#[tokio::test]
async fn every_conformance_range_case_end_to_end() {
    let (_dir, store) = store();
    let body: Vec<u8> = (0..2048u32).map(|i| (i % 256) as u8).collect();
    let digest = put(&store, "ranged", DigestAlgorithm::Sha256, &body).await;

    async fn slice(
        store: &BlobStore,
        digest: &Digest,
        range: ByteRange,
    ) -> Option<(u64, u64, Vec<u8>)> {
        let blob = store.open_blob(digest).await.expect("open");
        let resolved = blob.resolve(range)?;
        let mut stream = blob.stream_range(resolved);
        assert_eq!(stream.len(), resolved.len());
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        Some((resolved.start(), resolved.end(), out))
    }

    let (start, end, bytes) = slice(
        &store,
        &digest,
        ByteRange::Inclusive {
            start: 500,
            end: 1499,
        },
    )
    .await
    .expect("bytes=500-1499");
    assert_eq!((start, end, bytes.len()), (500, 1499, 1000));
    assert_eq!(bytes, body[500..=1499]);

    // The only form containerd sends, and the one to design around.
    let (start, end, bytes) = slice(&store, &digest, ByteRange::From { start: 500 })
        .await
        .expect("bytes=500-");
    assert_eq!((start, end, bytes.len()), (500, 2047, 1548));
    assert_eq!(bytes, body[500..]);

    let (start, end, bytes) = slice(&store, &digest, ByteRange::Suffix { len: 500 })
        .await
        .expect("bytes=-500");
    assert_eq!((start, end, bytes.len()), (1548, 2047, 500));
    assert_eq!(bytes, body[1548..]);

    let (start, end, bytes) = slice(
        &store,
        &digest,
        ByteRange::Inclusive {
            start: 2000,
            end: 5000,
        },
    )
    .await
    .expect("bytes=2000-5000 clamps to EOF");
    assert_eq!((start, end, bytes.len()), (2000, 2047, 48));
    assert_eq!(bytes, body[2000..]);

    // 416: start after end.
    assert!(
        slice(&store, &digest, ByteRange::Inclusive { start: 500, end: 0 })
            .await
            .is_none()
    );
    // 416: entirely past EOF.
    assert!(slice(
        &store,
        &digest,
        ByteRange::Inclusive {
            start: 5000,
            end: 10000
        }
    )
    .await
    .is_none());
}

#[tokio::test]
async fn an_open_ended_range_spans_several_chunks() {
    let body: Vec<u8> = (0..MIN_READ_CHUNK_SIZE * 3)
        .map(|i| (i % 241) as u8)
        .collect();
    let dir = TempDir::new().expect("tempdir");
    let store = BlobStore::open(dir.path())
        .expect("open")
        .with_read_chunk_size(MIN_READ_CHUNK_SIZE);

    let digest = put(&store, "spanning", DigestAlgorithm::Sha256, &body).await;
    let blob = store.open_blob(&digest).await.expect("open");
    let resolved = blob
        .resolve(ByteRange::From { start: 1000 })
        .expect("satisfiable");
    let mut stream = blob.stream_range(resolved);
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    assert_eq!(out, body[1000..]);
}

#[tokio::test]
async fn a_read_chunk_size_below_the_floor_is_raised_to_it() {
    let dir = TempDir::new().expect("tempdir");
    let store = BlobStore::open(dir.path())
        .expect("open")
        .with_read_chunk_size(4096);
    let body: Vec<u8> = (0..MIN_READ_CHUNK_SIZE + 7)
        .map(|i| (i % 97) as u8)
        .collect();
    let digest = put(&store, "floored", DigestAlgorithm::Sha256, &body).await;

    let mut stream = store.open_blob(&digest).await.expect("open").stream();
    let first = stream.next().await.expect("chunk").expect("ok");
    assert_eq!(
        first.len(),
        MIN_READ_CHUNK_SIZE,
        "4 KiB chunks are a 3-5x CPU regression"
    );
}

// -------------------------------------------------------------- aborted reads

/// How many of this process's descriptors point at `path`.
///
/// Counted by inode rather than by total descriptor count, because the test
/// harness runs these tests in parallel threads of one process and a raw count
/// would be whatever the other nineteen happened to be doing.
fn fds_open_on(path: &std::path::Path) -> usize {
    use std::os::unix::fs::MetadataExt;

    let target = match std::fs::metadata(path) {
        Ok(md) => md,
        Err(e) => panic!("stat {}: {e}", path.display()),
    };
    let fd_dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let entries = match std::fs::read_dir(fd_dir) {
        Ok(entries) => entries,
        Err(e) => panic!("read {fd_dir}: {e}"),
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            // Re-open rather than `fs::metadata`: on macOS a `stat` of
            // `/dev/fd/N` reports devfs's device id, so only an `fstat` through
            // a descriptor identifies the underlying file. Descriptors that
            // cannot be reopened (the inherited stdio pipes) are not blobs.
            std::fs::File::open(entry.path())
                .and_then(|f| f.metadata())
                .map(|md| md.ino() == target.ino() && md.dev() == target.dev())
                .unwrap_or(false)
        })
        .count()
}

fn blob_file(store: &BlobStore, digest: &Digest) -> std::path::PathBuf {
    let hex = digest
        .to_string()
        .split_once(':')
        .expect("algo:hex")
        .1
        .to_string();
    store
        .root()
        .join("blobs")
        .join(digest.algorithm())
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(&hex[4..6])
        .join(&hex)
}

/// containerd 2.1+ asks for `bytes=N-`, reads 8 MiB, and kills the connection;
/// Bottlerocket ships that on by default. Dropping the stream must release the
/// descriptor promptly - within one outstanding chunk read.
#[tokio::test]
async fn an_aborted_read_releases_its_descriptor() {
    let (_dir, store) = store();
    let body: Vec<u8> = (0..DEFAULT_READ_CHUNK_SIZE * 6)
        .map(|i| (i % 253) as u8)
        .collect();
    let digest = put(&store, "aborted", DigestAlgorithm::Sha256, &body).await;
    let path = blob_file(&store, &digest);
    assert_eq!(fds_open_on(&path), 0, "nothing should hold the blob yet");

    let blob = store.open_blob(&digest).await.expect("open");
    let mut stream = blob.stream();
    let first = stream.next().await.expect("chunk").expect("ok");
    assert_eq!(first.len(), DEFAULT_READ_CHUNK_SIZE);
    assert!(fds_open_on(&path) > 0, "the blob's fd should be open here");

    // The client goes away 1 MiB into a 6 MiB response.
    drop(stream);

    // The prefetched read cannot be cancelled, so the descriptor lives until it
    // returns - bounded by exactly one chunk, which is why the stream prefetches
    // one and not a window.
    let mut released = false;
    for _ in 0..200 {
        if fds_open_on(&path) == 0 {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(released, "descriptor still open after aborting the stream");
}

/// "Abort, do not apologise": a body that lost bytes mid-stream must fail, not
/// carry on past the gap. Appending anything after a short read converts a
/// retryable failure into a digest mismatch on the client.
#[tokio::test]
async fn a_blob_that_shrinks_mid_stream_aborts_instead_of_holing_the_body() {
    let dir = TempDir::new().expect("tempdir");
    let store = BlobStore::open(dir.path())
        .expect("open")
        .with_read_chunk_size(MIN_READ_CHUNK_SIZE);
    let body: Vec<u8> = (0..MIN_READ_CHUNK_SIZE * 3)
        .map(|i| (i % 233) as u8)
        .collect();
    let digest = put(&store, "shrinking", DigestAlgorithm::Sha256, &body).await;

    let blob = store.open_blob(&digest).await.expect("open");
    // Something outside this crate truncates the file after the size was read.
    std::fs::OpenOptions::new()
        .write(true)
        .open(blob_file(&store, &digest))
        .expect("reopen")
        .set_len(100)
        .expect("truncate");

    let mut stream = blob.stream();
    let err = stream
        .next()
        .await
        .expect("a frame")
        .expect_err("must abort");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    assert!(
        stream.next().await.is_none(),
        "the stream must end at the failure, not resume past it"
    );
}

#[tokio::test]
async fn dropping_a_stream_mid_blob_leaves_the_blob_readable() {
    let (_dir, store) = store();
    let body: Vec<u8> = (0..DEFAULT_READ_CHUNK_SIZE * 3)
        .map(|i| (i % 239) as u8)
        .collect();
    let digest = put(&store, "resumable-read", DigestAlgorithm::Sha256, &body).await;

    let mut stream = store.open_blob(&digest).await.expect("open").stream();
    let _ = stream.next().await.expect("chunk").expect("ok");
    drop(stream);

    assert_eq!(read_all(&store, &digest).await, body);
}
