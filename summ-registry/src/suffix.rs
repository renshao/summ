//! Reading the variable-length tail off an encoded key.
//!
//! `summ_core::keys` builds keys and can take the tag off a `T` or `G` key, but
//! it has no decoder for the digest-bearing suffixes. Paging needs them: an
//! engine cursor is a whole key, and every list here hands back a token the
//! caller can put in a URL - a tag name, a digest - rather than an opaque blob.
//! These mirror the encoders in `keys.rs` and would sit better next to them.

use summ_core::{Digest, RepoId};

/// `<prefix> <repo:4>`.
const REPO_SCOPED_HEAD: usize = 1 + 4;

/// The digest immediately after a key's repo component: `M`, `P`, `G`, and the
/// *subject* of an `F` or the *child* of an `S`.
pub fn digest_after_repo(key: &[u8]) -> Option<Digest> {
    Digest::decode(key.get(REPO_SCOPED_HEAD..)?).map(|(d, _)| d)
}

/// The second digest of a two-digest repo-scoped key: the referrer of an `F`
/// edge, the parent of an `S` edge.
pub fn second_digest_after_repo(key: &[u8]) -> Option<Digest> {
    let (first, used) = Digest::decode(key.get(REPO_SCOPED_HEAD..)?)?;
    let _ = first;
    Digest::decode(key.get(REPO_SCOPED_HEAD + used..)?).map(|(d, _)| d)
}

/// `R <blob> <repo:4> <manifest>` - the referencing manifest and the repo it
/// lives in. The blob digest is the caller's scan prefix, so it is not returned.
pub fn blob_ref_target(key: &[u8]) -> Option<(RepoId, Digest)> {
    let (_blob, used) = Digest::decode(key.get(1..)?)?;
    let repo_at = 1 + used;
    let repo_bytes: [u8; 4] = key.get(repo_at..repo_at + 4)?.try_into().ok()?;
    let (manifest, _) = Digest::decode(key.get(repo_at + 4..)?)?;
    Some((RepoId::from_be_bytes(repo_bytes), manifest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use summ_core::keys;

    fn d(b: u8) -> Digest {
        Digest::Sha256([b; 32])
    }

    #[test]
    fn suffixes_match_what_the_encoders_wrote() {
        assert_eq!(digest_after_repo(&keys::manifest(7, &d(1))), Some(d(1)));
        assert_eq!(digest_after_repo(&keys::repo_blob(7, &d(2))), Some(d(2)));
        assert_eq!(
            digest_after_repo(&keys::manifest_tag(7, &d(3), "latest")),
            Some(d(3))
        );
        assert_eq!(
            second_digest_after_repo(&keys::referrer(7, &d(4), &d(5))),
            Some(d(5))
        );
        assert_eq!(
            second_digest_after_repo(&keys::child_parent(7, &d(6), &d(7))),
            Some(d(7))
        );
        assert_eq!(
            blob_ref_target(&keys::blob_ref(&d(8), 9, &d(10))),
            Some((9, d(10)))
        );
    }

    #[test]
    fn a_sha512_component_does_not_shift_the_tail() {
        let big = Digest::Sha512([1; 64]);
        assert_eq!(
            second_digest_after_repo(&keys::referrer(7, &big, &d(2))),
            Some(d(2))
        );
        assert_eq!(
            blob_ref_target(&keys::blob_ref(&big, 3, &d(4))),
            Some((3, d(4)))
        );
    }

    #[test]
    fn a_truncated_key_decodes_to_nothing_rather_than_panicking() {
        assert_eq!(digest_after_repo(b"M"), None);
        assert_eq!(second_digest_after_repo(&keys::manifest(7, &d(1))), None);
        assert_eq!(blob_ref_target(b"R"), None);
    }
}
