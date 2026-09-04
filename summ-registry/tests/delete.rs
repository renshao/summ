//! Delete: the cascade, and what must be left behind.

mod common;

use common::*;
use summ_core::keys;

#[test]
fn deleting_a_manifest_removes_every_edge_and_leaves_it_purgeable() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let layer = upload(&reg, "demo/app", "layer");
    let subject = {
        let c = upload(&reg, "demo/app", "subject-config");
        let b = Image::new(c).json();
        (
            put(&reg, "demo/app", &sha256(&b).to_string(), &b, 100),
            b.len() as u64,
        )
    };

    let body = Image::new(config).layer(layer).subject(subject).json();
    let digest = put(&reg, "demo/app", "v1", &body, 200);
    reg.set_tag("demo/app", "also", &digest, at(210)).unwrap();

    let deleted = reg.delete_manifest("demo/app", &digest, at(300)).unwrap();
    let mut tags = deleted.removed_tags.clone();
    tags.sort();
    assert_eq!(tags, ["also", "v1"]);

    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    let engine = reg.engine();
    assert!(
        engine
            .get(&keys::manifest(repo, &digest))
            .unwrap()
            .is_none(),
        "M"
    );
    assert!(
        engine
            .get(&keys::manifest_body(repo, &digest))
            .unwrap()
            .is_none(),
        "B"
    );
    for (blob, _) in [config, layer] {
        assert!(
            !engine
                .exists_prefix(&keys::blob_ref(&blob, repo, &digest))
                .unwrap(),
            "R edge for {blob}"
        );
    }
    assert!(
        !engine
            .exists_prefix(&keys::referrer(repo, &subject.0, &digest))
            .unwrap(),
        "F edge to the subject"
    );
    for tag in ["v1", "also"] {
        assert!(
            engine.get(&keys::tag(repo, tag)).unwrap().is_none(),
            "T {tag}"
        );
    }
    assert!(
        !engine
            .exists_prefix(&keys::tags_of_manifest(repo, &digest))
            .unwrap(),
        "G edges"
    );

    // Purge keys off "is it tagged?" and "is the blob still referenced?", and
    // both must now answer no.
    assert!(!engine.exists_prefix(&keys::blob_refs(&config.0)).unwrap());
    assert!(reg
        .get_manifest_by_digest("demo/app", &digest)
        .unwrap()
        .is_none());

    // Blob membership survives: `P` is the repo's blob set, not one manifest's.
    assert!(engine
        .get(&keys::repo_blob(repo, &config.0))
        .unwrap()
        .is_some());
}

#[test]
fn deleting_a_manifest_writes_a_history_event_for_every_tag_it_took_with_it() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let body = Image::new(config).json();
    let digest = put(&reg, "demo/app", "v1", &body, 200);

    reg.delete_manifest("demo/app", &digest, at(300)).unwrap();
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    let event: summ_core::TagEvent = postcard::from_bytes(
        &reg.engine()
            .get(&keys::tag_history(repo, "v1", at(300), &digest))
            .unwrap()
            .expect("a cascaded tag removal is still an audited event"),
    )
    .unwrap();
    assert_eq!(event.event, summ_core::TagEventKind::Deleted);
}

#[test]
fn deleting_an_index_child_clears_the_edge_in_both_directions() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let child_body = Image::new(config).json();
    let child = put(
        &reg,
        "demo/app",
        &sha256(&child_body).to_string(),
        &child_body,
        100,
    );
    let index_body = index_json(&[(child, child_body.len() as u64, "amd64")]);
    let index = put(&reg, "demo/app", "multi", &index_body, 200);

    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    reg.delete_manifest("demo/app", &child, at(300)).unwrap();
    assert!(
        !reg.engine()
            .exists_prefix(&keys::child_parent(repo, &child, &index))
            .unwrap(),
        "an S edge to a manifest that no longer exists is a dangling edge"
    );

    // Deleting the index next must also work: nothing refuses a delete because
    // something references it, and the suite deletes in either order.
    reg.delete_manifest("demo/app", &index, at(400)).unwrap();
}

#[test]
fn a_referrer_survives_the_deletion_of_its_subject() {
    let (_dir, reg) = fixture();
    let c = upload(&reg, "demo/app", "subject-config");
    let subject_body = Image::new(c).json();
    let subject = (
        put(
            &reg,
            "demo/app",
            &sha256(&subject_body).to_string(),
            &subject_body,
            100,
        ),
        subject_body.len() as u64,
    );
    let sig_body = Image::new(upload(&reg, "demo/app", "sig"))
        .subject(subject)
        .artifact_type("application/vnd.example.sig")
        .json();
    let sig = put(
        &reg,
        "demo/app",
        &sha256(&sig_body).to_string(),
        &sig_body,
        200,
    );

    reg.delete_manifest("demo/app", &subject.0, at(300))
        .unwrap();

    // The spec permits a subject to dangle, and the referrer is still a real
    // manifest, so its edge stays.
    let list = reg
        .referrers("demo/app", &subject.0, None, None, 10)
        .unwrap();
    assert_eq!(
        list.entries.iter().map(|e| e.digest).collect::<Vec<_>>(),
        vec![sig]
    );
}

#[test]
fn deleting_a_manifest_that_is_not_there_is_manifest_unknown() {
    let (_dir, reg) = fixture();
    let _ = upload(&reg, "demo/app", "config");
    let err = reg
        .delete_manifest("demo/app", &sha256(b"nope"), at(300))
        .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_UNKNOWN");

    let err = reg
        .delete_manifest("no/such", &sha256(b"nope"), at(300))
        .unwrap_err();
    assert_eq!(err.code(), "NAME_UNKNOWN");
}

#[test]
fn deleting_a_blob_reference_makes_the_blob_unservable_immediately() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let body = Image::new(config).json();
    let digest = put(&reg, "demo/app", "v1", &body, 200);

    // Out-of-order delete: the manifest is still there, so both halves of the
    // servability predicate have to be cleared.
    let out = reg.delete_blob_reference("demo/app", &config.0).unwrap();
    assert!(out.was_member);
    assert_eq!(out.references_removed, 1);
    assert!(!reg.blob_is_servable("demo/app", &config.0).unwrap());
    assert!(reg.servable_blob("demo/app", &config.0).unwrap().is_none());

    // The global blob record is purge's business, not this operation's.
    assert!(reg.engine().get(&keys::blob(&config.0)).unwrap().is_some());
    let _ = digest;
}

#[test]
fn deleting_a_blob_reference_that_does_not_exist_is_blob_unknown() {
    let (_dir, reg) = fixture();
    let _ = upload(&reg, "demo/app", "config");
    let err = reg
        .delete_blob_reference("demo/app", &sha256(b"elsewhere"))
        .unwrap_err();
    assert_eq!(err.code(), "BLOB_UNKNOWN");
}
