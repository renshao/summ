//! Pull counts - the `A` range, both sides.
//!
//! This is the one place in summ where a stored value is read, added to, and
//! written back. That is not a hole in the no-read-modify-write rule; it is
//! where the rule is paid for. The rule exists so that a `WriteBatch` means the
//! same thing wherever it is replayed, and it still does: the fold happens
//! *before* the batch is built, so what lands in the log is a plain `Put` of an
//! absolute value with no engine-minted content in it. What the rule forbids is
//! a read-modify-write on the *request* path, and there is none - a pull adds
//! to a map in memory and returns.
//!
//! Three properties of these counters that a caller has to know, because they
//! decide what the numbers mean:
//!
//! - **They are best-effort.** The increments live in memory between flushes,
//!   so a crash loses up to one flush interval and a saturated accumulator
//!   drops the tail of a spike. They are a popularity signal for an operator,
//!   not billing data, and every API that serves them says so.
//! - **One writer owns the range.** Absolute values are last-write-wins, so two
//!   nodes flushing the same bucket would silently undercount. The `<shard>`
//!   key component is reserved for the writing node's id and is `0` today; the
//!   read path sums across shards so that adding one later is not a migration.
//! - **A bucket is a day, broken down by hour, fixed in UTC at write time.**
//!   The day boundary must never be recomputed for a viewer's timezone, or the
//!   same wall changes shape depending on who is looking at it. The hours are
//!   what let a reader re-sum honestly into their own zone instead.
//!
//! Counting a pull for a repository that is subsequently deleted writes keys
//! nobody will read. They are harmless: `A` is a clean prefix under the repo,
//! so the purge sweep drops them with a `DeletePrefix` like everything else.

use std::collections::BTreeMap;

use summ_core::{keys, CounterBucket, Digest};
use summ_meta::WriteBatch;

use crate::codec::{decode, encode};
use crate::error::Result;
use crate::registry::Registry;

/// Which counter a pull belongs to.
///
/// Every granularity that will be queried has to be maintained on write:
/// rolling repo totals up out of per-manifest buckets would be a scan across up
/// to 10M manifests, which is an unbounded read wearing a summary's clothing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CountSubject {
    /// `A m <repo> <digest>` - the per-manifest wall, the headline query.
    Manifest(Digest),
    /// `A t <repo> <tag>` - which tags people actually pull.
    Tag(String),
    /// `A r <repo>` - repo totals, and the only scope carrying blob traffic.
    Repo,
}

/// One flush entry: what to add, and where.
///
/// `day` and `hour` are the whole time coordinate, computed when the pull was
/// served rather than when the flush runs - a flush that straddles midnight
/// must not move an hour's traffic into the next day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountDelta {
    pub repo: String,
    pub subject: CountSubject,
    pub day: u16,
    pub hour: usize,
    pub manifest_pulls: u64,
    pub blob_pulls: u64,
    pub bytes_out: u64,
}

/// One day of counters, with the shards already summed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountDay {
    pub day: u16,
    pub bucket: CounterBucket,
}

/// The shard written by a single node. Reserved rather than used; see the
/// module docs.
const SHARD: u16 = 0;

impl Registry {
    /// Fold a flush's deltas into the store: one `get` per dirty bucket, one
    /// batch for all of them.
    ///
    /// Deltas for the same bucket are combined first, so a flush touching one
    /// manifest a million times still does one lookup and one `Put`. A repo the
    /// deltas name but the store does not have is **skipped, not interned**: a
    /// pull can only have been served for a repo that existed, so `None` here
    /// means it was deleted between the pull and the flush, and minting an id
    /// for it would resurrect a name in the catalog on the strength of a
    /// counter.
    pub fn add_pull_counts(&self, deltas: &[CountDelta]) -> Result<usize> {
        // Combined by key so the lookup below is per bucket rather than per
        // delta, and so the batch cannot contain two `Put`s to one key.
        let mut folded: BTreeMap<(String, CountSubject, u16), Vec<&CountDelta>> = BTreeMap::new();
        for delta in deltas {
            folded
                .entry((delta.repo.clone(), delta.subject.clone(), delta.day))
                .or_default()
                .push(delta);
        }

        let mut batch = WriteBatch::new();
        for ((repo, subject, day), entries) in folded {
            let Some(repo_id) = self.lookup_repo(&repo)? else {
                continue;
            };
            let key = match &subject {
                CountSubject::Manifest(digest) => {
                    keys::counter_manifest(repo_id, digest, day, SHARD)
                }
                CountSubject::Tag(tag) => keys::counter_tag(repo_id, tag, day, SHARD),
                CountSubject::Repo => keys::counter_repo(repo_id, day, SHARD),
            };
            let mut bucket = match self.engine().get(&key)? {
                Some(stored) => decode(&stored, "CounterBucket")?,
                None => CounterBucket::default(),
            };
            for delta in entries {
                bucket.add(
                    delta.hour,
                    delta.manifest_pulls,
                    delta.blob_pulls,
                    delta.bytes_out,
                );
            }
            batch.put(key, encode(&bucket)?);
        }

        let written = batch.len();
        if written > 0 {
            self.engine().apply(&batch)?;
        }
        Ok(written)
    }

    /// `A m <repo> <digest>` over a day window - the wall.
    pub fn manifest_counts(
        &self,
        repo: &str,
        digest: &Digest,
        from: u16,
        days: u16,
    ) -> Result<Vec<CountDay>> {
        self.counts(repo, from, days, |repo_id| {
            keys::counters_of_manifest(repo_id, digest)
        })
    }

    /// `A t <repo> <tag>` over a day window.
    pub fn tag_counts(&self, repo: &str, tag: &str, from: u16, days: u16) -> Result<Vec<CountDay>> {
        self.counts(repo, from, days, |repo_id| {
            keys::counters_of_tag(repo_id, tag)
        })
    }

    /// `A r <repo>` over a day window. The only scope carrying blob traffic.
    pub fn repo_counts(&self, repo: &str, from: u16, days: u16) -> Result<Vec<CountDay>> {
        self.counts(repo, from, days, keys::counters_of_repo)
    }

    /// One bounded forward scan over a day window, shards summed.
    ///
    /// The window is the bound: 53 weeks is 371 keys, so the whole
    /// visualisation is one scan arriving in chronological order with no
    /// pagination and no read-time aggregation. An unknown repository is an
    /// empty series rather than an error, for the same reason tag history never
    /// 404s - counts outlive what they describe, and after a delete nothing
    /// distinguishes "never pulled" from "gone".
    ///
    /// Only days present in the store come back. Zero-filling is the caller's
    /// job: a wall wants every day, a table wants only the ones with traffic,
    /// and the layer that knows which is the one that should decide.
    fn counts(
        &self,
        repo: &str,
        from: u16,
        days: u16,
        prefix_of: impl FnOnce(summ_core::RepoId) -> Vec<u8>,
    ) -> Result<Vec<CountDay>> {
        if days == 0 {
            return Ok(Vec::new());
        }
        let Some(repo_id) = self.lookup_repo(repo)? else {
            return Ok(Vec::new());
        };
        let prefix = prefix_of(repo_id);
        let start = keys::counters_from_day(&prefix, from);
        // One key per (day, shard). Shard is 0 today, so `days` would do; the
        // slack is what stops a second writer silently truncating the window
        // rather than merely making it approximate.
        let limit = (days as usize).saturating_mul(SHARD_SCAN_SLACK);
        let last = from.saturating_add(days.saturating_sub(1));

        let page = self.engine().scan(&prefix, Some(&start), limit)?;
        let mut out: Vec<CountDay> = Vec::new();
        for (key, value) in &page.entries {
            let Some((day, _shard)) = keys::counter_suffix(key, &prefix) else {
                // A key shaped unlike the group it was scanned out of can only
                // be corruption, and one bad row must not fail a wall.
                continue;
            };
            if day > last {
                break;
            }
            let bucket: CounterBucket = decode(value, "CounterBucket")?;
            match out.last_mut() {
                // Shards of one day are adjacent, so summing them is a fold
                // over the run rather than a map.
                Some(prev) if prev.day == day => {
                    for hour in 0..summ_core::types::HOURS_PER_DAY {
                        prev.bucket.add(
                            hour,
                            bucket.manifest_pulls[hour] as u64,
                            bucket.blob_pulls[hour] as u64,
                            bucket.bytes_out[hour],
                        );
                    }
                }
                _ => out.push(CountDay { day, bucket }),
            }
        }
        Ok(out)
    }
}

/// How many shards a day-windowed scan is willing to walk per day.
const SHARD_SCAN_SLACK: usize = 4;
