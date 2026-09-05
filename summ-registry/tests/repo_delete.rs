//! Dropping a whole repository: the tombstone, the sweep, and the ordering
//! between them.

mod common;

use common::*;
use summ_core::{keys, RepoId};
use summ_registry::{CountDelta, CountSubject, Registry};

/// Every range keyed by a repo id, so a test can assert the store holds
/// nothing under one.
fn repo_ranges(id: RepoId) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("M", keys::manifests_in_repo(id)),
        ("B", keys::manifest_bodies_in_repo(id)),
        ("T", keys::tags_in_repo(id)),
        ("G", keys::manifest_tags_in_repo(id)),
        ("P", keys::blobs_in_repo(id)),
        ("S", keys::children_in_repo(id)),
        ("F", keys::referrers_in_repo(id)),
        ("H", keys::tag_history_in_repo(id)),
        ("J", keys::manifest_tag_history_in_repo(id)),
        (
            "A m",
            keys::counters_in_repo_scope(keys::SCOPE_MANIFEST, id),
        ),
        ("A t", keys::counters_in_repo_scope(keys::SCOPE_TAG, id)),
        ("A r", keys::counters_in_repo_scope(keys::SCOPE_REPO, id)),
    ]
}

/// A repository with something in every range this drop has to clear: an
/// index over two children, a tag that has moved, a referrer, and counters in
/// all three scopes.
fn populate(reg: &Registry, repo: &str) -> Vec<(summ_core::Digest, u64)> {
    let config = upload(reg, repo, &format!("{repo}/config"));
    let layer = upload(reg, repo, &format!("{repo}/layer"));

    let amd = Image::new(config).layer(layer).json();
    let amd_digest = put(reg, repo, "amd", &amd, 100);
    let arm = Image::new(config)
        .layer(upload(reg, repo, "arm-layer"))
        .json();
    let arm_digest = put(reg, repo, "arm", &arm, 110);

    let index = index_json(&[
        (amd_digest, amd.len() as u64, "amd64"),
        (arm_digest, arm.len() as u64, "arm64"),
    ]);
    let index_digest = put(reg, repo, "latest", &index, 120);

    // A referrer, so `F` is not empty.
    let sig = Image::new(upload(reg, repo, "sig-config"))
        .subject((index_digest, index.len() as u64))
        .artifact_type("application/vnd.example.signature")
        .json();
    put(reg, repo, &sha256(&sig).to_string(), &sig, 130);

    // Move the tag, so `H`/`J` hold more than one event apiece.
    reg.set_tag(repo, "latest", &amd_digest, at(140)).unwrap();

    reg.add_pull_counts(&[
        CountDelta {
            repo: repo.to_string(),
            subject: CountSubject::Repo,
            day: 20_000,
            hour: 3,
            manifest_pulls: 0,
            blob_pulls: 2,
            bytes_out: 4096,
        },
        CountDelta {
            repo: repo.to_string(),
            subject: CountSubject::Tag("latest".into()),
            day: 20_000,
            hour: 3,
            manifest_pulls: 1,
            blob_pulls: 0,
            bytes_out: 0,
        },
        CountDelta {
            repo: repo.to_string(),
            subject: CountSubject::Manifest(index_digest),
            day: 20_000,
            hour: 3,
            manifest_pulls: 1,
            blob_pulls: 0,
            bytes_out: 0,
        },
    ])
    .unwrap();

    vec![config, layer]
}

/// Sweep to completion, the way the server's background task does.
fn sweep(reg: &Registry, id: RepoId, step: usize) -> usize {
    let mut cursor: Option<Vec<u8>> = None;
    let mut manifests = 0;
    loop {
        let out = reg.sweep_repo_refs(id, cursor.as_deref(), step).unwrap();
        manifests += out.manifests;
        match out.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    reg.finish_repo_sweep(id).unwrap();
    manifests
}

#[test]
fn the_tombstone_releases_the_name_and_nothing_else() {
    let (_dir, reg) = fixture();
    populate(&reg, "demo/app");
    let id = reg.lookup_repo("demo/app").unwrap().unwrap();

    // A real instant, so the seconds the record stores are worth asserting:
    // the timestamp arrives in milliseconds like every other one on a write
    // path, and `DeadRepo` is a stored record, so it holds seconds.
    let out = reg
        .delete_repository("demo/app", at(1_700_000_000_123))
        .unwrap();
    assert_eq!(out.id, id);
    assert_eq!(out.name, "demo/app");

    // Gone from every listing, which is the whole of what a client observes.
    assert_eq!(reg.lookup_repo("demo/app").unwrap(), None);
    assert!(reg.list_repos(None, 10).unwrap().repos.is_empty());

    // And still entirely present underneath, which is the point of splitting
    // the operation in two.
    for (label, prefix) in repo_ranges(id) {
        assert!(
            reg.engine().exists_prefix(&prefix).unwrap(),
            "{label} should survive the tombstone"
        );
    }
    let dead = reg.dead_repos(None, 10).unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].id, id);
    assert_eq!(dead[0].name, "demo/app");
    assert_eq!(dead[0].dropped_at, 1_700_000_000, "seconds, not millis");
}

#[test]
fn a_sweep_empties_every_range_keyed_by_the_id() {
    let (_dir, reg) = fixture();
    populate(&reg, "demo/app");
    let id = reg.lookup_repo("demo/app").unwrap().unwrap();

    reg.delete_repository("demo/app", at(200)).unwrap();
    assert_eq!(sweep(&reg, id, 500), 4, "3 images and an index");

    for (label, prefix) in repo_ranges(id) {
        assert!(
            !reg.engine().exists_prefix(&prefix).unwrap(),
            "{label} survived the sweep"
        );
    }
    assert!(reg.dead_repos(None, 10).unwrap().is_empty(), "D");
}

/// The failure this whole design is arranged around: `R` is keyed by the blob,
/// so a repository's edges are not under any prefix of its own and a sweep
/// that forgets them leaves a blob nothing can ever reclaim.
#[test]
fn a_sweep_retracts_the_blob_reference_edges_that_no_prefix_covers() {
    let (_dir, reg) = fixture();
    let blobs = populate(&reg, "demo/app");
    let id = reg.lookup_repo("demo/app").unwrap().unwrap();

    for (blob, _) in &blobs {
        assert!(
            reg.engine().exists_prefix(&keys::blob_refs(blob)).unwrap(),
            "referenced before the delete"
        );
    }

    reg.delete_repository("demo/app", at(200)).unwrap();
    sweep(&reg, id, 500);

    for (blob, _) in &blobs {
        assert!(
            !reg.engine().exists_prefix(&keys::blob_refs(blob)).unwrap(),
            "an `R` edge outlived the repository that owned it, so purge will \
             never reclaim {blob}"
        );
    }
}

/// The same, with the layer shared: what goes is this repository's edges, not
/// the blob.
#[test]
fn a_sweep_leaves_another_repositorys_edges_to_the_same_layer() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "shared-config");
    let layer = upload(&reg, "demo/app", "shared-layer");
    let body = Image::new(config).layer(layer).json();
    put(&reg, "demo/app", "v1", &body, 100);

    // The same content in a second repository, which must be untouched.
    reg.commit_blob("demo/other", &config.0, config.1, at(100))
        .unwrap();
    reg.commit_blob("demo/other", &layer.0, layer.1, at(100))
        .unwrap();
    put(&reg, "demo/other", "v1", &body, 110);

    let id = reg.lookup_repo("demo/app").unwrap().unwrap();
    let other = reg.lookup_repo("demo/other").unwrap().unwrap();

    reg.delete_repository("demo/app", at(200)).unwrap();
    sweep(&reg, id, 500);

    for (blob, _) in [config, layer] {
        assert!(
            !reg.engine()
                .exists_prefix(&keys::blob_refs_in_repo(&blob, id))
                .unwrap(),
            "the deleted repository's edge"
        );
        assert!(
            reg.engine()
                .exists_prefix(&keys::blob_refs_in_repo(&blob, other))
                .unwrap(),
            "the surviving repository's edge"
        );
        assert!(
            reg.blob_is_servable("demo/other", &blob).unwrap(),
            "a shared layer stays servable where it is still referenced"
        );
        // Global blob metadata is purge's business, not this operation's.
        assert!(reg.engine().get(&keys::blob(&blob)).unwrap().is_some(), "L");
    }
}

/// Stepping is what keeps the batch bounded at ten million manifests, so a
/// one-manifest step has to reach the same end state as a single pass.
#[test]
fn sweeping_one_manifest_at_a_time_reaches_the_same_place() {
    let (_dir, reg) = fixture();
    let blobs = populate(&reg, "demo/app");
    let id = reg.lookup_repo("demo/app").unwrap().unwrap();

    reg.delete_repository("demo/app", at(200)).unwrap();
    assert_eq!(sweep(&reg, id, 1), 4);

    for (blob, _) in &blobs {
        assert!(!reg.engine().exists_prefix(&keys::blob_refs(blob)).unwrap());
    }
    for (label, prefix) in repo_ranges(id) {
        assert!(!reg.engine().exists_prefix(&prefix).unwrap(), "{label}");
    }
}

/// A sweep is interrupted by anything from a crash to a rollback, and the `D`
/// record is what makes starting over correct rather than merely possible.
#[test]
fn a_half_finished_sweep_is_resumed_by_starting_it_again() {
    let (_dir, reg) = fixture();
    let blobs = populate(&reg, "demo/app");
    let id = reg.lookup_repo("demo/app").unwrap().unwrap();
    reg.delete_repository("demo/app", at(200)).unwrap();

    // One step, then nothing - the process died here.
    let first = reg.sweep_repo_refs(id, None, 1).unwrap();
    assert_eq!(first.manifests, 1);
    assert!(first.next.is_some());

    // A fresh pass finds the work still listed and redoes it from the top.
    let dead = reg.dead_repos(None, 10).unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(sweep(&reg, dead[0].id, 500), 4, "re-visits every manifest");

    for (blob, _) in &blobs {
        assert!(!reg.engine().exists_prefix(&keys::blob_refs(blob)).unwrap());
    }
    assert!(reg.dead_repos(None, 10).unwrap().is_empty());
}

/// The ordering rule, stated as a test: `M` is the only thing that knows which
/// `R` edges exist, so dropping it first strands them.
#[test]
fn finishing_before_sweeping_strands_the_edges_it_can_no_longer_find() {
    let (_dir, reg) = fixture();
    let blobs = populate(&reg, "demo/app");
    let id = reg.lookup_repo("demo/app").unwrap().unwrap();

    reg.delete_repository("demo/app", at(200)).unwrap();
    reg.finish_repo_sweep(id).unwrap();

    // Nothing is left to walk, so a sweep now finds nothing to retract...
    let step = reg.sweep_repo_refs(id, None, 500).unwrap();
    assert_eq!(step.manifests, 0);
    // ...and the edges are still there, unreachable, keeping a dead
    // repository's blobs alive against every purge that will ever run.
    assert!(
        reg.engine()
            .exists_prefix(&keys::blob_refs(&blobs[0].0))
            .unwrap(),
        "this is what the sweep-then-finish order exists to prevent"
    );
}

/// A name may be pushed again immediately. It must not land in the keyspace
/// that is still being swept, and the interner cache is the only thing that
/// could make it.
#[test]
fn recreating_a_repository_mints_a_new_id_rather_than_reusing_the_dead_one() {
    let (_dir, reg) = fixture();
    populate(&reg, "demo/app");
    let old = reg.lookup_repo("demo/app").unwrap().unwrap();
    reg.delete_repository("demo/app", at(200)).unwrap();

    // Pushed back before the sweep has run at all: the worst ordering there is.
    let config = upload(&reg, "demo/app", "fresh-config");
    let body = Image::new(config).json();
    let fresh = put(&reg, "demo/app", "v1", &body, 300);
    let new = reg.lookup_repo("demo/app").unwrap().unwrap();
    assert_ne!(new, old, "an id is never reused");

    sweep(&reg, old, 500);

    // The sweep of the dead id left the live repository alone.
    assert_eq!(reg.lookup_repo("demo/app").unwrap(), Some(new));
    assert_eq!(reg.list_repos(None, 10).unwrap().repos, ["demo/app"]);
    assert!(reg
        .get_manifest_by_digest("demo/app", &fresh)
        .unwrap()
        .is_some());
    assert!(reg.blob_is_servable("demo/app", &config.0).unwrap());
    assert!(reg
        .engine()
        .exists_prefix(&keys::blob_refs_in_repo(&config.0, new))
        .unwrap());
}

#[test]
fn deleting_a_repository_that_does_not_exist_is_name_unknown() {
    let (_dir, reg) = fixture();
    populate(&reg, "demo/app");
    let err = reg.delete_repository("no/such", at(200)).unwrap_err();
    assert_eq!(err.code(), "NAME_UNKNOWN");

    // And a second delete of the same name, for the same reason.
    reg.delete_repository("demo/app", at(200)).unwrap();
    let err = reg.delete_repository("demo/app", at(210)).unwrap_err();
    assert_eq!(err.code(), "NAME_UNKNOWN");
}

#[test]
fn several_outstanding_sweeps_are_listed_together_and_run_independently() {
    let (_dir, reg) = fixture();
    populate(&reg, "demo/one");
    populate(&reg, "demo/two");
    let one = reg.lookup_repo("demo/one").unwrap().unwrap();
    let two = reg.lookup_repo("demo/two").unwrap().unwrap();

    reg.delete_repository("demo/one", at(200)).unwrap();
    reg.delete_repository("demo/two", at(210)).unwrap();
    let dead = reg.dead_repos(None, 10).unwrap();
    assert_eq!(
        dead.iter().map(|d| d.id).collect::<Vec<_>>(),
        [one, two],
        "id order"
    );

    sweep(&reg, one, 500);
    assert_eq!(
        reg.dead_repos(None, 10).unwrap().len(),
        1,
        "the other is still outstanding"
    );
    assert!(reg
        .engine()
        .exists_prefix(&keys::manifests_in_repo(two))
        .unwrap());

    sweep(&reg, two, 500);
    assert!(reg.dead_repos(None, 10).unwrap().is_empty());
}
