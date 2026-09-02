//! The metadata engine boundary.
//!
//! Reads are point lookups, bounded ordered scans, and prefix-existence checks.
//! Writes go through [`WriteBatch`] and nothing else.
//!
//! Two properties of that write API matter beyond tidiness. It is atomic, so a
//! manifest push - which touches the manifest record, its body, a reference edge
//! per layer, and a tag - either lands whole or not at all. And it is
//! serialisable: a batch is a self-contained, idempotent description of a change
//! that can be written to a log and replayed elsewhere. That is the seam a
//! replica would consume, and it costs nothing to keep now.
//!
//! There is deliberately no read-modify-write primitive. The key schema has no
//! value whose size grows with the registry, so every write is a plain insert or
//! delete. Anything that needs a merge is a schema bug.

use serde::{Deserialize, Serialize};
use summ_core::Result;

/// A single mutation. Kept coarse and value-oriented so a batch can be
/// serialised, shipped, and replayed without reference to engine internals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetaOp {
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        key: Vec<u8>,
    },
    /// Remove every key under `prefix`. Used when dropping a repo or a
    /// manifest's edge set, where enumerating first would be wasteful.
    DeletePrefix {
        prefix: Vec<u8>,
    },
}

/// An atomically applied group of [`MetaOp`]s.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteBatch {
    pub ops: Vec<MetaOp>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: Vec<u8>, value: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(MetaOp::Put {
            key,
            value: value.into(),
        });
        self
    }

    /// Insert an edge key. Edges carry no value; presence is the fact.
    pub fn set(&mut self, key: Vec<u8>) -> &mut Self {
        self.put(key, Vec::new())
    }

    pub fn delete(&mut self, key: Vec<u8>) -> &mut Self {
        self.ops.push(MetaOp::Delete { key });
        self
    }

    pub fn delete_prefix(&mut self, prefix: Vec<u8>) -> &mut Self {
        self.ops.push(MetaOp::DeletePrefix { prefix });
        self
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

/// One page of a scan.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
    /// Key to pass as the next `start_after`. `None` means the scan is
    /// exhausted, which is how a handler decides whether to emit a `Link`
    /// header for the next page.
    pub next: Option<Vec<u8>>,
}

pub trait MetaEngine: Send + Sync + 'static {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Ordered scan of at most `limit` entries under `prefix`, resuming
    /// strictly after `start_after`.
    ///
    /// Bounded by construction: there is no API that materialises a whole
    /// prefix. A ten-million-repo catalog is only ever read a page at a time.
    fn scan(&self, prefix: &[u8], start_after: Option<&[u8]>, limit: usize) -> Result<Page>;

    /// Whether any key exists under `prefix`, without decoding a value.
    ///
    /// This is the purge hot path: "is this blob still referenced?" is one seek.
    fn exists_prefix(&self, prefix: &[u8]) -> Result<bool>;

    fn apply(&self, batch: &WriteBatch) -> Result<()>;
}
