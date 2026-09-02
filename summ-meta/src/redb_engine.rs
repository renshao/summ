//! redb-backed [`MetaEngine`].
//!
//! The database is held as a bare `Arc<Database>`, not behind a mutex. redb is
//! already MVCC: readers run concurrently with each other and with the writer,
//! and `begin_write` serialises writers internally. Wrapping it in a `Mutex`
//! would serialise every read against every other read, which on a pull-heavy
//! registry is the difference between scaling with cores and not scaling at all.

use std::ops::Bound;
use std::sync::Arc;

use redb::{Database, TableDefinition};
use summ_core::{Result, SummError};

use crate::engine::{KeyPage, MetaEngine, MetaOp, Page, WriteBatch};

const DATA: TableDefinition<&[u8], &[u8]> = TableDefinition::new("data");

fn storage<E: std::fmt::Display>(e: E) -> SummError {
    SummError::Storage(e.to_string())
}

pub struct RedbEngine {
    db: Arc<Database>,
}

impl RedbEngine {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let db = Database::create(path).map_err(storage)?;
        // Materialise the table so read transactions never race a missing table.
        let txn = db.begin_write().map_err(storage)?;
        txn.open_table(DATA).map_err(storage)?;
        txn.commit().map_err(storage)?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Shared body of [`MetaEngine::scan`] and [`MetaEngine::scan_keys`], so the
    /// two cannot drift apart on cursor semantics.
    ///
    /// redb hands back an `AccessGuard` per column and the value's bytes are
    /// only materialised by `value()`, so a keys-only page costs nothing extra
    /// simply by never asking - which is the whole point of `scan_keys` for the
    /// valueless edge ranges purge walks.
    ///
    /// Returns whether a further matching key exists past the page: read one
    /// past the limit, and its predecessor is the cursor.
    fn walk(
        &self,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
        with_values: bool,
        mut emit: impl FnMut(&[u8], &[u8]),
    ) -> Result<bool> {
        let txn = self.db.begin_read().map_err(storage)?;
        let table = txn.open_table(DATA).map_err(storage)?;

        let lower = match start_after {
            Some(k) => Bound::Excluded(k),
            None => Bound::Included(prefix),
        };
        let range = table
            .range::<&[u8]>((lower, Bound::Unbounded))
            .map_err(storage)?;

        for (taken, entry) in range.enumerate() {
            let (k, v) = entry.map_err(storage)?;
            let key = k.value();
            if !key.starts_with(prefix) {
                return Ok(false);
            }
            if taken == limit {
                return Ok(true);
            }
            if with_values {
                emit(key, v.value());
            } else {
                emit(key, &[]);
            }
        }
        Ok(false)
    }
}

impl MetaEngine for RedbEngine {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(storage)?;
        let table = txn.open_table(DATA).map_err(storage)?;
        Ok(table.get(key).map_err(storage)?.map(|v| v.value().to_vec()))
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
        let txn = self.db.begin_read().map_err(storage)?;
        let table = txn.open_table(DATA).map_err(storage)?;
        let mut range = table
            .range::<&[u8]>((Bound::Included(prefix), Bound::Unbounded))
            .map_err(storage)?;
        match range.next() {
            Some(entry) => Ok(entry.map_err(storage)?.0.value().starts_with(prefix)),
            None => Ok(false),
        }
    }

    fn apply(&self, batch: &WriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().map_err(storage)?;
        {
            let mut table = txn.open_table(DATA).map_err(storage)?;
            for op in &batch.ops {
                match op {
                    MetaOp::Put { key, value } => {
                        table
                            .insert(key.as_slice(), value.as_slice())
                            .map_err(storage)?;
                    }
                    MetaOp::Delete { key } => {
                        table.remove(key.as_slice()).map_err(storage)?;
                    }
                    MetaOp::DeletePrefix { prefix } => {
                        table
                            .retain_in(prefix.as_slice().., |k, _| !k.starts_with(prefix))
                            .map_err(storage)?;
                    }
                }
            }
        }
        txn.commit().map_err(storage)?;
        Ok(())
    }
}
