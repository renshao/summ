//! Registry operations: where a Distribution Spec operation becomes a
//! [`WriteBatch`](summ_meta::WriteBatch).
//!
//! This layer sits between the HTTP handlers and the metadata engine. It does
//! no HTTP and it never touches blob bytes - a manifest body is metadata (it
//! lives under `B`), a layer is not. What it owns is the translation from a
//! spec operation to a set of key writes, and the typed results the handlers
//! turn into responses.
//!
//! Three constraints from `CLAUDE.md` shape everything here:
//!
//! - **Every mutation is one atomic [`WriteBatch`].** Nothing writes through a
//!   side channel, because a side-channel write is invisible to the future WAL
//!   and would diverge replicas. Each mutating operation is available in two
//!   forms: `plan_*`, which builds the batch and the outcome without applying
//!   either, and the plain form, which applies it. The planner exists so a
//!   caller can compose several operations - a multi-tag push, say - into one
//!   batch rather than several.
//! - **Nothing non-deterministic goes into a batch.** Every builder takes
//!   `now: Timestamp` from the caller; none of them reads the clock. A batch
//!   therefore means the same thing wherever it is replayed.
//! - **No unbounded list.** Every query here takes a cursor and a limit,
//!   including the ones that look like aggregates. See [`Registry::repo_usage`]
//!   for the one place that costs the caller a fold.
//!
//! The one write that is not part of an operation's batch is repo interning:
//! resolving a name to a `RepoId` allocates one in its own batch the first time
//! a repo is written to. It is idempotent and it has to happen before any key
//! for that repo can be encoded at all.

pub mod codec;
pub mod cosign;
pub mod counters;
pub mod error;
pub mod manifest;
pub mod reference;
pub mod registry;
pub mod uploads;

mod delete;
mod discovery;
mod history;
mod referrers;
mod suffix;
mod tags;

pub use error::{RegistryError, Result};
pub use manifest::{BlobDesc, ManifestKind, ParsedManifest};
pub use reference::Reference;
pub use registry::{
    ManifestHead, ManifestPut, Planned, PushOutcome, Registry, RegistryOptions, StoredManifest,
};

pub use counters::{CountDay, CountDelta, CountSubject};
pub use delete::{BlobRefDeleted, ManifestDeleted};
pub use discovery::{
    BlobReference, BlobReferenceList, DigestList, ManifestCountPage, ManifestList, RepoList,
    RepoUsagePage, TagCountPage,
};
pub use history::{HistoryCursor, TagEventEntry, TagHistory};
pub use referrers::{ReferrerEntry, ReferrerList};
pub use tags::{TagList, TagSet};
pub use uploads::UploadKey;
