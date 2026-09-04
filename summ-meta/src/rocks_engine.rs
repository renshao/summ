//! RocksDB-backed [`MetaEngine`]. The v1 production engine.
//!
//! RocksDB is an LSM, which suits this workload: a push inserts tens of keys
//! whose positions are effectively random (they are digest-prefixed), and an
//! LSM absorbs random inserts into sequential writes rather than splitting
//! pages across a multi-terabyte B-tree. Purge is bulk deletes, which become
//! cheap tombstones. Block compression matters more here than usual, because
//! most of the keyspace is valueless edge keys that share long digest prefixes.
//!
//! The library is compiled from source and statically linked, so the registry
//! ships as one binary with no RocksDB installation to manage.
//!
//! Note that [`WriteBatch`] is our own type, not `rocksdb::WriteBatch`; the
//! latter is aliased as `RocksBatch` below. The names coincide because both
//! describe the same idea, and in RocksDB a write batch is literally the WAL
//! record format - which is the shape we want for replication anyway.

use rocksdb::{
    BlockBasedOptions, Cache, DBCompressionType, DBRawIterator, DataBlockIndexType, Direction,
    IteratorMode, Options, ReadOptions, SliceTransform, WriteBatch as RocksBatch, DB,
};
use summ_core::keys::{
    PREFIX_BLOB_REF, PREFIX_CHILD_PARENT, PREFIX_COUNTER, PREFIX_MANIFEST, PREFIX_MANIFEST_BODY,
    PREFIX_MANIFEST_TAG, PREFIX_MANIFEST_TAG_HISTORY, PREFIX_REFERRER, PREFIX_REPO_BLOB,
    PREFIX_TAG, PREFIX_TAG_HISTORY, SCOPE_MANIFEST, SCOPE_REPO, SCOPE_TAG,
};
use summ_core::{encoded_len_of, Result, SummError};

use crate::engine::{KeyPage, MetaEngine, MetaOp, Page, WriteBatch};

fn storage<E: std::fmt::Display>(e: E) -> SummError {
    SummError::Storage(e.to_string())
}

/// Prefix-group length for `key`, or `None` if the key type has no group worth
/// filtering (or the key is too short to classify).
///
/// RocksDB demands *prefix consistency*: whether one key is in another's group
/// must be decidable from the prefix alone. That holds here because the bytes
/// deciding the length — the type byte at 0, and for digest-bearing types the
/// algorithm byte — are themselves inside the prefix.
///
/// This mirrors the key builders in `summ_core::keys` and must change with them.
#[inline]
fn summ_prefix_len(key: &[u8]) -> Option<usize> {
    match *key.first()? {
        // `R <digest> <repo> <manifest>` grouped by `R <digest>` (34 or 66).
        // A 38-byte `blob_refs_in_repo` seek extracts to this same group, which
        // is why callers must still re-check `starts_with`.
        PREFIX_BLOB_REF => Some(1 + encoded_len_of(*key.get(1)?)?),
        // `G|S|F <repo:4> <digest> ...` grouped through the first digest.
        PREFIX_MANIFEST_TAG | PREFIX_CHILD_PARENT | PREFIX_REFERRER => {
            Some(1 + 4 + encoded_len_of(*key.get(5)?)?)
        }
        // Counters. The scope byte at 1 is what decides the length, and it is
        // itself inside every one of these prefixes, so consistency holds
        // across scopes just as it does across digest algorithms.
        PREFIX_COUNTER => match *key.get(1)? {
            // `A m <repo:4> <digest> <day> <shard>` grouped through the digest
            // (39 or 71). This is the group the contribution wall seeks into,
            // and the only counter scope whose fixed-width components reach
            // past the repo id.
            SCOPE_MANIFEST => Some(2 + 4 + encoded_len_of(*key.get(6)?)?),
            // `A t <repo:4> <tag> 0 <day> <shard>` and `A r <repo:4> <day>
            // <shard>` both group at `A <scope> <repo:4>`: a tag is
            // variable-length, so the tag scope cannot group any longer, and
            // the repo scope has nothing further to group on.
            SCOPE_TAG | SCOPE_REPO => Some(6),
            _ => None,
        },
        // Repo-scoped scans: `M|B|T|P|H|J <repo:4>`.
        //
        // `J <repo:4> <digest> ...` could in principle group through its digest
        // the way `G`/`S`/`F` do, but it is kept alongside `H` at the repo id:
        // a coarser group is still a correct filter, and one rule for both
        // history ranges is one fewer thing to keep in step with the key
        // builders.
        PREFIX_MANIFEST
        | PREFIX_MANIFEST_BODY
        | PREFIX_TAG
        | PREFIX_REPO_BLOB
        | PREFIX_TAG_HISTORY
        | PREFIX_MANIFEST_TAG_HISTORY => Some(5),
        // `L`, `U`, `n`, `i`: a one-byte group is worthless to a bloom filter,
        // so they stay out of the domain and rely on the whole-key filter.
        _ => None,
    }
}

/// MUST return a subslice of `key`: the binding hands the pointer straight to C.
fn summ_transform(key: &[u8]) -> &[u8] {
    match summ_prefix_len(key) {
        Some(n) if key.len() >= n => &key[..n],
        _ => key,
    }
}

fn summ_in_domain(key: &[u8]) -> bool {
    matches!(summ_prefix_len(key), Some(n) if key.len() >= n)
}

/// Bump the version whenever the key layout changes. RocksDB records this name
/// in every SST's table properties and would otherwise trust filters built
/// under the old rules.
fn summ_prefix_extractor() -> SliceTransform {
    SliceTransform::create("summ.prefix.v2", summ_transform, Some(summ_in_domain))
}

/// Smallest key strictly greater than every key beginning with `prefix`.
///
/// `DeleteRange` takes a half-open `[start, end)`, so a prefix delete needs the
/// prefix's successor as the exclusive end. Trailing `0xff` bytes carry, and a
/// prefix of all `0xff` has no successor - it runs to the end of the keyspace,
/// reported here as `None`.
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last < u8::MAX {
            *last += 1;
            return Some(end);
        }
        end.pop();
    }
    None
}

/// Default block cache. RocksDB's own default is 32 MiB, which is far too small
/// for a metadata store this size.
pub const DEFAULT_BLOCK_CACHE_BYTES: usize = 512 * 1024 * 1024;

pub struct RocksEngine {
    db: DB,
    /// Refcounted and must outlive the DB.
    _cache: Cache,
}

/// Read options for a prefix scan. `iterate_upper_bound` is what makes the scan
/// correct; `prefix_same_as_start` is what makes the SST prefix filter actually
/// be consulted on Seek.
///
/// Callers must still re-check `starts_with`: a seek key longer than its prefix
/// group (a 38-byte `R <digest> <repo>`, say) extracts to the shorter group, so
/// the iterator can legitimately return a neighbouring repo's edge.
fn prefix_read_opts(prefix: &[u8]) -> ReadOptions {
    let mut opts = ReadOptions::default();
    if let Some(end) = prefix_successor(prefix) {
        opts.set_iterate_upper_bound(end);
    }
    // The empty prefix is deliberately outside the extractor's domain, so there
    // is no group for RocksDB to compare a seek against and asking it to
    // transform the seek key would be asking it to classify a key it has just
    // said it cannot. A whole-keyspace scan has exactly one caller - the
    // store-emptiness probe in `crate::version` - and it wants no prefix
    // filtering anyway.
    if !prefix.is_empty() {
        opts.set_prefix_same_as_start(true);
    }
    opts
}

impl RocksEngine {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::open_with_cache(path, DEFAULT_BLOCK_CACHE_BYTES)
    }

    pub fn open_with_cache(
        path: impl AsRef<std::path::Path>,
        block_cache_bytes: usize,
    ) -> Result<Self> {
        let cache = Cache::new_lru_cache(block_cache_bytes);

        let mut bb = BlockBasedOptions::default();
        // 16 KiB blocks give a ~4x smaller index than the 4 KiB default for the
        // same data size, which is the real reason to raise it.
        bb.set_block_size(16 * 1024);
        bb.set_block_cache(&cache);
        // RocksDB's default filter_policy is nullptr (table.h:590), so without
        // this there are no filters at all. With the prefix extractor below
        // this builds both a prefix filter (one entry per group, which is what
        // `exists_prefix` needs) and a whole-key filter for `get`.
        bb.set_bloom_filter(10.0, false);
        bb.set_whole_key_filtering(true);
        // Bound index and filter memory inside the cache rather than letting it
        // grow outside, and pin the hottest parts.
        bb.set_cache_index_and_filter_blocks(true);
        bb.set_pin_l0_filter_and_index_blocks_in_cache(true);
        bb.set_pin_top_level_index_and_filter(true);
        bb.set_data_block_index_type(DataBlockIndexType::BinaryAndHash);

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_block_based_table_factory(&bb);

        // The point of all this: a prefix filter turns a negative
        // `exists_prefix` into a bloom miss instead of an SST seek.
        opts.set_prefix_extractor(summ_prefix_extractor());
        // The same filter for keys still in the memtable. The second line has
        // effect only because the first is non-zero.
        opts.set_memtable_prefix_bloom_ratio(0.02);
        opts.set_memtable_whole_key_filtering(true);

        // Cheap codec where compaction churns, expensive where data settles.
        opts.set_compression_type(DBCompressionType::Lz4);
        opts.set_bottommost_compression_type(DBCompressionType::Zstd);

        // Level compaction: ~1.11x space amplification against universal's ~2x,
        // and space is the binding constraint here.
        opts.set_target_file_size_base(256 * 1024 * 1024);
        opts.set_write_buffer_size(128 * 1024 * 1024);
        opts.set_max_write_buffer_number(4);
        opts.set_max_background_jobs(6);
        opts.set_bytes_per_sync(1024 * 1024);
        // Documented mitigation for the DeleteRange open-files trap.
        opts.set_max_open_files(-1);

        let db = DB::open(&opts, path).map_err(storage)?;
        Ok(Self { db, _cache: cache })
    }

    /// Iterator positioned at the first key of the scan, honouring an exclusive
    /// `start_after` cursor.
    ///
    /// Raw rather than `DBIterator`: the latter copies key *and* value into
    /// boxed slices on every `next()` before the caller copies them again, so a
    /// keys-only scan cannot avoid the value allocation through it. The raw
    /// iterator hands out borrowed slices, which is what makes `scan_keys`
    /// genuinely cheaper rather than cosmetically so. RocksDB still materialises
    /// the row internally on `Next` - there is no "keys only" mode in the C API
    /// to ask for - but the per-row `Vec` on our side disappears.
    fn seek<'a>(
        &'a self,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        opts: ReadOptions,
    ) -> DBRawIterator<'a> {
        let mut it = self.db.raw_iterator_opt(opts);
        it.seek(start_after.unwrap_or(prefix));
        it
    }

    /// Shared body of [`MetaEngine::scan`] and [`MetaEngine::scan_keys`], so the
    /// two cannot drift apart on cursor semantics.
    ///
    /// Returns whether a further matching key exists past the page - the "read
    /// one past the limit" rule - leaving the caller to take the cursor from its
    /// own last entry.
    fn walk(
        &self,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
        with_values: bool,
        mut emit: impl FnMut(&[u8], &[u8]),
    ) -> Result<bool> {
        let mut it = self.seek(prefix, start_after, prefix_read_opts(prefix));
        let mut taken = 0usize;
        while let Some(key) = it.key() {
            // `start_after` is exclusive; the seek is inclusive.
            if start_after == Some(key) {
                it.next();
                continue;
            }
            if !key.starts_with(prefix) {
                break;
            }
            if taken == limit {
                it.status().map_err(storage)?;
                return Ok(true);
            }
            // `value()` is what pulls the row's bytes across the binding, so a
            // keys-only page simply never asks for them.
            let value = if with_values {
                it.value().unwrap_or_default()
            } else {
                &[][..]
            };
            emit(key, value);
            taken += 1;
            it.next();
        }
        // A raw iterator reports an error by going invalid, so the end of the
        // walk is the only place it can be noticed.
        it.status().map_err(storage)?;
        Ok(false)
    }
}

impl MetaEngine for RocksEngine {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.db.get(key).map_err(storage)
    }

    fn scan(&self, prefix: &[u8], start_after: Option<&[u8]>, limit: usize) -> Result<Page> {
        if limit == 0 {
            return Ok(Page::default());
        }
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(limit.min(1024));
        let more = self.walk(prefix, start_after, limit, true, |k, v| {
            entries.push((k.to_vec(), v.to_vec()))
        })?;
        let next = if more {
            entries.last().map(|(k, _)| k.clone())
        } else {
            None
        };
        Ok(Page { entries, next })
    }

    fn scan_keys(
        &self,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<KeyPage> {
        if limit == 0 {
            return Ok(KeyPage::default());
        }
        let mut keys: Vec<Vec<u8>> = Vec::with_capacity(limit.min(1024));
        let more = self.walk(prefix, start_after, limit, false, |k, _| {
            keys.push(k.to_vec())
        })?;
        let next = if more { keys.last().cloned() } else { None };
        Ok(KeyPage { keys, next })
    }

    fn exists_prefix(&self, prefix: &[u8]) -> Result<bool> {
        let it = self.seek(prefix, None, prefix_read_opts(prefix));
        match it.key() {
            Some(k) => {
                let hit = k.starts_with(prefix);
                it.status().map_err(storage)?;
                Ok(hit)
            }
            None => {
                it.status().map_err(storage)?;
                Ok(false)
            }
        }
    }

    fn apply(&self, batch: &WriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut rocks = RocksBatch::default();
        for op in &batch.ops {
            match op {
                MetaOp::Put { key, value } => rocks.put(key, value),
                MetaOp::Delete { key } => rocks.delete(key),
                MetaOp::DeletePrefix { prefix } => match prefix_successor(prefix) {
                    Some(end) => rocks.delete_range(prefix, &end),
                    // A prefix of all 0xff reaches the end of the keyspace and
                    // has no exclusive upper bound to hand DeleteRange.
                    None => {
                        let mut it = self
                            .db
                            .iterator(IteratorMode::From(prefix, Direction::Forward));
                        for item in &mut it {
                            let (k, _) = item.map_err(storage)?;
                            if !k.starts_with(prefix) {
                                break;
                            }
                            rocks.delete(&k);
                        }
                    }
                },
            }
        }
        self.db.write(rocks).map_err(storage)
    }
}

#[cfg(test)]
mod tests {
    use super::{prefix_successor, summ_in_domain, summ_prefix_len, summ_transform};
    use summ_core::{keys, Digest, Timestamp};

    fn sha256(b: u8) -> Digest {
        Digest::Sha256([b; 32])
    }
    fn sha512(b: u8) -> Digest {
        Digest::Sha512([b; 64])
    }

    /// The property RocksDB actually demands: if two keys share a prefix group,
    /// the transform must agree on it. Length is decided by bytes inside the
    /// prefix, so this holds — including across digest algorithms.
    #[test]
    fn keys_in_a_group_all_extract_to_that_group() {
        for d in [sha256(1), sha512(1)] {
            let group = keys::blob_refs(&d);
            for (repo, manifest) in [(1u32, sha256(9)), (2, sha512(9)), (u32::MAX, sha256(0))] {
                let key = keys::blob_ref(&d, repo, &manifest);
                assert_eq!(summ_transform(&key), group.as_slice());
                assert!(summ_in_domain(&key));
            }
        }
    }

    #[test]
    fn different_digests_land_in_different_groups() {
        let a = keys::blob_ref(&sha256(1), 1, &sha256(9));
        let b = keys::blob_ref(&sha256(2), 1, &sha256(9));
        assert_ne!(summ_transform(&a), summ_transform(&b));
    }

    /// A 38-byte `R <digest> <repo>` seek extracts to the 34-byte digest group,
    /// which is exactly why the scan paths must re-check `starts_with`.
    #[test]
    fn a_repo_scoped_blob_seek_extracts_to_the_digest_group() {
        let d = sha256(1);
        assert_eq!(
            summ_transform(&keys::blob_refs_in_repo(&d, 7)),
            keys::blob_refs(&d).as_slice()
        );
    }

    #[test]
    fn repo_scoped_types_group_on_the_repo_id() {
        let d = sha256(3);
        for (key, group) in [
            (keys::manifest(7, &d), keys::manifests_in_repo(7)),
            (keys::tag(7, "latest"), keys::tags_in_repo(7)),
            (keys::repo_blob(7, &d), keys::blobs_in_repo(7)),
        ] {
            assert_eq!(summ_transform(&key), group.as_slice());
            assert_eq!(summ_prefix_len(&key), Some(5));
        }
    }

    #[test]
    fn digest_bearing_repo_types_group_through_the_digest() {
        for d in [sha256(4), sha512(4)] {
            let key = keys::manifest_tag(7, &d, "latest");
            assert_eq!(
                summ_transform(&key),
                keys::tags_of_manifest(7, &d).as_slice()
            );
            assert_eq!(summ_prefix_len(&key), Some(1 + 4 + d.encoded_len()));
        }
    }

    /// `A m` is the group the contribution wall seeks into: every day and shard
    /// bucket for one manifest must extract to the scan prefix the query uses,
    /// or the SST filter would answer for the wrong set of keys.
    #[test]
    fn manifest_counters_group_through_the_digest() {
        for d in [sha256(5), sha512(5)] {
            let group = keys::counters_of_manifest(7, &d);
            assert_eq!(group.len(), 2 + 4 + d.encoded_len());
            for (day, shard) in [(0u16, 0u16), (20_000, 0), (u16::MAX, u16::MAX)] {
                let key = keys::counter_manifest(7, &d, day, shard);
                assert_eq!(summ_transform(&key), group.as_slice());
                assert!(summ_in_domain(&key));
            }
        }
    }

    /// The tag and repo scopes stop at the repo id. A tag is variable-length, so
    /// the tag scope has nowhere further to go, and `A r` has nothing after the
    /// repo but the bucket itself.
    #[test]
    fn tag_and_repo_counters_group_on_the_repo_id() {
        let expected = keys::counters_in_repo_scope(keys::SCOPE_TAG, 7);
        assert_eq!(expected.len(), 6);
        for tag in ["latest", "a", "v1.2.3-rc1"] {
            let key = keys::counter_tag(7, tag, 20_000, 0);
            assert_eq!(summ_transform(&key), expected.as_slice());
            assert_eq!(summ_prefix_len(&key), Some(6));
        }

        let key = keys::counter_repo(7, 20_000, 0);
        assert_eq!(
            summ_transform(&key),
            keys::counters_of_repo(7).as_slice(),
            "the repo scope's scan prefix is its whole group"
        );
    }

    /// The scope byte is inside every counter prefix, which is what keeps the
    /// three scopes from ever being confused for one another by a filter.
    #[test]
    fn counter_scopes_never_share_a_group() {
        let d = sha256(6);
        let groups = [
            summ_transform(&keys::counter_manifest(7, &d, 1, 0)).to_vec(),
            summ_transform(&keys::counter_tag(7, "latest", 1, 0)).to_vec(),
            summ_transform(&keys::counter_repo(7, 1, 0)).to_vec(),
        ];
        for (i, a) in groups.iter().enumerate() {
            for b in &groups[i + 1..] {
                assert!(!a.starts_with(b) && !b.starts_with(a), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn tag_history_groups_on_the_repo_id() {
        let d = sha256(7);
        for (key, group) in [
            (
                keys::tag_history(7, "latest", Timestamp::from_millis(1_000), &d),
                keys::manifests_in_repo(7),
            ),
            (
                keys::manifest_tag_history(7, &d, Timestamp::from_millis(1_000), "latest"),
                keys::manifests_in_repo(7),
            ),
        ] {
            assert_eq!(summ_prefix_len(&key), Some(5));
            // Same length, different type byte - the group is the repo within
            // this key type, not across types.
            assert_eq!(summ_transform(&key).len(), group.len());
            assert_eq!(&summ_transform(&key)[1..], &group[1..]);
        }
    }

    /// The property RocksDB actually requires, demonstrated rather than
    /// asserted: sort a corpus spanning every in-domain range and check that
    /// keys sharing a group form one contiguous run.
    ///
    /// If that ever failed, a seek into a group could skip live keys, because
    /// `prefix_same_as_start` stops the iterator the moment the extracted prefix
    /// changes. It holds because every byte deciding a group's length - the type
    /// byte, the counter scope byte, the digest algorithm byte - is itself
    /// inside the prefix.
    #[test]
    fn prefix_groups_are_contiguous_in_key_order() {
        let mut corpus = Vec::new();
        for repo in [1u32, 2] {
            for d in [sha256(1), sha512(1), sha256(2)] {
                corpus.push(keys::manifest(repo, &d));
                corpus.push(keys::manifest_body(repo, &d));
                corpus.push(keys::repo_blob(repo, &d));
                corpus.push(keys::manifest_tag(repo, &d, "latest"));
                corpus.push(keys::child_parent(repo, &d, &sha256(9)));
                corpus.push(keys::referrer(repo, &d, &sha512(9)));
                corpus.push(keys::blob_ref(&d, repo, &sha256(9)));
                for day in [0u16, 20_000, u16::MAX] {
                    for shard in [0u16, 7] {
                        corpus.push(keys::counter_manifest(repo, &d, day, shard));
                    }
                }
                for ts in [1_000u64, 2_000] {
                    let at = Timestamp::from_millis(ts);
                    corpus.push(keys::manifest_tag_history(repo, &d, at, "latest"));
                }
            }
            for tag in ["a", "latest", "v1"] {
                corpus.push(keys::tag(repo, tag));
                corpus.push(keys::counter_tag(repo, tag, 20_000, 0));
                corpus.push(keys::tag_history(
                    repo,
                    tag,
                    Timestamp::from_millis(1_000),
                    &sha256(1),
                ));
            }
            corpus.push(keys::counter_repo(repo, 20_000, 0));
        }
        corpus.retain(|k| summ_in_domain(k));
        corpus.sort();
        corpus.dedup();
        assert!(corpus.len() > 100, "corpus too small to prove anything");

        // Every key must carry its own group as a literal prefix, the transform
        // must be idempotent, and the group itself must be classifiable.
        for key in &corpus {
            let g = summ_transform(key);
            assert_eq!(g, &key[..g.len()]);
            assert!(summ_in_domain(g));
            assert_eq!(summ_transform(g), g);
        }

        // Contiguity: once a group has been left behind it must never reappear.
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for key in &corpus {
            let g = summ_transform(key).to_vec();
            if seen.last() == Some(&g) {
                continue;
            }
            assert!(
                !seen.contains(&g),
                "group {g:?} is interrupted by a key from another group"
            );
            seen.push(g);
        }
    }

    #[test]
    fn one_byte_types_stay_out_of_the_domain() {
        for key in [
            keys::uploads(),
            keys::repos_by_name(),
            keys::blob(&sha256(1)),
        ] {
            assert!(!summ_in_domain(&key), "{key:?} should not be in domain");
        }
    }

    /// The binding hands the returned pointer to C, so it must be a subslice.
    #[test]
    fn transform_always_returns_a_subslice() {
        for key in [
            vec![],
            vec![b'R'],
            vec![b'R', 99],
            keys::blob_ref(&sha256(1), 1, &sha256(2)),
            keys::uploads(),
        ] {
            let out = summ_transform(&key);
            assert!(out.len() <= key.len());
            assert_eq!(out, &key[..out.len()]);
        }
    }

    #[test]
    fn an_unknown_algorithm_byte_is_not_classified() {
        assert_eq!(summ_prefix_len(&[b'R', 99, 0, 0]), None);
        assert_eq!(summ_prefix_len(b"R"), None);
        // An unrecognised counter scope, and a counter key too short to reach
        // the digest algorithm byte that would decide its length.
        assert_eq!(summ_prefix_len(b"Ax\0\0\0\0"), None);
        assert_eq!(summ_prefix_len(b"Am\0\0\0\0"), None);
    }

    #[test]
    fn successor_bumps_the_last_byte() {
        assert_eq!(prefix_successor(b"abc").unwrap(), b"abd".to_vec());
    }

    #[test]
    fn successor_carries_over_trailing_ff() {
        assert_eq!(prefix_successor(&[1, 2, 0xff]).unwrap(), vec![1, 3]);
        assert_eq!(prefix_successor(&[1, 0xff, 0xff]).unwrap(), vec![2]);
    }

    #[test]
    fn an_all_ff_prefix_has_no_successor() {
        assert_eq!(prefix_successor(&[0xff, 0xff]), None);
    }

    #[test]
    fn successor_bounds_every_key_with_the_prefix() {
        let prefix = &[1u8, 2, 0xff];
        let end = prefix_successor(prefix).unwrap();
        for tail in [vec![], vec![0], vec![0xff], vec![0xff, 0xff]] {
            let mut key = prefix.to_vec();
            key.extend_from_slice(&tail);
            assert!(key.as_slice() >= prefix.as_slice() && key < end, "{key:?}");
        }
    }
}
