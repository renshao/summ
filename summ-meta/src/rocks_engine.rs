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
    DBCompressionType, Direction, IteratorMode, Options, ReadOptions, WriteBatch as RocksBatch, DB,
};
use summ_core::{Result, SummError};

use crate::engine::{MetaEngine, MetaOp, Page, WriteBatch};

fn storage<E: std::fmt::Display>(e: E) -> SummError {
    SummError::Storage(e.to_string())
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

pub struct RocksEngine {
    db: DB,
}

impl RocksEngine {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        // Edge keys are valueless and share long prefixes, so they compress
        // well; zstd on the lower levels is worth the CPU.
        opts.set_compression_type(DBCompressionType::Lz4);
        opts.set_bottommost_compression_type(DBCompressionType::Zstd);
        // TODO(perf): a prefix extractor would let prefix bloom filters serve
        // `exists_prefix` without touching SSTs, but our prefixes are of
        // several different lengths (1, 5, 34, 66 bytes), so a fixed transform
        // does not fit all of them. Revisit with measurements from the scale
        // benchmark before picking one.
        let db = DB::open(&opts, path).map_err(storage)?;
        Ok(Self { db })
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
        let mut opts = ReadOptions::default();
        if let Some(end) = prefix_successor(prefix) {
            opts.set_iterate_upper_bound(end);
        }

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
        let mut opts = ReadOptions::default();
        if let Some(end) = prefix_successor(prefix) {
            opts.set_iterate_upper_bound(end);
        }
        match self.seek(prefix, None, opts).next() {
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
    use super::prefix_successor;

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
