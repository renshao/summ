//! The pull-count accumulator: the in-memory half of the `A` range.
//!
//! Nothing here names a crate below the seam, which is what lets the handlers
//! call it directly. It holds increments; `backend.rs` owns the task that
//! drains them and the writes that follow, because that is the one module
//! allowed to name `summ-registry`.
//!
//! **There is no queue and no channel.** The design note proposed a bounded
//! mpsc into a worker holding running totals, and both halves of that are worse
//! than what is here. A channel costs an allocation and a task wakeup per pull
//! and drops events under exactly the burst you most want counted, where a map
//! turns a burst on one manifest into repeated increments of one entry. And a
//! worker holding *absolute* totals has to seed each one from the store and
//! then keep it forever, which is unbounded in the ten-million-repo direction
//! and needs an eviction policy to fix. This map holds **deltas since the last
//! flush** and the flush drains it, so it is bounded by the traffic in one
//! interval rather than by the size of the registry, there is nothing to seed,
//! and a restart costs one interval instead of leaving stale totals behind.
//!
//! The lock is taken on the pull path, for the length of one hash lookup and
//! one addition, on a request that has already done a metadata lookup and a
//! socket write. It is not where the time goes.
//!
//! Two things this deliberately is not:
//!
//! - **Not exact.** Increments live in memory between flushes, so a crash loses
//!   up to one interval, and past [`MAX_BUCKETS`] a *new* bucket is dropped
//!   rather than allowed to grow the map without bound. Existing buckets keep
//!   counting, so what a spike costs is its long tail, never its volume. These
//!   are a popularity signal, and the API says so.
//! - **Not a clock the ops layer reads.** The day and hour are stamped here,
//!   when the pull is served, and travel with the delta. A flush that straddles
//!   midnight must not move an hour's traffic into the next day, and a
//!   `WriteBatch` must not contain an apply-time timestamp.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use futures_util::Stream;
use summ_core::{keys, Digest};

/// How many distinct buckets one flush interval may hold.
///
/// A bucket is `(subject, day, hour)`, so this is the number of distinct
/// manifests, tags and repositories pulled within an interval, not the size of
/// the registry. At roughly 120 bytes an entry the ceiling is a few megabytes,
/// and reaching it means dropping the tail of a burst rather than growing
/// without bound.
pub const MAX_BUCKETS: usize = 50_000;

/// Which counter an increment belongs to. Mirrors `summ_registry::CountSubject`
/// without naming it - this module sits above the seam.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Subject {
    Manifest(Digest),
    Tag(String),
    Repo,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct BucketKey {
    repo: String,
    subject: Subject,
    day: u16,
    hour: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    manifest_pulls: u64,
    blob_pulls: u64,
    bytes_out: u64,
}

/// One drained increment, ready for `backend.rs` to translate into a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub repo: String,
    pub subject: Subject,
    pub day: u16,
    pub hour: usize,
    pub manifest_pulls: u64,
    pub blob_pulls: u64,
    pub bytes_out: u64,
}

/// Increments waiting for the next flush.
#[derive(Debug)]
pub struct PullCounters {
    enabled: bool,
    dirty: Mutex<HashMap<BucketKey, Counts>>,
    /// Increments discarded because the map was at [`MAX_BUCKETS`]. Reported
    /// once per flush rather than per drop - a saturated accumulator would
    /// otherwise log as fast as it drops.
    dropped: AtomicU64,
}

impl PullCounters {
    pub fn new() -> Self {
        PullCounters {
            enabled: true,
            dirty: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// A counter that counts nothing, for `--no-pull-counts` and for every test
    /// and embedding that has no flush task behind it.
    ///
    /// Disabled rather than optional so that no call site has to ask. A
    /// recording call on this returns before it touches the lock.
    pub fn disabled() -> Self {
        PullCounters {
            enabled: false,
            dirty: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// One `GET /v2/<name>/manifests/<reference>`.
    ///
    /// Counted against the manifest, against the tag when the client asked by
    /// tag, and against the repository - every granularity that will be queried
    /// has to be maintained on write, because rolling repo totals up out of
    /// per-manifest buckets is a scan across up to 10M manifests.
    ///
    /// `HEAD` is not a pull and must never call this: containerd issues `HEAD`
    /// then `GET` on every cold pull, so counting both doubles every number.
    /// Pulling a multi-platform image is two `GET`s - the index and the chosen
    /// child - and each is counted against itself, which is what makes the
    /// index's wall "how often was this image pulled" and the children's the
    /// platform split.
    pub fn record_manifest_pull(&self, repo: &str, tag: Option<&str>, digest: &Digest) {
        if !self.enabled {
            return;
        }
        let (day, hour) = now_bucket();
        let counts = Counts {
            manifest_pulls: 1,
            ..Counts::default()
        };
        let mut dirty = self.lock();
        add(
            &mut dirty,
            key(repo, Subject::Manifest(*digest), day, hour),
            counts,
            &self.dropped,
        );
        if let Some(tag) = tag {
            add(
                &mut dirty,
                key(repo, Subject::Tag(tag.to_string()), day, hour),
                counts,
                &self.dropped,
            );
        }
        add(
            &mut dirty,
            key(repo, Subject::Repo, day, hour),
            counts,
            &self.dropped,
        );
    }

    /// One `GET /v2/<name>/blobs/<digest>`, with the bytes that reached the
    /// socket.
    ///
    /// Repo scope only. Attributing a shared layer's bytes to one manifest
    /// would be a lie, and doing it honestly needs the `R` fan-in, which is a
    /// scan. `day` and `hour` are the ones stamped when the response started,
    /// not when it finished: a download running over midnight belongs to the
    /// hour it was requested in, which is also the hour its `blob_pulls` was
    /// counted in.
    fn record_blob_bytes(&self, repo: &str, day: u16, hour: u8, bytes: u64) {
        if !self.enabled {
            return;
        }
        let counts = Counts {
            blob_pulls: 1,
            bytes_out: bytes,
            ..Counts::default()
        };
        let mut dirty = self.lock();
        add(
            &mut dirty,
            key(repo, Subject::Repo, day, hour),
            counts,
            &self.dropped,
        );
    }

    /// Take everything waiting, leaving the map empty.
    ///
    /// The map is deltas, so draining it is the whole handover: whatever a
    /// concurrent request adds after this lands in the next flush.
    pub fn drain(&self) -> Vec<Recorded> {
        let taken = std::mem::take(&mut *self.lock());
        taken
            .into_iter()
            .map(|(k, v)| Recorded {
                repo: k.repo,
                subject: k.subject,
                day: k.day,
                hour: k.hour as usize,
                manifest_pulls: v.manifest_pulls,
                blob_pulls: v.blob_pulls,
                bytes_out: v.bytes_out,
            })
            .collect()
    }

    /// Increments dropped since this was last called, and zero it.
    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    /// Wrap a blob response so the bytes that actually reach the socket are
    /// counted.
    ///
    /// Counting the requested window instead would be systematically wrong, not
    /// marginally: containerd 2.1+ asks for `bytes=N-`, reads 8 MiB and kills
    /// the connection, so a 900 MB layer would be counted about a hundred times
    /// over. The wrapper reports on drop, which is what a torn-down connection
    /// does to it.
    pub fn meter_blob(self: &Arc<Self>, repo: &str, body: Body) -> Body {
        if !self.enabled {
            return body;
        }
        let (day, hour) = now_bucket();
        Body::from_stream(MeteredBlob {
            inner: body.into_data_stream(),
            counters: Arc::clone(self),
            repo: repo.to_string(),
            day,
            hour,
            bytes: 0,
        })
    }

    /// The map, recovered from a poisoned lock rather than propagating a panic.
    ///
    /// A counter must never be able to fail a pull. The only code under this
    /// lock is a hash lookup and an addition, so a poisoned mutex means a panic
    /// elsewhere in the process, and the worst this recovery can do is count
    /// against a map that a panicking thread had half-updated.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<BucketKey, Counts>> {
        self.dirty.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for PullCounters {
    fn default() -> Self {
        Self::new()
    }
}

fn key(repo: &str, subject: Subject, day: u16, hour: u8) -> BucketKey {
    BucketKey {
        repo: repo.to_string(),
        subject,
        day,
        hour,
    }
}

/// Add into an existing bucket, or open a new one while there is room.
///
/// The cap applies only to *new* keys. A burst on one manifest is one entry
/// however large it gets, so saturating costs the long tail of distinct
/// subjects, never the volume of a hot one.
fn add(
    dirty: &mut HashMap<BucketKey, Counts>,
    key: BucketKey,
    counts: Counts,
    dropped: &AtomicU64,
) {
    if let Some(existing) = dirty.get_mut(&key) {
        existing.manifest_pulls += counts.manifest_pulls;
        existing.blob_pulls += counts.blob_pulls;
        existing.bytes_out += counts.bytes_out;
        return;
    }
    if dirty.len() >= MAX_BUCKETS {
        dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    dirty.insert(key, counts);
}

/// The `(day, hour)` of now, UTC.
///
/// Read here rather than in the ops layer: the coordinate belongs to the moment
/// the pull was served, and a `WriteBatch` may not contain an apply-time
/// timestamp. A clock before the epoch counts as the epoch, which is the same
/// choice the rest of the server makes.
fn now_bucket() -> (u16, u8) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (keys::day_bucket(secs), keys::hour_of_day(secs) as u8)
}

/// A blob body that counts what it yields and reports once, on drop.
struct MeteredBlob<S> {
    inner: S,
    counters: Arc<PullCounters>,
    repo: String,
    day: u16,
    hour: u8,
    bytes: u64,
}

impl<S, E> Stream for MeteredBlob<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(chunk))) = &polled {
            this.bytes += chunk.len() as u64;
        }
        polled
    }
}

impl<S> Drop for MeteredBlob<S> {
    fn drop(&mut self) {
        self.counters
            .record_blob_bytes(&self.repo, self.day, self.hour, self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(n: u8) -> Digest {
        Digest::Sha256([n; 32])
    }

    fn find(drained: &[Recorded], subject: &Subject) -> Recorded {
        drained
            .iter()
            .find(|r| &r.subject == subject)
            .unwrap_or_else(|| panic!("no {subject:?} in {drained:?}"))
            .clone()
    }

    #[test]
    fn a_manifest_pull_by_tag_counts_three_scopes() {
        let counters = PullCounters::new();
        counters.record_manifest_pull("demo", Some("latest"), &digest(1));

        let drained = counters.drain();
        assert_eq!(drained.len(), 3);
        for subject in [
            Subject::Manifest(digest(1)),
            Subject::Tag("latest".into()),
            Subject::Repo,
        ] {
            let row = find(&drained, &subject);
            assert_eq!(row.manifest_pulls, 1);
            assert_eq!(row.repo, "demo");
        }
    }

    /// A pull by digest has no tag to attribute, and must not invent one.
    #[test]
    fn a_manifest_pull_by_digest_counts_two() {
        let counters = PullCounters::new();
        counters.record_manifest_pull("demo", None, &digest(1));

        let drained = counters.drain();
        assert_eq!(drained.len(), 2);
        assert!(!drained.iter().any(|r| matches!(r.subject, Subject::Tag(_))));
    }

    #[test]
    fn repeated_pulls_of_one_subject_stay_one_entry() {
        let counters = PullCounters::new();
        for _ in 0..1_000 {
            counters.record_manifest_pull("demo", Some("latest"), &digest(1));
        }

        let drained = counters.drain();
        assert_eq!(drained.len(), 3);
        assert_eq!(find(&drained, &Subject::Repo).manifest_pulls, 1_000);
    }

    /// Draining is the handover: what has been taken is the flush's, and the
    /// map starts the next interval empty.
    #[test]
    fn draining_leaves_the_map_empty() {
        let counters = PullCounters::new();
        counters.record_manifest_pull("demo", None, &digest(1));
        assert_eq!(counters.drain().len(), 2);
        assert!(counters.drain().is_empty());
    }

    /// The cap bounds distinct subjects, not volume: a hot subject keeps
    /// counting after the map is full.
    #[test]
    fn saturating_drops_new_subjects_and_never_a_hot_one() {
        let counters = PullCounters::new();
        counters.record_manifest_pull("demo", None, &digest(0));
        {
            // Fill the rest of the map by hand, which is far cheaper than
            // pulling MAX_BUCKETS distinct manifests through the public API.
            let mut dirty = counters.lock();
            for n in 0..MAX_BUCKETS as u64 {
                dirty.insert(
                    BucketKey {
                        repo: format!("filler-{n}"),
                        subject: Subject::Repo,
                        day: 1,
                        hour: 0,
                    },
                    Counts::default(),
                );
            }
        }

        counters.record_manifest_pull("demo", None, &digest(0));
        counters.record_manifest_pull("demo", None, &digest(9));

        let drained = counters.drain();
        let hot = drained
            .iter()
            .find(|r| r.subject == Subject::Manifest(digest(0)))
            .expect("the existing subject still counts");
        assert_eq!(hot.manifest_pulls, 2);
        assert!(!drained
            .iter()
            .any(|r| r.subject == Subject::Manifest(digest(9))));
        assert!(counters.take_dropped() > 0);
        assert_eq!(counters.take_dropped(), 0, "reading resets the tally");
    }

    #[test]
    fn a_disabled_counter_records_nothing() {
        let counters = PullCounters::disabled();
        counters.record_manifest_pull("demo", Some("latest"), &digest(1));
        counters.record_blob_bytes("demo", 1, 0, 4096);
        assert!(counters.drain().is_empty());
        assert!(!counters.is_enabled());
    }

    #[tokio::test]
    async fn a_metered_blob_counts_the_bytes_that_reached_the_socket() {
        let counters = Arc::new(PullCounters::new());
        let body = counters.meter_blob("demo", Body::from("0123456789"));
        let collected = axum::body::to_bytes(body, 64).await.unwrap();
        assert_eq!(collected.len(), 10);

        let drained = counters.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].subject, Subject::Repo);
        assert_eq!(drained[0].blob_pulls, 1);
        assert_eq!(drained[0].bytes_out, 10);
    }

    /// The case the wrapper exists for: containerd asks for a window, reads a
    /// prefix of it and tears the connection down. What is counted is what it
    /// received.
    #[tokio::test]
    async fn an_abandoned_blob_counts_only_what_it_delivered() {
        use futures_util::StreamExt;

        let counters = Arc::new(PullCounters::new());
        let body = counters.meter_blob("demo", Body::from("0123456789"));
        let mut stream = body.into_data_stream();
        let first = stream.next().await.unwrap().unwrap();
        assert!(!first.is_empty());
        drop(stream);

        let drained = counters.drain();
        assert_eq!(drained[0].bytes_out, first.len() as u64);
    }
}
