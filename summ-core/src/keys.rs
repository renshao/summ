//! Binary key encoding.
//!
//! Every key begins with a single-byte prefix; uppercase prefixes hold registry
//! data, lowercase hold the repo-name interner. Repo names are interned to a
//! `u32` so a long name is not repeated in every key, and digests are stored raw
//! rather than as hex, halving their size.
//!
//! redb orders `&[u8]` keys lexicographically, so a prefix scan yields entries
//! already sorted the way the Distribution Spec's pagination requires.
//!
//! Fan-in relationships (which manifests reference a blob, which tags point at a
//! manifest) are stored as one key per edge rather than as a vector inside a
//! single value. At registry scale a popular base layer is referenced by
//! millions of manifests; an inline vector would mean rewriting a multi-megabyte
//! value on every push that touched that layer. One key per edge makes adding a
//! reference an O(1) insert, makes "is this still referenced?" a single seek,
//! and removes read-modify-write from the write path entirely.

use crate::digest::Digest;
use crate::types::RepoId;

pub const PREFIX_MANIFEST: u8 = b'M';
pub const PREFIX_MANIFEST_BODY: u8 = b'B';
pub const PREFIX_TAG: u8 = b'T';
pub const PREFIX_MANIFEST_TAG: u8 = b'G';
pub const PREFIX_BLOB: u8 = b'L';
pub const PREFIX_BLOB_REF: u8 = b'R';
pub const PREFIX_REPO_BLOB: u8 = b'P';
pub const PREFIX_CHILD_PARENT: u8 = b'S';
pub const PREFIX_REFERRER: u8 = b'F';
pub const PREFIX_UPLOAD: u8 = b'U';
pub const PREFIX_REPO_BY_NAME: u8 = b'n';
pub const PREFIX_REPO_BY_ID: u8 = b'i';

const REPO_LEN: usize = 4;

fn start(prefix: u8, cap: usize) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + cap);
    k.push(prefix);
    k
}

fn start_repo(prefix: u8, repo: RepoId, cap: usize) -> Vec<u8> {
    let mut k = start(prefix, REPO_LEN + cap);
    k.extend_from_slice(&repo.to_be_bytes());
    k
}

// --- manifests ---------------------------------------------------------

/// `M <repo> <digest>` -> `ManifestRecord`
pub fn manifest(repo: RepoId, digest: &Digest) -> Vec<u8> {
    let mut k = start_repo(PREFIX_MANIFEST, repo, digest.encoded_len());
    digest.encode_into(&mut k);
    k
}

/// `B <repo> <digest>` -> zstd-compressed manifest JSON
pub fn manifest_body(repo: RepoId, digest: &Digest) -> Vec<u8> {
    let mut k = start_repo(PREFIX_MANIFEST_BODY, repo, digest.encoded_len());
    digest.encode_into(&mut k);
    k
}

/// Scan prefix for every manifest in a repo, ordered by digest.
pub fn manifests_in_repo(repo: RepoId) -> Vec<u8> {
    start_repo(PREFIX_MANIFEST, repo, 0)
}

// --- tags --------------------------------------------------------------

/// `T <repo> <tag>` -> digest. Ordered by tag name, which is the order
/// `GET /v2/<name>/tags/list` must return.
pub fn tag(repo: RepoId, tag: &str) -> Vec<u8> {
    let mut k = start_repo(PREFIX_TAG, repo, tag.len());
    k.extend_from_slice(tag.as_bytes());
    k
}

pub fn tags_in_repo(repo: RepoId) -> Vec<u8> {
    start_repo(PREFIX_TAG, repo, 0)
}

/// `G <repo> <digest> <tag>` -> (). Reverse of `tag`: which tags point here.
pub fn manifest_tag(repo: RepoId, digest: &Digest, tag: &str) -> Vec<u8> {
    let mut k = start_repo(PREFIX_MANIFEST_TAG, repo, digest.encoded_len() + tag.len());
    digest.encode_into(&mut k);
    k.extend_from_slice(tag.as_bytes());
    k
}

/// Scan prefix for the tags pointing at one manifest. An empty scan means the
/// manifest is untagged and therefore purgeable.
pub fn tags_of_manifest(repo: RepoId, digest: &Digest) -> Vec<u8> {
    let mut k = start_repo(PREFIX_MANIFEST_TAG, repo, digest.encoded_len());
    digest.encode_into(&mut k);
    k
}

/// Extract the tag suffix from a `G` key.
pub fn parse_manifest_tag_suffix<'a>(key: &'a [u8], digest: &Digest) -> Option<&'a str> {
    let offset = 1 + REPO_LEN + digest.encoded_len();
    std::str::from_utf8(key.get(offset..)?).ok()
}

/// Extract the tag suffix from a `T` key.
pub fn parse_tag_suffix(key: &[u8]) -> Option<&str> {
    if key.first() != Some(&PREFIX_TAG) {
        return None;
    }
    std::str::from_utf8(key.get(1 + REPO_LEN..)?).ok()
}

// --- blobs -------------------------------------------------------------

/// `L <digest>` -> `BlobRecord`. Global, not repo-scoped: blob content is
/// deduplicated across the whole registry.
pub fn blob(digest: &Digest) -> Vec<u8> {
    let mut k = start(PREFIX_BLOB, digest.encoded_len());
    digest.encode_into(&mut k);
    k
}

/// `R <digest> <repo> <manifest>` -> (). One key per reference edge.
pub fn blob_ref(digest: &Digest, repo: RepoId, manifest: &Digest) -> Vec<u8> {
    let mut k = start(
        PREFIX_BLOB_REF,
        digest.encoded_len() + REPO_LEN + manifest.encoded_len(),
    );
    digest.encode_into(&mut k);
    k.extend_from_slice(&repo.to_be_bytes());
    manifest.encode_into(&mut k);
    k
}

/// Scan prefix over every manifest referencing a blob. Purge asks only whether
/// this prefix is empty, which is a single seek rather than a scan.
pub fn blob_refs(digest: &Digest) -> Vec<u8> {
    let mut k = start(PREFIX_BLOB_REF, digest.encoded_len());
    digest.encode_into(&mut k);
    k
}

/// Scan prefix over one repo's references to a blob. A non-empty result is what
/// authorises serving that blob under that repo name.
pub fn blob_refs_in_repo(digest: &Digest, repo: RepoId) -> Vec<u8> {
    let mut k = blob_refs(digest);
    k.extend_from_slice(&repo.to_be_bytes());
    k
}

/// `P <repo> <digest>` -> (). A repo's blob set, including blobs uploaded but
/// not yet referenced by a manifest. Drives per-repo size stats and cross-repo
/// mount checks.
pub fn repo_blob(repo: RepoId, digest: &Digest) -> Vec<u8> {
    let mut k = start_repo(PREFIX_REPO_BLOB, repo, digest.encoded_len());
    digest.encode_into(&mut k);
    k
}

pub fn blobs_in_repo(repo: RepoId) -> Vec<u8> {
    start_repo(PREFIX_REPO_BLOB, repo, 0)
}

// --- manifest graph ----------------------------------------------------

/// `S <repo> <child> <parent>` -> (). An index lists per-platform manifests as
/// children; a child may be shared by several indexes, so this is an edge set
/// rather than a single parent field.
pub fn child_parent(repo: RepoId, child: &Digest, parent: &Digest) -> Vec<u8> {
    let mut k = start_repo(
        PREFIX_CHILD_PARENT,
        repo,
        child.encoded_len() + parent.encoded_len(),
    );
    child.encode_into(&mut k);
    parent.encode_into(&mut k);
    k
}

pub fn parents_of(repo: RepoId, child: &Digest) -> Vec<u8> {
    let mut k = start_repo(PREFIX_CHILD_PARENT, repo, child.encoded_len());
    child.encode_into(&mut k);
    k
}

/// `F <repo> <subject> <referrer>` -> (). Backs the OCI 1.1 referrers API.
pub fn referrer(repo: RepoId, subject: &Digest, referrer: &Digest) -> Vec<u8> {
    let mut k = start_repo(
        PREFIX_REFERRER,
        repo,
        subject.encoded_len() + referrer.encoded_len(),
    );
    subject.encode_into(&mut k);
    referrer.encode_into(&mut k);
    k
}

pub fn referrers_of(repo: RepoId, subject: &Digest) -> Vec<u8> {
    let mut k = start_repo(PREFIX_REFERRER, repo, subject.encoded_len());
    subject.encode_into(&mut k);
    k
}

// --- uploads -----------------------------------------------------------

/// `U <uuid>` -> `UploadSession`.
pub fn upload(id: &[u8; 16]) -> Vec<u8> {
    let mut k = start(PREFIX_UPLOAD, 16);
    k.extend_from_slice(id);
    k
}

pub fn uploads() -> Vec<u8> {
    vec![PREFIX_UPLOAD]
}

// --- repo interner -----------------------------------------------------

/// `n <name>` -> id. Ordered by name, so `GET /v2/_catalog` pages by scanning
/// this range with a cursor. The reverse map is id-ordered and must not be used
/// for the catalog.
pub fn repo_by_name(name: &str) -> Vec<u8> {
    let mut k = start(PREFIX_REPO_BY_NAME, name.len());
    k.extend_from_slice(name.as_bytes());
    k
}

pub fn repos_by_name() -> Vec<u8> {
    vec![PREFIX_REPO_BY_NAME]
}

/// `i <id>` -> name.
pub fn repo_by_id(id: RepoId) -> Vec<u8> {
    let mut k = start(PREFIX_REPO_BY_ID, REPO_LEN);
    k.extend_from_slice(&id.to_be_bytes());
    k
}

pub fn parse_repo_name(key: &[u8]) -> Option<&str> {
    if key.first() != Some(&PREFIX_REPO_BY_NAME) {
        return None;
    }
    std::str::from_utf8(key.get(1..)?).ok()
}

pub fn parse_repo_id(value: &[u8]) -> Option<RepoId> {
    Some(RepoId::from_be_bytes(value.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(b: u8) -> Digest {
        Digest::Sha256([b; 32])
    }

    #[test]
    fn every_prefix_is_distinct() {
        let all = [
            PREFIX_MANIFEST,
            PREFIX_MANIFEST_BODY,
            PREFIX_TAG,
            PREFIX_MANIFEST_TAG,
            PREFIX_BLOB,
            PREFIX_BLOB_REF,
            PREFIX_REPO_BLOB,
            PREFIX_CHILD_PARENT,
            PREFIX_REFERRER,
            PREFIX_UPLOAD,
            PREFIX_REPO_BY_NAME,
            PREFIX_REPO_BY_ID,
        ];
        let mut seen = all.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), all.len(), "prefix collision");
    }

    #[test]
    fn scan_prefixes_actually_prefix_their_keys() {
        assert!(manifest(7, &d(1)).starts_with(&manifests_in_repo(7)));
        assert!(tag(7, "latest").starts_with(&tags_in_repo(7)));
        assert!(manifest_tag(7, &d(1), "latest").starts_with(&tags_of_manifest(7, &d(1))));
        assert!(blob_ref(&d(1), 7, &d(2)).starts_with(&blob_refs(&d(1))));
        assert!(blob_ref(&d(1), 7, &d(2)).starts_with(&blob_refs_in_repo(&d(1), 7)));
        assert!(repo_blob(7, &d(1)).starts_with(&blobs_in_repo(7)));
        assert!(child_parent(7, &d(1), &d(2)).starts_with(&parents_of(7, &d(1))));
        assert!(referrer(7, &d(1), &d(2)).starts_with(&referrers_of(7, &d(1))));
        assert!(repo_by_name("alpine").starts_with(&repos_by_name()));
    }

    #[test]
    fn a_repos_scan_cannot_reach_its_neighbour() {
        assert!(!manifest(8, &d(1)).starts_with(&manifests_in_repo(7)));
        assert!(!tag(8, "latest").starts_with(&tags_in_repo(7)));
        assert!(!blob_ref(&d(1), 8, &d(2)).starts_with(&blob_refs_in_repo(&d(1), 7)));
    }

    #[test]
    fn tags_sort_by_name_within_a_repo() {
        let mut keys = [tag(1, "v2"), tag(1, "latest"), tag(1, "alpha")];
        keys.sort();
        let names: Vec<_> = keys.iter().map(|k| parse_tag_suffix(k).unwrap()).collect();
        assert_eq!(names, ["alpha", "latest", "v2"]);
    }

    #[test]
    fn repos_sort_by_name_for_catalog_paging() {
        let mut keys = [
            repo_by_name("zeta"),
            repo_by_name("alpine"),
            repo_by_name("nginx"),
        ];
        keys.sort();
        let names: Vec<_> = keys.iter().map(|k| parse_repo_name(k).unwrap()).collect();
        assert_eq!(names, ["alpine", "nginx", "zeta"]);
    }

    #[test]
    fn tag_suffix_survives_a_sha512_digest() {
        let big = Digest::Sha512([3u8; 64]);
        let k = manifest_tag(7, &big, "release");
        assert_eq!(parse_manifest_tag_suffix(&k, &big), Some("release"));
    }

    #[test]
    fn repo_id_roundtrips() {
        let k = repo_by_id(9_000_000);
        assert_eq!(k.len(), 5);
        assert_eq!(parse_repo_id(&k[1..]), Some(9_000_000));
    }
}
