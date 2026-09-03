//! The `/v2/` surface over the real storage stack.
//!
//! `api.rs` drives the same router against `MemoryRegistry` and proves the
//! handlers. This file proves the wiring: that `summ-registry`, `summ-meta` and
//! `summ-storage` behind `seam::Registry` behave as the handlers were written
//! to expect, and - the part no in-memory implementation can check at all -
//! that what was pushed is still there once the process that took it has gone.
//!
//! Every test therefore runs against a real `Backend` on a `tempfile::TempDir`,
//! and the ones that matter most reopen it. A registry that loses a push on
//! restart passes every test in `api.rs`.

use std::path::Path;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use sha2::{Digest as _, Sha256};
use summ_registry::RegistryOptions;
use summ_server::backend::{Backend, Engine};
use summ_server::config::ServerConfig;
use summ_server::{router, AppState};
use tempfile::TempDir;
use tower::ServiceExt;

// ---------------------------------------------------------------- harness --

struct Harness {
    app: Router,
}

impl Harness {
    /// Open a registry on `dir`. Called twice on the same directory by the
    /// persistence tests, which is the whole point of taking a path rather than
    /// making its own.
    fn open(dir: &Path, engine: Engine, options: RegistryOptions) -> Self {
        let backend = Backend::open(dir, engine, options).expect("backend opens");
        Harness {
            app: router(AppState::new(Arc::new(backend), ServerConfig::default())),
        }
    }

    fn rocks(dir: &Path) -> Self {
        Self::open(dir, Engine::Rocks, RegistryOptions::default())
    }

    fn with_config(dir: &Path, config: ServerConfig) -> Self {
        let backend =
            Backend::open(dir, Engine::Rocks, RegistryOptions::default()).expect("backend opens");
        Harness {
            app: router(AppState::new(Arc::new(backend), config)),
        }
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

    async fn get(&self, uri: &str) -> Reply {
        self.request(Method::GET, uri, Vec::new(), Body::empty())
            .await
    }

    async fn head(&self, uri: &str) -> Reply {
        self.request(Method::HEAD, uri, Vec::new(), Body::empty())
            .await
    }

    /// The two-step blob push: open a session, close it with the digest.
    async fn push_blob(&self, repo: &str, bytes: &[u8]) -> String {
        let digest = sha256_hex(bytes);
        let opened = self
            .request(
                Method::POST,
                &format!("/v2/{repo}/blobs/uploads/"),
                Vec::new(),
                Body::empty(),
            )
            .await;
        assert_eq!(opened.status, StatusCode::ACCEPTED, "opening an upload");
        let location = opened
            .header(header::LOCATION)
            .expect("Location")
            .to_owned();

        let closed = self
            .request(
                Method::PUT,
                &format!("{location}?digest={digest}"),
                Vec::new(),
                Body::from(bytes.to_vec()),
            )
            .await;
        assert_eq!(closed.status, StatusCode::CREATED, "committing an upload");
        digest
    }

    /// One chunk of a chunked upload.
    ///
    /// Both headers, always. The handler treats a `Content-Range` without a
    /// `Content-Length` as a *streamed* `PATCH` and skips the offset check
    /// entirely, which is correct for a stream and silently turns an
    /// out-of-order-chunk test into an append.
    async fn patch_chunk(&self, location: &str, start: u64, chunk: &[u8]) -> Reply {
        let end = start + chunk.len() as u64 - 1;
        self.request(
            Method::PATCH,
            location,
            vec![
                (header::CONTENT_RANGE.as_str(), format!("{start}-{end}")),
                (header::CONTENT_LENGTH.as_str(), chunk.len().to_string()),
            ],
            Body::from(chunk.to_vec()),
        )
        .await
    }

    /// The closing `PUT`, which may carry a final chunk.
    async fn close_upload(&self, location: &str, digest: &str, start: u64, chunk: &[u8]) -> Reply {
        let mut headers = vec![];
        if !chunk.is_empty() {
            let end = start + chunk.len() as u64 - 1;
            headers.push((header::CONTENT_RANGE.as_str(), format!("{start}-{end}")));
            headers.push((header::CONTENT_LENGTH.as_str(), chunk.len().to_string()));
        }
        self.request(
            Method::PUT,
            &format!("{location}?digest={digest}"),
            headers,
            Body::from(chunk.to_vec()),
        )
        .await
    }

    async fn push_manifest(&self, repo: &str, reference: &str, body: &[u8]) -> Reply {
        self.request(
            Method::PUT,
            &format!("/v2/{repo}/manifests/{reference}"),
            vec![(header::CONTENT_TYPE.as_str(), IMAGE_MANIFEST.to_owned())],
            Body::from(body.to_vec()),
        )
        .await
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
        serde_json::from_slice(&self.body).expect("JSON body")
    }

    fn error_code(&self) -> String {
        self.json()["errors"][0]["code"]
            .as_str()
            .expect("an error code")
            .to_owned()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

const IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

const CONFIG: &[u8] = br#"{"architecture":"amd64","os":"linux"}"#;
const LAYER: &[u8] = b"the layer bytes, such as they are";

/// A manifest over [`CONFIG`] and [`LAYER`], laid out so the bytes are stable:
/// the digest is over exactly these, and several assertions compare it.
fn manifest() -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
        sha256_hex(CONFIG),
        CONFIG.len(),
        sha256_hex(LAYER),
        LAYER.len(),
    )
    .into_bytes()
}

/// Push both blobs and the manifest under `tag`. Returns the manifest digest.
async fn push_image(h: &Harness, repo: &str, tag: &str) -> String {
    h.push_blob(repo, CONFIG).await;
    h.push_blob(repo, LAYER).await;
    let body = manifest();
    let reply = h.push_manifest(repo, tag, &body).await;
    assert_eq!(reply.status, StatusCode::CREATED, "pushing the manifest");
    sha256_hex(&body)
}

// ---------------------------------------------------------------- discovery --

/// The discovery API over the real store, which is the only place several of
/// its fields are anything but zero.
///
/// `MemoryRegistry` recovers a manifest's shape by parsing the body back; the
/// backend reads a `ManifestRecord` written at push time, and `pushed_at`,
/// `total_layer_size` and the platform of an index child exist only there. A
/// test that ran solely against the in-memory store would assert nothing about
/// any of them.
#[tokio::test]
async fn the_discovery_api_reads_what_the_push_path_wrote() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let digest = push_image(&h, "demo/app", "v1").await;
    push_image(&h, "other", "latest").await;

    let repos = h.get("/api/v1/repositories").await.json();
    let names: Vec<&str> = repos["repositories"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["demo/app", "other"],
        "name order across the interner"
    );
    assert_eq!(repos["repositories"][0]["tags"]["count"], 1);
    assert_eq!(repos["repositories"][0]["manifests"]["count"], 1);

    // A prefix search is a narrowed key scan, all the way down to RocksDB.
    let found = h.get("/api/v1/repositories?q=demo").await.json();
    assert_eq!(found["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(found["repositories"][0]["name"], "demo/app");

    let detail = h.get("/api/v1/repositories/demo/app").await.json();
    assert_eq!(detail["blobs"]["count"], 2);
    assert_eq!(
        detail["size_bytes"].as_u64().unwrap(),
        (CONFIG.len() + LAYER.len()) as u64,
        "the size is folded from `P`, which is the repo's own blob set"
    );

    let manifests = h.get("/api/v1/manifests/demo/app").await.json();
    let manifest = &manifests["manifests"][0];
    assert_eq!(manifest["digest"], digest);
    assert_eq!(manifest["blobs"], 2, "config plus layer");
    assert_eq!(
        manifest["blob_size"].as_u64().unwrap(),
        (CONFIG.len() + LAYER.len()) as u64,
        "`blob_size` is the record's own total, not a re-parse of the body"
    );
    assert_eq!(
        manifest["platforms"],
        serde_json::json!([]),
        "an image manifest carries no platform of its own - it is in the config \
         blob, which the push path deliberately does not read"
    );
    assert_eq!(
        manifest["tags"],
        serde_json::json!(["v1"]),
        "the `G` reverse index is what says which manifests are still tagged"
    );
    assert!(
        manifest["pushed_at"].as_u64().unwrap() > 0,
        "the push clock is stamped by the backend and only exists there"
    );

    let tags = h.get("/api/v1/tags/demo/app").await.json();
    assert_eq!(tags["tags"][0]["name"], "v1");
    assert_eq!(tags["tags"][0]["digest"], digest);
    assert!(tags["tags"][0]["tagged_at"].as_u64().unwrap() > 0);
    assert_eq!(tags["tags"][0]["manifest"]["digest"], digest);

    // And the same manifest by either reference.
    let by_tag = h.get("/api/v1/manifests/demo/app@v1").await.json();
    assert_eq!(by_tag, *manifest);
}

/// Deleting a tag must show up in discovery immediately, and must not take the
/// manifest with it - it is still there, untagged, which is exactly the state
/// the reclaimable-set query exists to find.
#[tokio::test]
async fn an_untagged_manifest_still_lists_with_no_tags() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    push_image(&h, "demo/app", "v1").await;

    let reply = h
        .request(
            Method::DELETE,
            "/v2/demo/app/manifests/v1",
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);

    let detail = h.get("/api/v1/repositories/demo/app").await.json();
    assert_eq!(detail["tags"]["count"], 0);
    assert_eq!(detail["manifests"]["count"], 1);

    let manifests = h.get("/api/v1/manifests/demo/app").await.json();
    assert_eq!(
        manifests["manifests"][0]["tags"],
        serde_json::json!([]),
        "the manifest is reachable by digest and has nothing pointing at it"
    );
}

/// The one shape that does report a platform: an index, from its children.
///
/// `ManifestRecord.platform` is never set on an image manifest - the platform
/// is in the config blob and reading it would put a blob fetch on the push path
/// - so `ChildRef` is the only place a platform enters the store, and this is
/// the only test that can prove it comes back out.
#[tokio::test]
async fn an_index_reports_the_platforms_of_its_children() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());

    let child = push_image(&h, "demo/multi", "amd64").await;
    let body = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"{IMAGE_MANIFEST}","digest":"{child}","size":{},"platform":{{"os":"linux","architecture":"amd64"}}}},{{"mediaType":"{IMAGE_MANIFEST}","digest":"{child}","size":{},"platform":{{"os":"linux","architecture":"arm64","variant":"v8"}}}}]}}"#,
        manifest().len(),
        manifest().len(),
    )
    .into_bytes();
    assert_eq!(
        h.push_manifest("demo/multi", "latest", &body).await.status,
        StatusCode::CREATED
    );

    let index = h.get("/api/v1/manifests/demo/multi@latest").await.json();
    assert_eq!(
        index["platforms"],
        serde_json::json!(["linux/amd64", "linux/arm64/v8"]),
        "a variant is part of an image's identity, so it is rendered"
    );
    assert_eq!(index["children"], 2);
    assert_eq!(
        index["blobs"], 0,
        "an index references manifests, not blobs; its weight is in its children"
    );
}

// ------------------------------------------------------------ persistence --

#[tokio::test]
async fn a_push_survives_the_process_that_took_it() {
    let dir = TempDir::new().expect("tempdir");
    let digest = {
        let h = Harness::rocks(dir.path());
        push_image(&h, "acme/app", "v1").await
    };

    // Everything above is dropped here: the engine is closed and the blob
    // store's handles are gone. What follows can only be answered from disk.
    let h = Harness::rocks(dir.path());

    assert_eq!(
        h.get("/v2/_catalog").await.json()["repositories"],
        serde_json::json!(["acme/app"]),
    );
    assert_eq!(
        h.get("/v2/acme/app/tags/list").await.json()["tags"],
        serde_json::json!(["v1"]),
    );

    let pulled = h.get("/v2/acme/app/manifests/v1").await;
    assert_eq!(pulled.status, StatusCode::OK);
    assert_eq!(
        pulled.body,
        Bytes::from(manifest()),
        "the manifest must come back byte-exact: the digest is over these bytes"
    );
    assert_eq!(
        pulled.header("docker-content-digest"),
        Some(digest.as_str())
    );

    let layer = h
        .get(&format!("/v2/acme/app/blobs/{}", sha256_hex(LAYER)))
        .await;
    assert_eq!(layer.status, StatusCode::OK);
    assert_eq!(layer.body, Bytes::from_static(LAYER));
}

#[tokio::test]
async fn the_same_push_and_pull_works_on_redb() {
    // Not a formality: the whole binary running on the second engine is a
    // stronger check of the `MetaEngine` boundary than the trait's own tests,
    // because it exercises every key range a real push touches.
    let dir = TempDir::new().expect("tempdir");
    let options = RegistryOptions::default();
    let digest = {
        let h = Harness::open(dir.path(), Engine::Redb, options.clone());
        push_image(&h, "acme/app", "v1").await
    };

    let h = Harness::open(dir.path(), Engine::Redb, options);
    let pulled = h.get("/v2/acme/app/manifests/v1").await;
    assert_eq!(pulled.status, StatusCode::OK);
    assert_eq!(
        pulled.header("docker-content-digest"),
        Some(digest.as_str())
    );
}

// ------------------------------------------------------------ blob serving --

#[tokio::test]
async fn a_blob_is_served_from_the_file_a_range_at_a_time() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    // Two chunks' worth at the 1 MiB read size, so the response is genuinely
    // assembled from more than one `pread` rather than from a single buffer.
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let digest = h.push_blob("acme/big", &big).await;

    let whole = h.get(&format!("/v2/acme/big/blobs/{digest}")).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.body.len(), big.len());
    assert_eq!(whole.body, Bytes::from(big.clone()));

    let window = h
        .request(
            Method::GET,
            &format!("/v2/acme/big/blobs/{digest}"),
            vec![(header::RANGE.as_str(), "bytes=1500000-1500099".to_owned())],
            Body::empty(),
        )
        .await;
    assert_eq!(window.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        window.header(header::CONTENT_RANGE),
        Some("bytes 1500000-1500099/3000000")
    );
    assert_eq!(window.body, Bytes::from(big[1_500_000..1_500_100].to_vec()));

    // containerd's actual shape: an open-ended resume from a byte offset.
    let resumed = h
        .request(
            Method::GET,
            &format!("/v2/acme/big/blobs/{digest}"),
            vec![(header::RANGE.as_str(), "bytes=2999990-".to_owned())],
            Body::empty(),
        )
        .await;
    assert_eq!(resumed.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(resumed.body, Bytes::from(big[2_999_990..].to_vec()));
}

#[tokio::test]
async fn a_blob_is_not_servable_from_a_repository_that_never_had_it() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let digest = h.push_blob("acme/one", LAYER).await;

    // The content is in the store, and that must not be enough. Blobs are
    // deduplicated registry-wide, so serving on the global record alone would
    // let any name pull any layer by digest.
    assert_eq!(
        h.head(&format!("/v2/acme/two/blobs/{digest}")).await.status,
        StatusCode::NOT_FOUND,
    );
    assert_eq!(
        h.head(&format!("/v2/acme/one/blobs/{digest}")).await.status,
        StatusCode::OK,
    );
}

#[tokio::test]
async fn a_mounted_blob_is_servable_from_both_repositories_and_deletable_from_one() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let digest = h.push_blob("acme/source", LAYER).await;

    let mounted = h
        .request(
            Method::POST,
            &format!("/v2/acme/target/blobs/uploads/?mount={digest}&from=acme/source"),
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(mounted.status, StatusCode::CREATED, "mount is one edge");

    assert_eq!(
        h.head(&format!("/v2/acme/target/blobs/{digest}"))
            .await
            .status,
        StatusCode::OK,
    );

    let deleted = h
        .request(
            Method::DELETE,
            &format!("/v2/acme/target/blobs/{digest}"),
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(deleted.status, StatusCode::ACCEPTED);
    assert_eq!(
        h.head(&format!("/v2/acme/target/blobs/{digest}"))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "deleting drops this repository's membership",
    );
    assert_eq!(
        h.head(&format!("/v2/acme/source/blobs/{digest}"))
            .await
            .status,
        StatusCode::OK,
        "and must not touch anyone else's",
    );
}

// ---------------------------------------------------------------- uploads --

#[tokio::test]
async fn a_chunked_upload_resumes_across_a_restart() {
    // The claim being tested is the one that makes chunked uploads survivable
    // without pinning a client to a node: the resume point is the offset and
    // the hasher state in the `U` record, so a session opened by one process
    // can be finished by another. If the hasher state were not restored
    // faithfully the commit below would fail its digest check.
    let dir = TempDir::new().expect("tempdir");
    let body: Vec<u8> = (0..30_000u32).map(|i| (i % 253) as u8).collect();
    let digest = sha256_hex(&body);
    let (first, rest) = body.split_at(10_000);
    let (second, third) = rest.split_at(10_000);

    let location = {
        let h = Harness::rocks(dir.path());
        let opened = h
            .request(
                Method::POST,
                "/v2/acme/chunked/blobs/uploads/",
                Vec::new(),
                Body::empty(),
            )
            .await;
        let location = opened
            .header(header::LOCATION)
            .expect("Location")
            .to_owned();

        let patched = h.patch_chunk(&location, 0, first).await;
        assert_eq!(patched.status, StatusCode::ACCEPTED);
        assert_eq!(
            patched.header(header::RANGE),
            Some("0-9999"),
            "the upload dialect is a bare range, with no `bytes ` prefix",
        );
        location
    };

    // A different process, holding no open file and no hasher.
    let h = Harness::rocks(dir.path());
    assert_eq!(
        h.get(&location).await.header(header::RANGE),
        Some("0-9999"),
        "the session's offset came back from the store",
    );

    let patched = h.patch_chunk(&location, 10_000, second).await;
    assert_eq!(patched.status, StatusCode::ACCEPTED);

    let closed = h.close_upload(&location, &digest, 20_000, third).await;
    assert_eq!(
        closed.status,
        StatusCode::CREATED,
        "the digest accumulated across three chunks and two processes",
    );

    let pulled = h.get(&format!("/v2/acme/chunked/blobs/{digest}")).await;
    assert_eq!(pulled.body, Bytes::from(body));
}

#[tokio::test]
async fn an_out_of_order_chunk_leaves_the_session_untouched() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let opened = h
        .request(
            Method::POST,
            "/v2/acme/ooo/blobs/uploads/",
            Vec::new(),
            Body::empty(),
        )
        .await;
    let location = opened
        .header(header::LOCATION)
        .expect("Location")
        .to_owned();

    let chunk = vec![b'x'; 1000];
    let accepted = h.patch_chunk(&location, 0, &chunk).await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);

    // Replaying a chunk already committed is the case a retrying client hits.
    let replayed = h.patch_chunk(&location, 0, &chunk).await;
    assert_eq!(replayed.status, StatusCode::RANGE_NOT_SATISFIABLE);

    // And a gap, which would otherwise be a hole in the hash.
    let skipped = h.patch_chunk(&location, 2000, &chunk).await;
    assert_eq!(skipped.status, StatusCode::RANGE_NOT_SATISFIABLE);

    assert_eq!(
        h.get(&location).await.header(header::RANGE),
        Some("0-999"),
        "a rejected chunk must leave the session byte-identical",
    );

    // The proof that it really is byte-identical: the upload still commits to
    // the digest of the bytes that were accepted, so nothing was appended.
    let digest = sha256_hex(&chunk);
    let closed = h.close_upload(&location, &digest, 1000, b"").await;
    assert_eq!(closed.status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_digest_mismatch_commits_nothing_and_keeps_the_session() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let opened = h
        .request(
            Method::POST,
            "/v2/acme/bad/blobs/uploads/",
            Vec::new(),
            Body::empty(),
        )
        .await;
    let location = opened
        .header(header::LOCATION)
        .expect("Location")
        .to_owned();

    let wrong = sha256_hex(b"not what was uploaded");
    let closed = h
        .request(
            Method::PUT,
            &format!("{location}?digest={wrong}"),
            Vec::new(),
            Body::from(LAYER.to_vec()),
        )
        .await;
    assert_eq!(closed.status, StatusCode::BAD_REQUEST);
    assert_eq!(closed.error_code(), "DIGEST_INVALID");

    assert_eq!(
        h.head(&format!("/v2/acme/bad/blobs/{wrong}")).await.status,
        StatusCode::NOT_FOUND,
        "a failed commit must create nothing",
    );
    assert_eq!(
        h.get(&location).await.status,
        StatusCode::NO_CONTENT,
        "the session survives so the client can retry rather than restart",
    );
}

#[tokio::test]
async fn a_cancelled_upload_is_gone_from_both_the_store_and_the_disk() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let opened = h
        .request(
            Method::POST,
            "/v2/acme/cancel/blobs/uploads/",
            Vec::new(),
            Body::empty(),
        )
        .await;
    let location = opened
        .header(header::LOCATION)
        .expect("Location")
        .to_owned();
    h.patch_chunk(&location, 0, &[b'x'; 1000]).await;

    let staged = dir.path().join("uploads");
    assert_eq!(
        std::fs::read_dir(&staged).expect("uploads dir").count(),
        1,
        "the staging file is on disk while the upload is open",
    );

    let cancelled = h
        .request(Method::DELETE, &location, Vec::new(), Body::empty())
        .await;
    assert_eq!(cancelled.status, StatusCode::NO_CONTENT);

    assert_eq!(
        h.get(&location).await.status,
        StatusCode::NOT_FOUND,
        "the session record is gone",
    );
    assert_eq!(
        std::fs::read_dir(&staged).expect("uploads dir").count(),
        0,
        "and so are the bytes it staged",
    );
}

#[tokio::test]
async fn one_repository_cannot_continue_anothers_upload() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let opened = h
        .request(
            Method::POST,
            "/v2/acme/mine/blobs/uploads/",
            Vec::new(),
            Body::empty(),
        )
        .await;
    let location = opened
        .header(header::LOCATION)
        .expect("Location")
        .to_owned();
    let id = location.rsplit('/').next().expect("an id");

    // The id is guessable from a `Location`; the repository in the path is the
    // thing that must gate it.
    let stolen = h.get(&format!("/v2/acme/theirs/blobs/uploads/{id}")).await;
    assert_eq!(stolen.status, StatusCode::NOT_FOUND);
    assert_eq!(stolen.error_code(), "BLOB_UPLOAD_UNKNOWN");
}

// --------------------------------------------------------------- manifests --

#[tokio::test]
async fn a_push_lands_every_tag_it_names_or_none_of_them() {
    let dir = TempDir::new().expect("tempdir");
    let digest = {
        let h = Harness::rocks(dir.path());
        h.push_blob("acme/many", CONFIG).await;
        h.push_blob("acme/many", LAYER).await;
        let body = manifest();
        let reply = h
            .push_manifest("acme/many", "v1?tag=latest&tag=stable", &body)
            .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        sha256_hex(&body)
    };

    let h = Harness::rocks(dir.path());
    assert_eq!(
        h.get("/v2/acme/many/tags/list").await.json()["tags"],
        serde_json::json!(["latest", "stable", "v1"]),
        "the reference's own tag and every `?tag=` are one atomic push",
    );
    for tag in ["v1", "latest", "stable"] {
        let head = h.head(&format!("/v2/acme/many/manifests/{tag}")).await;
        assert_eq!(head.status, StatusCode::OK, "{tag}");
        assert_eq!(head.header("docker-content-digest"), Some(digest.as_str()));
    }
}

#[tokio::test]
async fn deleting_by_digest_cascades_to_tags_and_deleting_a_tag_does_not() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    h.push_blob("acme/del", CONFIG).await;
    h.push_blob("acme/del", LAYER).await;
    let body = manifest();
    h.push_manifest("acme/del", "v1?tag=also", &body).await;
    let digest = sha256_hex(&body);

    // A tag delete leaves the manifest reachable by digest.
    let dropped = h
        .request(
            Method::DELETE,
            "/v2/acme/del/manifests/also",
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(dropped.status, StatusCode::ACCEPTED);
    assert_eq!(
        h.head("/v2/acme/del/manifests/also").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.head(&format!("/v2/acme/del/manifests/{digest}"))
            .await
            .status,
        StatusCode::OK,
    );

    // A digest delete takes every tag with it.
    let dropped = h
        .request(
            Method::DELETE,
            &format!("/v2/acme/del/manifests/{digest}"),
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(dropped.status, StatusCode::ACCEPTED);
    assert_eq!(
        h.head("/v2/acme/del/manifests/v1").await.status,
        StatusCode::NOT_FOUND,
    );
    assert_eq!(
        h.get("/v2/acme/del/tags/list").await.json()["tags"],
        serde_json::json!([]),
    );
}

#[tokio::test]
async fn a_manifest_naming_a_blob_this_repository_lacks_is_refused_by_default() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let reply = h.push_manifest("acme/sparse", "v1", &manifest()).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.error_code(),
        "MANIFEST_BLOB_UNKNOWN",
        "the document is well-formed; what is missing is the blob, and the \
         spec gives that its own code so a client knows to push rather than \
         to rewrite",
    );
}

#[tokio::test]
async fn the_same_manifest_is_accepted_when_validation_is_turned_off() {
    // This is the `OCI_DATA_SPARSE` shape the conformance suite pushes, and the
    // reason the switch exists at all: the check is optional per spec, and a
    // client pushing layers and manifest concurrently is legitimate.
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::open(
        dir.path(),
        Engine::Rocks,
        RegistryOptions {
            validate_references: false,
            ..RegistryOptions::default()
        },
    );
    let reply = h.push_manifest("acme/sparse", "v1", &manifest()).await;
    assert_eq!(reply.status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_manifest_pushed_by_digest_must_hash_to_it() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    h.push_blob("acme/claim", CONFIG).await;
    h.push_blob("acme/claim", LAYER).await;

    let wrong = sha256_hex(b"some other document");
    let reply = h.push_manifest("acme/claim", &wrong, &manifest()).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error_code(), "DIGEST_INVALID");
    assert_eq!(
        h.head(&format!("/v2/acme/claim/manifests/{wrong}"))
            .await
            .status,
        StatusCode::NOT_FOUND,
    );
}

#[tokio::test]
async fn an_unknown_repository_is_name_unknown_and_leaves_nothing_behind() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());

    let missing = h.get("/v2/acme/ghost/tags/list").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.error_code(), "NAME_UNKNOWN");

    // A read must never intern a name. If it did, every 404 would leave a
    // repository behind and `_catalog` would fill with names nobody pushed.
    assert_eq!(
        h.get("/v2/_catalog").await.json()["repositories"],
        serde_json::json!([]),
    );
}

// -------------------------------------------------------------- pagination --

#[tokio::test]
async fn listing_pages_in_name_order_and_links_only_when_there_is_more() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    for name in ["acme/a", "acme/b", "acme/c"] {
        h.push_blob(name, LAYER).await;
    }

    let first = h.get("/v2/_catalog?n=2").await;
    assert_eq!(
        first.json()["repositories"],
        serde_json::json!(["acme/a", "acme/b"]),
    );
    let link = first.header(header::LINK).expect("a Link header");
    assert!(
        link.contains("last=acme%2Fb") || link.contains("last=acme/b"),
        "{link}"
    );

    let last = h.get("/v2/_catalog?n=2&last=acme/b").await;
    assert_eq!(last.json()["repositories"], serde_json::json!(["acme/c"]));
    assert_eq!(
        last.header(header::LINK),
        None,
        "no Link on the final page: the reference implementation cannot tell \
         and so costs every client a wasted request",
    );
}

// --------------------------------------------------------------- streaming --

#[tokio::test]
async fn a_body_that_disagrees_with_its_declared_length_is_rejected_and_commits_nothing() {
    // The check moved into the body consumer when pushes stopped being
    // buffered, so it now happens *after* some bytes have reached the staging
    // file. What must not change is what the client sees: the request fails and
    // the session's recorded offset is exactly where it was.
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let opened = h
        .request(
            Method::POST,
            "/v2/acme/short/blobs/uploads/",
            Vec::new(),
            Body::empty(),
        )
        .await;
    let location = opened
        .header(header::LOCATION)
        .expect("Location")
        .to_owned();

    let reply = h
        .request(
            Method::PATCH,
            &location,
            vec![
                (header::CONTENT_RANGE.as_str(), "0-999".to_owned()),
                (header::CONTENT_LENGTH.as_str(), "1000".to_owned()),
            ],
            // The grammar checks pass - range size and Content-Length agree -
            // and only the body itself is short.
            Body::from(vec![b'x'; 400]),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error_code(), "SIZE_INVALID");

    assert_eq!(
        h.get(&location).await.header(header::RANGE),
        Some("0-0"),
        "the session is still at zero: nothing was committed",
    );

    // And the staged excess is discarded rather than resumed onto, which the
    // next chunk landing at 0 and committing to its own digest proves.
    let chunk = vec![b'y'; 100];
    let accepted = h.patch_chunk(&location, 0, &chunk).await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);
    let digest = sha256_hex(&chunk);
    let closed = h.close_upload(&location, &digest, 100, b"").await;
    assert_eq!(
        closed.status,
        StatusCode::CREATED,
        "the digest is over the 100 bytes that were accepted, not the 400 that \
         were staged and abandoned",
    );
}

#[tokio::test]
async fn a_body_over_the_ceiling_is_refused_before_it_is_all_written() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::with_config(
        dir.path(),
        ServerConfig {
            max_upload_chunk_bytes: 4096,
            ..ServerConfig::default()
        },
    );
    let opened = h
        .request(
            Method::POST,
            "/v2/acme/huge/blobs/uploads/",
            Vec::new(),
            Body::empty(),
        )
        .await;
    let location = opened
        .header(header::LOCATION)
        .expect("Location")
        .to_owned();

    let reply = h
        .request(
            Method::PATCH,
            &location,
            Vec::new(),
            Body::from(vec![b'x'; 64 * 1024]),
        )
        .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.error_code(), "SIZE_INVALID");
    assert_eq!(
        h.get(&location).await.header(header::RANGE),
        Some("0-0"),
        "an over-long body advances nothing",
    );
}

#[tokio::test]
async fn a_single_post_pushes_a_whole_blob_in_one_request() {
    // end-4b, which the reference implementation does not do at all, and which
    // is one round trip instead of two on the hot push path.
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let body: Vec<u8> = (0..2_000_000u32).map(|i| (i % 199) as u8).collect();
    let digest = sha256_hex(&body);

    let pushed = h
        .request(
            Method::POST,
            &format!("/v2/acme/oneshot/blobs/uploads/?digest={digest}"),
            Vec::new(),
            Body::from(body.clone()),
        )
        .await;
    assert_eq!(pushed.status, StatusCode::CREATED);
    assert_eq!(
        pushed.header(header::LOCATION),
        Some(format!("/v2/acme/oneshot/blobs/{digest}").as_str()),
        "the Location is a pullable blob URL, not the upload URL",
    );

    let pulled = h.get(&format!("/v2/acme/oneshot/blobs/{digest}")).await;
    assert_eq!(pulled.body, Bytes::from(body));
}

// -------------------------------------------------------------- referrers --

/// An artifact manifest attached to `subject`, distinguished by `n` so each one
/// hashes differently.
fn referring_manifest(subject: &str, artifact_type: &str, n: usize) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE_MANIFEST}","artifactType":"{artifact_type}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}],"subject":{{"mediaType":"{IMAGE_MANIFEST}","digest":"{subject}","size":{n}}},"annotations":{{"org.example.n":"{n}"}}}}"#,
        sha256_hex(CONFIG),
        CONFIG.len(),
        sha256_hex(LAYER),
        LAYER.len(),
    )
    .into_bytes()
}

/// Follow a `Link` to its target, or `None` on the last page.
fn next_page(reply: &Reply) -> Option<String> {
    let link = reply.header(header::LINK)?;
    Some(
        link.trim_start_matches('<')
            .split('>')
            .next()
            .expect("a bracketed URL")
            .to_owned(),
    )
}

#[tokio::test]
async fn referrers_page_over_real_edges_and_survive_a_restart() {
    let dir = TempDir::new().expect("tempdir");
    let subject = {
        let h = Harness::rocks(dir.path());
        let subject = push_image(&h, "acme/signed", "v1").await;

        for n in 0..5 {
            let body = referring_manifest(&subject, "application/vnd.example.sig", n);
            let reply = h
                .push_manifest("acme/signed", &sha256_hex(&body), &body)
                .await;
            assert_eq!(reply.status, StatusCode::CREATED);
            assert_eq!(
                reply.header("oci-subject"),
                Some(subject.as_str()),
                "the registry serves the referrers API, so it must acknowledge the subject",
            );
        }
        subject
    };

    // Reopened: the `F` edges are metadata, and metadata that does not survive
    // a restart is the failure no in-memory test can see.
    let h = Harness::with_config(
        dir.path(),
        ServerConfig {
            default_page_size: 2,
            max_page_size: 2,
            ..ServerConfig::default()
        },
    );

    let mut seen: Vec<String> = Vec::new();
    let mut url = format!("/v2/acme/signed/referrers/{subject}");
    let mut pages = 0;
    loop {
        let reply = h.get(&url).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.header(header::CONTENT_TYPE),
            Some("application/vnd.oci.image.index.v1+json")
        );
        let body = reply.json();
        assert_eq!(body["schemaVersion"], 2);
        let manifests = body["manifests"].as_array().cloned().expect("an array");
        assert!(manifests.len() <= 2, "a page may never exceed the limit");
        for entry in manifests {
            // Resolved at push time, and for a manifest that declares one it is
            // the declared value rather than the config's media type.
            assert_eq!(entry["artifactType"], "application/vnd.example.sig");
            assert!(
                entry["annotations"]["org.example.n"].is_string(),
                "the response cannot be built without the annotations on the edge",
            );
            seen.push(entry["digest"].as_str().expect("a digest").to_owned());
        }
        pages += 1;
        assert!(pages < 10, "paging did not terminate");
        match next_page(&reply) {
            Some(next) => url = next,
            None => break,
        }
    }

    let mut expected = seen.clone();
    expected.sort();
    expected.dedup();
    assert_eq!(expected.len(), 5, "every referrer, exactly once");
    assert_eq!(seen, expected, "digest order, across page boundaries");
    assert_eq!(pages, 3, "5 at 2 a page, with no wasted final page");
}

#[tokio::test]
async fn a_referrers_filter_is_exact_and_the_link_carries_it() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::with_config(
        dir.path(),
        ServerConfig {
            default_page_size: 1,
            max_page_size: 1,
            ..ServerConfig::default()
        },
    );
    let subject = push_image(&h, "acme/mixed", "v1").await;

    let mut sboms = Vec::new();
    for n in 0..6 {
        // One in three is an SBOM, so most pages hold no match at all.
        let artifact_type = if n % 3 == 0 {
            "application/vnd.example.sbom"
        } else {
            "application/vnd.example.sig"
        };
        let body = referring_manifest(&subject, artifact_type, n);
        let digest = sha256_hex(&body);
        assert_eq!(
            h.push_manifest("acme/mixed", &digest, &body).await.status,
            StatusCode::CREATED
        );
        if n % 3 == 0 {
            sboms.push(digest);
        }
    }
    sboms.sort();

    let mut seen = Vec::new();
    let mut url =
        format!("/v2/acme/mixed/referrers/{subject}?artifactType=application/vnd.example.sbom");
    for _ in 0..20 {
        let reply = h.get(&url).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.header("oci-filters-applied"),
            Some("artifactType"),
            "the filter is exact on every page, so it is claimed on every page",
        );
        for entry in reply.json()["manifests"].as_array().expect("an array") {
            assert_eq!(
                entry["artifactType"], "application/vnd.example.sbom",
                "claiming the filter means no descriptor of another type may appear",
            );
            seen.push(entry["digest"].as_str().expect("a digest").to_owned());
        }
        match next_page(&reply) {
            Some(next) => {
                assert!(
                    next.contains("artifactType=application%2Fvnd.example.sbom"),
                    "a link that drops the filter is a link to a different query: {next}",
                );
                url = next;
            }
            None => break,
        }
    }

    // The point of the exercise: a page of one, filtered to a third, walks to
    // the end anyway. A `Link` driven by page fullness stops on page one and
    // reports a single SBOM.
    assert_eq!(
        seen, sboms,
        "every match, across pages that were mostly empty"
    );
}
