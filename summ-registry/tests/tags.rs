//! Tags, their reverse index, and the history events that ride with them.

mod common;

use common::*;
use summ_core::{keys, TagEventKind};

/// Push a distinct manifest and return its digest.
fn manifest(reg: &summ_registry::Registry, repo: &str, seed: &str, now: u64) -> summ_core::Digest {
    let config = upload(reg, repo, &format!("config-{seed}"));
    let body = Image::new(config).json();
    put(reg, repo, &sha256(&body).to_string(), &body, now)
}

#[test]
fn repointing_a_tag_leaves_no_stale_reverse_edge() {
    let (_dir, reg) = fixture();
    let first = manifest(&reg, "demo/app", "one", 100);
    let second = manifest(&reg, "demo/app", "two", 100);

    reg.set_tag("demo/app", "latest", &first, at(200)).unwrap();
    let moved = reg.set_tag("demo/app", "latest", &second, at(300)).unwrap();
    assert_eq!(moved.displaced, Some(first));

    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    assert!(
        !reg.engine()
            .exists_prefix(&keys::manifest_tag(repo, &first, "latest"))
            .unwrap(),
        "the old G edge must be gone"
    );
    assert!(reg
        .engine()
        .exists_prefix(&keys::manifest_tag(repo, &second, "latest"))
        .unwrap());

    // The now-untagged manifest must look reclaimable, which is the whole
    // reason the stale edge matters.
    assert_eq!(
        reg.untagged_manifests("demo/app", None, 10)
            .unwrap()
            .digests,
        vec![first]
    );
    assert!(reg
        .tags_of_manifest("demo/app", &first, None, 10)
        .unwrap()
        .tags
        .is_empty());
    assert_eq!(
        reg.tags_of_manifest("demo/app", &second, None, 10)
            .unwrap()
            .tags,
        vec!["latest"]
    );
}

#[test]
fn a_push_that_moves_a_tag_retracts_the_old_edge_too() {
    let (_dir, reg) = fixture();
    let first = manifest(&reg, "demo/app", "one", 100);
    reg.set_tag("demo/app", "latest", &first, at(200)).unwrap();

    let config = upload(&reg, "demo/app", "config-two");
    let body = Image::new(config)
        .layer(upload(&reg, "demo/app", "l"))
        .json();
    let second = put(&reg, "demo/app", "latest", &body, 300);

    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    assert!(!reg
        .engine()
        .exists_prefix(&keys::manifest_tag(repo, &first, "latest"))
        .unwrap());
    assert_eq!(
        reg.get_tag("demo/app", "latest").unwrap().unwrap().digest,
        second
    );
}

#[test]
fn tag_history_is_written_in_the_same_batch_as_the_tag() {
    let (_dir, reg) = fixture();
    let digest = manifest(&reg, "demo/app", "one", 100);
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();

    let planned = reg
        .plan_set_tag("demo/app", "latest", &digest, at(500))
        .unwrap();
    let history_key = keys::tag_history(repo, "latest", at(500), &digest);
    assert!(
        planned
            .batch
            .ops
            .iter()
            .any(|op| matches!(op, summ_meta::MetaOp::Put { key, .. } if *key == history_key)),
        "the H event belongs in the tag's own batch"
    );
    reg.engine().apply(&planned.batch).unwrap();

    let event: summ_core::TagEvent =
        postcard::from_bytes(&reg.engine().get(&history_key).unwrap().unwrap()).unwrap();
    assert_eq!(event.event, TagEventKind::Created);
}

#[test]
fn deleting_a_tag_records_the_digest_it_displaced() {
    let (_dir, reg) = fixture();
    let digest = manifest(&reg, "demo/app", "one", 100);
    reg.set_tag("demo/app", "latest", &digest, at(500)).unwrap();

    let displaced = reg.delete_tag("demo/app", "latest", at(600)).unwrap();
    assert_eq!(displaced, digest);

    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    let event: summ_core::TagEvent = postcard::from_bytes(
        &reg.engine()
            .get(&keys::tag_history(repo, "latest", at(600), &digest))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(event.event, TagEventKind::Deleted);

    // The manifest survives a tag delete and is reachable by digest.
    assert!(reg.get_tag("demo/app", "latest").unwrap().is_none());
    assert!(reg
        .get_manifest_by_digest("demo/app", &digest)
        .unwrap()
        .is_some());
}

#[test]
fn history_is_addressable_from_both_directions() {
    let (_dir, reg) = fixture();
    let digest = manifest(&reg, "demo/app", "one", 100);
    reg.set_tag("demo/app", "latest", &digest, at(500)).unwrap();
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();

    let by_tag = reg
        .engine()
        .get(&keys::tag_history(repo, "latest", at(500), &digest))
        .unwrap();
    let by_digest = reg
        .engine()
        .get(&keys::manifest_tag_history(
            repo,
            &digest,
            at(500),
            "latest",
        ))
        .unwrap();
    assert_eq!(by_tag, by_digest);
    assert!(by_tag.is_some());
}

#[test]
fn tagging_a_manifest_that_is_not_there_fails() {
    let (_dir, reg) = fixture();
    let _ = manifest(&reg, "demo/app", "one", 100);
    let err = reg
        .set_tag("demo/app", "latest", &sha256(b"nothing"), at(500))
        .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_UNKNOWN");
}

#[test]
fn an_ungrammatical_tag_is_refused_before_it_can_corrupt_a_history_key() {
    let (_dir, reg) = fixture();
    let digest = manifest(&reg, "demo/app", "one", 100);
    let err = reg
        .set_tag("demo/app", "bad tag", &digest, at(500))
        .unwrap_err();
    assert_eq!(err.code(), "NAME_INVALID");
}

#[test]
fn tags_page_in_name_order_without_exceeding_the_limit_or_crossing_repos() {
    let (_dir, reg) = fixture();
    let here = manifest(&reg, "demo/app", "one", 100);
    let there = manifest(&reg, "other/app", "two", 100);
    for tag in ["a1", "a2", "a3", "a4", "a5"] {
        reg.set_tag("demo/app", tag, &here, at(200)).unwrap();
    }
    for tag in ["a1", "a9", "zz"] {
        reg.set_tag("other/app", tag, &there, at(200)).unwrap();
    }

    let first = reg.list_tags("demo/app", None, 3).unwrap();
    assert_eq!(first.tags, ["a1", "a2", "a3"]);
    assert_eq!(first.next.as_deref(), Some("a3"));

    let second = reg.list_tags("demo/app", first.next.as_deref(), 3).unwrap();
    assert_eq!(second.tags, ["a4", "a5"]);
    assert_eq!(second.next, None, "the last page carries no cursor");

    // Exactly at the boundary: a limit equal to the number of tags must come
    // back full and with no cursor, because nothing follows.
    let exact = reg.list_tags("demo/app", None, 5).unwrap();
    assert_eq!(exact.tags.len(), 5);
    assert_eq!(exact.next, None);

    let other = reg.list_tags("other/app", None, 10).unwrap();
    assert_eq!(other.tags, ["a1", "a9", "zz"]);
}

#[test]
fn listing_a_repo_that_does_not_exist_is_name_unknown() {
    let (_dir, reg) = fixture();
    let err = reg.list_tags("nope/nope", None, 10).unwrap_err();
    assert_eq!(err.code(), "NAME_UNKNOWN");
}

// --- reading the history back ------------------------------------------

/// The sequence from the design note, end to end: a tag points somewhere, is
/// deleted, then points somewhere else. All three events must come back, in
/// order, with the delete naming the digest it displaced.
#[test]
fn a_create_delete_recreate_sequence_recovers_completely() {
    let (_dir, reg) = fixture();
    let a = manifest(&reg, "demo/app", "a", 100);
    let b = manifest(&reg, "demo/app", "b", 100);

    reg.set_tag("demo/app", "latest", &a, at(1_000)).unwrap();
    reg.delete_tag("demo/app", "latest", at(2_000)).unwrap();
    reg.set_tag("demo/app", "latest", &b, at(3_000)).unwrap();

    let page = reg
        .tag_history("demo/app", "latest", None, None, 10)
        .unwrap();
    let seen: Vec<_> = page
        .events
        .iter()
        .map(|e| (e.at.millis(), e.digest, e.event.event))
        .collect();
    assert_eq!(
        seen,
        vec![
            (3_000, b, TagEventKind::Created),
            // The delete names what it displaced - only `T` knew that, and
            // only at that moment.
            (2_000, a, TagEventKind::Deleted),
            (1_000, a, TagEventKind::Created),
        ]
    );
    assert_eq!(page.next, None, "an exhausted scan carries no cursor");
}

/// The reason the timestamps are milliseconds. At a second's resolution these
/// two events encode to the same key and the create is silently overwritten.
#[test]
fn two_events_inside_one_second_are_both_kept() {
    let (_dir, reg) = fixture();
    let a = manifest(&reg, "demo/app", "a", 100);

    reg.set_tag("demo/app", "latest", &a, at(1_700_000_000_001))
        .unwrap();
    reg.delete_tag("demo/app", "latest", at(1_700_000_000_002))
        .unwrap();

    let page = reg
        .tag_history("demo/app", "latest", None, None, 10)
        .unwrap();
    assert_eq!(page.events.len(), 2, "both events survive the same second");
    assert_eq!(page.events[0].event.event, TagEventKind::Deleted);
    assert_eq!(page.events[1].event.event, TagEventKind::Created);
}

#[test]
fn history_pages_with_the_cursor_it_hands_back() {
    let (_dir, reg) = fixture();
    let a = manifest(&reg, "demo/app", "a", 100);
    for ts in [1_000, 2_000, 3_000, 4_000, 5_000] {
        reg.set_tag("demo/app", "latest", &a, at(ts)).unwrap();
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = reg
            .tag_history(
                "demo/app",
                "latest",
                cursor
                    .as_ref()
                    .map(|c: &summ_registry::HistoryCursor| c.before),
                cursor
                    .as_ref()
                    .map(|c: &summ_registry::HistoryCursor| c.last.as_str()),
                2,
            )
            .unwrap();
        seen.extend(page.events.iter().map(|e| e.at.millis()));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, vec![5_000, 4_000, 3_000, 2_000, 1_000]);
}

/// `before` on its own is a filter, and it is strictly-before: an event at
/// exactly the boundary is excluded. That is the bug the cursor used to have.
#[test]
fn before_excludes_its_own_instant() {
    let (_dir, reg) = fixture();
    let a = manifest(&reg, "demo/app", "a", 100);
    for ts in [1_000, 2_000, 3_000] {
        reg.set_tag("demo/app", "latest", &a, at(ts)).unwrap();
    }

    let page = reg
        .tag_history("demo/app", "latest", Some(at(2_000)), None, 10)
        .unwrap();
    assert_eq!(
        page.events
            .iter()
            .map(|e| e.at.millis())
            .collect::<Vec<_>>(),
        vec![1_000]
    );
}

/// The `J` direction: what was this manifest ever called.
#[test]
fn digest_addressed_history_lists_every_tag_the_manifest_wore() {
    let (_dir, reg) = fixture();
    let a = manifest(&reg, "demo/app", "a", 100);
    let b = manifest(&reg, "demo/app", "b", 100);

    reg.set_tag("demo/app", "latest", &a, at(1_000)).unwrap();
    reg.set_tag("demo/app", "v1", &a, at(2_000)).unwrap();
    reg.set_tag("demo/app", "latest", &b, at(3_000)).unwrap();

    let page = reg
        .manifest_tag_history("demo/app", &a, None, None, 10)
        .unwrap();
    assert_eq!(
        page.events
            .iter()
            .map(|e| (e.at.millis(), e.tag.as_str()))
            .collect::<Vec<_>>(),
        vec![(2_000, "v1"), (1_000, "latest")]
    );

    // `b`'s history is its own, and does not sweep up `a`'s.
    let page = reg
        .manifest_tag_history("demo/app", &b, None, None, 10)
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].tag, "latest");
}

/// History outlives what it describes. This is the case the denormalised
/// descriptor exists for: after the manifest is gone, `M` cannot supply its
/// media type or size, so the event has to carry them.
#[test]
fn history_survives_the_manifest_it_describes() {
    let (_dir, reg) = fixture();
    let a = manifest(&reg, "demo/app", "a", 100);
    reg.set_tag("demo/app", "latest", &a, at(1_000)).unwrap();
    reg.delete_manifest("demo/app", &a, at(2_000)).unwrap();

    assert!(reg.get_manifest_record("demo/app", &a).unwrap().is_none());

    let page = reg
        .tag_history("demo/app", "latest", None, None, 10)
        .unwrap();
    assert_eq!(page.events.len(), 2, "the cascade wrote a Deleted event");
    for event in &page.events {
        assert!(
            !event.event.media_type.is_empty(),
            "a row must still render with no manifest to read"
        );
        assert!(event.event.size > 0);
    }
}

/// A tag that never existed and a repository that never existed are both an
/// empty page. There is nothing left to tell them apart from a tag whose
/// history has been read after deletion, which must answer.
#[test]
fn an_unknown_tag_or_repo_is_an_empty_page_not_an_error() {
    let (_dir, reg) = fixture();
    assert!(reg
        .tag_history("no/such", "latest", None, None, 10)
        .unwrap()
        .events
        .is_empty());

    let a = manifest(&reg, "demo/app", "a", 100);
    let _ = a;
    assert!(reg
        .tag_history("demo/app", "never", None, None, 10)
        .unwrap()
        .events
        .is_empty());
}

/// A scan of `foo` must not sweep up `foobar`. That is what the NUL separator
/// after the tag is for, and it is the kind of thing that only shows up with
/// real neighbours in the store.
#[test]
fn a_tag_prefix_does_not_reach_its_neighbour() {
    let (_dir, reg) = fixture();
    let a = manifest(&reg, "demo/app", "a", 100);
    reg.set_tag("demo/app", "foo", &a, at(1_000)).unwrap();
    reg.set_tag("demo/app", "foobar", &a, at(2_000)).unwrap();

    let page = reg.tag_history("demo/app", "foo", None, None, 10).unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].at.millis(), 1_000);
}
