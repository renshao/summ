//! Pull, existence, blob authorisation, and the discovery queries.

mod common;

use common::*;
use summ_core::Digest;
use summ_registry::{BlobReference, Reference, Registry};

fn image(reg: &Registry, repo: &str, seed: &str, tag: Option<&str>) -> Digest {
    let config = upload(reg, repo, &format!("config-{seed}"));
    let body = Image::new(config).json();
    let reference = tag.map_or_else(|| sha256(&body).to_string(), str::to_string);
    put(reg, repo, &reference, &body, 100)
}

// --- pull and existence -------------------------------------------------

#[test]
fn head_resolves_a_tag_without_reading_the_body() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let body = Image::new(config).json();
    let digest = put(&reg, "demo/app", "v1", &body, 100);

    let head = reg
        .head_manifest("demo/app", &"v1".parse::<Reference>().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(head.digest, digest);
    assert_eq!(head.media_type, OCI_MANIFEST);
    assert_eq!(head.size, body.len() as u64);

    let by_digest = reg
        .head_manifest("demo/app", &Reference::Digest(digest))
        .unwrap()
        .unwrap();
    assert_eq!(by_digest, head);
}

#[test]
fn head_and_get_distinguish_an_unknown_name_from_an_unknown_manifest() {
    let (_dir, reg) = fixture();
    let _ = image(&reg, "demo/app", "one", Some("v1"));

    assert!(reg
        .head_manifest("demo/app", &"v9".parse::<Reference>().unwrap())
        .unwrap()
        .is_none());
    assert!(reg.get_manifest_by_tag("demo/app", "v9").unwrap().is_none());

    let err = reg
        .head_manifest("no/such", &"v1".parse::<Reference>().unwrap())
        .unwrap_err();
    assert_eq!(err.code(), "NAME_UNKNOWN");
}

// --- blob authorisation -------------------------------------------------

#[test]
fn a_blob_is_servable_only_where_p_or_r_says_so() {
    let (_dir, reg) = fixture();
    let shared = upload(&reg, "demo/app", "shared bytes");
    // A second repo exists but has never been given this blob, even though the
    // content itself is deduplicated registry-wide under `L`.
    let _ = upload(&reg, "other/app", "unrelated");

    assert!(reg.blob_is_servable("demo/app", &shared.0).unwrap());
    assert!(
        !reg.blob_is_servable("other/app", &shared.0).unwrap(),
        "serving on `L` alone would leak content across repos"
    );
    assert!(!reg.blob_is_servable("never/heard/of", &shared.0).unwrap());

    // A mount is exactly a `P` edge under the target name.
    reg.commit_blob("other/app", &shared.0, shared.1, at(400))
        .unwrap();
    assert!(reg.blob_is_servable("other/app", &shared.0).unwrap());
    assert_eq!(
        reg.servable_blob("other/app", &shared.0)
            .unwrap()
            .unwrap()
            .size,
        shared.1
    );
}

#[test]
fn an_r_edge_alone_keeps_a_blob_servable() {
    let (_dir, reg) = fixture();
    let config = upload(&reg, "demo/app", "config");
    let body = Image::new(config).json();
    put(&reg, "demo/app", "v1", &body, 100);

    // Drop the membership edge but leave the manifest: the blob is still
    // reachable through the manifest that references it.
    let repo = reg.lookup_repo("demo/app").unwrap().unwrap();
    let mut batch = summ_meta::WriteBatch::new();
    batch.delete(summ_core::keys::repo_blob(repo, &config.0));
    reg.engine().apply(&batch).unwrap();

    assert!(reg.blob_is_servable("demo/app", &config.0).unwrap());
}

// --- discovery ----------------------------------------------------------

#[test]
fn repositories_page_in_name_order() {
    let (_dir, reg) = fixture();
    // Interned in an order that is not name order, so paging over `i` would
    // give a different answer from paging over `n`.
    for repo in ["zeta/app", "alpine/base", "nginx/web", "beta/app"] {
        let _ = upload(&reg, repo, "config");
    }

    let first = reg.list_repos(None, 2).unwrap();
    assert_eq!(first.repos, ["alpine/base", "beta/app"]);
    assert_eq!(first.next.as_deref(), Some("beta/app"));

    let second = reg.list_repos(first.next.as_deref(), 2).unwrap();
    assert_eq!(second.repos, ["nginx/web", "zeta/app"]);
    assert_eq!(second.next, None);

    assert_eq!(reg.list_repos(None, 10).unwrap().repos.len(), 4);
}

#[test]
fn manifests_page_in_digest_order_and_stay_inside_the_repo() {
    let (_dir, reg) = fixture();
    let mut here: Vec<_> = (0..5)
        .map(|i| image(&reg, "demo/app", &format!("m{i}"), None))
        .collect();
    here.sort();
    let elsewhere = image(&reg, "other/app", "x", None);

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = reg.list_manifests("demo/app", cursor.as_ref(), 2).unwrap();
        assert!(page.manifests.len() <= 2);
        seen.extend(page.manifests.iter().map(|m| m.digest));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, here);
    assert!(!seen.contains(&elsewhere));
}

#[test]
fn repo_usage_and_manifest_count_fold_across_pages() {
    let (_dir, reg) = fixture();
    let mut expected_bytes = 0u64;
    for i in 0..5 {
        let content = "x".repeat(10 + i);
        expected_bytes += content.len() as u64;
        upload(&reg, "demo/app", &content);
    }
    for i in 0..3 {
        image(&reg, "demo/app", &format!("m{i}"), None);
    }
    // Each image push adds its own config blob to `P` as well.
    for i in 0..3 {
        expected_bytes += format!("config-m{i}").len() as u64;
    }

    let mut blobs = 0;
    let mut bytes = 0;
    let mut cursor = None;
    loop {
        let page = reg.repo_usage("demo/app", cursor.as_ref(), 3).unwrap();
        blobs += page.blobs;
        bytes += page.bytes;
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(blobs, 8);
    assert_eq!(bytes, expected_bytes);

    let mut manifests = 0;
    let mut cursor = None;
    loop {
        let page = reg.count_manifests("demo/app", cursor.as_ref(), 2).unwrap();
        assert!(page.manifests <= 2);
        manifests += page.manifests;
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(manifests, 3);
}

#[test]
fn a_shared_layer_lists_every_manifest_that_references_it() {
    let (_dir, reg) = fixture();
    let bytes = "a widely shared base layer";
    let shared = upload(&reg, "demo/app", bytes);
    reg.commit_blob("other/app", &shared.0, shared.1, at(100))
        .unwrap();

    let mut expected = Vec::new();
    for repo in ["demo/app", "other/app"] {
        let config = upload(&reg, repo, &format!("config-{repo}"));
        let body = Image::new(config).layer(shared).json();
        expected.push(BlobReference {
            repo: repo.to_string(),
            manifest: put(&reg, repo, "v1", &body, 200),
        });
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = reg
            .manifests_referencing_blob(&shared.0, cursor.as_ref(), 1)
            .unwrap();
        assert!(page.references.len() <= 1);
        seen.extend(page.references.clone());
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.len(), 2);
    for reference in &expected {
        assert!(seen.contains(reference), "{reference:?}");
    }
}

#[test]
fn untagged_manifests_page_without_exceeding_the_limit() {
    let (_dir, reg) = fixture();
    let tagged = image(&reg, "demo/app", "kept", Some("v1"));
    let mut untagged: Vec<_> = (0..4)
        .map(|i| image(&reg, "demo/app", &format!("loose{i}"), None))
        .collect();
    untagged.sort();

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = reg
            .untagged_manifests("demo/app", cursor.as_ref(), 2)
            .unwrap();
        assert!(page.digests.len() <= 2);
        seen.extend(page.digests);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, untagged);
    assert!(!seen.contains(&tagged));
}
