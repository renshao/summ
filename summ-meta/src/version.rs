//! The schema version marker and the migration seam.
//!
//! A lost or unreadable metadata store is a dead registry: manifest bytes live
//! only under `B` and tags only under `T`, so there is nothing to rebuild from.
//! The cheapest insurance against the *silent* half of that failure is a version
//! marker, and it exists now rather than when it is first needed because
//! retrofitting one onto a populated store means guessing what that store
//! already contains.
//!
//! What makes it load-bearing is postcard. It is not self-describing: a record
//! written before a field was added does not decode afterwards, and the error it
//! produces is indistinguishable from corruption. Refusing to open a store this
//! build does not understand turns that into a sentence an operator can act on.
//!
//! This is engine-agnostic on purpose - it is written against [`MetaEngine`], so
//! both engines get it and neither can drift.

use summ_core::types::SCHEMA_VERSION;
use summ_core::{keys, Result, SummError};

use crate::engine::{MetaEngine, WriteBatch};

type Step = Box<dyn Fn(&dyn MetaEngine) -> Result<()> + Send + Sync>;

/// One forward step, run when an opened store's version is below `to`.
pub struct Migration {
    to: u32,
    run: Step,
}

/// The ordered set of steps this build knows how to apply.
///
/// Empty today - there has only ever been one schema - but the seam is here
/// because the analytics records are the first that are likely to gain fields,
/// and by then the store will be populated.
#[derive(Default)]
pub struct Migrations {
    steps: Vec<Migration>,
}

impl Migrations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a step that leaves the store at version `to`.
    ///
    /// A step does its own writes and the version stamp lands in a separate
    /// batch afterwards. That is deliberate: [`WriteBatch`] is the only atomic
    /// unit there is, and no batch is large enough to hold a migration that
    /// rewrites millions of records. The consequence is that a crash between
    /// the two replays the step, so **a step must be safe to re-run against a
    /// store it has already migrated.**
    pub fn register(
        &mut self,
        to: u32,
        run: impl Fn(&dyn MetaEngine) -> Result<()> + Send + Sync + 'static,
    ) -> &mut Self {
        self.steps.push(Migration {
            to,
            run: Box::new(run),
        });
        self
    }
}

fn decode(raw: &[u8]) -> Result<u32> {
    let bytes: [u8; 4] = raw.try_into().map_err(|_| {
        SummError::InvalidData(format!(
            "schema version marker is {} bytes, expected 4",
            raw.len()
        ))
    })?;
    Ok(u32::from_be_bytes(bytes))
}

/// The version this store declares, or `None` if it carries no marker.
pub fn read(engine: &dyn MetaEngine) -> Result<Option<u32>> {
    match engine.get(&keys::db_version())? {
        Some(raw) => Ok(Some(decode(&raw)?)),
        None => Ok(None),
    }
}

/// Write the marker. Goes through [`WriteBatch`] like every other mutation, so
/// it is visible to the log a replica would consume.
pub fn stamp(engine: &dyn MetaEngine, version: u32) -> Result<()> {
    let mut batch = WriteBatch::new();
    batch.put(keys::db_version(), version.to_be_bytes().to_vec());
    engine.apply(&batch)
}

/// One key, anywhere. The marker is written on creation, so this is only ever
/// asked of a store that has none.
fn is_empty(engine: &dyn MetaEngine) -> Result<bool> {
    Ok(engine.scan_keys(&[], None, 1)?.keys.is_empty())
}

/// Check an opened store against this build, migrating it forward if it is
/// behind, and return the version it ends up at.
///
/// Four cases, and the interesting one is the third:
///
/// - **No marker, no data.** A store this build just created. Stamp it.
/// - **Version above [`SCHEMA_VERSION`].** Refuse. A newer summ may have written
///   records this build cannot decode, and postcard would report that as
///   corruption rather than as a version skew.
/// - **No marker, but data.** Also refuse. The marker is written at creation, so
///   its absence on a populated store means the store predates versioning - and
///   this build has no way to tell *which* pre-versioning layout its records are
///   in. Stamping it would be asserting a compatibility claim nobody checked,
///   and the failure would surface later as a mis-decoded record rather than
///   here as a refusal. [`stamp`] is the deliberate escape hatch for an operator
///   who has established the answer, so refusing is loud rather than terminal.
/// - **Version below.** Run the registered steps in ascending order.
pub fn ensure(engine: &dyn MetaEngine, migrations: &Migrations) -> Result<u32> {
    let Some(found) = read(engine)? else {
        if !is_empty(engine)? {
            return Err(SummError::InvalidData(
                "store holds data but carries no schema version marker: it predates versioning, \
                 and this build cannot tell which layout its records are in. Stamp it \
                 deliberately once that is established."
                    .into(),
            ));
        }
        stamp(engine, SCHEMA_VERSION)?;
        return Ok(SCHEMA_VERSION);
    };

    if found > SCHEMA_VERSION {
        return Err(SummError::InvalidData(format!(
            "store schema version {found} is newer than this build's {current}: a newer summ may \
             have written records this build cannot decode",
            current = SCHEMA_VERSION
        )));
    }

    let mut ordered: Vec<&Migration> = migrations.steps.iter().collect();
    ordered.sort_by_key(|m| m.to);

    let mut at = found;
    for step in ordered {
        if step.to <= at {
            continue;
        }
        if step.to > SCHEMA_VERSION {
            return Err(SummError::InvalidData(format!(
                "migration to schema {to} is beyond this build's {current}",
                to = step.to,
                current = SCHEMA_VERSION
            )));
        }
        (step.run)(engine)?;
        // Stamped per step, not once at the end, so an interrupted upgrade
        // resumes at the step it died in rather than replaying the whole chain.
        stamp(engine, step.to)?;
        at = step.to;
    }

    if at != SCHEMA_VERSION {
        return Err(SummError::InvalidData(format!(
            "store is at schema {at} and no registered migration reaches {current}",
            current = SCHEMA_VERSION
        )));
    }
    Ok(at)
}

/// Version-check a freshly opened engine and hand it back.
///
/// The check is not inside `RocksEngine::open` / `RedbEngine::open` for a
/// concrete reason: a store that is behind needs migrations run *against it*,
/// which is impossible if being behind is what stops you obtaining the handle.
/// So construction stays dumb and this is the line that follows it:
///
/// ```no_run
/// # use summ_meta::{version, version::Migrations, RocksEngine};
/// let db = version::open(RocksEngine::open("meta")?, &Migrations::new())?;
/// # Ok::<(), summ_core::SummError>(())
/// ```
pub fn open<E: MetaEngine>(engine: E, migrations: &Migrations) -> Result<E> {
    ensure(&engine, migrations)?;
    Ok(engine)
}
