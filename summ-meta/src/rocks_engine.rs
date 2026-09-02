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
    BlockBasedOptions, Cache, DBCompressionType, DataBlockIndexType, Direction, IteratorMode,
    Options, ReadOptions, SliceTransform, WriteBatch as RocksBatch, DB,
};
use summ_core::keys::{
    PREFIX_BLOB_REF, PREFIX_CHILD_PARENT, PREFIX_MANIFEST, PREFIX_MANIFEST_BODY,
    PREFIX_MANIFEST_TAG, PREFIX_REFERRER, PREFIX_REPO_BLOB, PREFIX_TAG,
};
use summ_core::{encoded_len_of, Result, SummError};

use crate::engine::{MetaEngine, MetaOp, Page, WriteBatch};

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
        // Repo-scoped scans: `M|B|T|P <repo:4>`.
        PREFIX_MANIFEST | PREFIX_MANIFEST_BODY | PREFIX_TAG | PREFIX_REPO_BLOB => Some(5),
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
    SliceTransform::create("summ.prefix.v1", summ_transform, Some(summ_in_domain))
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
    opts.set_prefix_same_as_start(true);
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
    fn seek<'a>(
        &'a self,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        opts: ReadOptions,
    ) -> rocksdb::DBIteratorWithThreadMode<'a, DB> {
        let from = start_after.unwrap_or(prefix);
        self.db
            .iterator_opt(IteratorMode::From(from, Direction::Forward), opts)
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
        let opts = prefix_read_opts(prefix);

        let mut entries = Vec::with_capacity(limit.min(1024));
        let mut next = None;
        for item in self.seek(prefix, start_after, opts) {
            let (k, v) = item.map_err(storage)?;
            // `start_after` is exclusive; the iterator seeks inclusively.
            if start_after == Some(k.as_ref()) {
                continue;
            }
            if !k.starts_with(prefix) {
                break;
            }
            if entries.len() == limit {
                next = entries.last().map(|(k, _): &(Vec<u8>, Vec<u8>)| k.clone());
                break;
            }
            entries.push((k.to_vec(), v.to_vec()));
        }
        Ok(Page { entries, next })
    }

    fn exists_prefix(&self, prefix: &[u8]) -> Result<bool> {
        match self.seek(prefix, None, prefix_read_opts(prefix)).next() {
            Some(item) => Ok(item.map_err(storage)?.0.starts_with(prefix)),
            None => Ok(false),
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
    use summ_core::{keys, Digest};

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
