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
use summ_server::counters::PullCounters;
use summ_server::handlers::api::{DEFAULT_PAGE, MAX_PAGE};
use summ_server::memory::MemoryRegistry;
use summ_server::{router, AppState};
use tower::ServiceExt;

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

struct Harness {
    app: Router,
    registry: Arc<MemoryRegistry>,
    counters: Arc<PullCounters>,
}

impl Harness {
    fn new() -> Self {
        let registry = Arc::new(MemoryRegistry::new());
        // Counting is on, as it is in `summ serve`. There is no flush task
        // behind it here - `flush` below is the tick, taken by hand so a test
        // does not have to wait five seconds to see a pull land.
        let counters = Arc::new(PullCounters::new());
        let app = router(AppState::with_counters(
            registry.clone(),
            ServerConfig::default(),
            counters.clone(),
        ));
        Harness {
            app,
            registry,
            counters,
        }
    }

    /// One flush interval, on demand: drain the accumulator into the store.
    fn flush(&self) {
        self.registry.apply_pull_counts(self.counters.drain());
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
async fn search_matches_anywhere_in_the_name() {
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
        ["my-nginx", "nginx", "nginx-ingress", "nginx/base"],
        "a substring matches wherever it falls - `my-nginx` included - and the \
         results stay in name order, not in match position or relevance"
    );

    let none = h.get("/api/v1/repositories?q=zzz").await;
    assert!(none["repositories"].as_array().unwrap().is_empty());
    assert!(
        none["next"].is_null(),
        "a search that walked the range to prove a miss must say so, or the \
         client pages forever"
    );

    // A name cannot contain an uppercase byte, so matching one literally would
    // guarantee an empty result rather than report an honest one.
    let shouted = h.get("/api/v1/repositories?q=NGINX").await;
    assert_eq!(
        shouted["repositories"].as_array().unwrap().len(),
        4,
        "an uppercase query is folded, not answered with nothing"
    );
}

#[tokio::test]
async fn a_filtered_page_carries_a_cursor_past_the_names_it_skipped() {
    let h = Harness::new();
    // Interleaved so every page of matches has non-matching names inside it:
    // a cursor taken from the last row served is still correct here, but only
    // because the scan stopped *on* a match.
    for i in 0..6 {
        seed_image(&h, &format!("keep{i}"), "latest", "x");
        seed_image(&h, &format!("skip{i}"), "latest", "x");
    }

    let mut seen: Vec<String> = Vec::new();
    let mut url = "/api/v1/repositories?q=keep&n=2".to_string();
    for _ in 0..10 {
        let body = h.get(&url).await;
        for row in body["repositories"].as_array().unwrap() {
            seen.push(row["name"].as_str().unwrap().to_owned());
        }
        let Some(next) = body["next"].as_str() else {
            break;
        };
        url = format!("/api/v1/repositories?q=keep&n=2&last={next}");
    }

    assert_eq!(
        seen,
        ["keep0", "keep1", "keep2", "keep3", "keep4", "keep5"],
        "paging a filtered scan must yield every match exactly once"
    );
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
async fn the_api_mutates_through_exactly_one_route() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "v1", "one");

    for method in [Method::POST, Method::PUT, Method::PATCH] {
        let (status, ..) = h.send(method.clone(), "/api/v1/repositories").await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} must not mutate through the discovery API: every write \
             with a spec-defined meaning belongs on `/v2/`, and a second way \
             to do it would be a second set of rules to keep in agreement"
        );
    }

    // `DELETE` of a single repository is the exception, and only there: the
    // spec has no repository delete for it to duplicate.
    for uri in [
        "/api/v1/repositories",
        "/api/v1/tags/demo/app",
        "/api/v1/manifests/demo/app",
    ] {
        let (status, ..) = h.send(Method::DELETE, uri).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "DELETE {uri}");
    }

    let (status, _, body) = h.send(Method::HEAD, "/api/v1/repositories").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty(), "a HEAD carries no body");
}

// ---- deleting a repository -----------------------------------------------

#[tokio::test]
async fn deleting_a_repository_removes_it_from_every_listing() {
    let h = Harness::new();
    seed_index(&h, "demo/app", "latest");
    seed_image(&h, "demo/keep", "v1", "keep");

    let (status, _, body) = h
        .send(Method::DELETE, "/api/v1/repositories/demo/app")
        .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "`202`: the name is released, and the keys under it may still be going"
    );
    assert!(body.is_empty());

    // Gone the instant the call returns, which is the half of the operation a
    // client can observe.
    let listing = h.get("/api/v1/repositories").await;
    let names: Vec<&str> = listing["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["demo/keep"], "the other repository is untouched");

    for uri in [
        "/api/v1/repositories/demo/app",
        "/api/v1/tags/demo/app",
        "/api/v1/manifests/demo/app",
    ] {
        assert_eq!(h.status(uri).await, StatusCode::NOT_FOUND, "{uri}");
    }
    // And through `/v2/`, which is the same store seen from the spec side.
    assert_eq!(
        h.status("/v2/demo/app/tags/list").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.status("/v2/demo/keep/tags/list").await, StatusCode::OK);
}

#[tokio::test]
async fn deleting_a_repository_that_does_not_exist_is_a_404() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "v1", "one");

    let (status, _, body) = h
        .send(Method::DELETE, "/api/v1/repositories/demo/nope")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error: Value = serde_json::from_slice(&body).expect("a spec error body");
    assert_eq!(error["errors"][0]["code"], "NAME_UNKNOWN");

    // A second delete of the same name, for the same reason: the name is gone
    // after the first, whatever is still being swept underneath it.
    assert_eq!(
        h.send(Method::DELETE, "/api/v1/repositories/demo/app")
            .await
            .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        h.send(Method::DELETE, "/api/v1/repositories/demo/app")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

/// The route table is flat precisely so a name containing `/` cannot be
/// mistaken for a collection under another name. A delete is the operation
/// where getting that wrong is unrecoverable.
#[tokio::test]
async fn deleting_a_repository_whose_name_looks_like_a_collection() {
    let h = Harness::new();
    seed_image(&h, "foo", "v1", "one");
    seed_image(&h, "foo/tags", "v1", "two");

    assert_eq!(
        h.send(Method::DELETE, "/api/v1/repositories/foo/tags")
            .await
            .0,
        StatusCode::ACCEPTED
    );

    let listing = h.get("/api/v1/repositories").await;
    let names: Vec<&str> = listing["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["foo"], "`foo` kept its tags");
    assert_eq!(h.get("/api/v1/tags/foo").await["tags"][0]["name"], "v1");
}

#[tokio::test]
async fn a_deleted_repository_can_be_pushed_again_immediately() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "v1", "one");
    h.send(Method::DELETE, "/api/v1/repositories/demo/app")
        .await;

    seed_image(&h, "demo/app", "v2", "two");
    let tags = h.get("/api/v1/tags/demo/app").await;
    let names: Vec<&str> = tags["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["v2"], "nothing of the old repository came back");
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

    // The favicon is an asset, not a client-side route: served as SVG, never
    // as the shell. Getting this wrong shows up as a blank tab icon rather
    // than as an error, which is why it is pinned here.
    let (_, content_type, logo) = h.send(Method::GET, "/logo.svg").await;
    assert_eq!(
        content_type.as_deref(),
        Some("image/svg+xml; charset=utf-8")
    );
    assert!(logo.starts_with(b"<svg"));
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
    let logo = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/logo.svg"))
        .expect("the logo is in the crate");

    for (name, source) in [
        ("index.html", &html),
        ("app.js", &js),
        ("app.css", &css),
        ("logo.svg", &logo),
    ] {
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

// ---- tag history ---------------------------------------------------------

#[tokio::test]
async fn tag_history_returns_events_newest_first() {
    let h = Harness::new();
    let first = seed_image(&h, "demo/app", "latest", "one");
    let second = seed_image(&h, "demo/app", "latest", "two");

    let body = h.get("/api/v1/tag-history/demo/app@latest").await;
    let events = body["events"].as_array().expect("an array");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["digest"].as_str(), Some(second.as_str()));
    assert_eq!(events[1]["digest"].as_str(), Some(first.as_str()));
    for event in events {
        assert_eq!(event["tag"].as_str(), Some("latest"));
        assert_eq!(event["event"].as_str(), Some("created"));
        assert!(event["at"].as_u64().expect("a timestamp") > 0);
        // The descriptor is denormalised so a row renders without the manifest.
        assert_eq!(event["media_type"].as_str(), Some(OCI_MANIFEST));
        assert!(event["size"].as_u64().unwrap() > 0);
    }
    assert!(
        body["next"].is_null(),
        "an exhausted scan carries no cursor"
    );
    // Newest first, strictly: the two events must not share an instant.
    assert!(events[0]["at"].as_u64() > events[1]["at"].as_u64());
}

#[tokio::test]
async fn deleting_a_tag_appends_a_deleted_event_naming_what_it_displaced() {
    let h = Harness::new();
    let digest = seed_image(&h, "demo/app", "latest", "one");

    let (status, _, _) = h
        .send(Method::DELETE, "/v2/demo/app/manifests/latest")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let body = h.get("/api/v1/tag-history/demo/app@latest").await;
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event"].as_str(), Some("deleted"));
    assert_eq!(
        events[0]["digest"].as_str(),
        Some(digest.as_str()),
        "the delete has to name the digest it displaced"
    );
    assert_eq!(events[1]["event"].as_str(), Some("created"));
}

/// History outlives the tag: the endpoint still answers after the tag is gone,
/// which is why an unknown tag cannot be a 404 either.
#[tokio::test]
async fn history_answers_after_the_tag_is_deleted() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "latest", "one");
    h.send(Method::DELETE, "/v2/demo/app/manifests/latest")
        .await;

    assert_eq!(
        h.status("/v2/demo/app/manifests/latest").await,
        StatusCode::NOT_FOUND
    );
    let body = h.get("/api/v1/tag-history/demo/app@latest").await;
    assert_eq!(body["events"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn an_unknown_repo_or_tag_is_an_empty_page() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "latest", "one");

    for uri in [
        "/api/v1/tag-history/no/such@latest",
        "/api/v1/tag-history/demo/app@never",
    ] {
        let body = h.get(uri).await;
        assert!(body["events"].as_array().unwrap().is_empty(), "{uri}");
        assert!(body["next"].is_null());
    }
}

#[tokio::test]
async fn digest_addressed_history_asks_a_different_question() {
    let h = Harness::new();
    // The same seed is the same body, so this is one manifest wearing two
    // names - which is exactly what the digest-addressed range answers.
    let digest = seed_image(&h, "demo/app", "latest", "one");
    assert_eq!(seed_image(&h, "demo/app", "v1", "one"), digest);
    let other = seed_image(&h, "demo/app", "edge", "two");

    let body = h
        .get(&format!("/api/v1/tag-history/demo/app@{digest}"))
        .await;
    let tags: Vec<&str> = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["tag"].as_str().unwrap())
        .collect();
    assert_eq!(
        tags,
        vec!["v1", "latest"],
        "newest first, and only this one"
    );

    // The neighbouring manifest's history is its own.
    let body = h
        .get(&format!("/api/v1/tag-history/demo/app@{other}"))
        .await;
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["tag"].as_str(), Some("edge"));
}

#[tokio::test]
async fn history_pages_with_the_cursor_it_hands_back() {
    let h = Harness::new();
    for seed in ["a", "b", "c", "d", "e"] {
        seed_image(&h, "demo/app", "latest", seed);
    }

    let mut seen: Vec<u64> = Vec::new();
    let mut uri = "/api/v1/tag-history/demo/app@latest?n=2".to_string();
    loop {
        let body = h.get(&uri).await;
        let events = body["events"].as_array().unwrap();
        assert!(events.len() <= 2, "a page never exceeds its limit");
        seen.extend(events.iter().map(|e| e["at"].as_u64().unwrap()));
        let Some(next) = body["next"].as_object() else {
            break;
        };
        uri = format!(
            "/api/v1/tag-history/demo/app@latest?n=2&before={}&last={}",
            next["before"].as_u64().unwrap(),
            next["last"].as_str().unwrap()
        );
    }

    assert_eq!(seen.len(), 5, "every event is seen exactly once");
    let mut descending = seen.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        seen, descending,
        "pages stay newest-first across the cursor"
    );
}

/// `?before=` on its own is a filter rather than a resume, and it is
/// strictly-before.
#[tokio::test]
async fn before_on_its_own_filters_and_excludes_its_own_instant() {
    let h = Harness::new();
    for seed in ["a", "b", "c"] {
        seed_image(&h, "demo/app", "latest", seed);
    }
    let all = h.get("/api/v1/tag-history/demo/app@latest").await;
    let stamps: Vec<u64> = all["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["at"].as_u64().unwrap())
        .collect();

    let body = h
        .get(&format!(
            "/api/v1/tag-history/demo/app@latest?before={}",
            stamps[1]
        ))
        .await;
    let seen: Vec<u64> = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["at"].as_u64().unwrap())
        .collect();
    assert_eq!(seen, vec![stamps[2]], "the boundary event is excluded");
}

#[tokio::test]
async fn a_malformed_history_query_is_rejected_rather_than_ignored() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "latest", "one");

    assert_eq!(
        h.status("/api/v1/tag-history/demo/app@latest?before=yesterday")
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.status("/api/v1/tag-history/demo/app@latest?n=lots").await,
        StatusCode::BAD_REQUEST
    );
    // A reference is mandatory: there is no whole-repository history
    // collection, because that would be an unbounded scan.
    assert_eq!(
        h.status("/api/v1/tag-history/demo/app").await,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------- pull counts --

/// Pull an image through `/v2/`, then flush, so the counters see exactly what a
/// real client would produce.
async fn pull_manifest(h: &Harness, repo: &str, reference: &str) {
    let (status, _, _) = h
        .send(Method::GET, &format!("/v2/{repo}/manifests/{reference}"))
        .await;
    assert_eq!(status, StatusCode::OK, "GET {repo}:{reference}");
}

fn day_of(row: &Value) -> u16 {
    row["day"].as_u64().expect("day") as u16
}

/// The window is always `days` long, ends today, and every day is present -
/// the client is a grid, and a gap in a grid is a missing cell rather than a
/// zero one.
#[tokio::test]
async fn a_pull_count_window_is_zero_filled_and_ends_today() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "latest", "a");

    let body = h.get("/api/v1/pull-counts/demo/app").await;
    let days = body["days"].as_array().expect("days");
    assert_eq!(days.len(), 30, "the default window");
    assert_eq!(body["scope"], "repository");
    assert_eq!(body["repository"], "demo/app");
    assert!(body["reference"].is_null());
    assert_eq!(body["approximate"], true);
    assert_eq!(body["from"], days[0]["date"]);
    assert_eq!(body["to"], days[29]["date"]);

    // Contiguous, ascending, one per day, with the hour arrays present even
    // where nothing happened.
    for (i, row) in days.iter().enumerate() {
        assert_eq!(day_of(row), day_of(&days[0]) + i as u16);
        assert_eq!(row["manifest_pulls"], 0);
        assert_eq!(row["hours"]["manifest_pulls"].as_array().unwrap().len(), 24);
        assert_eq!(row["hours"]["blob_pulls"].as_array().unwrap().len(), 24);
        assert_eq!(row["hours"]["bytes_out"].as_array().unwrap().len(), 24);
    }

    // The weekday is arithmetic on the bucket, so consecutive days advance it.
    let first = days[0]["weekday"].as_u64().unwrap();
    assert_eq!(days[1]["weekday"].as_u64().unwrap(), (first + 1) % 7);
}

/// One `GET` lands on all three scopes, and each is its own series rather than
/// a view of the others.
#[tokio::test]
async fn a_manifest_pull_counts_against_manifest_tag_and_repository() {
    let h = Harness::new();
    let digest = seed_image(&h, "demo/app", "latest", "a");

    pull_manifest(&h, "demo/app", "latest").await;
    h.flush();

    for (uri, scope) in [
        ("/api/v1/pull-counts/demo/app", "repository"),
        ("/api/v1/pull-counts/demo/app@latest", "tag"),
        (
            &format!("/api/v1/pull-counts/demo/app@{digest}") as &str,
            "manifest",
        ),
    ] {
        let body = h.get(uri).await;
        assert_eq!(body["scope"], scope, "{uri}");
        assert_eq!(body["totals"]["manifest_pulls"], 1, "{uri}");
        let today = body["days"].as_array().unwrap().last().unwrap();
        assert_eq!(today["manifest_pulls"], 1, "{uri}");
        // The day is the sum of the hours, and there is no stored total.
        let hours = today["hours"]["manifest_pulls"].as_array().unwrap();
        assert_eq!(
            hours.iter().map(|h| h.as_u64().unwrap()).sum::<u64>(),
            1,
            "{uri}"
        );
    }
}

/// containerd issues `HEAD` then `GET` on every cold pull, so counting both
/// would double every number in the registry.
#[tokio::test]
async fn a_head_is_not_a_pull() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "latest", "a");

    let (status, _, _) = h.send(Method::HEAD, "/v2/demo/app/manifests/latest").await;
    assert_eq!(status, StatusCode::OK);
    h.flush();

    let body = h.get("/api/v1/pull-counts/demo/app").await;
    assert_eq!(body["totals"]["manifest_pulls"], 0);
}

/// A pull by digest has no tag to attribute and must not invent one - the tag
/// series answers "how often is this name pulled", which is a different
/// question from "how often is this content pulled".
#[tokio::test]
async fn a_pull_by_digest_leaves_the_tag_series_alone() {
    let h = Harness::new();
    let digest = seed_image(&h, "demo/app", "latest", "a");

    pull_manifest(&h, "demo/app", &digest).await;
    h.flush();

    assert_eq!(
        h.get(&format!("/api/v1/pull-counts/demo/app@{digest}"))
            .await["totals"]["manifest_pulls"],
        1
    );
    assert_eq!(
        h.get("/api/v1/pull-counts/demo/app@latest").await["totals"]["manifest_pulls"],
        0
    );
    // The repository scope still sees it: it counts pulls, not names.
    assert_eq!(
        h.get("/api/v1/pull-counts/demo/app").await["totals"]["manifest_pulls"],
        1
    );
}

/// Blob traffic is repository-scoped only. Attributing a shared layer's bytes
/// to one manifest would be a lie, and doing it honestly needs the `R` fan-in.
#[tokio::test]
async fn blob_bytes_are_counted_against_the_repository_and_nowhere_else() {
    let h = Harness::new();
    let digest = seed_image(&h, "demo/app", "latest", "a");
    let layer = h.registry.seed_blob("demo/app", b"0123456789");

    let (status, _, body) = h
        .send(Method::GET, &format!("/v2/demo/app/blobs/{layer}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), 10);
    h.flush();

    let repo = h.get("/api/v1/pull-counts/demo/app").await;
    assert_eq!(repo["totals"]["blob_pulls"], 1);
    assert_eq!(repo["totals"]["bytes_out"], 10);
    // A blob fetch is not a manifest pull.
    assert_eq!(repo["totals"]["manifest_pulls"], 0);

    let manifest = h
        .get(&format!("/api/v1/pull-counts/demo/app@{digest}"))
        .await;
    assert_eq!(manifest["totals"]["blob_pulls"], 0);
    assert_eq!(manifest["totals"]["bytes_out"], 0);
}

/// A ranged read counts the bytes it actually received. containerd 2.1+ asks
/// for `bytes=N-` and reads a prefix of it, so counting the requested window
/// would over-report a large layer many times over.
#[tokio::test]
async fn a_ranged_blob_read_counts_the_bytes_it_received() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "latest", "a");
    let layer = h.registry.seed_blob("demo/app", b"0123456789");

    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/v2/demo/app/blobs/{layer}"))
        .header(header::RANGE, "bytes=0-3")
        .body(Body::empty())
        .unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.len(), 4);
    h.flush();

    let repo = h.get("/api/v1/pull-counts/demo/app").await;
    assert_eq!(repo["totals"]["bytes_out"], 4);
    assert_eq!(repo["totals"]["blob_pulls"], 1);
}

/// Counts outlive what they describe: after a delete nothing distinguishes
/// "never pulled" from "gone", and the second case still has to answer.
#[tokio::test]
async fn nothing_in_pull_counts_404s() {
    let h = Harness::new();
    for uri in [
        "/api/v1/pull-counts/ghost/repo",
        "/api/v1/pull-counts/ghost/repo@latest",
        "/api/v1/pull-counts/ghost/repo@sha256:0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        let body = h.get(uri).await;
        assert_eq!(body["totals"]["manifest_pulls"], 0, "{uri}");
        assert_eq!(body["days"].as_array().unwrap().len(), 30, "{uri}");
    }
}

#[tokio::test]
async fn the_pull_count_window_is_clamped_and_validated() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "latest", "a");

    assert_eq!(
        h.get("/api/v1/pull-counts/demo/app?days=7").await["days"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    // Clamped rather than rejected, as everywhere else in this API.
    assert_eq!(
        h.get("/api/v1/pull-counts/demo/app?days=99999").await["days"]
            .as_array()
            .unwrap()
            .len(),
        400
    );
    assert_eq!(
        h.get("/api/v1/pull-counts/demo/app?days=0").await["days"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // A cursor that does not parse is a 400, not a silent default.
    assert_eq!(
        h.status("/api/v1/pull-counts/demo/app?days=lots").await,
        StatusCode::BAD_REQUEST
    );
}

/// The route splits at the last `@`, which is what makes a flat table
/// unambiguous for a repository name containing `/`.
#[tokio::test]
async fn the_pull_count_route_splits_the_reference_off_the_end() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "latest", "a");

    let body = h.get("/api/v1/pull-counts/demo/app@latest").await;
    assert_eq!(body["repository"], "demo/app");
    assert_eq!(body["reference"], "latest");
    assert_eq!(body["scope"], "tag");

    assert_eq!(h.status("/api/v1/pull-counts").await, StatusCode::NOT_FOUND);
}

/// Repeated pulls accumulate rather than replace, which is the property the
/// whole flush scheme rests on.
#[tokio::test]
async fn pulls_accumulate_across_flushes() {
    let h = Harness::new();
    seed_image(&h, "demo/app", "latest", "a");

    for _ in 0..3 {
        pull_manifest(&h, "demo/app", "latest").await;
        h.flush();
    }
    assert_eq!(
        h.get("/api/v1/pull-counts/demo/app@latest").await["totals"]["manifest_pulls"],
        3
    );
}
