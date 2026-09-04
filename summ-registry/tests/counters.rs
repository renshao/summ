//! The `A` range: what a flush writes, and what a day window reads back.

mod common;

use common::*;
use summ_registry::{CountDelta, CountSubject};

fn delta(repo: &str, subject: CountSubject, day: u16, hour: usize) -> CountDelta {
    CountDelta {
        repo: repo.to_string(),
        subject,
        day,
        hour,
        manifest_pulls: 1,
        blob_pulls: 0,
        bytes_out: 0,
    }
}

/// A repo has to exist before its counters can be keyed: the key holds an
/// interned id, and a read must never mint one.
fn seed_repo(reg: &summ_registry::Registry, repo: &str) -> summ_core::Digest {
    let config = upload(reg, repo, "config");
    let body = Image::new(config).json();
    put(reg, repo, "latest", &body, 1_000)
}

#[test]
fn a_flush_folds_into_the_hour_it_names() {
    let (_dir, reg) = fixture();
    let digest = seed_repo(&reg, "demo");

    reg.add_pull_counts(&[
        delta("demo", CountSubject::Manifest(digest), 20_000, 3),
        delta("demo", CountSubject::Manifest(digest), 20_000, 3),
        delta("demo", CountSubject::Manifest(digest), 20_000, 17),
    ])
    .unwrap();

    let days = reg.manifest_counts("demo", &digest, 20_000, 1).unwrap();
    assert_eq!(days.len(), 1);
    assert_eq!(days[0].day, 20_000);
    assert_eq!(days[0].bucket.manifest_pulls[3], 2);
    assert_eq!(days[0].bucket.manifest_pulls[17], 1);
    assert_eq!(days[0].bucket.manifest_pulls[0], 0);
    // The day figure is the sum of the hours and is not stored anywhere.
    assert_eq!(days[0].bucket.manifest_pulls_total(), 3);
}

/// The property the whole scheme rests on: a later flush adds to what is
/// already there rather than replacing it, so a restart between flushes costs
/// one interval and not the day.
#[test]
fn a_second_flush_accumulates_onto_the_first() {
    let (_dir, reg) = fixture();
    let digest = seed_repo(&reg, "demo");

    for _ in 0..3 {
        reg.add_pull_counts(&[delta("demo", CountSubject::Manifest(digest), 20_000, 5)])
            .unwrap();
    }

    let days = reg.manifest_counts("demo", &digest, 20_000, 1).unwrap();
    assert_eq!(days[0].bucket.manifest_pulls[5], 3);
}

/// One bucket touched a thousand times in one flush is one lookup and one
/// `Put`, and a batch may never carry two writes to one key.
#[test]
fn a_flush_writes_one_key_per_bucket_however_many_deltas_it_holds() {
    let (_dir, reg) = fixture();
    let digest = seed_repo(&reg, "demo");

    let deltas: Vec<_> = (0..50)
        .map(|_| delta("demo", CountSubject::Manifest(digest), 20_000, 9))
        .collect();
    assert_eq!(reg.add_pull_counts(&deltas).unwrap(), 1);

    let days = reg.manifest_counts("demo", &digest, 20_000, 1).unwrap();
    assert_eq!(days[0].bucket.manifest_pulls[9], 50);
}

#[test]
fn the_three_scopes_are_separate_walls() {
    let (_dir, reg) = fixture();
    let digest = seed_repo(&reg, "demo");

    reg.add_pull_counts(&[
        delta("demo", CountSubject::Manifest(digest), 20_000, 1),
        delta("demo", CountSubject::Tag("latest".into()), 20_000, 1),
        CountDelta {
            blob_pulls: 2,
            bytes_out: 4096,
            manifest_pulls: 1,
            ..delta("demo", CountSubject::Repo, 20_000, 1)
        },
    ])
    .unwrap();

    assert_eq!(
        reg.manifest_counts("demo", &digest, 20_000, 1).unwrap()[0]
            .bucket
            .manifest_pulls_total(),
        1
    );
    assert_eq!(
        reg.tag_counts("demo", "latest", 20_000, 1).unwrap()[0]
            .bucket
            .manifest_pulls_total(),
        1
    );
    let repo = reg.repo_counts("demo", 20_000, 1).unwrap();
    assert_eq!(repo[0].bucket.blob_pulls_total(), 2);
    assert_eq!(repo[0].bucket.bytes_out_total(), 4096);

    // A tag's counters cannot be reached from a prefix-neighbour's scan.
    reg.add_pull_counts(&[delta(
        "demo",
        CountSubject::Tag("latest-arm".into()),
        20_000,
        1,
    )])
    .unwrap();
    assert_eq!(
        reg.tag_counts("demo", "latest", 20_000, 1).unwrap()[0]
            .bucket
            .manifest_pulls_total(),
        1
    );
}

/// The window is the bound, and it is inclusive at both ends. Getting the start
/// wrong is invisible in a wall - the first column just reads zero.
#[test]
fn a_day_window_includes_both_of_its_ends_and_nothing_outside_them() {
    let (_dir, reg) = fixture();
    let digest = seed_repo(&reg, "demo");

    for day in [19_998_u16, 19_999, 20_000, 20_001, 20_002] {
        reg.add_pull_counts(&[delta("demo", CountSubject::Manifest(digest), day, 0)])
            .unwrap();
    }

    let days: Vec<u16> = reg
        .manifest_counts("demo", &digest, 19_999, 3)
        .unwrap()
        .iter()
        .map(|d| d.day)
        .collect();
    assert_eq!(days, vec![19_999, 20_000, 20_001]);

    // A single-day window is the degenerate case and still works.
    let one = reg.manifest_counts("demo", &digest, 20_002, 1).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].day, 20_002);

    assert!(reg
        .manifest_counts("demo", &digest, 20_000, 0)
        .unwrap()
        .is_empty());
}

/// Only days with traffic come back; zero-filling is the caller's job.
#[test]
fn a_window_returns_only_the_days_that_have_counts() {
    let (_dir, reg) = fixture();
    let digest = seed_repo(&reg, "demo");
    reg.add_pull_counts(&[delta("demo", CountSubject::Manifest(digest), 20_005, 0)])
        .unwrap();

    let days = reg.manifest_counts("demo", &digest, 20_000, 30).unwrap();
    assert_eq!(days.len(), 1);
    assert_eq!(days[0].day, 20_005);
}

/// Counts outlive what they describe, so an unknown repository is an empty
/// series rather than an error - the same rule tag history follows.
#[test]
fn an_unknown_repository_is_an_empty_series() {
    let (_dir, reg) = fixture();
    let digest = sha256(b"nothing");
    assert!(reg
        .manifest_counts("ghost", &digest, 20_000, 30)
        .unwrap()
        .is_empty());
    assert!(reg.repo_counts("ghost", 20_000, 30).unwrap().is_empty());
    assert!(reg
        .tag_counts("ghost", "latest", 20_000, 30)
        .unwrap()
        .is_empty());
}

/// A pull counted against a repo that has since been deleted must not mint an
/// id for it: that would resurrect the name in the catalog on the strength of a
/// counter.
#[test]
fn a_flush_never_interns_a_repository() {
    let (_dir, reg) = fixture();
    let digest = sha256(b"nothing");

    assert_eq!(
        reg.add_pull_counts(&[delta("ghost", CountSubject::Manifest(digest), 20_000, 0)])
            .unwrap(),
        0
    );
    let repos = reg.list_repos(None, 10).unwrap();
    assert!(repos.repos.is_empty(), "{:?}", repos.repos);
}
