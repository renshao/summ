//! Path construction and directory durability.
//!
//! Every path here is a pure function of a digest or of an opaque upload id.
//! Nothing encodes a relationship, nothing is ever listed, and no answer is ever
//! derived from what happens to be on disk - that is the whole point of the
//! layout (see the crate docs).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use summ_core::Digest;

pub(crate) const BLOBS_DIR: &str = "blobs";
pub(crate) const UPLOADS_DIR: &str = "uploads";

/// Fan-out levels of two hex characters each, before the file itself.
const FANOUT_LEVELS: usize = 3;

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// `<root>/blobs/<algo>/ab/cd/ef/<full-hex>`.
///
/// Returned alongside its parent so a caller that is about to create the
/// directory does not have to ask for it a second time.
pub(crate) fn blob_path(root: &Path, digest: &Digest) -> (PathBuf, PathBuf) {
    let hex = hex_encode(digest.raw());
    let mut dir = root.join(BLOBS_DIR);
    dir.push(digest.algorithm());
    for level in 0..FANOUT_LEVELS {
        dir.push(&hex[level * 2..level * 2 + 2]);
    }
    let file = dir.join(&hex);
    (dir, file)
}

pub(crate) fn upload_path(root: &Path, id: &str) -> PathBuf {
    let mut p = root.join(UPLOADS_DIR);
    p.push(id);
    p
}

/// fsync a directory, which is what makes the names it contains durable.
///
/// Renaming a file into place is atomic, but on a crash the *rename* can still
/// be lost unless the containing directory is synced. That is the difference
/// between an orphan blob (harmless, purge reclaims it) and metadata pointing
/// at a blob that is not there (corruption).
pub(crate) fn fsync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Create `dir` and every ancestor up to `stop` (exclusive), fsyncing as it
/// goes.
///
/// fsyncing a directory makes the entries *inside* it durable, so making
/// `a/b/c` durable requires syncing `a` (for `b`'s entry) and `a/b` (for `c`'s).
/// `c` itself is synced later, after the blob is renamed into it. Skipped
/// entirely when the leaf already exists, which is the common case once a bucket
/// has been used - a bucket holds about six blobs at 10^8, so this runs roughly
/// once per six commits at full scale and never after that.
///
/// # Concurrency
///
/// **A level another writer created first is a success, not a failure.** Every
/// check here is a check-then-create, so it races by construction: two commits
/// into a fresh store both find `blobs/<algo>` missing and both try to create
/// it, and any two blobs at all share at least that much of their path. This is
/// not a rare interleaving - `oras push` uploads an artifact's blobs in
/// parallel, so it is what an ordinary push does. Taking `EEXIST` out to the
/// caller turned that into a `500` with the layer already uploaded.
///
/// So `AlreadyExists` is absorbed, and the fsync still runs on that path. The
/// winner is presumably about to sync the same parent, but "presumably" is not
/// a durability argument and the two calls have not synchronised on anything:
/// this function must not return until *its* caller may rename into the leaf
/// and rely on the entry surviving a crash. An extra fsync of a directory that
/// is already durable costs nothing worth measuring at once per six commits.
///
/// What is *not* absorbed is a non-directory holding the name. That cannot
/// happen from our own writes - a level is two hex characters and a blob file
/// is the full hex - so it means a corrupted or hand-edited store, and it must
/// surface here rather than as a baffling `ENOTDIR` from the rename.
///
/// Errors name the level that actually failed, which is not usually `dir`: the
/// caller knows which blob it was committing, and the thing it cannot work out
/// is which of the four levels above it went wrong.
pub(crate) fn create_dir_durable(dir: &Path, stop: &Path) -> io::Result<()> {
    if dir.try_exists()? {
        return Ok(());
    }

    // Deepest-first walk up to `stop`, then create and sync top-down.
    let mut missing: Vec<&Path> = Vec::new();
    let mut cursor = dir;
    loop {
        if cursor == stop || cursor.try_exists()? {
            break;
        }
        missing.push(cursor);
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => break,
        }
    }

    let at =
        |level: &Path, e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", level.display()));
    for level in missing.iter().rev() {
        match std::fs::create_dir(level) {
            Ok(()) => {}
            // Lost the race. Fine, so long as what won is a directory.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !level.is_dir() {
                    return Err(at(level, e));
                }
            }
            Err(e) => return Err(at(level, e)),
        }
        if let Some(parent) = level.parent() {
            fsync_dir(parent).map_err(|e| at(parent, e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_path_is_three_levels_of_two_hex_chars_and_the_file_is_the_blob() {
        let digest: Digest = ("sha256:".to_string() + &"ab".repeat(32))
            .parse()
            .expect("digest");
        let (dir, file) = blob_path(Path::new("/r"), &digest);
        assert_eq!(dir, Path::new("/r/blobs/sha256/ab/ab/ab"));
        assert_eq!(file, dir.join("ab".repeat(32)));
        // Not distribution's `.../<full-hex>/data`: the leaf *is* the blob.
        assert_ne!(file.file_name().and_then(|n| n.to_str()), Some("data"));
    }

    /// The bug this guards: two commits into a fresh store both walk up to the
    /// blobs root, both find `blobs/<algo>` missing, and both create it. One
    /// wins and the loser used to take `EEXIST` all the way out as a `500` on
    /// an ordinary concurrent push.
    ///
    /// Sixteen threads on a barrier, sharing the algorithm directory and
    /// pairwise sharing a whole leaf, so the collision is exercised at an
    /// ancestor *and* at the leaf itself.
    #[test]
    fn levels_created_concurrently_do_not_fight_over_a_shared_ancestor() {
        use std::sync::{Arc, Barrier};

        const THREADS: usize = 16;

        let root = tempfile::tempdir().expect("tempdir");
        let blobs = root.path().join(BLOBS_DIR);
        std::fs::create_dir(&blobs).expect("blobs root");

        let barrier = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let barrier = Arc::clone(&barrier);
                let blobs = blobs.clone();
                std::thread::spawn(move || {
                    let leaf = blobs.join(format!("sha256/{:02x}/00/ff", i / 2));
                    barrier.wait();
                    create_dir_durable(&leaf, &blobs).map(|()| leaf)
                })
            })
            .collect();

        for handle in handles {
            let leaf = handle
                .join()
                .expect("thread")
                .expect("a level another thread created first is not a failure");
            assert!(leaf.is_dir());
        }
    }

    #[test]
    fn a_file_where_a_level_should_be_is_still_an_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let blobs = root.path().join(BLOBS_DIR);
        std::fs::create_dir(&blobs).expect("blobs root");
        // Tolerating `AlreadyExists` must not tolerate a *file* squatting on a
        // level, which is a broken store rather than a lost race.
        std::fs::write(blobs.join("sha256"), b"not a directory").expect("write");
        assert!(create_dir_durable(&blobs.join("sha256/ab/cd/ef"), &blobs).is_err());
    }

    #[test]
    fn sha512_lands_under_its_own_algorithm_directory() {
        let digest: Digest = ("sha512:".to_string() + &"0f".repeat(64))
            .parse()
            .expect("digest");
        let (dir, _) = blob_path(Path::new("/r"), &digest);
        assert_eq!(dir, Path::new("/r/blobs/sha512/0f/0f/0f"));
    }
}
