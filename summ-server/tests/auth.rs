//! API-key authentication, driven through the router.
//!
//! `src/auth.rs` unit-tests the decision - which key admits which method, how a
//! credential is parsed out of a header. This file tests that the decision is
//! actually *applied*, which is a different claim and the one that breaks: a
//! policy is only as good as the set of routes it covers, and the failure mode
//! is one surface quietly left open. So every test here goes through
//! [`summ_server::router`] and the assertions are about `/v2/`, `/api/v1/` and
//! the UI together.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use summ_server::auth::{ApiKey, AuthPolicy};
use summ_server::config::ServerConfig;
use summ_server::error::ErrorCode;
use summ_server::memory::MemoryRegistry;
use summ_server::{router, AppState};
use tower::ServiceExt;

const READ_KEY: &str = "read-key-0123456789";
const WRITE_KEY: &str = "write-key-9876543210";

const IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// Every route the server answers, one per shape it is reached by.
///
/// The list is the point of this file: the `/v2/` dispatcher, the discovery
/// dispatcher, a UI asset and the UI shell each arrive through a different arm
/// of [`summ_server::router`], and a policy attached to only some of them would
/// pass any test that checked one.
const READABLE: &[&str] = &[
    "/v2/",
    "/v2/_catalog",
    "/v2/lib/nginx/tags/list",
    "/v2/lib/nginx/manifests/v1",
    "/api/v1/repositories",
    "/api/v1/tags/lib/nginx",
    "/app.css",
    "/app.js",
    "/logo.svg",
    "/",
    "/r/lib/nginx",
];

// ---------------------------------------------------------------- harness --

struct Harness {
    app: Router,
    registry: Arc<MemoryRegistry>,
}

impl Harness {
    /// A registry requiring the two keys above.
    fn guarded() -> Self {
        Self::with_auth(AuthPolicy::ApiKey {
            read: ApiKey::new(READ_KEY),
            write: ApiKey::new(WRITE_KEY),
        })
    }

    fn anonymous() -> Self {
        Self::with_auth(AuthPolicy::Anonymous)
    }

    fn with_auth(auth: AuthPolicy) -> Self {
        let registry = Arc::new(MemoryRegistry::new());
        let config = ServerConfig {
            auth,
            ..ServerConfig::default()
        };
        let app = router(AppState::new(registry.clone(), config));
        Harness { app, registry }
    }

    /// One tagged manifest, put there without going through HTTP, so a read
    /// test has something to read that no write test had to be allowed to
    /// create.
    fn seed(&self) {
        self.registry
            .seed_manifest("lib/nginx", Some("v1"), IMAGE_MANIFEST, &manifest_bytes());
    }

    async fn send(&self, method: Method, uri: &str, credential: Option<&str>, body: Body) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(credential) = credential {
            builder = builder.header(header::AUTHORIZATION, credential);
        }
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).expect("valid request"))
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

    async fn get(&self, uri: &str, credential: Option<&str>) -> Reply {
        self.send(Method::GET, uri, credential, Body::empty()).await
    }

    /// A manifest `PUT` - the shortest complete write the registry has.
    async fn push(&self, credential: Option<&str>) -> Reply {
        let mut builder = Request::builder()
            .method(Method::PUT)
            .uri("/v2/lib/nginx/manifests/pushed")
            .header(header::CONTENT_TYPE, IMAGE_MANIFEST);
        if let Some(credential) = credential {
            builder = builder.header(header::AUTHORIZATION, credential);
        }
        let request = builder
            .body(Body::from(manifest_bytes()))
            .expect("valid request");
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

    /// Assert the spec's error envelope and return the code.
    fn error_code(&self) -> String {
        assert_eq!(
            self.header(header::CONTENT_TYPE),
            Some("application/json"),
            "an error body must be JSON, got {:?}",
            String::from_utf8_lossy(&self.body)
        );
        let body: serde_json::Value = serde_json::from_slice(&self.body).expect("JSON body");
        let errors = body["errors"].as_array().expect("`errors` is an array");
        assert_eq!(errors.len(), 1);
        errors[0]["code"]
            .as_str()
            .expect("`code` is a string")
            .to_owned()
    }
}

/// `Basic` with the key as the password, which is what `docker login`,
/// `oras login` and a browser all send.
fn basic(user: &str, password: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{password}").into_bytes();
    let mut out = String::new();
    for chunk in raw.chunks(3) {
        let mut acc = 0u32;
        for (i, &byte) in chunk.iter().enumerate() {
            acc |= u32::from(byte) << (16 - 8 * i);
        }
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[((acc >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
        for _ in 0..3 - chunk.len() {
            out.push('=');
        }
    }
    format!("Basic {out}")
}

fn manifest_bytes() -> Vec<u8> {
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":2},"layers":[]}"#
        .to_vec()
}

// ------------------------------------------------------------- anonymous --

#[tokio::test]
async fn anonymous_is_the_default_and_asks_for_nothing() {
    assert!(
        !ServerConfig::default().auth.is_enabled(),
        "`summ serve` with no arguments must stay an open registry"
    );

    let h = Harness::anonymous();
    h.seed();
    for uri in READABLE {
        let reply = h.get(uri, None).await;
        assert!(reply.status.is_success(), "GET {uri} -> {}", reply.status);
        assert_eq!(
            reply.header(header::WWW_AUTHENTICATE),
            None,
            "an anonymous registry must not advertise a challenge on {uri}"
        );
    }
    assert_eq!(h.push(None).await.status, StatusCode::CREATED);
}

// ------------------------------------------------------- the challenge --

#[tokio::test]
async fn every_surface_challenges_when_no_credential_is_sent() {
    let h = Harness::guarded();
    h.seed();
    for uri in READABLE {
        let reply = h.get(uri, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "GET {uri}");
        assert_eq!(reply.error_code(), ErrorCode::Unauthorized.as_str());
        let challenge = reply.header(header::WWW_AUTHENTICATE).unwrap_or_default();
        assert!(
            challenge.starts_with("Basic ") && challenge.contains(r#"realm="summ""#),
            "GET {uri} sent {challenge:?}; `Basic` is what docker and a browser \
             answer with the credentials they hold, and `Bearer` would send them \
             looking for a token server that does not exist"
        );
    }
    // The write surface challenges too, rather than reporting a 403 to someone
    // who has not been asked for a credential yet.
    let reply = h.push(None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.header(header::WWW_AUTHENTICATE).is_some());
}

#[tokio::test]
async fn the_v2_base_endpoint_is_the_discovery_point_and_is_not_exempt() {
    // A client learns it needs credentials from a `401` here (spec
    // §Authentication). Exempting `GET /v2/` to keep a ping working would
    // break the one flow it exists for.
    let h = Harness::guarded();
    assert_eq!(h.get("/v2/", None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(h.get("/v2", None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.get("/v2/", Some(&format!("Bearer {READ_KEY}")))
            .await
            .status,
        StatusCode::OK
    );
}

// --------------------------------------------------------------- the keys --

#[tokio::test]
async fn the_read_key_reads_every_surface() {
    let h = Harness::guarded();
    h.seed();
    let credential = basic("anyone", READ_KEY);
    for uri in READABLE {
        let reply = h.get(uri, Some(&credential)).await;
        assert!(reply.status.is_success(), "GET {uri} -> {}", reply.status);
    }
}

#[tokio::test]
async fn the_read_key_is_denied_a_write_and_the_write_does_not_happen() {
    let h = Harness::guarded();
    let read = basic("anyone", READ_KEY);
    let write = format!("Bearer {WRITE_KEY}");

    let reply = h.push(Some(&read)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.error_code(), ErrorCode::Denied.as_str());
    assert!(
        reply.header(header::WWW_AUTHENTICATE).is_none(),
        "a genuine credential that is merely insufficient must not be \
         re-challenged - the client would retry with the same key"
    );

    // The denial is a denial, not a slow accept.
    let reply = h.get("/v2/lib/nginx/manifests/pushed", Some(&write)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // Every other mutating method is refused the same way.
    for method in [Method::POST, Method::PATCH, Method::DELETE] {
        let reply = h
            .send(
                method.clone(),
                "/v2/lib/nginx/blobs/uploads/",
                Some(&read),
                Body::empty(),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method}");
    }
}

#[tokio::test]
async fn the_write_key_both_writes_and_reads() {
    let h = Harness::guarded();
    h.seed();
    let credential = basic("anyone", WRITE_KEY);

    assert_eq!(h.push(Some(&credential)).await.status, StatusCode::CREATED);
    for uri in READABLE {
        let reply = h.get(uri, Some(&credential)).await;
        assert!(
            reply.status.is_success(),
            "a CI job holding only the write key must still be able to pull: \
             GET {uri} -> {}",
            reply.status
        );
    }
}

// -------------------------------------------------------- the credential --

#[tokio::test]
async fn basic_and_bearer_carry_the_same_key() {
    let h = Harness::guarded();
    for credential in [
        // What `docker login -u <anything> -p <key>` sends.
        basic("summ", WRITE_KEY),
        basic("", WRITE_KEY),
        // What `curl -u <key>:` sends.
        basic(WRITE_KEY, ""),
        // What a person with `curl -H` reaches for.
        format!("Bearer {WRITE_KEY}"),
        format!("bearer {WRITE_KEY}"),
    ] {
        let reply = h.get("/v2/", Some(&credential)).await;
        assert_eq!(reply.status, StatusCode::OK, "{credential}");
    }
}

#[tokio::test]
async fn a_wrong_or_unreadable_credential_is_challenged_again() {
    let h = Harness::guarded();
    for credential in [
        basic("anyone", "not-the-key"),
        format!("Bearer {READ_KEY}-almost"),
        // A prefix of a real key must not pass; nor may the empty string.
        format!("Bearer {}", &READ_KEY[..5]),
        "Bearer ".to_owned(),
        "Basic ".to_owned(),
        "Basic !!!not-base64".to_owned(),
        // A scheme with no token endpoint behind it.
        "Digest username=\"x\"".to_owned(),
        "nonsense".to_owned(),
    ] {
        let reply = h.get("/v2/", Some(&credential)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{credential:?}");
        assert_eq!(reply.error_code(), ErrorCode::Unauthorized.as_str());
        assert!(
            reply.header(header::WWW_AUTHENTICATE).is_some(),
            "{credential:?} should be re-challenged"
        );
    }
}

#[tokio::test]
async fn the_read_key_is_not_accepted_where_the_write_key_is_expected_by_luck() {
    // Guards the ordering in `AuthPolicy::authorize`: write is tried first, so
    // a read key must not be admitted to a write by falling through.
    let h = Harness::guarded();
    let reply = h.push(Some(&format!("Bearer {READ_KEY}"))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------- leakage --

#[tokio::test]
async fn a_401_body_names_no_key() {
    let h = Harness::guarded();
    let body = h.get("/v2/", None).await.body;
    let body = String::from_utf8_lossy(&body).into_owned();
    assert!(!body.contains(READ_KEY), "{body}");
    assert!(!body.contains(WRITE_KEY), "{body}");

    let rendered = format!(
        "{:?}",
        ServerConfig {
            auth: AuthPolicy::ApiKey {
                read: ApiKey::new(READ_KEY),
                write: ApiKey::new(WRITE_KEY),
            },
            ..ServerConfig::default()
        }
    );
    assert!(!rendered.contains(READ_KEY), "{rendered}");
    assert!(!rendered.contains(WRITE_KEY), "{rendered}");
}
