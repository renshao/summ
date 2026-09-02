//! Referrer edges: from `subject`, from a legacy cosign tag, and the query.

mod common;

use common::*;
use summ_core::{keys, Digest};
use summ_registry::Registry;

const SIGNATURE: &str = "application/vnd.dev.cosign.artifact.sig.v1+json";
const SBOM: &str = "application/vnd.example.sbom.v1+json";

fn subject_manifest(reg: &Registry, repo: &str) -> (Digest, u64) {
    let config = upload(reg, repo, "subject-config");
    let body = Image::new(config).json();
    let digest = put(reg, repo, &sha256(&body).to_string(), &body, 100);
    (digest, body.len() as u64)
}

/// Push a referrer of `subject` with the given artifactType.
fn referrer(reg: &Registry, repo: &str, subject: (Digest, u64), artifact_type: &str) -> Digest {
    let config = upload(reg, repo, &format!("cfg-{artifact_type}"));
    let body = Image::new(config)
        .subject(subject)
        .artifact_type(artifact_type)
        .annotation("org.example.author", "ren")
        .json();
    put(reg, repo, &sha256(&body).to_string(), &body, 200)
}

#[test]
fn a_subject_writes_an_edge_carrying_the_referrers_descriptor() {
    let (_dir, reg) = fixture();
    let subject = subject_manifest(&reg, "demo/app");
    let sig = referrer(&reg, "demo/app", subject, SIGNATURE);

    let list = reg
        .referrers("demo/app", &subject.0, None, None, 10)
        .unwrap();
    assert_eq!(list.entries.len(), 1);
    let entry = &list.entries[0];
    assert_eq!(entry.digest, sig);
    assert_eq!(entry.record.artifact_type.as_deref(), Some(SIGNATURE));
    assert_eq!(entry.record.media_type, OCI_MANIFEST);
    assert_eq!(
        entry
            .record
            .annotations
            .get("org.example.author")
            .map(String::as_str),
        Some("ren")
    );
    assert!(entry.record.size > 0);
    assert!(!list.filter_applied);
}

#[test]
fn the_artifact_type_filter_is_applied_during_the_scan() {
    let (_dir, reg) = fixture();
    let subject = subject_manifest(&reg, "demo/app");
    let sig = referrer(&reg, "demo/app", subject, SIGNATURE);
    let sbom = referrer(&reg, "demo/app", subject, SBOM);

    let all = reg
        .referrers("demo/app", &subject.0, None, None, 10)
        .unwrap();
    let mut seen: Vec<_> = all.entries.iter().map(|e| e.digest).collect();
    seen.sort();
    let mut expected = vec![sig, sbom];
    expected.sort();
    assert_eq!(seen, expected);

    let filtered = reg
        .referrers("demo/app", &subject.0, Some(SIGNATURE), None, 10)
        .unwrap();
    assert_eq!(filtered.entries.len(), 1);
    assert_eq!(filtered.entries[0].digest, sig);
    assert!(
        filtered.filter_applied,
        "the response must be entitled to say OCI-Filters-Applied"
    );

    let none = reg
        .referrers(
            "demo/app",
            &subject.0,
            Some("application/vnd.nothing"),
            None,
            10,
        )
        .unwrap();
    assert!(none.entries.is_empty());
}

#[test]
fn an_image_manifest_without_an_artifact_type_reports_its_config_media_type() {
    let (_dir, reg) = fixture();
    let subject = subject_manifest(&reg, "demo/app");
    let config = upload(&reg, "demo/app", "plain-config");
    let body = Image::new(config).subject(subject).json();
    put(&reg, "demo/app", &sha256(&body).to_string(), &body, 200);

    let list = reg
        .referrers("demo/app", &subject.0, None, None, 10)
        .unwrap();
    assert_eq!(
        list.entries[0].record.artifact_type.as_deref(),
        Some(OCI_CONFIG)
    );
}

#[test]
fn a_subject_that_does_not_exist_is_accepted_and_still_listed() {
    let (_dir, reg) = fixture();
    // The spec requires a referrer and its subject to be pushable in either
    // order, so the edge is written with no existence check.
    let ghost = (sha256(b"pushed later, or never"), 500);
    let sig = referrer(&reg, "demo/app", ghost, SIGNATURE);

    let list = reg.referrers("demo/app", &ghost.0, None, None, 10).unwrap();
    assert_eq!(list.entries.len(), 1);
    assert_eq!(list.entries[0].digest, sig);
}

#[test]
fn an_unknown_repo_or_subject_is_an_empty_list_and_never_an_error() {
    let (_dir, reg) = fixture();
    let empty = reg
        .referrers("nope/nope", &sha256(b"x"), None, None, 10)
        .unwrap();
    assert!(empty.entries.is_empty() && empty.next.is_none());

    let subject = subject_manifest(&reg, "demo/app");
    let _ = subject;
    let none = reg
        .referrers("demo/app", &sha256(b"unheard of"), None, None, 10)
        .unwrap();
    assert!(none.entries.is_empty());
}

#[test]
fn referrers_page_without_exceeding_the_limit() {
    let (_dir, reg) = fixture();
    let subject = subject_manifest(&reg, "demo/app");
    for i in 0..5 {
        let config = upload(&reg, "demo/app", &format!("cfg-{i}"));
        let body = Image::new(config)
            .subject(subject)
            .artifact_type(SIGNATURE)
            .json();
        put(&reg, "demo/app", &sha256(&body).to_string(), &body, 200);
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = reg
            .referrers("demo/app", &subject.0, None, cursor.as_ref(), 2)
            .unwrap();
        assert!(
            page.entries.len() <= 2,
            "a page must never exceed its limit"
        );
        seen.extend(page.entries.iter().map(|e| e.digest));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5);
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted, "referrers arrive in digest order");
}

#[test]
fn a_legacy_cosign_tag_synthesises_its_edge() {
    let (_dir, reg) = fixture();
    let subject = subject_manifest(&reg, "demo/app");

    // A cosign signature has no `subject` field at all - the subject is in the
    // tag name.
    let config = upload(&reg, "demo/app", "sig-config");
    let body = Image::new(config).json();
    let tag = format!("{}-{}.sig", subject.0.algorithm(), hex_of(&subject.0));
    let sig = put(&reg, "demo/app", &tag, &body, 300);

    let record = reg.get_manifest_record("demo/app", &sig).unwrap().unwrap();
    assert_eq!(record.subject, None, "no subject field was pushed");

    let list = reg
        .referrers("demo/app", &subject.0, None, None, 10)
        .unwrap();
    assert_eq!(list.entries.len(), 1);
    assert_eq!(list.entries[0].digest, sig);
    assert_eq!(
        list.entries[0].record.artifact_type.as_deref(),
        Some(OCI_CONFIG),
        "the effective artifactType comes from the config descriptor"
    );
}

#[test]
fn a_cosign_tag_set_after_the_push_synthesises_the_same_edge() {
    let (_dir, reg) = fixture();
    let subject = subject_manifest(&reg, "demo/app");
    let config = upload(&reg, "demo/app", "sig-config");
    let body = Image::new(config).json();
    let sig = put(&reg, "demo/app", &sha256(&body).to_string(), &body, 300);

    let tag = format!("sha256-{}.sig", hex_of(&subject.0));
    reg.set_tag("demo/app", &tag, &sig, 400).unwrap();

    let list = reg
        .referrers("demo/app", &subject.0, None, None, 10)
        .unwrap();
    assert_eq!(list.entries.len(), 1);
    assert_eq!(list.entries[0].digest, sig);
}

#[test]
fn moving_a_cosign_tag_retracts_the_edge_it_used_to_imply() {
    let (_dir, reg) = fixture();
    let subject = subject_manifest(&reg, "demo/app");
    let tag = format!("sha256-{}.sig", hex_of(&subject.0));

    let mut sigs = Vec::new();
    for i in 0..2 {
        let config = upload(&reg, "demo/app", &format!("sig-config-{i}"));
        let body = Image::new(config).json();
        sigs.push(put(
            &reg,
            "demo/app",
            &sha256(&body).to_string(),
            &body,
            300,
        ));
    }
    reg.set_tag("demo/app", &tag, &sigs[0], 400).unwrap();
    reg.set_tag("demo/app", &tag, &sigs[1], 500).unwrap();

    let list = reg
        .referrers("demo/app", &subject.0, None, None, 10)
        .unwrap();
    assert_eq!(
        list.entries.iter().map(|e| e.digest).collect::<Vec<_>>(),
        vec![sigs[1]],
        "the edge implied by the old target must be gone"
    );

    // And deleting the tag removes the last one, which is what lets purge
    // reclaim the signature once its subject is gone.
    reg.delete_tag("demo/app", &tag, 600).unwrap();
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    assert!(!reg
        .engine()
        .exists_prefix(&keys::referrers_of(repo, &subject.0))
        .unwrap());
}

fn hex_of(d: &Digest) -> String {
    d.to_string().split_once(':').unwrap().1.to_string()
}
