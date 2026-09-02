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

    for level in missing.iter().rev() {
        std::fs::create_dir(level)?;
        if let Some(parent) = level.parent() {
            fsync_dir(parent)?;
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

    #[test]
    fn sha512_lands_under_its_own_algorithm_directory() {
        let digest: Digest = ("sha512:".to_string() + &"0f".repeat(64))
            .parse()
            .expect("digest");
        let (dir, _) = blob_path(Path::new("/r"), &digest);
        assert_eq!(dir, Path::new("/r/blobs/sha512/0f/0f/0f"));
    }
}
