//! Repo name <-> id interning.
//!
//! Names are interned to a `u32` so a long repo name is not repeated in every
//! key. Both directions are persisted (`n` name->id, `i` id->name) and the
//! in-memory side is a bounded LRU, not a full map.
//!
//! The bound is the point. Ten million repos at a typical name length would be
//! well over a gigabyte resident across both directions, plus a full scan at
//! startup before the registry could serve anything. A cache of the hot set
//! instead makes startup O(1), and a miss costs one B-tree lookup that is
//! usually already in the page cache.

use std::num::NonZeroUsize;

use lru::LruCache;
use parking_lot::Mutex;
use summ_core::{keys, types::RepoId, Result, SummError};

use crate::engine::{MetaEngine, WriteBatch};

/// Hot repos held in memory per direction. Registries are heavily skewed, so a
/// small cache absorbs nearly all lookups.
pub const DEFAULT_CACHE_ENTRIES: usize = 100_000;

/// `i <u32::MAX>` is reserved as the allocation counter, so ids stop one short.
const NEXT_ID_KEY_ID: RepoId = RepoId::MAX;
const MAX_REPO_ID: RepoId = RepoId::MAX - 1;

pub struct RepoInterner {
    by_name: Mutex<LruCache<String, RepoId>>,
    by_id: Mutex<LruCache<RepoId, String>>,
    /// Held only while allocating a new id, to keep the read-then-write on the
    /// counter atomic against other allocators in this process.
    alloc: Mutex<()>,
}

impl Default for RepoInterner {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CACHE_ENTRIES)
    }
}

impl RepoInterner {
    pub fn with_capacity(entries: usize) -> Self {
        let cap = NonZeroUsize::new(entries.max(1)).expect("cap >= 1");
        Self {
            by_name: Mutex::new(LruCache::new(cap)),
            by_id: Mutex::new(LruCache::new(cap)),
            alloc: Mutex::new(()),
        }
    }

    /// Resolve a name to an id without allocating one. `None` means the repo
    /// does not exist, which is the check a pull must make.
    pub fn lookup(&self, engine: &dyn MetaEngine, name: &str) -> Result<Option<RepoId>> {
        if let Some(id) = self.by_name.lock().get(name) {
            return Ok(Some(*id));
        }
        let Some(raw) = engine.get(&keys::repo_by_name(name))? else {
            return Ok(None);
        };
        let id = keys::parse_repo_id(&raw)
            .ok_or_else(|| SummError::InvalidData(format!("bad repo id for {name:?}")))?;
        self.remember(name, id);
        Ok(Some(id))
    }

    /// Resolve an id back to a name.
    pub fn resolve(&self, engine: &dyn MetaEngine, id: RepoId) -> Result<Option<String>> {
        if let Some(name) = self.by_id.lock().get(&id) {
            return Ok(Some(name.clone()));
        }
        let Some(raw) = engine.get(&keys::repo_by_id(id))? else {
            return Ok(None);
        };
        let name = String::from_utf8(raw)
            .map_err(|e| SummError::InvalidData(format!("repo name: {e}")))?;
        self.remember(&name, id);
        Ok(Some(name))
    }

    /// Resolve a name, allocating an id if it is new.
    ///
    /// The forward and reverse keys and the bumped counter are written in one
    /// batch, so a crash can never leave half a mapping behind.
    pub fn intern(&self, engine: &dyn MetaEngine, name: &str) -> Result<RepoId> {
        if let Some(id) = self.lookup(engine, name)? {
            return Ok(id);
        }
        let _guard = self.alloc.lock();
        // Another thread may have won the race while we waited.
        if let Some(id) = self.lookup(engine, name)? {
            return Ok(id);
        }

        let counter_key = keys::repo_by_id(NEXT_ID_KEY_ID);
        let id = match engine.get(&counter_key)? {
            Some(raw) => keys::parse_repo_id(&raw)
                .ok_or_else(|| SummError::InvalidData("bad repo id counter".into()))?,
            None => 0,
        };
        if id > MAX_REPO_ID {
            return Err(SummError::InvalidData("repo id space exhausted".into()));
        }

        let mut batch = WriteBatch::new();
        batch
            .put(keys::repo_by_name(name), id.to_be_bytes().to_vec())
            .put(keys::repo_by_id(id), name.as_bytes().to_vec())
            .put(counter_key, (id + 1).to_be_bytes().to_vec());
        engine.apply(&batch)?;

        self.remember(name, id);
        Ok(id)
    }

    fn remember(&self, name: &str, id: RepoId) {
        self.by_name.lock().put(name.to_string(), id);
        self.by_id.lock().put(id, name.to_string());
    }
}
