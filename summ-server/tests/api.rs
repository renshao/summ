//! The `/v2/` surface, driven in process.
//!
//! Every test calls the router through `tower::ServiceExt::oneshot` rather than
//! binding a port. That is not only faster: it keeps the assertions honest
//! about what the *handler* produced, since nothing between here and the
//! handler can quietly add a `Content-Length` or drop a body the way a real
//! HTTP stack might.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use sha2::{Digest as _, Sha256, Sha512};
use summ_server::config::ServerConfig;
use summ_server::error::ErrorCode;
use summ_server::memory::MemoryRegistry;
use summ_server::{router, AppState};
use tower::ServiceExt;

// ---------------------------------------------------------------- harness --

struct Harness {
    app: Router,
    registry: Arc<MemoryRegistry>,
}

impl Harness {
    fn new() -> Self {
        Self::with_config(ServerConfig::default())
    }

    fn with_config(config: ServerConfig) -> Self {
        let registry = Arc::new(MemoryRegistry::new());
        let app = router(AppState::new(registry.clone(), config));
        Harness { app, registry }
    }

    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collects");
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.request(Method::GET, uri, Vec::new(), Body::empty())
            .await
    }

    async fn head(&self, uri: &str) -> Reply {
        self.request(Method::HEAD, uri, Vec::new(), Body::empty())
            .await
    }

    async fn delete(&self, uri: &str) -> Reply {
        self.request(Method::DELETE, uri, Vec::new(), Body::empty())
            .await
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        headers: Vec<(&str, String)>,
        body: Body,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        self.send(builder.body(body).expect("valid request")).await
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn header(&self, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "expected JSON, got {:?}: {e}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// Assert the spec's error envelope and return the code.
    fn error_code(&self) -> String {
        assert_eq!(
            self.header(header::CONTENT_TYPE),
            Some("application/json"),
            "an error body must be JSON"
        );
        let body = self.json();
        let errors = body["errors"].as_array().expect("`errors` is an array");
        assert_eq!(errors.len(), 1);
        let code = errors[0]["code"].as_str().expect("`code` is a string");
        assert!(
            code.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
            "`code` must be uppercase alphabetic and underscores only, got {code}"
        );
        assert!(
            errors[0]["message"].is_string(),
            "`message` should be present and human readable"
        );
        code.to_owned()
    }

    fn assert_error(&self, status: StatusCode, code: ErrorCode) {
        assert_eq!(self.status, status, "unexpected status");
        assert_eq!(self.error_code(), code.as_str());
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

fn sha512_hex(bytes: &[u8]) -> String {
    format!("sha512:{}", hex(&Sha512::digest(bytes)))
}

const IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// A manifest with a field outside the OCI schema, to prove nothing
/// schema-validates it and the bytes round-trip untouched.
fn manifest_bytes() -> Vec<u8> {
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":2},"layers":[],"x-unknown-field":{"kept":true}}"#
        .to_vec()
}

// ------------------------------------------------------------------ end-1 --

#[tokio::test]
async fn ping_returns_200_and_an_empty_object() {
    let h = Harness::new();
    let reply = h.get("/v2/").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header(header::CONTENT_TYPE), Some("application/json"));
    assert_eq!(reply.header(header::CONTENT_LENGTH), Some("2"));
    assert_eq!(&reply.body[..], b"{}");
    assert_eq!(
        reply.header("docker-distribution-api-version"),
        Some("registry/2.0")
    );

    // `/v2` without the trailing slash reaches the same endpoint; clients send
    // both.
    assert_eq!(h.get("/v2").await.status, StatusCode::OK);
}

#[tokio::test]
async fn ping_head_carries_content_length_with_no_body() {
    let h = Harness::new();
    let reply = h.head("/v2/").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header(header::CONTENT_LENGTH), Some("2"));
    assert!(reply.body.is_empty());
}

// ------------------------------------------------------------ error model --

#[tokio::test]
async fn every_error_code_has_a_status_and_the_spec_body_shape() {
    use axum::response::IntoResponse;
    use summ_server::ApiError;

    let cases = [
        (ErrorCode::BlobUnknown, StatusCode::NOT_FOUND),
        (ErrorCode::BlobUploadInvalid, StatusCode::BAD_REQUEST),
        (ErrorCode::BlobUploadUnknown, StatusCode::NOT_FOUND),
        (ErrorCode::DigestInvalid, StatusCode::BAD_REQUEST),
        (ErrorCode::ManifestBlobUnknown, StatusCode::BAD_REQUEST),
        (ErrorCode::ManifestInvalid, StatusCode::BAD_REQUEST),
        (ErrorCode::ManifestUnknown, StatusCode::NOT_FOUND),
        (ErrorCode::NameInvalid, StatusCode::BAD_REQUEST),
        (ErrorCode::NameUnknown, StatusCode::NOT_FOUND),
        (ErrorCode::SizeInvalid, StatusCode::BAD_REQUEST),
        (ErrorCode::Unauthorized, StatusCode::UNAUTHORIZED),
        (ErrorCode::Denied, StatusCode::FORBIDDEN),
        (ErrorCode::Unsupported, StatusCode::METHOD_NOT_ALLOWED),
        (ErrorCode::TooManyRequests, StatusCode::TOO_MANY_REQUESTS),
        (ErrorCode::PaginationNumberInvalid, StatusCode::BAD_REQUEST),
    ];

    for (code, status) in cases {
        let response = ApiError::new(code).with_detail("detail").into_response();
        assert_eq!(response.status(), status, "{}", code.as_str());
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body collects");
        let reply = Reply {
            status,
            headers,
            body,
        };
        assert_eq!(reply.error_code(), code.as_str());
        assert_eq!(reply.json()["errors"][0]["detail"], "detail");
    }
}

#[tokio::test]
async fn an_unknown_path_and_an_invalid_name_are_distinguished() {
    let h = Harness::new();

    // The shape matched, the name did not: `NAME_INVALID`, not a bare 404.
    h.get("/v2/UPPERCASE/tags/list")
        .await
        .assert_error(StatusCode::BAD_REQUEST, ErrorCode::NameInvalid);
    h.get("/v2/trailing-/tags/list")
        .await
        .assert_error(StatusCode::BAD_REQUEST, ErrorCode::NameInvalid);

    // No endpoint has this shape at all.
    h.get("/v2/foo")
        .await
        .assert_error(StatusCode::NOT_FOUND, ErrorCode::NameUnknown);
    h.get("/api/v1/nothing")
        .await
        .assert_error(StatusCode::NOT_FOUND, ErrorCode::NameUnknown);

    // Outside `/v2/` and `/api/` the answer is the web UI, not an error: the
    // UI routes client-side, so a deep link into a repository page has to come
    // back as the shell rather than as a 404.
    let response = h.get("/notv2/foo").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.header(header::CONTENT_TYPE),
        Some("text/html; charset=utf-8")
    );
}

#[tokio::test]
async fn a_name_longer_than_255_bytes_is_rejected() {
    let h = Harness::new();
    let long = "a".repeat(256);
    h.get(&format!("/v2/{long}/tags/list"))
        .await
        .assert_error(StatusCode::BAD_REQUEST, ErrorCode::NameInvalid);
}

#[tokio::test]
async fn an_unsupported_method_is_405_with_allow() {
    let h = Harness::new();
    let reply = h
        .request(
            Method::POST,
            "/v2/demo/tags/list",
            Vec::new(),
            Body::empty(),
        )
        .await;
    reply.assert_error(StatusCode::METHOD_NOT_ALLOWED, ErrorCode::Unsupported);
    assert_eq!(reply.header(header::ALLOW), Some("GET, HEAD"));
}

// -------------------------------------------------------------- manifests --

#[tokio::test]
async fn a_manifest_round_trips_under_a_multi_component_name() {
    let h = Harness::new();
    let body = manifest_bytes();
    let digest = sha256_hex(&body);

    let put = h
        .request(
            Method::PUT,
            "/v2/homebrew/core/hello/manifests/v1",
            vec![(header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned())],
            Body::from(body.clone()),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
    assert_eq!(
        put.header(header::LOCATION),
        Some(format!("/v2/homebrew/core/hello/manifests/{digest}").as_str())
    );
    assert_eq!(put.header("docker-content-digest"), Some(digest.as_str()));

    let get = h.get("/v2/homebrew/core/hello/manifests/v1").await;
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(&get.body[..], &body[..], "manifests are stored byte-exact");
    assert_eq!(get.header(header::CONTENT_TYPE), Some(IMAGE_MANIFEST));
    assert_eq!(get.header("docker-content-digest"), Some(digest.as_str()));
    assert_eq!(
        get.header(header::CONTENT_LENGTH),
        Some(body.len().to_string().as_str())
    );

    // Reachable by digest as well as by tag.
    let by_digest = h
        .get(&format!("/v2/homebrew/core/hello/manifests/{digest}"))
        .await;
    assert_eq!(by_digest.status, StatusCode::OK);
    assert_eq!(&by_digest.body[..], &body[..]);
}

#[tokio::test]
async fn manifest_head_carries_content_length_and_an_empty_body() {
    let h = Harness::new();
    let body = manifest_bytes();
    let digest = h
        .registry
        .seed_manifest("demo/app", Some("v1"), IMAGE_MANIFEST, &body);

    let reply = h.head("/v2/demo/app/manifests/v1").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header(header::CONTENT_LENGTH),
        Some(body.len().to_string().as_str()),
        "HEAD MUST report the real size"
    );
    assert!(reply.body.is_empty(), "HEAD MUST have an empty body");
    assert_eq!(
        reply.header("docker-content-digest"),
        Some(digest.to_string().as_str())
    );
    assert_eq!(reply.header(header::CONTENT_TYPE), Some(IMAGE_MANIFEST));
}

#[tokio::test]
async fn a_content_type_parameter_does_not_survive_the_round_trip() {
    let h = Harness::new();
    let put = h
        .request(
            Method::PUT,
            "/v2/demo/app/manifests/v1",
            vec![(
                header::CONTENT_TYPE.as_str(),
                format!("{IMAGE_MANIFEST}; charset=utf-8"),
            )],
            Body::from(manifest_bytes()),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
    let get = h.get("/v2/demo/app/manifests/v1").await;
    assert_eq!(get.header(header::CONTENT_TYPE), Some(IMAGE_MANIFEST));
}

#[tokio::test]
async fn a_manifest_is_served_regardless_of_accept() {
    let h = Harness::new();
    h.registry
        .seed_manifest("demo/app", Some("v1"), IMAGE_MANIFEST, &manifest_bytes());
    let reply = h
        .request(
            Method::GET,
            "/v2/demo/app/manifests/v1",
            vec![(header::ACCEPT.as_str(), "text/plain".to_owned())],
            Body::empty(),
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "an Accept mismatch must not become a 404"
    );
}

#[tokio::test]
async fn a_colon_bearing_reference_is_a_digest_error_not_a_tag_miss() {
    let h = Harness::new();
    // The suite's `invalid-digest-format` case. PUT MUST be 400; GET may be
    // 400 or 404 and we choose 400, because the client's request was malformed
    // rather than merely unsatisfied.
    h.request(
        Method::PUT,
        "/v2/demo/app/manifests/sha256:baddigeststring",
        vec![(header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned())],
        Body::from(manifest_bytes()),
    )
    .await
    .assert_error(StatusCode::BAD_REQUEST, ErrorCode::DigestInvalid);

    h.get("/v2/demo/app/manifests/sha256:baddigeststring")
        .await
        .assert_error(StatusCode::BAD_REQUEST, ErrorCode::DigestInvalid);

    // Uppercase hex is outside the digest grammar even though it would parse.
    let upper = format!("sha256:{}", "AB".repeat(32));
    h.get(&format!("/v2/demo/app/manifests/{upper}"))
        .await
        .assert_error(StatusCode::BAD_REQUEST, ErrorCode::DigestInvalid);
}

#[tokio::test]
async fn a_tag_outside_the_grammar_is_rejected_by_method() {
    let h = Harness::new();
    // On a read, an unrepresentable reference cannot name anything.
    h.get("/v2/demo/app/manifests/-bad")
        .await
        .assert_error(StatusCode::NOT_FOUND, ErrorCode::ManifestUnknown);
    // On a write, the request is definitively invalid.
    h.request(
        Method::PUT,
        "/v2/demo/app/manifests/-bad",
        vec![(header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned())],
        Body::from(manifest_bytes()),
    )
    .await
    .assert_error(StatusCode::BAD_REQUEST, ErrorCode::ManifestInvalid);
}

#[tokio::test]
async fn pushing_by_digest_verifies_the_content() {
    let h = Harness::new();
    let wrong = sha256_hex(b"something else");
    h.request(
        Method::PUT,
        &format!("/v2/demo/app/manifests/{wrong}"),
        vec![(header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned())],
        Body::from(manifest_bytes()),
    )
    .await
    .assert_error(StatusCode::BAD_REQUEST, ErrorCode::DigestInvalid);
}

#[tokio::test]
async fn an_oversized_manifest_is_413() {
    let h = Harness::with_config(ServerConfig {
        max_manifest_bytes: 64,
        ..ServerConfig::default()
    });
    let big = vec![b'x'; 128];
    let reply = h
        .request(
            Method::PUT,
            "/v2/demo/app/manifests/v1",
            vec![
                (header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned()),
                (header::CONTENT_LENGTH.as_str(), big.len().to_string()),
            ],
            Body::from(big),
        )
        .await;
    reply.assert_error(StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::ManifestInvalid);
}

#[tokio::test]
async fn tag_parameters_are_applied_and_echoed() {
    let h = Harness::new();
    let body = manifest_bytes();
    let digest = sha256_hex(&body);

    let put = h
        .request(
            Method::PUT,
            &format!("/v2/demo/app/manifests/{digest}?tag=a&tag=b"),
            vec![(header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned())],
            Body::from(body),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
    let echoed: Vec<&str> = put
        .headers
        .get_all("oci-tag")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    assert_eq!(echoed, vec!["a", "b"]);

    let tags = h.get("/v2/demo/app/tags/list").await.json();
    assert_eq!(tags["tags"], serde_json::json!(["a", "b"]));
}

#[tokio::test]
async fn too_many_tag_parameters_is_414() {
    let h = Harness::with_config(ServerConfig {
        max_tag_params: 2,
        ..ServerConfig::default()
    });
    let body = manifest_bytes();
    let digest = sha256_hex(&body);
    h.request(
        Method::PUT,
        &format!("/v2/demo/app/manifests/{digest}?tag=a&tag=b&tag=c"),
        vec![(header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned())],
        Body::from(body),
    )
    .await
    .assert_error(StatusCode::URI_TOO_LONG, ErrorCode::ManifestInvalid);
}

#[tokio::test]
async fn a_subject_is_acknowledged_with_oci_subject() {
    let h = Harness::new();
    let subject = sha256_hex(b"the subject");
    let body = format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE_MANIFEST}","artifactType":"application/example","config":{{"mediaType":"application/vnd.oci.empty.v1+json","digest":"{}","size":2}},"layers":[],"subject":{{"mediaType":"{IMAGE_MANIFEST}","digest":"{subject}","size":10}}}}"#,
        sha256_hex(b"{}")
    );

    let put = h
        .request(
            Method::PUT,
            "/v2/demo/app/manifests/sig",
            vec![(header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned())],
            Body::from(body),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
    assert_eq!(
        put.header("oci-subject"),
        Some(subject.as_str()),
        "a subject that names a manifest which does not exist MUST still be accepted"
    );
}

#[tokio::test]
async fn a_conditional_get_returns_304() {
    let h = Harness::new();
    let digest =
        h.registry
            .seed_manifest("demo/app", Some("v1"), IMAGE_MANIFEST, &manifest_bytes());
    let etag = format!("\"{digest}\"");

    let reply = h
        .request(
            Method::GET,
            "/v2/demo/app/manifests/v1",
            vec![(header::IF_NONE_MATCH.as_str(), etag.clone())],
            Body::empty(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_MODIFIED);
    assert!(reply.body.is_empty());
    assert_eq!(reply.header(header::ETAG), Some(etag.as_str()));
}

#[tokio::test]
async fn deleting_by_digest_cascades_to_tags_and_by_tag_does_not() {
    let h = Harness::new();
    let body = manifest_bytes();
    let digest = h
        .registry
        .seed_manifest("demo/app", Some("v1"), IMAGE_MANIFEST, &body);
    h.registry
        .seed_manifest("demo/app", Some("v2"), IMAGE_MANIFEST, &body);

    // Deleting one tag leaves the manifest, and the other tag, reachable.
    assert_eq!(
        h.delete("/v2/demo/app/manifests/v1").await.status,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        h.head("/v2/demo/app/manifests/v1").await.status,
        StatusCode::NOT_FOUND,
        "a delete is visible immediately, not eventually"
    );
    assert_eq!(
        h.head("/v2/demo/app/manifests/v2").await.status,
        StatusCode::OK
    );

    // Deleting by digest takes every remaining tag with it.
    assert_eq!(
        h.delete(&format!("/v2/demo/app/manifests/{digest}"))
            .await
            .status,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        h.head("/v2/demo/app/manifests/v2").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn an_unknown_repository_is_name_unknown() {
    let h = Harness::new();
    h.get("/v2/no/such/repo/manifests/v1")
        .await
        .assert_error(StatusCode::NOT_FOUND, ErrorCode::NameUnknown);
    h.get("/v2/no/such/repo/tags/list")
        .await
        .assert_error(StatusCode::NOT_FOUND, ErrorCode::NameUnknown);
}

// ------------------------------------------------------------- pagination --

async fn seeded_tags(h: &Harness) {
    for tag in ["v1", "v2", "v3", "v4", "v5"] {
        h.registry
            .seed_manifest("demo/app", Some(tag), IMAGE_MANIFEST, tag.as_bytes());
    }
}

#[tokio::test]
async fn tags_are_byte_ordered_and_last_is_exclusive() {
    let h = Harness::new();
    seeded_tags(&h).await;

    let body = h.get("/v2/demo/app/tags/list").await.json();
    assert_eq!(body["name"], "demo/app");
    assert_eq!(
        body["tags"],
        serde_json::json!(["v1", "v2", "v3", "v4", "v5"])
    );

    let body = h.get("/v2/demo/app/tags/list?last=v3").await.json();
    assert_eq!(
        body["tags"],
        serde_json::json!(["v4", "v5"]),
        "`last` is exclusive"
    );
}

#[tokio::test]
async fn link_is_emitted_only_when_a_further_page_exists() {
    let h = Harness::new();
    seeded_tags(&h).await;

    let page = h.get("/v2/demo/app/tags/list?n=2").await;
    assert_eq!(page.json()["tags"], serde_json::json!(["v1", "v2"]));
    assert_eq!(
        page.header(header::LINK),
        Some("</v2/demo/app/tags/list?last=v2&n=2>; rel=\"next\"")
    );

    // A page that exactly consumes the range has nothing after it. The
    // reference implementation sends `Link` here anyway and costs every client
    // a wasted request.
    let exact = h.get("/v2/demo/app/tags/list?n=5").await;
    assert_eq!(exact.header(header::LINK), None);

    let last_page = h.get("/v2/demo/app/tags/list?n=2&last=v3").await;
    assert_eq!(last_page.json()["tags"], serde_json::json!(["v4", "v5"]));
    assert_eq!(last_page.header(header::LINK), None);
}

#[tokio::test]
async fn n_zero_is_an_empty_list_with_no_link() {
    let h = Harness::new();
    seeded_tags(&h).await;
    let reply = h.get("/v2/demo/app/tags/list?n=0").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["tags"], serde_json::json!([]));
    assert_eq!(reply.header(header::LINK), None);
}

#[tokio::test]
async fn a_malformed_n_is_pagination_number_invalid() {
    let h = Harness::new();
    seeded_tags(&h).await;
    for query in ["n=-1", "n=abc", "n=1.5"] {
        h.get(&format!("/v2/demo/app/tags/list?{query}"))
            .await
            .assert_error(StatusCode::BAD_REQUEST, ErrorCode::PaginationNumberInvalid);
    }
}

#[tokio::test]
async fn an_oversized_n_is_clamped_rather_than_rejected() {
    let h = Harness::with_config(ServerConfig {
        max_page_size: 3,
        ..ServerConfig::default()
    });
    seeded_tags(&h).await;
    let reply = h.get("/v2/demo/app/tags/list?n=1000000").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["tags"], serde_json::json!(["v1", "v2", "v3"]));
    assert_eq!(
        reply.header(header::LINK),
        Some("</v2/demo/app/tags/list?last=v3&n=3>; rel=\"next\"")
    );
}

#[tokio::test]
async fn the_catalog_pages_over_names_and_escapes_the_cursor() {
    let h = Harness::new();
    for repo in ["conformance/repo1", "conformance/repo2", "other"] {
        h.registry
            .seed_manifest(repo, Some("v1"), IMAGE_MANIFEST, b"{}");
    }

    let all = h.get("/v2/_catalog").await;
    assert_eq!(
        all.json()["repositories"],
        serde_json::json!(["conformance/repo1", "conformance/repo2", "other"])
    );
    assert_eq!(all.header(header::LINK), None);

    let first = h.get("/v2/_catalog?n=1").await;
    assert_eq!(
        first.json()["repositories"],
        serde_json::json!(["conformance/repo1"])
    );
    assert_eq!(
        first.header(header::LINK),
        Some("</v2/_catalog?last=conformance%2Frepo1&n=1>; rel=\"next\"")
    );
}

// ------------------------------------------------------------------ blobs --

fn blob_2048() -> Vec<u8> {
    (0..2048u32).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn a_blob_is_served_with_its_digest_and_length() {
    let h = Harness::new();
    let bytes = blob_2048();
    let digest = h.registry.seed_blob("demo/app", &bytes);

    let get = h.get(&format!("/v2/demo/app/blobs/{digest}")).await;
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(&get.body[..], &bytes[..]);
    assert_eq!(get.header(header::CONTENT_LENGTH), Some("2048"));
    assert_eq!(get.header(header::ACCEPT_RANGES), Some("bytes"));
    assert_eq!(
        get.header("docker-content-digest"),
        Some(digest.to_string().as_str())
    );
    assert_eq!(
        get.header(header::CONTENT_ENCODING),
        None,
        "a blob body is never transformed; the digest is over the plaintext"
    );

    let head = h.head(&format!("/v2/demo/app/blobs/{digest}")).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header(header::CONTENT_LENGTH), Some("2048"));
    assert!(head.body.is_empty());
}

#[tokio::test]
async fn the_six_range_cases_answer_as_the_suite_expects() {
    let h = Harness::new();
    let bytes = blob_2048();
    let digest = h.registry.seed_blob("demo/app", &bytes);
    let uri = format!("/v2/demo/app/blobs/{digest}");

    for (range, length, content_range, span) in [
        ("bytes=500-1499", "1000", "bytes 500-1499/2048", 500..1500),
        ("bytes=500-", "1548", "bytes 500-2047/2048", 500..2048),
        ("bytes=-500", "500", "bytes 1548-2047/2048", 1548..2048),
        ("bytes=2000-5000", "48", "bytes 2000-2047/2048", 2000..2048),
    ] {
        let reply = h
            .request(
                Method::GET,
                &uri,
                vec![(header::RANGE.as_str(), range.to_owned())],
                Body::empty(),
            )
            .await;
        assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(
            reply.header(header::CONTENT_LENGTH),
            Some(length),
            "{range}"
        );
        assert_eq!(
            reply.header(header::CONTENT_RANGE),
            Some(content_range),
            "{range}"
        );
        assert_eq!(&reply.body[..], &bytes[span], "{range}");
    }

    for range in ["bytes=500-0", "bytes=5000-10000"] {
        let reply = h
            .request(
                Method::GET,
                &uri,
                vec![(header::RANGE.as_str(), range.to_owned())],
                Body::empty(),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::RANGE_NOT_SATISFIABLE,
            "{range} must not fall back to a 200"
        );
        assert_eq!(reply.header(header::CONTENT_RANGE), Some("bytes */2048"));
        assert!(reply.body.is_empty());
    }
}

#[tokio::test]
async fn an_empty_blob_round_trips() {
    let h = Harness::new();
    let digest = h.registry.seed_blob("demo/app", b"");
    let reply = h.get(&format!("/v2/demo/app/blobs/{digest}")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header(header::CONTENT_LENGTH),
        Some("0"),
        "a zero-length body must still declare its length"
    );
    assert_eq!(
        digest.to_string(),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[tokio::test]
async fn unknown_and_malformed_blobs_are_distinguished() {
    let h = Harness::new();
    h.registry.seed_blob("demo/app", b"present");
    let missing = sha256_hex(b"absent");
    h.get(&format!("/v2/demo/app/blobs/{missing}"))
        .await
        .assert_error(StatusCode::NOT_FOUND, ErrorCode::BlobUnknown);
    h.get("/v2/demo/app/blobs/sha256:nonsense")
        .await
        .assert_error(StatusCode::BAD_REQUEST, ErrorCode::DigestInvalid);
}

#[tokio::test]
async fn blob_membership_is_per_repository() {
    let h = Harness::new();
    let digest = h.registry.seed_blob("demo/one", b"shared");
    h.registry
        .seed_manifest("demo/two", None, IMAGE_MANIFEST, b"{}");

    assert_eq!(
        h.head(&format!("/v2/demo/one/blobs/{digest}")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        h.head(&format!("/v2/demo/two/blobs/{digest}")).await.status,
        StatusCode::NOT_FOUND,
        "a blob present elsewhere must not leak across repositories"
    );
}

#[tokio::test]
async fn deleting_a_blob_from_one_repository_leaves_the_other() {
    let h = Harness::new();
    let bytes = b"shared layer";
    let digest = h.registry.seed_blob("demo/one", bytes);
    h.registry.seed_blob("demo/two", bytes);

    assert_eq!(
        h.delete(&format!("/v2/demo/one/blobs/{digest}"))
            .await
            .status,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        h.head(&format!("/v2/demo/one/blobs/{digest}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.head(&format!("/v2/demo/two/blobs/{digest}")).await.status,
        StatusCode::OK
    );

    h.delete(&format!("/v2/demo/one/blobs/{digest}"))
        .await
        .assert_error(StatusCode::NOT_FOUND, ErrorCode::BlobUnknown);
}

// ---------------------------------------------------------------- uploads --

/// `POST` a session and return its `Location`.
async fn open_upload(h: &Harness, uri: &str) -> String {
    let reply = h
        .request(Method::POST, uri, Vec::new(), Body::empty())
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.header(header::RANGE), Some("0-0"));
    reply
        .header(header::LOCATION)
        .expect("a 202 must carry a Location")
        .to_owned()
}

#[tokio::test]
async fn flow_a_post_then_put() {
    let h = Harness::new();
    let bytes = blob_2048();
    let digest = sha256_hex(&bytes);
    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/").await;

    let put = h
        .request(
            Method::PUT,
            &format!("{location}?digest={digest}"),
            vec![(header::CONTENT_LENGTH.as_str(), bytes.len().to_string())],
            Body::from(bytes.clone()),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
    assert_eq!(put.header("docker-content-digest"), Some(digest.as_str()));

    // `Location` on the 201 is a pullable blob URL, not the upload URL: the
    // suite immediately GETs it and byte-compares.
    let blob_url = put.header(header::LOCATION).expect("Location");
    assert_eq!(blob_url, format!("/v2/demo/app/blobs/{digest}"));
    let get = h.get(blob_url).await;
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(&get.body[..], &bytes[..]);
}

#[tokio::test]
async fn flow_b_single_post() {
    let h = Harness::new();
    let bytes = b"one shot".to_vec();
    let digest = sha256_hex(&bytes);

    let reply = h
        .request(
            Method::POST,
            &format!("/v2/demo/app/blobs/uploads/?digest={digest}"),
            vec![(header::CONTENT_LENGTH.as_str(), bytes.len().to_string())],
            Body::from(bytes),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(
        reply.header(header::LOCATION),
        Some(format!("/v2/demo/app/blobs/{digest}").as_str())
    );
    assert_eq!(reply.header("docker-content-digest"), Some(digest.as_str()));
}

#[tokio::test]
async fn flow_c_streamed_patch_needs_no_content_range() {
    let h = Harness::new();
    let bytes = b"streamed with chunked transfer encoding".to_vec();
    let digest = sha256_hex(&bytes);
    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/").await;

    // No `Content-Range` and no `Content-Length`: this is what docker push and
    // BuildKit actually send, and requiring a range here would break them.
    let patch = h
        .request(
            Method::PATCH,
            &location,
            Vec::new(),
            Body::from(bytes.clone()),
        )
        .await;
    assert_eq!(patch.status, StatusCode::ACCEPTED);
    assert_eq!(
        patch.header(header::RANGE),
        Some(format!("0-{}", bytes.len() - 1).as_str())
    );

    let put = h
        .request(
            Method::PUT,
            &format!("{location}?digest={digest}"),
            vec![(header::CONTENT_LENGTH.as_str(), "0".to_owned())],
            Body::empty(),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
}

#[tokio::test]
async fn flow_d_chunked_patch_reports_the_last_written_byte() {
    let h = Harness::new();
    let bytes = blob_2048();
    let digest = sha256_hex(&bytes);
    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/").await;

    for (start, end) in [(0usize, 1023usize), (1024, 2047)] {
        let chunk = bytes[start..=end].to_vec();
        let reply = h
            .request(
                Method::PATCH,
                &location,
                vec![
                    (header::CONTENT_RANGE.as_str(), format!("{start}-{end}")),
                    (header::CONTENT_LENGTH.as_str(), chunk.len().to_string()),
                ],
                Body::from(chunk),
            )
            .await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
        assert_eq!(
            reply.header(header::RANGE),
            Some(format!("0-{end}").as_str()),
            "Range names the last uploaded byte, not the next offset"
        );
    }

    let put = h
        .request(
            Method::PUT,
            &format!("{location}?digest={digest}"),
            vec![(header::CONTENT_LENGTH.as_str(), "0".to_owned())],
            Body::empty(),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
}

#[tokio::test]
async fn an_out_of_order_chunk_is_416_and_leaves_the_session_untouched() {
    let h = Harness::new();
    let bytes = blob_2048();
    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/").await;

    let first = bytes[0..1024].to_vec();
    let reply = h
        .request(
            Method::PATCH,
            &location,
            vec![
                (header::CONTENT_RANGE.as_str(), "0-1023".to_owned()),
                (header::CONTENT_LENGTH.as_str(), "1024".to_owned()),
            ],
            Body::from(first),
        )
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);

    // A chunk that starts anywhere but the committed offset.
    let bad = bytes[1536..2048].to_vec();
    let reply = h
        .request(
            Method::PATCH,
            &location,
            vec![
                (header::CONTENT_RANGE.as_str(), "1536-2047".to_owned()),
                (header::CONTENT_LENGTH.as_str(), "512".to_owned()),
            ],
            Body::from(bad),
        )
        .await;
    reply.assert_error(
        StatusCode::RANGE_NOT_SATISFIABLE,
        ErrorCode::BlobUploadInvalid,
    );

    // end-13: `204`, not `200`, and the offset is exactly where it was.
    let status = h.get(&location).await;
    assert_eq!(status.status, StatusCode::NO_CONTENT);
    assert_eq!(status.header(header::RANGE), Some("0-1023"));
    assert_eq!(status.header(header::LOCATION), Some(location.as_str()));

    // The recovery path works: the correct next chunk still lands.
    let reply = h
        .request(
            Method::PATCH,
            &location,
            vec![
                (header::CONTENT_RANGE.as_str(), "1024-2047".to_owned()),
                (header::CONTENT_LENGTH.as_str(), "1024".to_owned()),
            ],
            Body::from(bytes[1024..2048].to_vec()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.header(header::RANGE), Some("0-2047"));
}

#[tokio::test]
async fn an_out_of_order_final_chunk_on_the_put_is_also_416() {
    let h = Harness::new();
    let bytes = blob_2048();
    let digest = sha256_hex(&bytes);
    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/").await;

    h.request(
        Method::PATCH,
        &location,
        vec![
            (header::CONTENT_RANGE.as_str(), "0-1023".to_owned()),
            (header::CONTENT_LENGTH.as_str(), "1024".to_owned()),
        ],
        Body::from(bytes[0..1024].to_vec()),
    )
    .await;

    h.request(
        Method::PUT,
        &format!("{location}?digest={digest}"),
        vec![
            (header::CONTENT_RANGE.as_str(), "2000-2047".to_owned()),
            (header::CONTENT_LENGTH.as_str(), "48".to_owned()),
        ],
        Body::from(bytes[2000..2048].to_vec()),
    )
    .await
    .assert_error(
        StatusCode::RANGE_NOT_SATISFIABLE,
        ErrorCode::BlobUploadInvalid,
    );
}

#[tokio::test]
async fn the_download_content_range_grammar_is_rejected_on_an_upload() {
    let h = Harness::new();
    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/").await;

    // Chunked upload takes a bare `start-end`. The RFC 9110 form belongs to
    // blob *download* and must not be silently accepted here.
    for wrong in ["bytes 0-1023/2048", "bytes=0-1023", "0-1023/2048"] {
        h.request(
            Method::PATCH,
            &location,
            vec![
                (header::CONTENT_RANGE.as_str(), wrong.to_owned()),
                (header::CONTENT_LENGTH.as_str(), "1024".to_owned()),
            ],
            Body::from(vec![0u8; 1024]),
        )
        .await
        .assert_error(StatusCode::BAD_REQUEST, ErrorCode::BlobUploadInvalid);
    }
}

#[tokio::test]
async fn a_content_length_that_disagrees_with_the_range_is_size_invalid() {
    let h = Harness::new();
    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/").await;
    h.request(
        Method::PATCH,
        &location,
        vec![
            (header::CONTENT_RANGE.as_str(), "0-1023".to_owned()),
            (header::CONTENT_LENGTH.as_str(), "512".to_owned()),
        ],
        Body::from(vec![0u8; 512]),
    )
    .await
    .assert_error(StatusCode::BAD_REQUEST, ErrorCode::SizeInvalid);
}

#[tokio::test]
async fn closing_an_upload_verifies_the_digest() {
    let h = Harness::new();
    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/").await;
    h.request(
        Method::PATCH,
        &location,
        Vec::new(),
        Body::from(b"the real bytes".to_vec()),
    )
    .await;

    let wrong = sha256_hex(b"different bytes");
    h.request(
        Method::PUT,
        &format!("{location}?digest={wrong}"),
        vec![(header::CONTENT_LENGTH.as_str(), "0".to_owned())],
        Body::empty(),
    )
    .await
    .assert_error(StatusCode::BAD_REQUEST, ErrorCode::DigestInvalid);

    // A close with no `?digest=` at all cannot be verified, so it is refused.
    h.request(
        Method::PUT,
        &location,
        vec![(header::CONTENT_LENGTH.as_str(), "0".to_owned())],
        Body::empty(),
    )
    .await
    .assert_error(StatusCode::BAD_REQUEST, ErrorCode::DigestInvalid);
}

#[tokio::test]
async fn an_upload_can_be_cancelled() {
    let h = Harness::new();
    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/").await;
    let reply = h.delete(&location).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);

    h.get(&location)
        .await
        .assert_error(StatusCode::NOT_FOUND, ErrorCode::BlobUploadUnknown);
}

#[tokio::test]
async fn sha512_works_end_to_end() {
    let h = Harness::new();
    let bytes = b"hashed under sha512".to_vec();
    let digest = sha512_hex(&bytes);

    let location = open_upload(&h, "/v2/demo/app/blobs/uploads/?digest-algorithm=sha512").await;
    let put = h
        .request(
            Method::PUT,
            &format!("{location}?digest={digest}"),
            vec![(header::CONTENT_LENGTH.as_str(), bytes.len().to_string())],
            Body::from(bytes.clone()),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
    assert_eq!(
        put.header("docker-content-digest"),
        Some(digest.as_str()),
        "the digest is echoed under the algorithm the client chose"
    );

    let get = h.get(&format!("/v2/demo/app/blobs/{digest}")).await;
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(&get.body[..], &bytes[..]);
}

#[tokio::test]
async fn an_unsupported_digest_algorithm_is_rejected_at_the_post() {
    let h = Harness::new();
    h.request(
        Method::POST,
        "/v2/demo/app/blobs/uploads/?digest-algorithm=md5",
        Vec::new(),
        Body::empty(),
    )
    .await
    .assert_error(StatusCode::BAD_REQUEST, ErrorCode::DigestInvalid);
}

#[tokio::test]
async fn a_mount_succeeds_or_degrades_to_a_session() {
    let h = Harness::new();
    let bytes = b"a shared base layer".to_vec();
    let digest = h.registry.seed_blob("demo/source", &bytes);

    let mounted = h
        .request(
            Method::POST,
            &format!("/v2/demo/target/blobs/uploads/?mount={digest}&from=demo/source"),
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(mounted.status, StatusCode::CREATED);
    assert_eq!(
        mounted.header(header::LOCATION),
        Some(format!("/v2/demo/target/blobs/{digest}").as_str())
    );
    assert_eq!(
        mounted.header("docker-content-digest"),
        Some(digest.to_string().as_str())
    );
    assert_eq!(
        h.head(&format!("/v2/demo/target/blobs/{digest}"))
            .await
            .status,
        StatusCode::OK
    );

    // Anonymous mount: `from` is optional, and a registry-wide blob record
    // makes "present anywhere?" one lookup.
    let anonymous = h
        .request(
            Method::POST,
            &format!("/v2/demo/third/blobs/uploads/?mount={digest}"),
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(anonymous.status, StatusCode::CREATED);

    // A blob nobody has: refusal is a `202` with an ordinary upload session,
    // not an error.
    let missing = sha256_hex(b"nowhere to be found");
    let refused = h
        .request(
            Method::POST,
            &format!("/v2/demo/target/blobs/uploads/?mount={missing}&from=demo/source"),
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(refused.status, StatusCode::ACCEPTED);
    assert!(refused
        .header(header::LOCATION)
        .is_some_and(|l| l.contains("/blobs/uploads/")));
}

// -------------------------------------------------------------- referrers --

#[tokio::test]
async fn referrers_is_404_while_disabled_but_still_validates_the_digest() {
    let h = Harness::new();
    let subject = sha256_hex(b"subject");
    let reply = h.get(&format!("/v2/demo/app/referrers/{subject}")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // A malformed digest is a `400` whether or not the endpoint is enabled.
    h.get("/v2/demo/app/referrers/sha256:nonsense")
        .await
        .assert_error(StatusCode::BAD_REQUEST, ErrorCode::DigestInvalid);
}

#[tokio::test]
async fn referrers_never_404s_once_enabled() {
    let h = Harness::with_config(ServerConfig {
        referrers_enabled: true,
        ..ServerConfig::default()
    });
    let unknown = sha256_hex(b"a subject nobody pushed");

    // Unknown subject *and* unknown repository: still `200` with an empty list.
    let reply = h.get(&format!("/v2/demo/app/referrers/{unknown}")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header(header::CONTENT_TYPE),
        Some("application/vnd.oci.image.index.v1+json")
    );
    let body = reply.json();
    assert_eq!(body["schemaVersion"], 2);
    assert_eq!(body["mediaType"], "application/vnd.oci.image.index.v1+json");
    assert_eq!(body["manifests"], serde_json::json!([]));
}

#[tokio::test]
async fn referrers_lists_and_filters() {
    let h = Harness::with_config(ServerConfig {
        referrers_enabled: true,
        ..ServerConfig::default()
    });
    let subject_digest =
        h.registry
            .seed_manifest("demo/app", Some("v1"), IMAGE_MANIFEST, b"{\"subject\":1}");

    let sig = h
        .registry
        .seed_manifest("demo/app", None, IMAGE_MANIFEST, b"{\"kind\":\"sig\"}");
    h.registry
        .seed_subject("demo/app", &sig, subject_digest, Some("application/sig"));
    let sbom = h
        .registry
        .seed_manifest("demo/app", None, IMAGE_MANIFEST, b"{\"kind\":\"sbom\"}");
    h.registry
        .seed_subject("demo/app", &sbom, subject_digest, Some("application/sbom"));

    let all = h
        .get(&format!("/v2/demo/app/referrers/{subject_digest}"))
        .await;
    assert_eq!(all.status, StatusCode::OK);
    assert_eq!(all.json()["manifests"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        all.header("oci-filters-applied"),
        None,
        "no filter was requested, so none may be claimed"
    );

    let filtered = h
        .get(&format!(
            "/v2/demo/app/referrers/{subject_digest}?artifactType=application/sig"
        ))
        .await;
    let manifests = filtered.json()["manifests"].clone();
    assert_eq!(manifests.as_array().map(Vec::len), Some(1));
    assert_eq!(manifests[0]["artifactType"], "application/sig");
    assert_eq!(manifests[0]["digest"], sig.to_string());
    assert_eq!(filtered.header("oci-filters-applied"), Some("artifactType"));
}
