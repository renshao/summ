//! `/api/v1/` and the web UI shell.
//!
//! These have no conformance suite behind them and never will: `_catalog` was
//! removed from the Distribution Spec before v1.0.0 and nothing standard
//! answers "what is in this registry". That freedom is the point - the
//! operation this project exists to make fast carries no external obligation -
//! and this file is the price of it. Nothing else checks these shapes.
//!
//! Driven through the router in process, like `api.rs`, so the assertions are
//! about what the handler produced rather than what an HTTP stack made of it.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use serde_json::Value;
use summ_server::config::ServerConfig;
use summ_server::handlers::api::{DEFAULT_PAGE, MAX_PAGE};
use summ_server::memory::MemoryRegistry;
use summ_server::{router, AppState};
use tower::ServiceExt;

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

struct Harness {
    app: Router,
    registry: Arc<MemoryRegistry>,
}

impl Harness {
    fn new() -> Self {
        let registry = Arc::new(MemoryRegistry::new());
        let app = router(AppState::new(registry.clone(), ServerConfig::default()));
        Harness { app, registry }
    }

    async fn send(&self, method: Method, uri: &str) -> (StatusCode, Option<String>, Bytes) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("valid request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible");
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collects");
        (status, content_type, body)
    }

    async fn get(&self, uri: &str) -> Value {
        let (status, content_type, body) = self.send(Method::GET, uri).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}");
        assert_eq!(content_type.as_deref(), Some("application/json"));
        serde_json::from_slice(&body).expect("JSON body")
    }

    async fn status(&self, uri: &str) -> StatusCode {
        self.send(Method::GET, uri).await.0
    }
}

/// A one-layer image manifest, tagged.
fn seed_image(h: &Harness, repo: &str, tag: &str, seed: &str) -> String {
    let config = h
        .registry
        .seed_blob(repo, format!("config-{seed}").as_bytes());
    let layer = h
        .registry
        .seed_blob(repo, format!("layer-{seed}").as_bytes());
    let body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": config.to_string(), "size": 12 },
        "layers": [{ "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                     "digest": layer.to_string(), "size": 4096 }],
        "annotations": { "org.opencontainers.image.revision": seed },
    })
    .to_string();
    h.registry
        .seed_manifest(repo, Some(tag), OCI_MANIFEST, body.as_bytes())
        .to_string()
}

/// A two-platform index, tagged.
fn seed_index(h: &Harness, repo: &str, tag: &str) -> String {
    let amd = seed_image(h, repo, "child-amd64", "amd64");
    let arm = seed_image(h, repo, "child-arm64", "arm64");
    let body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": OCI_INDEX,
        "manifests": [
            { "mediaType": OCI_MANIFEST, "digest": amd, "size": 500,
              "platform": { "os": "linux", "architecture": "amd64" } },
            { "mediaType": OCI_MANIFEST, "digest": arm, "size": 500,
              "platform": { "os": "linux", "architecture": "arm64", "variant": "v8" } },
        ],
    })
    .to_string();
    h.registry
        .seed_manifest(repo, Some(tag), OCI_INDEX, body.as_bytes())
        .to_string()
}

// ---- repositories --------------------------------------------------------

#[tokio::test]
async fn repositories_list_in_name_order_with_their_counts() {
    let h = Harness::new();
    seed_image(&h, "zebra", "latest", "z");
    seed_image(&h, "alpine", "latest", "a");
    seed_image(&h, "alpine", "edge", "b");

    let body = h.get("/api/v1/repositories").await;
    let repos = body["repositories"].as_array().expect("an array");
    let names: Vec<&str> = repos.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["alpine", "zebra"],
        "name order, not insertion order"
    );

    assert_eq!(repos[0]["tags"]["count"], 2);
    assert_eq!(repos[0]["manifests"]["count"], 2);
    assert_eq!(
        repos[0]["tags"]["complete"], true,
        "a count that ran to the end must say so, or a UI cannot tell a total \
         from a floor"
    );
    assert!(body["next"].is_null(), "no cursor on the final page");
}

#[tokio::test]
async fn the_cursor_appears_only_when_a_further_page_exists() {
    let h = Harness::new();
    for name in ["a", "b", "c"] {
        seed_image(&h, name, "latest", name);
    }

    let first = h.get("/api/v1/repositories?n=2").await;
    assert_eq!(first["repositories"].as_array().unwrap().len(), 2);
    assert_eq!(first["next"], "b", "the cursor is the last row's own key");

    let second = h.get("/api/v1/repositories?n=2&last=b").await;
    let names: Vec<&str> = second["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["c"], "`last` is exclusive");
    assert!(
        second["next"].is_null(),
        "a page that exactly exhausts the range must not offer a cursor - the \
         reference implementation cannot tell and sends one anyway, costing \
         every client a wasted request"
    );

    // And a page that is full but final is still final.
    let exact = h.get("/api/v1/repositories?n=3").await;
    assert_eq!(exact["repositories"].as_array().unwrap().len(), 3);
    assert!(exact["next"].is_null());
}

#[tokio::test]
async fn search_is_a_name_prefix_not_a_substring() {
    let h = Harness::new();
    for name in ["nginx", "nginx-ingress", "nginx/base", "nginy", "my-nginx"] {
        seed_image(&h, name, "latest", name);
    }

    let body = h.get("/api/v1/repositories?q=nginx").await;
    let names: Vec<&str> = body["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["nginx", "nginx-ingress", "nginx/base"],
        "a prefix narrows the key scan; `my-nginx` would require a pass over \
         the whole catalogue and is deliberately not matched"
    );

    let none = h.get("/api/v1/repositories?q=zzz").await;
    assert!(none["repositories"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn the_page_size_is_clamped_and_never_zero() {
    let h = Harness::new();
    for i in 0..(MAX_PAGE + 5) {
        seed_image(&h, &format!("repo{i:04}"), "latest", "x");
    }

    let clamped = h.get("/api/v1/repositories?n=100000").await;
    assert_eq!(
        clamped["repositories"].as_array().unwrap().len(),
        MAX_PAGE,
        "an oversized page is clamped, not rejected: a row here costs a \
         bounded count per repository"
    );

    let defaulted = h.get("/api/v1/repositories").await;
    assert_eq!(
        defaulted["repositories"].as_array().unwrap().len(),
        DEFAULT_PAGE
    );

    // `/v2/` gives `n=0` a spec meaning. This API has no reason to reproduce it,
    // and a zero-row page with a cursor is a client that never advances.
    let zero = h.get("/api/v1/repositories?n=0").await;
    assert_eq!(zero["repositories"].as_array().unwrap().len(), 1);

    assert_eq!(
        h.status("/api/v1/repositories?n=nope").await,
        StatusCode::BAD_REQUEST
    );
}

// ---- one repository ------------------------------------------------------

#[tokio::test]
async fn a_repository_reports_counts_and_size() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "v1", "one");
    seed_image(&h, "demo/app", "v2", "two");

    let body = h.get("/api/v1/repositories/demo/app").await;
    assert_eq!(body["name"], "demo/app");
    assert_eq!(body["tags"]["count"], 2);
    assert_eq!(body["manifests"]["count"], 2);
    assert_eq!(body["blobs"]["count"], 4, "two configs and two layers");
    assert!(body["size_bytes"].as_u64().unwrap() > 0);

    assert_eq!(
        h.status("/api/v1/repositories/demo/missing").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.status("/api/v1/repositories/UPPERCASE").await,
        StatusCode::BAD_REQUEST,
        "the shape matched and the name did not: NAME_INVALID, not a bare 404"
    );
}

/// The reason the route table is flat rather than nested.
///
/// A registry may hold both `foo` and `foo/tags`. Under a nested
/// `/repositories/<name>/tags` those two collide on one path, and whichever way
/// it resolves the other repository is either unreachable or - far worse -
/// silently answered with the first one's data. With the collection as its own
/// resource and the name running to the end of the path, there is nothing to
/// resolve.
#[tokio::test]
async fn a_repository_may_be_called_tags_and_still_be_reachable() {
    let h = Harness::new();
    seed_image(&h, "foo", "latest", "parent");
    seed_image(&h, "foo/tags", "only", "child");

    let parent = h.get("/api/v1/tags/foo").await;
    let names: Vec<&str> = parent["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["latest"]);

    let child = h.get("/api/v1/tags/foo/tags").await;
    let names: Vec<&str> = child["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["only"]);

    // And both have their own reachable detail.
    assert_eq!(h.get("/api/v1/repositories/foo").await["name"], "foo");
    assert_eq!(
        h.get("/api/v1/repositories/foo/tags").await["name"],
        "foo/tags"
    );
}

/// A tag may be `latest`; a digest contains a `:`. Neither may contain `@`, and
/// neither may a repository name, which is what makes the split unambiguous.
#[tokio::test]
async fn a_manifest_reference_is_split_at_the_last_at_sign() {
    let h = Harness::new();
    let digest = seed_image(&h, "a/b/c", "latest", "one");

    assert_eq!(
        h.get("/api/v1/manifests/a/b/c@latest").await["digest"],
        digest
    );
    assert_eq!(
        h.get(&format!("/api/v1/manifests/a/b/c@{digest}")).await["digest"],
        digest
    );
}

// ---- tags and manifests --------------------------------------------------

#[tokio::test]
async fn a_tag_row_carries_the_manifest_it_resolves_to() {
    let h = Harness::new();
    let digest = seed_index(&h, "demo/app", "latest");

    let body = h.get("/api/v1/tags/demo/app").await;
    let tags = body["tags"].as_array().unwrap();
    let latest = tags
        .iter()
        .find(|t| t["name"] == "latest")
        .expect("the tag is listed");

    assert_eq!(latest["digest"], digest);
    let manifest = &latest["manifest"];
    assert_eq!(manifest["media_type"], OCI_INDEX);
    assert_eq!(manifest["children"], 2);
    assert_eq!(
        manifest["platforms"],
        serde_json::json!(["linux/amd64", "linux/arm64/v8"]),
        "a variant is part of an image's identity and must be rendered"
    );
    assert!(
        manifest["tags"]
            .as_array()
            .unwrap()
            .contains(&Value::from("latest")),
        "the reverse index is what tells a manifest list which rows are live"
    );
}

#[tokio::test]
async fn manifests_page_by_digest_and_report_their_layers() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "v1", "one");
    seed_image(&h, "demo/app", "v2", "two");

    let body = h.get("/api/v1/manifests/demo/app?n=1").await;
    let first = &body["manifests"][0];
    assert_eq!(first["blobs"], 2, "the config is a blob too");
    assert_eq!(first["blob_size"], 4096 + 12);
    assert!(first["annotations"]["org.opencontainers.image.revision"].is_string());

    let cursor = body["next"].as_str().expect("a second page exists");
    assert!(cursor.starts_with("sha256:"), "the cursor is a digest");

    let second = h
        .get(&format!("/api/v1/manifests/demo/app?n=1&last={cursor}"))
        .await;
    assert_ne!(second["manifests"][0]["digest"], first["digest"]);
    assert!(second["next"].is_null());

    // Digest order is byte order, so the two pages together are the whole set.
    let both = h.get("/api/v1/manifests/demo/app").await;
    assert_eq!(both["manifests"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_malformed_manifest_cursor_is_rejected_rather_than_ignored() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "v1", "one");

    // Silently restarting from the top would page forever.
    assert_eq!(
        h.status("/api/v1/manifests/demo/app?last=not-a-digest")
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.status("/api/v1/manifests/demo/app?last=sha256:ABCD")
            .await,
        StatusCode::BAD_REQUEST,
        "the digest grammar is lowercase, here as on `/v2/`"
    );
}

#[tokio::test]
async fn one_manifest_resolves_by_tag_and_by_digest() {
    let h = Harness::new();
    let digest = seed_image(&h, "demo/app", "v1", "one");

    let by_tag = h.get("/api/v1/manifests/demo/app@v1").await;
    let by_digest = h.get(&format!("/api/v1/manifests/demo/app@{digest}")).await;
    assert_eq!(by_tag, by_digest);
    assert_eq!(by_tag["digest"], digest);

    assert_eq!(
        h.status("/api/v1/manifests/demo/app@nope").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.status("/api/v1/manifests/demo/app@sha256:zz").await,
        StatusCode::BAD_REQUEST
    );
}

// ---- shape and method ----------------------------------------------------

#[tokio::test]
async fn the_api_is_read_only_and_answers_head() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "v1", "one");

    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        let (status, ..) = h.send(method.clone(), "/api/v1/repositories").await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} must not mutate through the discovery API: every write \
             has a spec-defined meaning on `/v2/`, and a second way to do it \
             would be a second set of rules to keep in agreement"
        );
    }

    let (status, _, body) = h.send(Method::HEAD, "/api/v1/repositories").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty(), "a HEAD carries no body");
}

#[tokio::test]
async fn discovery_answers_are_never_cached() {
    let h = Harness::new();
    let request = Request::builder()
        .uri("/api/v1/repositories")
        .body(Body::empty())
        .unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "a discovery answer is true until the next push lands, which is to say \
         not long"
    );
}

// ---- the UI shell --------------------------------------------------------

#[tokio::test]
async fn the_ui_is_served_from_the_binary_on_every_non_api_path() {
    let h = Harness::new();

    for path in ["/", "/r/demo/app", "/anything/at/all"] {
        let (status, content_type, body) = h.send(Method::GET, path).await;
        assert_eq!(status, StatusCode::OK, "GET {path}");
        assert_eq!(content_type.as_deref(), Some("text/html; charset=utf-8"));
        assert!(
            body.starts_with(b"<!doctype html>"),
            "a deep link into a client-side route must come back as the shell"
        );
    }

    let (_, content_type, css) = h.send(Method::GET, "/app.css").await;
    assert_eq!(content_type.as_deref(), Some("text/css; charset=utf-8"));
    assert!(!css.is_empty());

    let (_, content_type, js) = h.send(Method::GET, "/app.js").await;
    assert_eq!(
        content_type.as_deref(),
        Some("text/javascript; charset=utf-8")
    );
    assert!(!js.is_empty());
}

#[tokio::test]
async fn the_ui_never_shadows_the_registry() {
    let h = Harness::new();

    // `/v2/` and `/api/` keep their JSON errors: a machine asking for an
    // endpoint that does not exist must not be handed an HTML page.
    for path in ["/v2/nope", "/api/v1/nope", "/api/v2/repositories"] {
        let (status, content_type, _) = h.send(Method::GET, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {path}");
        assert_eq!(content_type.as_deref(), Some("application/json"));
    }
}

#[tokio::test]
async fn the_ui_loads_nothing_from_the_network() {
    // A registry is exactly the kind of service that runs air-gapped, so a UI
    // that needs a CDN to render is a UI that does not render.
    let html = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/index.html"))
        .expect("the shell is in the crate");
    let js = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app.js"))
        .expect("the script is in the crate");
    let css = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app.css"))
        .expect("the stylesheet is in the crate");

    for (name, source) in [("index.html", &html), ("app.js", &js), ("app.css", &css)] {
        // Anything the browser would *fetch*. An XML namespace identifier such
        // as `xmlns='http://www.w3.org/2000/svg'` is a name, not a URL, and is
        // never dereferenced - so the check is on the attributes and at-rules
        // that actually load something.
        for pattern in [
            "src=\"http",
            "src='http",
            "href=\"http",
            "href='http",
            "@import",
            "//cdn.",
        ] {
            assert!(
                !source.contains(pattern),
                "{name} loads {pattern}… from the network"
            );
        }
    }
}
