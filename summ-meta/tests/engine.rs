//! Behaviour the registry depends on at scale: bounded paging, cheap existence
//! checks, atomic batches, and an interner that stays correct once its cache
//! has evicted everything.
//!
//! Every case runs against both engines. The trait is the contract that keeps
//! the engine choice reversible, so it is only worth anything if the
//! implementations are actually interchangeable.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use summ_core::types::{CounterBucket, SCHEMA_VERSION};
use summ_core::{keys, Digest};
use summ_meta::{
    version, MetaEngine, Migrations, RedbEngine, RepoInterner, RocksEngine, WriteBatch,
};

fn redb() -> (RedbEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = RedbEngine::open(dir.path().join("test.db")).unwrap();
    (db, dir)
}

fn rocks() -> (RocksEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = RocksEngine::open(dir.path().join("test.rocks")).unwrap();
    (db, dir)
}

fn digest(b: u8) -> Digest {
    Digest::Sha256([b; 32])
}

/// Generates the same suite against one engine constructor.
macro_rules! engine_tests {
    ($name:ident, $open:expr, $($case:ident),+ $(,)?) => {
        mod $name {
            use super::*;
            $(
                #[test]
                fn $case() {
                    let (db, _guard) = $open();
                    super::suite::$case(&db);
                }
            )+
        }
    };
}

engine_tests!(
    redb_engine,
    redb,
    scan_pages_in_order_and_stops_at_the_prefix,
    a_scan_never_materialises_more_than_its_limit,
    exists_prefix_answers_the_purge_question,
    dropping_the_last_reference_makes_a_blob_purgeable,
    delete_prefix_removes_only_its_own_range,
    a_batch_is_atomic_across_every_key_a_push_touches,
    interner_survives_full_cache_eviction,
    interning_the_same_name_twice_is_stable,
    an_unknown_repo_is_not_silently_created_by_lookup,
    catalog_pages_by_name_not_by_id,
    scan_keys_agrees_with_scan_over_every_new_range,
    scan_keys_pages_a_valueless_edge_range_identically,
    the_pull_count_wall_is_one_bounded_chronological_scan,
    tag_history_pages_newest_first_without_a_reverse_iterator,
    a_fresh_store_is_stamped_with_the_current_schema_version,
    a_store_from_the_future_is_refused,
    an_unversioned_store_holding_data_is_refused,
    a_store_behind_with_no_registered_migration_is_refused,
    a_registered_migration_runs_once_and_advances_the_version,
    a_migration_past_this_build_is_refused,
);

engine_tests!(
    rocks_engine,
    rocks,
    scan_pages_in_order_and_stops_at_the_prefix,
    a_scan_never_materialises_more_than_its_limit,
    exists_prefix_answers_the_purge_question,
    dropping_the_last_reference_makes_a_blob_purgeable,
    delete_prefix_removes_only_its_own_range,
    a_batch_is_atomic_across_every_key_a_push_touches,
    interner_survives_full_cache_eviction,
    interning_the_same_name_twice_is_stable,
    an_unknown_repo_is_not_silently_created_by_lookup,
    catalog_pages_by_name_not_by_id,
    scan_keys_agrees_with_scan_over_every_new_range,
    scan_keys_pages_a_valueless_edge_range_identically,
    the_pull_count_wall_is_one_bounded_chronological_scan,
    tag_history_pages_newest_first_without_a_reverse_iterator,
    a_fresh_store_is_stamped_with_the_current_schema_version,
    a_store_from_the_future_is_refused,
    an_unversioned_store_holding_data_is_refused,
    a_store_behind_with_no_registered_migration_is_refused,
    a_registered_migration_runs_once_and_advances_the_version,
    a_migration_past_this_build_is_refused,
);

mod suite {
    use super::*;

    pub fn scan_pages_in_order_and_stops_at_the_prefix(db: &dyn MetaEngine) {
        let mut batch = WriteBatch::new();
        for i in 0..10u8 {
            batch.put(keys::tag(1, &format!("v{i}")), digest(i).raw().to_vec());
        }
        // A neighbouring repo the scan must not stray into.
        batch.put(keys::tag(2, "v0"), digest(99).raw().to_vec());
        db.apply(&batch).unwrap();

        let prefix = keys::tags_in_repo(1);
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = db.scan(&prefix, cursor.as_deref(), 3).unwrap();
            assert!(page.entries.len() <= 3, "page exceeded the requested limit");
            for (k, _) in &page.entries {
                seen.push(keys::parse_tag_suffix(k).unwrap().to_string());
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert_eq!(seen.len(), 10, "paging lost or duplicated entries");
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(seen, sorted, "pages did not come back in key order");
    }

    pub fn a_scan_never_materialises_more_than_its_limit(db: &dyn MetaEngine) {
        let mut batch = WriteBatch::new();
        for i in 0..5_000u32 {
            batch.put(
                keys::repo_by_name(&format!("repo{i:06}")),
                i.to_be_bytes().to_vec(),
            );
        }
        db.apply(&batch).unwrap();

        let page = db.scan(&keys::repos_by_name(), None, 50).unwrap();
        assert_eq!(page.entries.len(), 50);
        assert!(page.next.is_some(), "expected a cursor for the next page");
    }

    pub fn exists_prefix_answers_the_purge_question(db: &dyn MetaEngine) {
        let layer = digest(1);
        let orphan = digest(2);

        let mut batch = WriteBatch::new();
        batch.set(keys::blob_ref(&layer, 7, &digest(50)));
        db.apply(&batch).unwrap();

        assert!(db.exists_prefix(&keys::blob_refs(&layer)).unwrap());
        assert!(
            !db.exists_prefix(&keys::blob_refs(&orphan)).unwrap(),
            "an unreferenced blob must report no references"
        );
        // Referenced by repo 7, but not by repo 8 - which is what gates serving.
        assert!(db
            .exists_prefix(&keys::blob_refs_in_repo(&layer, 7))
            .unwrap());
        assert!(!db
            .exists_prefix(&keys::blob_refs_in_repo(&layer, 8))
            .unwrap());
    }

    pub fn dropping_the_last_reference_makes_a_blob_purgeable(db: &dyn MetaEngine) {
        let layer = digest(1);
        let (m1, m2) = (digest(10), digest(11));

        let mut batch = WriteBatch::new();
        batch
            .set(keys::blob_ref(&layer, 1, &m1))
            .set(keys::blob_ref(&layer, 1, &m2));
        db.apply(&batch).unwrap();

        let mut batch = WriteBatch::new();
        batch.delete(keys::blob_ref(&layer, 1, &m1));
        db.apply(&batch).unwrap();
        assert!(
            db.exists_prefix(&keys::blob_refs(&layer)).unwrap(),
            "one reference remains, blob must survive"
        );

        let mut batch = WriteBatch::new();
        batch.delete(keys::blob_ref(&layer, 1, &m2));
        db.apply(&batch).unwrap();
        assert!(!db.exists_prefix(&keys::blob_refs(&layer)).unwrap());
    }

    pub fn delete_prefix_removes_only_its_own_range(db: &dyn MetaEngine) {
        let manifest = digest(5);

        let mut batch = WriteBatch::new();
        batch
            .set(keys::manifest_tag(1, &manifest, "latest"))
            .set(keys::manifest_tag(1, &manifest, "v1"))
            .set(keys::manifest_tag(1, &digest(6), "other"))
            .set(keys::manifest_tag(2, &manifest, "elsewhere"));
        db.apply(&batch).unwrap();

        let mut batch = WriteBatch::new();
        batch.delete_prefix(keys::tags_of_manifest(1, &manifest));
        db.apply(&batch).unwrap();

        assert!(!db
            .exists_prefix(&keys::tags_of_manifest(1, &manifest))
            .unwrap());
        assert!(db
            .exists_prefix(&keys::tags_of_manifest(1, &digest(6)))
            .unwrap());
        assert!(db
            .exists_prefix(&keys::tags_of_manifest(2, &manifest))
            .unwrap());
    }

    pub fn a_batch_is_atomic_across_every_key_a_push_touches(db: &dyn MetaEngine) {
        let manifest = digest(20);
        let layers = [digest(21), digest(22)];

        let mut batch = WriteBatch::new();
        batch.put(keys::manifest(1, &manifest), b"record".to_vec());
        batch.put(keys::manifest_body(1, &manifest), b"json".to_vec());
        for layer in &layers {
            batch
                .set(keys::blob_ref(layer, 1, &manifest))
                .set(keys::repo_blob(1, layer));
        }
        batch.put(keys::tag(1, "latest"), manifest.raw().to_vec());
        batch.set(keys::manifest_tag(1, &manifest, "latest"));
        db.apply(&batch).unwrap();

        assert!(db.get(&keys::manifest(1, &manifest)).unwrap().is_some());
        assert!(db
            .get(&keys::manifest_body(1, &manifest))
            .unwrap()
            .is_some());
        assert!(db.get(&keys::tag(1, "latest")).unwrap().is_some());
        for layer in &layers {
            assert!(db.exists_prefix(&keys::blob_refs(layer)).unwrap());
        }
    }

    pub fn interner_survives_full_cache_eviction(db: &dyn MetaEngine) {
        // Cache one entry, then intern many, so nearly every lookup must fall
        // through to the engine - the ten-million-repo case.
        let interner = RepoInterner::with_capacity(1);

        let names: Vec<String> = (0..200).map(|i| format!("team/service-{i}")).collect();
        let ids: Vec<_> = names
            .iter()
            .map(|n| interner.intern(db, n).unwrap())
            .collect();

        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "ids must be unique");

        for (name, id) in names.iter().zip(&ids) {
            assert_eq!(interner.lookup(db, name).unwrap(), Some(*id));
            assert_eq!(interner.resolve(db, *id).unwrap().as_ref(), Some(name));
        }
    }

    pub fn interning_the_same_name_twice_is_stable(db: &dyn MetaEngine) {
        let interner = RepoInterner::with_capacity(8);
        let first = interner.intern(db, "library/alpine").unwrap();
        assert_eq!(interner.intern(db, "library/alpine").unwrap(), first);

        // A fresh interner over the same database must agree.
        let reopened = RepoInterner::with_capacity(8);
        assert_eq!(reopened.lookup(db, "library/alpine").unwrap(), Some(first));
    }

    pub fn an_unknown_repo_is_not_silently_created_by_lookup(db: &dyn MetaEngine) {
        let interner = RepoInterner::with_capacity(8);
        assert_eq!(interner.lookup(db, "does/not-exist").unwrap(), None);
    }

    pub fn catalog_pages_by_name_not_by_id(db: &dyn MetaEngine) {
        let interner = RepoInterner::with_capacity(8);
        // Intern out of alphabetical order so id order and name order differ.
        for name in ["zebra", "alpine", "nginx"] {
            interner.intern(db, name).unwrap();
        }

        let page = db.scan(&keys::repos_by_name(), None, 10).unwrap();
        let names: Vec<_> = page
            .entries
            .iter()
            .map(|(k, _)| keys::parse_repo_name(k).unwrap())
            .collect();
        assert_eq!(names, ["alpine", "nginx", "zebra"]);
    }

    // --- keys-only scans -----------------------------------------------

    /// Page a range to exhaustion, recording the cursor handed back at every
    /// step as well as the keys, so a `scan_keys` that agreed on contents while
    /// disagreeing on where the next page starts would still be caught.
    fn page_scan(db: &dyn MetaEngine, prefix: &[u8], limit: usize) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let (mut keys, mut cursors) = (Vec::new(), Vec::new());
        let mut cursor = None;
        loop {
            let page = db.scan(prefix, cursor.as_deref(), limit).unwrap();
            assert!(page.entries.len() <= limit);
            keys.extend(page.entries.into_iter().map(|(k, _)| k));
            match page.next {
                Some(next) => {
                    cursors.push(next.clone());
                    cursor = Some(next);
                }
                None => return (keys, cursors),
            }
        }
    }

    fn page_scan_keys(
        db: &dyn MetaEngine,
        prefix: &[u8],
        limit: usize,
    ) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let (mut keys, mut cursors) = (Vec::new(), Vec::new());
        let mut cursor = None;
        loop {
            let page = db.scan_keys(prefix, cursor.as_deref(), limit).unwrap();
            assert!(page.keys.len() <= limit);
            keys.extend(page.keys);
            match page.next {
                Some(next) => {
                    cursors.push(next.clone());
                    cursor = Some(next);
                }
                None => return (keys, cursors),
            }
        }
    }

    /// Populate every range the R4 batch added, plus neighbours a scan must not
    /// stray into.
    fn populate_new_ranges(db: &dyn MetaEngine) {
        let (m, other) = (digest(1), digest(2));
        let mut batch = WriteBatch::new();
        for ts in [1_000u64, 2_000, 3_000, 4_000, 5_000] {
            batch.put(keys::tag_history(1, "latest", ts, &m), b"event".to_vec());
            batch.put(
                keys::manifest_tag_history(1, &m, ts, "latest"),
                b"e".to_vec(),
            );
            // Neighbours: `foobar` must not answer a scan of `foo`, and another
            // manifest must not answer this one's.
            batch.put(keys::tag_history(1, "foobar", ts, &m), b"event".to_vec());
            batch.put(
                keys::manifest_tag_history(1, &other, ts, "latest"),
                b"e".to_vec(),
            );
        }
        for day in 0..7u16 {
            for shard in [0u16, 1] {
                batch.put(keys::counter_manifest(1, &m, day, shard), b"c".to_vec());
                batch.put(keys::counter_tag(1, "latest", day, shard), b"c".to_vec());
                batch.put(keys::counter_repo(1, day, shard), b"c".to_vec());
                batch.put(keys::counter_manifest(1, &other, day, shard), b"c".to_vec());
                batch.put(keys::counter_tag(1, "latestish", day, shard), b"c".to_vec());
                batch.put(keys::counter_repo(2, day, shard), b"c".to_vec());
            }
        }
        db.apply(&batch).unwrap();
    }

    pub fn scan_keys_agrees_with_scan_over_every_new_range(db: &dyn MetaEngine) {
        populate_new_ranges(db);
        let m = digest(1);

        let ranges = [
            keys::tag_history_of(1, "latest"),
            keys::tag_history_of(1, "foo"), // empty: `foobar` must not answer it
            keys::manifest_tag_history_of(1, &m),
            keys::counters_of_manifest(1, &m),
            keys::counters_of_tag(1, "latest"),
            keys::counters_of_repo(1),
        ];
        for prefix in ranges {
            for limit in [1usize, 3, 5, 1_000] {
                let with_values = page_scan(db, &prefix, limit);
                let keys_only = page_scan_keys(db, &prefix, limit);
                assert_eq!(
                    with_values, keys_only,
                    "scan_keys diverged from scan on {prefix:?} at limit {limit}"
                );
            }
        }

        // The neighbours really were there to be strayed into.
        assert!(page_scan_keys(db, &keys::tag_history_of(1, "foo"), 10)
            .0
            .is_empty());
        assert_eq!(
            page_scan_keys(db, &keys::counters_of_repo(1), 10).0.len(),
            14
        );
    }

    /// The case `scan_keys` exists for: `R`, `G` and `P` hold no values at all,
    /// and purge walks millions of them.
    pub fn scan_keys_pages_a_valueless_edge_range_identically(db: &dyn MetaEngine) {
        let layer = digest(3);
        let mut batch = WriteBatch::new();
        for i in 0..25u8 {
            batch.set(keys::blob_ref(&layer, 1, &digest(100 + i)));
            batch.set(keys::manifest_tag(1, &digest(4), &format!("v{i:02}")));
            batch.set(keys::repo_blob(1, &digest(150 + i)));
        }
        // Edges a repo-scoped scan must not reach.
        batch.set(keys::blob_ref(&layer, 2, &digest(200)));
        db.apply(&batch).unwrap();

        for prefix in [
            keys::blob_refs(&layer),
            keys::blob_refs_in_repo(&layer, 1),
            keys::tags_of_manifest(1, &digest(4)),
            keys::blobs_in_repo(1),
        ] {
            for limit in [1usize, 7, 100] {
                assert_eq!(
                    page_scan(db, &prefix, limit),
                    page_scan_keys(db, &prefix, limit),
                    "{prefix:?} at limit {limit}"
                );
            }
        }
        assert_eq!(page_scan_keys(db, &keys::blob_refs(&layer), 10).0.len(), 26);
        assert_eq!(
            page_scan_keys(db, &keys::blob_refs_in_repo(&layer, 1), 10)
                .0
                .len(),
            25,
            "the repo-scoped seek is longer than its prefix group, so the \
             starts_with re-check is what stops it at the repo boundary"
        );
    }

    // --- analytics ------------------------------------------------------

    /// The whole contribution wall in one bounded scan, exactly as PLAN.md
    /// specifies it: 53 weeks is 371 day buckets, arriving in chronological
    /// order with no read-time aggregation and nothing to paginate.
    pub fn the_pull_count_wall_is_one_bounded_chronological_scan(db: &dyn MetaEngine) {
        let (repo, manifest) = (3u32, digest(42));
        let today: u16 = 20_000;

        let mut batch = WriteBatch::new();
        for back in 0..400u16 {
            let day = today - back;
            let bucket = CounterBucket {
                manifest_pulls: u64::from(day),
                blob_pulls: 1,
                bytes_out: 2,
            };
            batch.put(
                keys::counter_manifest(repo, &manifest, day, 0),
                postcard::to_allocvec(&bucket).unwrap(),
            );
        }
        // A sibling manifest and the repo scope, neither of which may leak into
        // this manifest's wall.
        batch.put(
            keys::counter_manifest(repo, &digest(43), today, 0),
            b"x".to_vec(),
        );
        batch.put(keys::counter_repo(repo, today, 0), b"x".to_vec());
        db.apply(&batch).unwrap();

        // `start_after = <cutoff day - 1>`, with the maximum shard so the whole
        // of that day is excluded rather than half of it.
        let cutoff = today - 370;
        let start_after = keys::counter_manifest(repo, &manifest, cutoff - 1, u16::MAX);
        let page = db
            .scan(
                &keys::counters_of_manifest(repo, &manifest),
                Some(&start_after),
                371,
            )
            .unwrap();

        assert_eq!(page.entries.len(), 371);
        assert!(
            page.next.is_none(),
            "53 weeks fits in one page; the wall must never need a cursor"
        );

        let expected: Vec<Vec<u8>> = (0..371u16)
            .map(|i| keys::counter_manifest(repo, &manifest, cutoff + i, 0))
            .collect();
        let got: Vec<Vec<u8>> = page.entries.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(got, expected, "buckets must arrive oldest-first, gap-free");

        let first: CounterBucket = postcard::from_bytes(&page.entries[0].1).unwrap();
        assert_eq!(first.manifest_pulls, u64::from(cutoff));
    }

    /// Complemented timestamps are what let a forward-only `scan` serve an
    /// endpoint the spec defines as descending.
    pub fn tag_history_pages_newest_first_without_a_reverse_iterator(db: &dyn MetaEngine) {
        let m = digest(7);
        let stamps = [1_000u64, 2_000, 3_000, 4_000];
        let mut batch = WriteBatch::new();
        for ts in stamps {
            batch.put(keys::tag_history(1, "latest", ts, &m), b"event".to_vec());
        }
        db.apply(&batch).unwrap();

        let (keys_seen, _) = page_scan_keys(db, &keys::tag_history_of(1, "latest"), 2);
        let expected: Vec<Vec<u8>> = stamps
            .iter()
            .rev()
            .map(|ts| keys::tag_history(1, "latest", *ts, &m))
            .collect();
        assert_eq!(keys_seen, expected);

        // `before = 3_000` is just a `start_after` seek - there is no cursor
        // token to invent. Note the boundary is inclusive: the cursor stops at
        // the complemented timestamp, and an event at exactly that instant
        // sorts after it because its digest follows in the key.
        let page = db
            .scan(
                &keys::tag_history_of(1, "latest"),
                Some(&keys::tag_history_before(1, "latest", 3_000)),
                10,
            )
            .unwrap();
        let seen: Vec<Vec<u8>> = page.entries.into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            seen,
            [3_000u64, 2_000, 1_000]
                .iter()
                .map(|ts| keys::tag_history(1, "latest", *ts, &m))
                .collect::<Vec<_>>()
        );
    }

    // --- schema version and migrations ----------------------------------

    pub fn a_fresh_store_is_stamped_with_the_current_schema_version(db: &dyn MetaEngine) {
        assert_eq!(version::read(db).unwrap(), None);
        assert_eq!(
            version::ensure(db, &Migrations::new()).unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(version::read(db).unwrap(), Some(SCHEMA_VERSION));
        // Opening again must be a no-op, not a re-stamp of a store that has
        // since been migrated by someone else.
        assert_eq!(
            version::ensure(db, &Migrations::new()).unwrap(),
            SCHEMA_VERSION
        );
    }

    pub fn a_store_from_the_future_is_refused(db: &dyn MetaEngine) {
        version::stamp(db, SCHEMA_VERSION + 1).unwrap();
        let err = version::ensure(db, &Migrations::new()).unwrap_err();
        assert!(err.to_string().contains("newer than this build"), "{err}");
    }

    pub fn an_unversioned_store_holding_data_is_refused(db: &dyn MetaEngine) {
        let mut batch = WriteBatch::new();
        batch.put(keys::tag(1, "latest"), digest(1).raw().to_vec());
        db.apply(&batch).unwrap();

        let err = version::ensure(db, &Migrations::new()).unwrap_err();
        assert!(err.to_string().contains("predates versioning"), "{err}");
        assert_eq!(
            version::read(db).unwrap(),
            None,
            "a refusal must not stamp the store it just declined to understand"
        );
    }

    pub fn a_store_behind_with_no_registered_migration_is_refused(db: &dyn MetaEngine) {
        version::stamp(db, 0).unwrap();
        let err = version::ensure(db, &Migrations::new()).unwrap_err();
        assert!(err.to_string().contains("no registered migration"), "{err}");
        assert_eq!(
            version::read(db).unwrap(),
            Some(0),
            "the marker must still say what the store actually is"
        );
    }

    pub fn a_registered_migration_runs_once_and_advances_the_version(db: &dyn MetaEngine) {
        version::stamp(db, 0).unwrap();

        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        let mut migrations = Migrations::new();
        migrations.register(SCHEMA_VERSION, move |engine| {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut batch = WriteBatch::new();
            batch.put(keys::tag(9, "migrated"), b"1".to_vec());
            engine.apply(&batch)
        });

        assert_eq!(version::ensure(db, &migrations).unwrap(), SCHEMA_VERSION);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(db.get(&keys::tag(9, "migrated")).unwrap().is_some());
        assert_eq!(version::read(db).unwrap(), Some(SCHEMA_VERSION));

        // The stamp is what stops the next open replaying it.
        assert_eq!(version::ensure(db, &migrations).unwrap(), SCHEMA_VERSION);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    pub fn a_migration_past_this_build_is_refused(db: &dyn MetaEngine) {
        version::stamp(db, 0).unwrap();
        let mut migrations = Migrations::new();
        migrations.register(SCHEMA_VERSION + 1, |_| Ok(()));
        let err = version::ensure(db, &migrations).unwrap_err();
        assert!(err.to_string().contains("beyond this build"), "{err}");
    }
}
