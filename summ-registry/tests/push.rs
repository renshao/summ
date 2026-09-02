//! Manifest push: what lands, and what is refused.

mod common;

use common::*;
use summ_core::keys;
use summ_registry::{ManifestPut, Reference};

#[test]
fn a_tagged_push_lands_every_key_range_at_once() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let layer = upload(&reg, "demo/app", "layer-one");
    let body = Image::new(config).layer(layer).json();

    let digest = put(&reg, "demo/app", "v1", &body, 1_700);
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    let engine = reg.engine();

    assert!(
        engine
            .get(&keys::manifest(repo, &digest))
            .unwrap()
            .is_some(),
        "M"
    );
    assert!(
        engine
            .get(&keys::manifest_body(repo, &digest))
            .unwrap()
            .is_some(),
        "B"
    );
    for (blob, _) in [config, layer] {
        assert!(engine.get(&keys::blob(&blob)).unwrap().is_some(), "L");
        assert!(
            engine.get(&keys::repo_blob(repo, &blob)).unwrap().is_some(),
            "P"
        );
        assert!(
            engine
                .exists_prefix(&keys::blob_ref(&blob, repo, &digest))
                .unwrap(),
            "R"
        );
    }
    assert!(engine.get(&keys::tag(repo, "v1")).unwrap().is_some(), "T");
    assert!(
        engine
            .exists_prefix(&keys::manifest_tag(repo, &digest, "v1"))
            .unwrap(),
        "G"
    );
    assert!(
        engine
            .get(&keys::tag_history(repo, "v1", 1_700, &digest))
            .unwrap()
            .is_some(),
        "H"
    );
    assert!(
        engine
            .get(&keys::manifest_tag_history(repo, &digest, 1_700, "v1"))
            .unwrap()
            .is_some(),
        "J"
    );
}

#[test]
fn a_manifest_referencing_a_missing_blob_is_rejected_and_writes_nothing() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let ghost = (sha256(b"never uploaded"), 42);
    let body = Image::new(config).layer(ghost).json();

    let err = push(&reg, "demo/app", "v1", &body, 1_700).unwrap_err();
    assert_eq!(err.code(), "MANIFEST_BLOB_UNKNOWN");

    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    assert!(reg.engine().get(&keys::tag(repo, "v1")).unwrap().is_none());
    assert!(reg
        .engine()
        .get(&keys::manifest(repo, &sha256(&body)))
        .unwrap()
        .is_none());
    // The would-be layer must not have acquired metadata of its own either.
    assert!(reg.engine().get(&keys::blob(&ghost.0)).unwrap().is_none());
}

#[test]
fn a_blob_uploaded_to_another_repo_does_not_count_as_present() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let elsewhere = upload(&reg, "other/app", "secret layer");
    let body = Image::new(config).layer(elsewhere).json();

    let err = push(&reg, "demo/app", "v1", &body, 1_700).unwrap_err();
    assert_eq!(err.code(), "MANIFEST_BLOB_UNKNOWN");
}

#[test]
fn validation_can_be_turned_off_and_the_declared_size_is_then_believed() {
    let (_dir, reg) = fixture_with(summ_registry::RegistryOptions {
        validate_references: false,
        ..Default::default()
    });
    let config = upload(&reg, "demo/app", "config");
    let ghost = (sha256(b"never uploaded"), 4_096);
    let body = Image::new(config).layer(ghost).json();

    let digest = put(&reg, "demo/app", "v1", &body, 1_700);
    let record = reg
        .get_manifest_record("demo/app", &digest)
        .unwrap()
        .unwrap();
    assert_eq!(record.total_layer_size, config.1 + 4_096);
    assert!(reg.blob_is_servable("demo/app", &ghost.0).unwrap());
}

#[test]
fn the_body_round_trips_byte_exact_through_zstd() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    // Deliberately not what serde_json would emit: odd spacing, key order and a
    // trailing newline. The digest is over exactly these bytes.
    let body = format!(
        "{{ \"schemaVersion\":2,\n  \"mediaType\": \"{OCI_MANIFEST}\",\n  \"layers\" : [],\n  \"config\":{}\n}}\n",
        descriptor(OCI_CONFIG, &config.0, config.1)
    )
    .into_bytes();

    let digest = put(&reg, "demo/app", "exact", &body, 1_700);
    assert_eq!(digest, sha256(&body));

    let stored = reg
        .get_manifest_by_tag("demo/app", "exact")
        .unwrap()
        .unwrap();
    assert_eq!(stored.body, body, "stored bytes must be identical");
    assert_eq!(sha256(&stored.body), digest);
}

#[test]
fn a_push_by_digest_must_match_the_bytes() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let body = Image::new(config).json();
    let wrong = sha256(b"something else").to_string();

    let err = push(&reg, "demo/app", &wrong, &body, 1_700).unwrap_err();
    assert_eq!(err.code(), "DIGEST_INVALID");

    let right = sha256(&body).to_string();
    assert_eq!(put(&reg, "demo/app", &right, &body, 1_700), sha256(&body));
}

#[test]
fn a_reference_offered_as_a_digest_never_falls_back_to_being_a_tag() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let body = Image::new(config).json();
    let err = push(&reg, "demo/app", "sha256:baddigeststring", &body, 1).unwrap_err();
    assert_eq!(err.code(), "DIGEST_INVALID");
}

#[test]
fn an_index_push_writes_a_child_edge_per_entry() {
    let (_dir, reg) = fixture();
    let mut children = Vec::new();
    for arch in ["amd64", "arm64"] {
        let config = upload(&reg, "demo/app", &format!("config-{arch}"));
        let body = Image::new(config).json();
        let digest = put(&reg, "demo/app", &sha256(&body).to_string(), &body, 1_700);
        children.push((digest, body.len() as u64, arch));
    }

    let body = index_json(&children);
    let index = put(&reg, "demo/app", "multi", &body, 1_800);
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();

    for (child, _, _) in &children {
        assert!(
            reg.engine()
                .exists_prefix(&keys::child_parent(repo, child, &index))
                .unwrap(),
            "S edge for {child}"
        );
        let parents = reg
            .parents_of_manifest("demo/app", child, None, 10)
            .unwrap();
        assert_eq!(parents.digests, vec![index]);
    }

    let record = reg
        .get_manifest_record("demo/app", &index)
        .unwrap()
        .unwrap();
    assert_eq!(record.children.len(), 2);
    assert_eq!(record.children[0].platform.as_ref().unwrap().arch, "amd64");
    // An index owns no blobs of its own, so it writes no `R` edges and totals
    // zero. Its real size comes from walking its children.
    assert!(record.layers.is_empty());
    assert_eq!(record.total_layer_size, 0);
}

#[test]
fn an_index_naming_a_child_that_is_not_there_is_rejected() {
    let (_dir, reg) = fixture();
    let ghost = (sha256(b"no such manifest"), 100, "amd64");
    let body = index_json(&[ghost]);
    let err = push(&reg, "demo/app", "multi", &body, 1_700).unwrap_err();
    assert_eq!(err.code(), "MANIFEST_BLOB_UNKNOWN");
}

#[test]
fn a_malformed_manifest_is_manifest_invalid() {
    let (_dir, reg) = fixture();
    for body in [&b"not json at all"[..], b"{}", b"[]"] {
        let err = push(&reg, "demo/app", "v1", body, 1_700).unwrap_err();
        assert_eq!(err.code(), "MANIFEST_INVALID", "{err}");
    }
}

#[test]
fn a_manifest_larger_than_the_limit_is_refused_before_it_is_parsed() {
    let (_dir, reg) = fixture_with(summ_registry::RegistryOptions {
        max_manifest_bytes: 64,
        ..Default::default()
    });
    let config = upload(&reg, "demo/app", "config");
    let body = Image::new(config).json();
    assert!(body.len() > 64);
    let err = push(&reg, "demo/app", "v1", &body, 1_700).unwrap_err();
    assert_eq!(err.code(), "MANIFEST_INVALID");
}

#[test]
fn a_plan_writes_nothing_until_it_is_applied() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let body = Image::new(config).json();
    let reference: Reference = "v1".parse().unwrap();

    let planned = reg
        .plan_manifest_put(&ManifestPut {
            repo: "demo/app",
            reference: &reference,
            body: &body,
            content_type: Some(OCI_MANIFEST),
            now: 1_700,
        })
        .unwrap();

    assert!(reg.get_manifest_by_tag("demo/app", "v1").unwrap().is_none());
    reg.engine().apply(&planned.batch).unwrap();
    assert!(reg.get_manifest_by_tag("demo/app", "v1").unwrap().is_some());
}
