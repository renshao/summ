//! Binary key encoding.
//!
//! Every key begins with a single-byte prefix; uppercase prefixes hold registry
//! data, lowercase hold internal bookkeeping - the repo-name interner and the
//! schema version. Repo names are interned to a
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
use crate::time::Timestamp;
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
pub const PREFIX_TAG_HISTORY: u8 = b'H';
pub const PREFIX_MANIFEST_TAG_HISTORY: u8 = b'J';
pub const PREFIX_COUNTER: u8 = b'A';
pub const PREFIX_DEAD_REPO: u8 = b'D';
pub const PREFIX_REPO_BY_NAME: u8 = b'n';
pub const PREFIX_REPO_BY_ID: u8 = b'i';
pub const PREFIX_DB_VERSION: u8 = b'v';

/// Counter scopes, the byte after `A`. Every granularity that will be queried
/// has to be maintained on write: rolling repo totals up out of per-manifest
/// buckets would be a scan across up to 10M manifests.
pub const SCOPE_MANIFEST: u8 = b'm';
pub const SCOPE_TAG: u8 = b't';
pub const SCOPE_REPO: u8 = b'r';

const REPO_LEN: usize = 4;

/// Separates a variable-length tag from what follows it in a key.
///
/// Tag names match `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`, so NUL cannot occur in
/// one, and it sorts below every legal tag byte. Without it a scan of tag `foo`
/// would also sweep up `foobar`'s history. The `T` key needs no separator only
/// because the tag is terminal there.
const TAG_END: u8 = 0x00;

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

/// Scan prefix over every stored manifest body in a repo. Only a whole-repo
/// drop wants this - a body is otherwise read by digest, never swept.
pub fn manifest_bodies_in_repo(repo: RepoId) -> Vec<u8> {
    start_repo(PREFIX_MANIFEST_BODY, repo, 0)
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

/// Scan prefix over every manifest-to-tag edge in a repo. Unlike
/// [`tags_of_manifest`] this is not a query anything answers from: it exists so
/// a repo drop can take the range in one op.
pub fn manifest_tags_in_repo(repo: RepoId) -> Vec<u8> {
    start_repo(PREFIX_MANIFEST_TAG, repo, 0)
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

/// Scan prefix over every child-to-parent edge in a repo, for a repo drop.
pub fn children_in_repo(repo: RepoId) -> Vec<u8> {
    start_repo(PREFIX_CHILD_PARENT, repo, 0)
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

/// Scan prefix over every referrer edge in a repo, for a repo drop.
pub fn referrers_in_repo(repo: RepoId) -> Vec<u8> {
    start_repo(PREFIX_REFERRER, repo, 0)
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

// --- tag history -------------------------------------------------------

/// Timestamps in history keys are stored complemented so that newest sorts
/// first: [`crate::keys`] scans forward only, and adding a reverse iterator to
/// both engines to serve one endpoint is not worth it.
///
/// The consequence for callers is that a `before`/`since` cursor is just a
/// `start_after` seek to the complement of the boundary instant, so there is no
/// pagination token to invent.
fn push_desc_time(k: &mut Vec<u8>, at: Timestamp) {
    k.extend_from_slice(&(!at.millis()).to_be_bytes());
}

/// Read a complemented timestamp back out of a history key.
fn read_desc_time(bytes: &[u8]) -> Option<Timestamp> {
    let raw: [u8; 8] = bytes.get(..8)?.try_into().ok()?;
    Some(Timestamp::from_millis(!u64::from_be_bytes(raw)))
}

/// `H <repo> <tag> 0 <!ts> <digest>` -> `TagEvent`, newest first.
///
/// The digest is in the key rather than only in the value because it is the
/// collision breaker: two events on one tag at the same instant with the same
/// digest are the same event.
pub fn tag_history(repo: RepoId, tag: &str, at: Timestamp, digest: &Digest) -> Vec<u8> {
    let mut k = start_repo(
        PREFIX_TAG_HISTORY,
        repo,
        tag.len() + 1 + 8 + digest.encoded_len(),
    );
    k.extend_from_slice(tag.as_bytes());
    k.push(TAG_END);
    push_desc_time(&mut k, at);
    digest.encode_into(&mut k);
    k
}

/// Scan prefix over one tag's history.
pub fn tag_history_of(repo: RepoId, tag: &str) -> Vec<u8> {
    let mut k = start_repo(PREFIX_TAG_HISTORY, repo, tag.len() + 1);
    k.extend_from_slice(tag.as_bytes());
    k.push(TAG_END);
    k
}

/// Scan prefix over every tag's history in a repo, for a repo drop. Not a
/// query: a repo-wide fold across every tag's events is exactly the unbounded
/// read this API does not offer.
pub fn tag_history_in_repo(repo: RepoId) -> Vec<u8> {
    start_repo(PREFIX_TAG_HISTORY, repo, 0)
}

/// Seek key for "events strictly before this instant", given the descending
/// order. Pass as `start_after`.
///
/// Seeking to `!at` would be *inclusive*: an event at exactly `at` encodes to
/// `<prefix> !at <digest>`, which is longer than the cursor and therefore sorts
/// after it, so `start_after` would return it. Backing the cursor up by one
/// millisecond excludes the whole instant and nothing else.
pub fn tag_history_before(repo: RepoId, tag: &str, at: Timestamp) -> Vec<u8> {
    let mut k = tag_history_of(repo, tag);
    push_desc_time(
        &mut k,
        Timestamp::from_millis(at.millis().saturating_sub(1)),
    );
    k
}

/// Split `H <repo> <tag> 0 <!ts> <digest>` after the tag separator.
///
/// The timestamp lives only in the key - `TagEvent` deliberately does not
/// repeat it - so a reader has to decode it to render a row.
pub fn tag_history_suffix(key: &[u8], scan_prefix: &[u8]) -> Option<(Timestamp, Digest)> {
    let rest = key.strip_prefix(scan_prefix)?;
    let at = read_desc_time(rest)?;
    let (digest, _) = Digest::decode(rest.get(8..)?)?;
    Some((at, digest))
}

/// Split `J <repo> <digest> <!ts> <tag>` after the digest.
pub fn manifest_tag_history_suffix(key: &[u8], scan_prefix: &[u8]) -> Option<(Timestamp, String)> {
    let rest = key.strip_prefix(scan_prefix)?;
    let at = read_desc_time(rest)?;
    Some((at, String::from_utf8(rest.get(8..)?.to_vec()).ok()?))
}

/// `J <repo> <digest> <!ts> <tag>` -> `TagEvent`. The digest-addressed form of
/// the same history: what was this manifest ever tagged, and when.
pub fn manifest_tag_history(repo: RepoId, digest: &Digest, at: Timestamp, tag: &str) -> Vec<u8> {
    let mut k = start_repo(
        PREFIX_MANIFEST_TAG_HISTORY,
        repo,
        digest.encoded_len() + 8 + tag.len(),
    );
    digest.encode_into(&mut k);
    push_desc_time(&mut k, at);
    k.extend_from_slice(tag.as_bytes());
    k
}

pub fn manifest_tag_history_of(repo: RepoId, digest: &Digest) -> Vec<u8> {
    let mut k = start_repo(PREFIX_MANIFEST_TAG_HISTORY, repo, digest.encoded_len());
    digest.encode_into(&mut k);
    k
}

/// The `J` counterpart of [`tag_history_in_repo`].
pub fn manifest_tag_history_in_repo(repo: RepoId) -> Vec<u8> {
    start_repo(PREFIX_MANIFEST_TAG_HISTORY, repo, 0)
}

/// The `J` counterpart of [`tag_history_before`], with the same
/// strictly-before semantics.
pub fn manifest_tag_history_before(repo: RepoId, digest: &Digest, at: Timestamp) -> Vec<u8> {
    let mut k = manifest_tag_history_of(repo, digest);
    push_desc_time(
        &mut k,
        Timestamp::from_millis(at.millis().saturating_sub(1)),
    );
    k
}

// --- pull counters -----------------------------------------------------

/// Days since the Unix epoch, UTC. The bucket boundary is fixed at write time;
/// a UI may relabel to a local zone but must not re-bucket, or the same wall
/// changes shape depending on who is looking at it. Good to 2149.
pub fn day_bucket(unix_seconds: u64) -> u16 {
    (unix_seconds / 86_400) as u16
}

/// The hour within [`day_bucket`]'s day, UTC, `0..24`.
///
/// Indexes the arrays in [`CounterBucket`](crate::types::CounterBucket). The
/// pair `(day, hour)` is the whole time coordinate of a counter increment;
/// nothing finer is stored, and nothing coarser is stored either, because a day
/// total is the sum of the hours.
pub fn hour_of_day(unix_seconds: u64) -> usize {
    ((unix_seconds % 86_400) / 3_600) as usize
}

/// The weekday of a day bucket, `0` = Sunday.
///
/// Pure arithmetic on the key: 1970-01-01 was a Thursday, so day 0 is weekday
/// 4. A contribution grid's row index and a "which day of the week do people
/// pull on" fold both come out of this with nothing stored for them.
pub fn weekday(day: u16) -> u8 {
    ((day as u32 + 4) % 7) as u8
}

/// `<shard>` is the writing node's id, `0` on a single node. Two nodes each
/// writing an absolute value for one bucket would be last-write-wins, which is
/// silent undercounting; reserving the component now is free, adding a key
/// component to a populated store is a migration.
fn push_bucket(k: &mut Vec<u8>, day: u16, shard: u16) {
    k.extend_from_slice(&day.to_be_bytes());
    k.extend_from_slice(&shard.to_be_bytes());
}

fn start_counter(scope: u8, repo: RepoId, cap: usize) -> Vec<u8> {
    let mut k = start(PREFIX_COUNTER, 1 + REPO_LEN + cap);
    k.push(scope);
    k.extend_from_slice(&repo.to_be_bytes());
    k
}

/// `A m <repo> <digest> <day> <shard>` -> `CounterBucket`. The per-manifest
/// wall: 53 weeks is 371 buckets, so the whole visualisation is one bounded
/// scan arriving in chronological order.
pub fn counter_manifest(repo: RepoId, digest: &Digest, day: u16, shard: u16) -> Vec<u8> {
    let mut k = start_counter(SCOPE_MANIFEST, repo, digest.encoded_len() + 4);
    digest.encode_into(&mut k);
    push_bucket(&mut k, day, shard);
    k
}

pub fn counters_of_manifest(repo: RepoId, digest: &Digest) -> Vec<u8> {
    let mut k = start_counter(SCOPE_MANIFEST, repo, digest.encoded_len());
    digest.encode_into(&mut k);
    k
}

/// `A t <repo> <tag> 0 <day> <shard>` -> `CounterBucket`.
pub fn counter_tag(repo: RepoId, tag: &str, day: u16, shard: u16) -> Vec<u8> {
    let mut k = start_counter(SCOPE_TAG, repo, tag.len() + 5);
    k.extend_from_slice(tag.as_bytes());
    k.push(TAG_END);
    push_bucket(&mut k, day, shard);
    k
}

pub fn counters_of_tag(repo: RepoId, tag: &str) -> Vec<u8> {
    let mut k = start_counter(SCOPE_TAG, repo, tag.len() + 1);
    k.extend_from_slice(tag.as_bytes());
    k.push(TAG_END);
    k
}

/// `A r <repo> <day> <shard>` -> `CounterBucket`. Repo totals, kept
/// indefinitely because they are tiny.
pub fn counter_repo(repo: RepoId, day: u16, shard: u16) -> Vec<u8> {
    let mut k = start_counter(SCOPE_REPO, repo, 4);
    push_bucket(&mut k, day, shard);
    k
}

pub fn counters_of_repo(repo: RepoId) -> Vec<u8> {
    start_counter(SCOPE_REPO, repo, 0)
}

/// Everything under one repo, for the prefix deletes purge does when a repo is
/// dropped. Not a scan target: the three scopes have different key shapes.
pub fn counters_in_repo_scope(scope: u8, repo: RepoId) -> Vec<u8> {
    start_counter(scope, repo, 0)
}

/// Where a day-windowed scan resumes so that `day` itself is included.
///
/// `scan`'s `start_after` is exclusive, and a full counter key carries a shard
/// after the day. `<prefix> <day>` is a proper prefix of every key in that day
/// and therefore sorts strictly before all of them, so resuming after it yields
/// the day rather than skipping it. Passing `<prefix> <day> <shard 0>` would
/// silently drop shard 0, which on a single node is every key there is.
pub fn counters_from_day(scan_prefix: &[u8], day: u16) -> Vec<u8> {
    let mut k = Vec::with_capacity(scan_prefix.len() + 2);
    k.extend_from_slice(scan_prefix);
    k.extend_from_slice(&day.to_be_bytes());
    k
}

/// `(day, shard)` off the end of a counter key.
pub fn counter_suffix(key: &[u8], scan_prefix: &[u8]) -> Option<(u16, u16)> {
    let rest = key.strip_prefix(scan_prefix)?;
    if rest.len() != 4 {
        return None;
    }
    Some((
        u16::from_be_bytes([rest[0], rest[1]]),
        u16::from_be_bytes([rest[2], rest[3]]),
    ))
}

// --- dead repos --------------------------------------------------------

/// `D <id>` -> `DeadRepo`. The sweeper's worklist.
///
/// A repository is deleted in two steps because the work splits in two. The
/// name mapping (`n`/`i`) goes in one O(1) batch, which is what a client waits
/// for; everything keyed by the id is swept afterwards. This record is what
/// joins them, and it is a record rather than a rediscoverable fact because the
/// alternative - looking for ids that have no `i` entry - is a scan of the
/// whole store.
///
/// Deliberately keyed by id and not by name: the name is free the instant the
/// tombstone lands, and a second delete of a recreated repository must not
/// overwrite the first one's outstanding work.
pub fn dead_repo(id: RepoId) -> Vec<u8> {
    let mut k = start(PREFIX_DEAD_REPO, REPO_LEN);
    k.extend_from_slice(&id.to_be_bytes());
    k
}

pub fn dead_repos() -> Vec<u8> {
    vec![PREFIX_DEAD_REPO]
}

pub fn parse_dead_repo_id(key: &[u8]) -> Option<RepoId> {
    if key.first() != Some(&PREFIX_DEAD_REPO) {
        return None;
    }
    Some(RepoId::from_be_bytes(key.get(1..)?.try_into().ok()?))
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

// --- store metadata ----------------------------------------------------

/// `v` -> `SCHEMA_VERSION` (BE u32). Single key, written when the store is
/// created.
///
/// A version marker is cheap now and unpleasant to retrofit onto a populated
/// store, which is the whole reason it lands before there is anything to
/// migrate. postcard is not self-describing, so without it a record written
/// before a field was added simply fails to decode with no way to tell that
/// from corruption.
pub fn db_version() -> Vec<u8> {
    vec![PREFIX_DB_VERSION]
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
            PREFIX_TAG_HISTORY,
            PREFIX_MANIFEST_TAG_HISTORY,
            PREFIX_COUNTER,
            PREFIX_REPO_BY_NAME,
            PREFIX_REPO_BY_ID,
            PREFIX_DB_VERSION,
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

    fn ms(millis: u64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    #[test]
    fn tag_history_is_newest_first_and_cannot_reach_a_prefix_neighbour() {
        // Later timestamp must sort earlier, since the endpoint returns
        // descending order and the engines only scan forward.
        let old = tag_history(1, "latest", ms(1_000), &d(1));
        let new = tag_history(1, "latest", ms(2_000), &d(1));
        assert!(new < old);

        // `foo` must not sweep up `foobar`: that is what the NUL is for.
        assert!(!tag_history(1, "foobar", ms(1_000), &d(1)).starts_with(&tag_history_of(1, "foo")));
        assert!(tag_history(1, "foo", ms(1_000), &d(1)).starts_with(&tag_history_of(1, "foo")));
    }

    /// Milliseconds are what stop a create and a delete of the same tag at the
    /// same digest, inside one second, from encoding to the same key - which
    /// would drop the earlier event entirely.
    #[test]
    fn two_events_a_millisecond_apart_are_two_keys() {
        let create = tag_history(1, "latest", ms(1_700_000_000_001), &d(1));
        let delete = tag_history(1, "latest", ms(1_700_000_000_002), &d(1));
        assert_ne!(create, delete);
        assert!(delete < create, "the later event sorts first");
    }

    #[test]
    fn a_history_before_cursor_excludes_its_own_instant() {
        // `before = 2_000` is strictly-before: the 2_000 event is excluded
        // along with the newer one, and the 1_999 event is the first row.
        let cursor = tag_history_before(1, "latest", ms(2_000));
        for newer in [3_000, 2_000] {
            assert!(
                tag_history(1, "latest", ms(newer), &d(1)) < cursor,
                "an event at {newer} must sort before a cursor of 2000"
            );
        }
        for older in [1_999, 1_000] {
            assert!(
                tag_history(1, "latest", ms(older), &d(1)) > cursor,
                "an event at {older} must sort after a cursor of 2000"
            );
        }
    }

    #[test]
    fn history_suffixes_decode_what_the_encoders_wrote() {
        let scan = tag_history_of(1, "latest");
        let key = tag_history(1, "latest", ms(1_700_000_000_123), &d(1));
        assert_eq!(
            tag_history_suffix(&key, &scan),
            Some((ms(1_700_000_000_123), d(1)))
        );

        let scan = manifest_tag_history_of(1, &d(1));
        let key = manifest_tag_history(1, &d(1), ms(1_700_000_000_123), "latest");
        assert_eq!(
            manifest_tag_history_suffix(&key, &scan),
            Some((ms(1_700_000_000_123), "latest".to_string()))
        );

        // A sha512 digest shifts the tail; the scan prefix carries its own
        // length, so nothing has to know which algorithm it was.
        let big = Digest::Sha512([7; 64]);
        let scan = manifest_tag_history_of(1, &big);
        let key = manifest_tag_history(1, &big, ms(42), "v1");
        assert_eq!(
            manifest_tag_history_suffix(&key, &scan),
            Some((ms(42), "v1".to_string()))
        );
    }

    #[test]
    fn a_truncated_history_key_decodes_to_nothing_rather_than_panicking() {
        let scan = tag_history_of(1, "latest");
        assert_eq!(tag_history_suffix(&scan, &scan), None);
        assert_eq!(tag_history_suffix(b"H", &scan), None);
        let scan = manifest_tag_history_of(1, &d(1));
        assert_eq!(manifest_tag_history_suffix(&scan, &scan), None);
    }

    #[test]
    fn manifest_tag_history_scans_within_one_manifest() {
        assert!(manifest_tag_history(1, &d(1), ms(5), "v1")
            .starts_with(&manifest_tag_history_of(1, &d(1))));
        assert!(!manifest_tag_history(1, &d(2), ms(5), "v1")
            .starts_with(&manifest_tag_history_of(1, &d(1))));
    }

    #[test]
    fn counter_scopes_do_not_collide_and_scan_cleanly() {
        assert!(counter_manifest(1, &d(1), 20_000, 0).starts_with(&counters_of_manifest(1, &d(1))));
        assert!(counter_tag(1, "latest", 20_000, 0).starts_with(&counters_of_tag(1, "latest")));
        assert!(counter_repo(1, 20_000, 0).starts_with(&counters_of_repo(1)));

        // A manifest bucket must not be reachable from the repo-scope scan,
        // or repo totals would double-count every manifest.
        assert!(!counter_manifest(1, &d(1), 20_000, 0).starts_with(&counters_of_repo(1)));
        assert!(!counter_tag(1, "latest", 20_000, 0).starts_with(&counters_of_repo(1)));

        // Tag counters need the same separator guarantee as tag history.
        assert!(!counter_tag(1, "foobar", 20_000, 0).starts_with(&counters_of_tag(1, "foo")));
    }

    /// 1970-01-01 was a Thursday, and every weekday claim in the UI rests on
    /// that one fact.
    #[test]
    fn weekdays_are_arithmetic_on_the_day_bucket() {
        assert_eq!(weekday(0), 4); // Thursday
        assert_eq!(weekday(1), 5);
        assert_eq!(weekday(3), 0); // Sunday
        assert_eq!(weekday(7), 4);
        // 2024-01-01 was a Monday: 19_723 days after the epoch.
        assert_eq!(weekday(19_723), 1);
    }

    #[test]
    fn the_day_and_hour_of_an_instant_are_its_whole_coordinate() {
        let midnight = 19_723_u64 * 86_400;
        assert_eq!(day_bucket(midnight), 19_723);
        assert_eq!(hour_of_day(midnight), 0);
        assert_eq!(hour_of_day(midnight + 3_599), 0);
        assert_eq!(hour_of_day(midnight + 3_600), 1);
        assert_eq!(hour_of_day(midnight + 86_399), 23);
        assert_eq!(day_bucket(midnight + 86_400), 19_724);
        assert_eq!(hour_of_day(midnight + 86_400), 0);
    }

    #[test]
    fn a_day_window_starts_on_the_day_it_names() {
        let prefix = counters_of_manifest(1, &d(1));
        let from = counters_from_day(&prefix, 20_000);
        // Strictly before every key in that day, so an exclusive `start_after`
        // includes shard 0 rather than skipping it.
        assert!(from < counter_manifest(1, &d(1), 20_000, 0));
        assert!(from > counter_manifest(1, &d(1), 19_999, u16::MAX));
    }

    #[test]
    fn counter_suffixes_decode_what_the_encoders_wrote() {
        let prefix = counters_of_manifest(1, &d(1));
        let key = counter_manifest(1, &d(1), 20_000, 3);
        assert_eq!(counter_suffix(&key, &prefix), Some((20_000, 3)));

        let tag_prefix = counters_of_tag(1, "latest");
        let tag_key = counter_tag(1, "latest", 19_999, 0);
        assert_eq!(counter_suffix(&tag_key, &tag_prefix), Some((19_999, 0)));

        let repo_prefix = counters_of_repo(1);
        assert_eq!(
            counter_suffix(&counter_repo(1, 1, 1), &repo_prefix),
            Some((1, 1))
        );
        // A key from another group is not silently reinterpreted.
        assert_eq!(counter_suffix(&key, &repo_prefix), None);
    }

    #[test]
    fn counter_days_sort_chronologically_so_a_wall_is_one_scan() {
        let mut keys = [
            counter_manifest(1, &d(1), 20_100, 0),
            counter_manifest(1, &d(1), 19_900, 0),
            counter_manifest(1, &d(1), 20_000, 0),
        ];
        keys.sort();
        assert_eq!(keys[0], counter_manifest(1, &d(1), 19_900, 0));
        assert_eq!(keys[2], counter_manifest(1, &d(1), 20_100, 0));
    }

    #[test]
    fn day_bucket_is_utc_days_since_the_epoch() {
        assert_eq!(day_bucket(0), 0);
        assert_eq!(day_bucket(86_399), 0);
        assert_eq!(day_bucket(86_400), 1);
    }

    #[test]
    fn the_version_key_is_one_byte_and_shares_no_prefix_with_data() {
        assert_eq!(db_version(), vec![PREFIX_DB_VERSION]);
    }
}
