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

    reg.set_tag("demo/app", "latest", &first, 200).unwrap();
    let moved = reg.set_tag("demo/app", "latest", &second, 300).unwrap();
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
    reg.set_tag("demo/app", "latest", &first, 200).unwrap();

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
        .plan_set_tag("demo/app", "latest", &digest, 500)
        .unwrap();
    let history_key = keys::tag_history(repo, "latest", 500, &digest);
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
    reg.set_tag("demo/app", "latest", &digest, 500).unwrap();

    let displaced = reg.delete_tag("demo/app", "latest", 600).unwrap();
    assert_eq!(displaced, digest);

    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    let event: summ_core::TagEvent = postcard::from_bytes(
        &reg.engine()
            .get(&keys::tag_history(repo, "latest", 600, &digest))
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
    reg.set_tag("demo/app", "latest", &digest, 500).unwrap();
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();

    let by_tag = reg
        .engine()
        .get(&keys::tag_history(repo, "latest", 500, &digest))
        .unwrap();
    let by_digest = reg
        .engine()
        .get(&keys::manifest_tag_history(repo, &digest, 500, "latest"))
        .unwrap();
    assert_eq!(by_tag, by_digest);
    assert!(by_tag.is_some());
}

#[test]
fn tagging_a_manifest_that_is_not_there_fails() {
    let (_dir, reg) = fixture();
    let _ = manifest(&reg, "demo/app", "one", 100);
    let err = reg
        .set_tag("demo/app", "latest", &sha256(b"nothing"), 500)
        .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_UNKNOWN");
}

#[test]
fn an_ungrammatical_tag_is_refused_before_it_can_corrupt_a_history_key() {
    let (_dir, reg) = fixture();
    let digest = manifest(&reg, "demo/app", "one", 100);
    let err = reg
        .set_tag("demo/app", "bad tag", &digest, 500)
        .unwrap_err();
    assert_eq!(err.code(), "NAME_INVALID");
}

#[test]
fn tags_page_in_name_order_without_exceeding_the_limit_or_crossing_repos() {
    let (_dir, reg) = fixture();
    let here = manifest(&reg, "demo/app", "one", 100);
    let there = manifest(&reg, "other/app", "two", 100);
    for tag in ["a1", "a2", "a3", "a4", "a5"] {
        reg.set_tag("demo/app", tag, &here, 200).unwrap();
    }
    for tag in ["a1", "a9", "zz"] {
        reg.set_tag("other/app", tag, &there, 200).unwrap();
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
