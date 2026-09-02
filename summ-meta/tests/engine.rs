//! Behaviour the registry depends on at scale: bounded paging, cheap existence
//! checks, atomic batches, and an interner that stays correct once its cache
//! has evicted everything.

use summ_core::{keys, Digest};
use summ_meta::{MetaEngine, RedbEngine, RepoInterner, WriteBatch};

fn engine() -> (RedbEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = RedbEngine::open(dir.path().join("test.db")).unwrap();
    (db, dir)
}

fn digest(b: u8) -> Digest {
    Digest::Sha256([b; 32])
}

#[test]
fn scan_pages_in_order_and_stops_at_the_prefix() {
    let (db, _dir) = engine();
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

#[test]
fn a_scan_never_materialises_more_than_its_limit() {
    let (db, _dir) = engine();
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

#[test]
fn exists_prefix_answers_the_purge_question() {
    let (db, _dir) = engine();
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

#[test]
fn dropping_the_last_reference_makes_a_blob_purgeable() {
    let (db, _dir) = engine();
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

#[test]
fn delete_prefix_removes_only_its_own_range() {
    let (db, _dir) = engine();
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

#[test]
fn a_batch_is_atomic_across_every_key_a_push_touches() {
    let (db, _dir) = engine();
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

#[test]
fn interner_survives_full_cache_eviction() {
    let (db, _dir) = engine();
    // Cache one entry, then intern many, so nearly every lookup must fall
    // through to the engine - the ten-million-repo case.
    let interner = RepoInterner::with_capacity(1);

    let names: Vec<String> = (0..200).map(|i| format!("team/service-{i}")).collect();
    let ids: Vec<_> = names
        .iter()
        .map(|n| interner.intern(&db, n).unwrap())
        .collect();

    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "ids must be unique");

    for (name, id) in names.iter().zip(&ids) {
        assert_eq!(interner.lookup(&db, name).unwrap(), Some(*id));
        assert_eq!(interner.resolve(&db, *id).unwrap().as_ref(), Some(name));
    }
}

#[test]
fn interning_the_same_name_twice_is_stable() {
    let (db, _dir) = engine();
    let interner = RepoInterner::with_capacity(8);
    let first = interner.intern(&db, "library/alpine").unwrap();
    assert_eq!(interner.intern(&db, "library/alpine").unwrap(), first);

    // A fresh interner over the same database must agree.
    let reopened = RepoInterner::with_capacity(8);
    assert_eq!(reopened.lookup(&db, "library/alpine").unwrap(), Some(first));
}

#[test]
fn an_unknown_repo_is_not_silently_created_by_lookup() {
    let (db, _dir) = engine();
    let interner = RepoInterner::with_capacity(8);
    assert_eq!(interner.lookup(&db, "does/not-exist").unwrap(), None);
}

#[test]
fn catalog_pages_by_name_not_by_id() {
    let (db, _dir) = engine();
    let interner = RepoInterner::with_capacity(8);
    // Intern out of alphabetical order so id order and name order differ.
    for name in ["zebra", "alpine", "nginx"] {
        interner.intern(&db, name).unwrap();
    }

    let page = db.scan(&keys::repos_by_name(), None, 10).unwrap();
    let names: Vec<_> = page
        .entries
        .iter()
        .map(|(k, _)| keys::parse_repo_name(k).unwrap())
        .collect();
    assert_eq!(names, ["alpine", "nginx", "zebra"]);
}
