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

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use futures_util::StreamExt;
use sha2::{Digest as _, Sha256, Sha512};
use summ_core::Digest;
use summ_registry::RegistryOptions;
use summ_server::backend::{Backend, Engine};
use summ_server::config::ServerConfig;
use summ_server::counters::PullCounters;
use summ_server::{router, AppState};
use summ_storage::BlobStore;
use tempfile::TempDir;
use tower::ServiceExt;

// ---------------------------------------------------------------- harness --

struct Harness {
    app: Router,
    backend: Arc<Backend>,
    counters: Arc<PullCounters>,
}

impl Harness {
    /// Open a registry on `dir`. Called twice on the same directory by the
    /// persistence tests, which is the whole point of taking a path rather than
    /// making its own.
    fn open(dir: &Path, engine: Engine, options: RegistryOptions) -> Self {
        Self::build(dir, engine, options, ServerConfig::default())
    }

    fn rocks(dir: &Path) -> Self {
        Self::open(dir, Engine::Rocks, RegistryOptions::default())
    }

    fn with_config(dir: &Path, config: ServerConfig) -> Self {
        Self::build(dir, Engine::Rocks, RegistryOptions::default(), config)
    }

    /// The `--no-pull-counts` server: the same wiring with a counter that
    /// discards, which is what `spawn_pull_counters(false)` hands back.
    fn without_pull_counts(dir: &Path) -> Self {
        Self::build_with(
            dir,
            Engine::Rocks,
            RegistryOptions::default(),
            ServerConfig::default(),
            false,
        )
    }

    /// Pull counting is on, as it is in `summ serve`, but with no flush task
    /// behind it: [`Harness::flush`] is the tick, taken by hand so a test does
    /// not have to wait `FLUSH_INTERVAL` to see a pull land.
    fn build(dir: &Path, engine: Engine, options: RegistryOptions, config: ServerConfig) -> Self {
        Self::build_with(dir, engine, options, config, true)
    }

    fn build_with(
        dir: &Path,
        engine: Engine,
        options: RegistryOptions,
        config: ServerConfig,
        counting: bool,
    ) -> Self {
        let backend = Arc::new(Backend::open(dir, engine, options).expect("backend opens"));
        // `spawn_pull_counters` would start a flush task, which these tests
        // replace with `flush`; what is exercised here is the disabled path it
        // returns without spawning anything.
        let counters = if counting {
            Arc::new(PullCounters::new())
        } else {
            backend.spawn_pull_counters(false)
        };
        let app = router(AppState::with_counters(
            backend.clone(),
            config,
            counters.clone(),
        ));
        Harness {
            app,
            backend,
            counters,
        }
    }

    /// One flush interval, on demand.
    async fn flush(&self) -> usize {
        self.backend.flush_pull_counts(&self.counters).await
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

fn sha512_hex(bytes: &[u8]) -> String {
    let hex: String = Sha512::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha512:{hex}")
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
            max_upload_bytes: Some(4096),
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
async fn a_declared_length_over_the_ceiling_is_refused_before_a_byte_is_written() {
    // The ceiling is a guard against filling a disk, so a client that says up
    // front that it will exceed it must be turned away up front - not after
    // the guard's worth of bytes has been streamed into the staging file.
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::with_config(
        dir.path(),
        ServerConfig {
            max_upload_bytes: Some(4096),
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
            vec![(header::CONTENT_LENGTH.as_str(), "8192".to_owned())],
            Body::from(vec![b'x'; 8192]),
        )
        .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.error_code(), "SIZE_INVALID");
    assert_eq!(
        h.get(&location).await.header(header::RANGE),
        Some("0-0"),
        "nothing was appended",
    );
}

#[tokio::test]
async fn no_ceiling_accepts_a_body_that_would_otherwise_be_refused() {
    // `--max-upload-bytes 0`. No client chunks a layer, so the ceiling is the
    // largest layer the registry accepts, and an operator must be able to
    // remove it rather than guess a number above the largest image they hold.
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::with_config(
        dir.path(),
        ServerConfig {
            max_upload_bytes: None,
            ..ServerConfig::default()
        },
    );
    let blob = vec![b'z'; 64 * 1024];
    let digest = h.push_blob("acme/huge", &blob).await;
    let fetched = h.get(&format!("/v2/acme/huge/blobs/{digest}")).await;
    assert_eq!(fetched.status, StatusCode::OK);
    assert_eq!(fetched.body.len(), blob.len());
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

// ------------------------------------------------------- digest algorithms --

/// A sha512 blob pushed the way every real client pushes one: no
/// `?digest-algorithm=` on the `POST`, because the spec makes it a SHOULD and
/// no client in the conformance suite sends it. The session therefore stages
/// bytes under sha256 and only learns the truth from the closing `?digest=`,
/// which is the case that used to fail with `400 DIGEST_INVALID` on content
/// that was perfectly good.
#[tokio::test]
async fn a_sha512_blob_pushes_chunked_without_the_algorithm_hint() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());

    let bytes: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let digest = sha512_hex(&bytes);

    let opened = h
        .request(
            Method::POST,
            "/v2/demo/app/blobs/uploads/",
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(opened.status, StatusCode::ACCEPTED);
    let location = opened
        .header(header::LOCATION)
        .expect("Location")
        .to_owned();

    // Three chunks, all hashed under sha256 before the algorithm is named.
    let mut offset = 0u64;
    for chunk in bytes.chunks(1500).take(2) {
        let reply = h.patch_chunk(&location, offset, chunk).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
        offset += chunk.len() as u64;
    }
    let closed = h
        .close_upload(&location, &digest, offset, &bytes[offset as usize..])
        .await;
    assert_eq!(
        closed.status,
        StatusCode::CREATED,
        "a sha512 close on a hint-less session is a rehash, not a client error"
    );
    assert_eq!(
        closed.header("docker-content-digest"),
        Some(digest.as_str())
    );

    let get = h.get(&format!("/v2/demo/app/blobs/{digest}")).await;
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(&get.body[..], &bytes[..], "the blob reads back byte-exact");

    // Addressed under sha512 only. A rehash that quietly stored the content
    // under the algorithm the session started with would still pass everything
    // above if the response echoed the client's digest.
    let head = h
        .head(&format!("/v2/demo/app/blobs/{}", sha256_hex(&bytes)))
        .await;
    assert_eq!(head.status, StatusCode::NOT_FOUND);
}

/// The rehash survives the same interruption the ordinary resume path does: the
/// staged bytes and the sha256 hasher state come back from the `U` record in a
/// second process, and the switch to sha512 happens there.
#[tokio::test]
async fn a_sha512_close_works_on_an_upload_resumed_by_another_process() {
    let dir = TempDir::new().expect("tempdir");
    let bytes: Vec<u8> = (0..3000u32).map(|i| (i % 199) as u8).collect();
    let digest = sha512_hex(&bytes);
    let split = 1024usize;

    let location = {
        let h = Harness::rocks(dir.path());
        let opened = h
            .request(
                Method::POST,
                "/v2/demo/app/blobs/uploads/",
                Vec::new(),
                Body::empty(),
            )
            .await;
        let location = opened
            .header(header::LOCATION)
            .expect("Location")
            .to_owned();
        let reply = h.patch_chunk(&location, 0, &bytes[..split]).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
        location
    };

    let h = Harness::rocks(dir.path());
    let closed = h
        .close_upload(&location, &digest, split as u64, &bytes[split..])
        .await;
    assert_eq!(closed.status, StatusCode::CREATED);

    let get = h.get(&format!("/v2/demo/app/blobs/{digest}")).await;
    assert_eq!(&get.body[..], &bytes[..]);
}

/// A manifest is content the client addresses by digest too, so the same
/// hint-less sha512 push has to work all the way through to a pullable image.
#[tokio::test]
async fn a_sha512_manifest_and_its_blobs_round_trip() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());

    for blob in [CONFIG, LAYER] {
        let digest = sha512_hex(blob);
        let opened = h
            .request(
                Method::POST,
                "/v2/demo/app/blobs/uploads/",
                Vec::new(),
                Body::empty(),
            )
            .await;
        let location = opened
            .header(header::LOCATION)
            .expect("Location")
            .to_owned();
        let closed = h.close_upload(&location, &digest, 0, blob).await;
        assert_eq!(closed.status, StatusCode::CREATED, "pushing {digest}");
    }

    let body = format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
        sha512_hex(CONFIG),
        CONFIG.len(),
        sha512_hex(LAYER),
        LAYER.len(),
    )
    .into_bytes();
    let digest = sha512_hex(&body);

    let pushed = h.push_manifest("demo/app", &digest, &body).await;
    assert_eq!(pushed.status, StatusCode::CREATED);

    let pulled = h.get(&format!("/v2/demo/app/manifests/{digest}")).await;
    assert_eq!(pulled.status, StatusCode::OK);
    assert_eq!(
        &pulled.body[..],
        &body[..],
        "manifests come back byte-exact"
    );
}

// ---------------------------------------------------- non-distributable ----

/// A manifest whose layers carry `urls` pushes with reference validation on.
///
/// Windows base images are the case in the wild: the descriptor names where the
/// content actually lives and the registry is not expected to hold it, so
/// demanding the blob rejects an image that is entirely valid. This is the
/// conformance suite's *Non-distributable Layers* row, and failing it cascades
/// into every manifest and tag check that uses the data.
#[tokio::test]
async fn a_manifest_with_foreign_layers_pushes_without_its_blobs() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());

    // Only the config and the ordinary layer are pushed. The two foreign
    // layers are never uploaded and never will be.
    let config_digest = h.push_blob("demo/win", CONFIG).await;
    let layer_digest = h.push_blob("demo/win", LAYER).await;
    let foreign = "sha256:6b10979a4ee507b5c28f3c5687f6675e8683c34a27754e15e323a5171b033aca";

    let body = format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.nondistributable.v1.tar+gzip","digest":"{foreign}","size":123456,"urls":["https://store.example.com/blobs/sha256/6b1097"]}},{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{layer_digest}","size":{}}}]}}"#,
        CONFIG.len(),
        LAYER.len(),
    )
    .into_bytes();

    let pushed = h.push_manifest("demo/win", "latest", &body).await;
    assert_eq!(
        pushed.status,
        StatusCode::CREATED,
        "a foreign layer must not be demanded: {}",
        String::from_utf8_lossy(&pushed.body)
    );

    let pulled = h.get("/v2/demo/win/manifests/latest").await;
    assert_eq!(pulled.status, StatusCode::OK);
    assert_eq!(&pulled.body[..], &body[..], "byte-exact, `urls` and all");

    // The foreign layer got no `L`, `P` or `R`, so nothing claims summ has it.
    // An edge here would advertise a blob that is not on disk, which turns a
    // pull into a failed read rather than an honest 404.
    let head = h.get(&format!("/v2/demo/win/blobs/{foreign}")).await;
    assert_eq!(
        head.status,
        StatusCode::NOT_FOUND,
        "a foreign layer must never look servable"
    );
}

/// The same manifest when the foreign blob *has* been pushed: `urls` stops
/// being an exemption and the layer is an ordinary blob.
#[tokio::test]
async fn a_foreign_layer_that_was_pushed_anyway_is_an_ordinary_blob() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());

    let config_digest = h.push_blob("demo/win", CONFIG).await;
    let layer_digest = h.push_blob("demo/win", LAYER).await;

    let body = format!(
        r#"{{"schemaVersion":2,"mediaType":"{IMAGE_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.nondistributable.v1.tar+gzip","digest":"{layer_digest}","size":{},"urls":["https://store.example.com/blobs/whatever"]}}]}}"#,
        CONFIG.len(),
        LAYER.len(),
    )
    .into_bytes();

    let pushed = h.push_manifest("demo/win", "present", &body).await;
    assert_eq!(pushed.status, StatusCode::CREATED);

    let get = h.get(&format!("/v2/demo/win/blobs/{layer_digest}")).await;
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(&get.body[..], LAYER);
}

// ------------------------------------------------------- the manifest copy --

/// Read a blob back out of the store on `dir`, or `None` if it is not there.
///
/// Through `BlobStore` rather than by reconstructing a path: where a digest
/// lives is the store's business, and a test that hard-coded the fan-out would
/// pass while asserting nothing about the store's own idea of the layout.
async fn archived(dir: &Path, digest: &str) -> Option<Vec<u8>> {
    let digest: Digest = digest.parse().expect("a digest");
    let store = BlobStore::open(dir).expect("blob store opens");
    let blob = store.open_blob(&digest).await.ok()?;
    let mut stream = blob.stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.expect("a chunk"));
    }
    Some(bytes)
}

/// Risk 0's first mitigation: the corpus is self-describing.
///
/// Manifest bytes live under `B <repo> <digest>` and nowhere else, so a lost
/// metadata store leaves a disk of blobs that nothing on it can identify. The
/// copy is what makes the manifests findable again - byte-exact, because the
/// digest is over exactly these bytes and a recovery that cannot verify what it
/// found has recovered nothing.
#[tokio::test]
async fn a_pushed_manifest_is_copied_into_the_blob_store_under_its_own_digest() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let digest = push_image(&h, "acme/app", "v1").await;

    assert_eq!(
        archived(dir.path(), &digest).await.as_deref(),
        Some(manifest().as_slice()),
        "the copy is the document as pushed, not a re-serialisation of it",
    );

    // And it is a copy, not a move: `B` is still the read path, and the bytes
    // it returns are the ones the client sent.
    let pulled = h.get("/v2/acme/app/manifests/v1").await;
    assert_eq!(pulled.status, StatusCode::OK);
    assert_eq!(pulled.body.as_ref(), manifest().as_slice());
}

/// The copy carries no `L` or `P` record, and this is the observable
/// consequence of that decision.
///
/// Writing them would make a manifest servable as a blob of its repository and
/// would fold its bytes into the repository's blob count and size. It is not a
/// blob of the repository; `M` is the record that keeps it, and purge has to
/// honour that rather than reclaiming a file nothing appears to reference.
#[tokio::test]
async fn the_copy_is_not_servable_as_a_blob_and_is_not_counted_as_one() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let digest = push_image(&h, "acme/app", "v1").await;
    assert!(archived(dir.path(), &digest).await.is_some());

    let served = h.get(&format!("/v2/acme/app/blobs/{digest}")).await;
    assert_eq!(served.status, StatusCode::NOT_FOUND);
    assert_eq!(served.error_code(), "BLOB_UNKNOWN");

    let detail = h.get("/api/v1/repositories/acme/app").await;
    assert_eq!(
        detail.json()["blobs"]["count"],
        2,
        "the config and the layer, and not the manifest",
    );
    assert_eq!(
        detail.json()["size_bytes"],
        (CONFIG.len() + LAYER.len()) as u64,
    );
}

/// A re-push writes the same content to the same name, so it must be a no-op
/// rather than a second file or a failure.
#[tokio::test]
async fn re_pushing_a_manifest_leaves_one_identical_copy() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let digest = push_image(&h, "acme/app", "v1").await;

    // Same bytes, a second tag, and then the same tag again.
    for reference in ["v2", "v1"] {
        let reply = h.push_manifest("acme/app", reference, &manifest()).await;
        assert_eq!(reply.status, StatusCode::CREATED);
    }
    // And into a second repository, where the manifest is new to `M` but the
    // content is already in the store.
    h.push_blob("acme/other", CONFIG).await;
    h.push_blob("acme/other", LAYER).await;
    assert_eq!(
        h.push_manifest("acme/other", "v1", &manifest())
            .await
            .status,
        StatusCode::CREATED,
    );

    assert_eq!(
        archived(dir.path(), &digest).await.as_deref(),
        Some(manifest().as_slice()),
    );
}

/// Deleting a manifest does not remove the copy.
///
/// The same rule as `DELETE /v2/<name>/blobs/<digest>`, which drops membership
/// and leaves the bytes: `M` is repo-scoped and the store is global, so another
/// repository may still name these bytes. Deciding that nothing does is purge's
/// job, and it takes the whole sweep to decide it.
#[tokio::test]
async fn deleting_a_manifest_leaves_the_copy_for_purge() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let digest = push_image(&h, "acme/app", "v1").await;

    let deleted = h
        .request(
            Method::DELETE,
            &format!("/v2/acme/app/manifests/{digest}"),
            Vec::new(),
            Body::empty(),
        )
        .await;
    assert_eq!(deleted.status, StatusCode::ACCEPTED);
    assert_eq!(
        h.get("/v2/acme/app/manifests/v1").await.status,
        StatusCode::NOT_FOUND,
    );

    assert_eq!(
        archived(dir.path(), &digest).await.as_deref(),
        Some(manifest().as_slice()),
        "a manifest delete is a metadata operation; reclaiming bytes is purge's",
    );
}

/// The ordering rule, from the failing side: no metadata without the bytes.
///
/// The copy is redundant - `B` is still the read path - so a warning would be
/// tempting here. It is refused: the state that would leave is metadata with no
/// copy, silently, which is exactly the state the mitigation exists to prevent
/// and which nobody discovers until they are already recovering.
#[tokio::test]
async fn a_push_that_cannot_write_the_copy_commits_no_metadata() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    h.push_blob("acme/app", CONFIG).await;
    h.push_blob("acme/app", LAYER).await;

    // Every commit into the store is a rename out of `uploads/`, so sealing
    // that directory is the one way to fail a blob write without corrupting
    // anything.
    let uploads = dir.path().join("uploads");
    let mode = std::fs::metadata(&uploads).expect("uploads/").permissions();
    std::fs::set_permissions(&uploads, std::fs::Permissions::from_mode(0o500))
        .expect("sealing uploads/");
    let reply = h.push_manifest("acme/app", "v1", &manifest()).await;
    std::fs::set_permissions(&uploads, mode).expect("unsealing uploads/");

    if reply.status == StatusCode::CREATED {
        // Running as root, where the mode is advisory. Nothing was learned
        // about the failure path, so assert what is still true and stop.
        assert!(archived(dir.path(), &sha256_hex(&manifest()))
            .await
            .is_some());
        return;
    }
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        h.get("/v2/acme/app/manifests/v1").await.status,
        StatusCode::NOT_FOUND,
        "the batch is the commit point and it never ran",
    );
    assert_eq!(
        h.get("/v2/acme/app/tags/list").await.json()["tags"],
        serde_json::json!([]),
    );
}

// -------------------------------------------------------------- pull counts --

/// The counters over the real store: the accumulator, the flush, and the `A`
/// range behind them.
///
/// `discovery.rs` proves the endpoint's shape against `MemoryRegistry`. What
/// only this file can prove is that a flush turns into keys `summ-registry`
/// wrote and `summ-meta` kept - and, below, that they are still there after the
/// process that counted them is gone.
#[tokio::test]
async fn pulls_are_counted_through_the_real_store() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    let digest = push_image(&h, "demo/app", "v1").await;

    // Nothing is written until the flush: the pull path only touches memory.
    assert_eq!(
        h.get("/v2/demo/app/manifests/v1").await.status,
        StatusCode::OK
    );
    assert_eq!(
        h.get("/api/v1/pull-counts/demo/app").await.json()["totals"]["manifest_pulls"],
        0,
        "a pull must not reach the store before its flush"
    );

    // One `GET` by tag lands on all three scopes.
    assert_eq!(h.flush().await, 3, "manifest, tag and repository");
    for uri in [
        "/api/v1/pull-counts/demo/app".to_string(),
        "/api/v1/pull-counts/demo/app@v1".to_string(),
        format!("/api/v1/pull-counts/demo/app@{digest}"),
    ] {
        let body = h.get(&uri).await.json();
        assert_eq!(body["totals"]["manifest_pulls"], 1, "{uri}");
    }

    // A blob `GET` is repository-scoped, and the bytes are the ones served.
    let layer = sha256_hex(LAYER);
    assert_eq!(
        h.get(&format!("/v2/demo/app/blobs/{layer}")).await.status,
        StatusCode::OK
    );
    h.flush().await;
    let repo = h.get("/api/v1/pull-counts/demo/app").await.json();
    assert_eq!(repo["totals"]["blob_pulls"], 1);
    assert_eq!(repo["totals"]["bytes_out"], LAYER.len());

    // An empty accumulator writes nothing at all rather than a batch of zeroes.
    assert_eq!(h.flush().await, 0);
}

/// A flush is a fold, not a replace - which is what makes losing an interval to
/// a crash cost an interval rather than a day.
#[tokio::test]
async fn counts_accumulate_across_flushes_and_survive_a_restart() {
    let dir = TempDir::new().expect("tempdir");
    let digest = {
        let h = Harness::rocks(dir.path());
        let digest = push_image(&h, "demo/app", "v1").await;
        for _ in 0..3 {
            h.get("/v2/demo/app/manifests/v1").await;
            h.flush().await;
        }
        assert_eq!(
            h.get("/api/v1/pull-counts/demo/app@v1").await.json()["totals"]["manifest_pulls"],
            3
        );
        digest
    };

    let h = Harness::rocks(dir.path());
    let body = h
        .get(&format!("/api/v1/pull-counts/demo/app@{digest}"))
        .await
        .json();
    assert_eq!(
        body["totals"]["manifest_pulls"], 3,
        "counts outlive the process"
    );

    // And a pull after the restart adds to what is there rather than starting
    // the day again.
    h.get("/v2/demo/app/manifests/v1").await;
    h.flush().await;
    assert_eq!(
        h.get(&format!("/api/v1/pull-counts/demo/app@{digest}"))
            .await
            .json()["totals"]["manifest_pulls"],
        4
    );
}

/// The hour is stamped when the pull is served, so a day's traffic lands in one
/// hour of the array and the day total is its sum.
#[tokio::test]
async fn a_pull_lands_in_exactly_one_hour_of_the_day() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    push_image(&h, "demo/app", "v1").await;
    h.get("/v2/demo/app/manifests/v1").await;
    h.flush().await;

    let body = h.get("/api/v1/pull-counts/demo/app@v1").await.json();
    let days = body["days"].as_array().expect("days");
    let today = days.last().expect("today");
    let hours: Vec<u64> = today["hours"]["manifest_pulls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_u64().unwrap())
        .collect();
    assert_eq!(hours.len(), 24);
    assert_eq!(hours.iter().sum::<u64>(), 1);
    assert_eq!(
        hours.iter().filter(|&&n| n > 0).count(),
        1,
        "one pull is one hour"
    );
    assert_eq!(
        today["manifest_pulls"], 1,
        "the day is the sum of the hours"
    );
}

/// A pull counted for a repository that has since been deleted must not
/// resurrect it: the flush resolves the name and skips what it cannot find.
#[tokio::test]
async fn a_flush_does_not_resurrect_a_deleted_repository() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::rocks(dir.path());
    push_image(&h, "demo/app", "v1").await;
    h.get("/v2/demo/app/manifests/v1").await;
    h.flush().await;

    // The catalog is unchanged by counting, and a count for a name the store
    // never had writes nothing.
    let before = h.get("/api/v1/repositories").await.json();
    assert_eq!(before["repositories"].as_array().unwrap().len(), 1);

    assert_eq!(
        h.get("/v2/ghost/manifests/v1").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.flush().await, 0, "a 404 is not a pull");
    let after = h.get("/api/v1/repositories").await.json();
    assert_eq!(after["repositories"], before["repositories"]);
}

/// `--no-pull-counts` is the switch, and it turns off the recording rather than
/// the endpoint: counts outlive what they describe, so the endpoint keeps
/// answering with whatever was recorded before.
#[tokio::test]
async fn disabled_counters_record_nothing_and_still_answer() {
    let dir = TempDir::new().expect("tempdir");
    let h = Harness::without_pull_counts(dir.path());
    assert!(!h.counters.is_enabled());

    push_image(&h, "demo/app", "v1").await;
    h.get("/v2/demo/app/manifests/v1").await;
    assert_eq!(h.flush().await, 0);

    let body = h.get("/api/v1/pull-counts/demo/app").await.json();
    assert_eq!(body["totals"]["manifest_pulls"], 0);
    assert_eq!(body["days"].as_array().unwrap().len(), 30);
}
