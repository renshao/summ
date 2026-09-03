//! Server configuration and the CLI that builds it.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::backend::Engine;

/// Limits and switches the handlers consult. Separate from [`Cli`] so tests can
/// construct one directly and so a future config file has somewhere to land.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Largest manifest accepted, in bytes. Above it, `413`.
    ///
    /// The spec asks for at least 4 MiB and the conformance suite pushes a
    /// 3.92 MB manifest (390 annotations of 10,000 characters), which is
    /// uncomfortably close to a 4 MiB cap. 8 MiB costs nothing - the body is
    /// compressed at rest - and moves the margin from 2 % to 100 %.
    pub max_manifest_bytes: usize,
    /// Largest single request body accepted on an upload `POST`/`PATCH`/`PUT`.
    ///
    /// This is a per-request bound, not a blob-size bound: a larger blob is
    /// pushed in chunks. It exists because the skeleton buffers a chunk in
    /// memory; when `summ-storage` lands the body streams and this becomes a
    /// guard rather than a wall.
    pub max_upload_chunk_bytes: usize,
    /// Page size used when `?n=` is absent.
    pub default_page_size: usize,
    /// Ceiling for `?n=`. An oversized `n` is **clamped, not rejected**: the
    /// spec explicitly permits returning fewer than `n` results when a `Link`
    /// header is supplied, and rejecting is how the reference implementation
    /// makes a 10M-repo catalog unusable.
    pub max_page_size: usize,
    /// Maximum `?tag=` parameters on one manifest `PUT` (end-7b). The spec says
    /// a registry SHOULD accept at least 10 and MAY answer `414` above its own
    /// limit.
    pub max_tag_params: usize,
    /// Advertised `OCI-Chunk-Min-Length`, if any.
    ///
    /// Off by default. It is optional, and advertising a minimum makes the
    /// conformance suite size its test blobs to match, so claiming one we do
    /// not need only makes the suite push more bytes.
    pub chunk_min_length: Option<u64>,
    /// Whether `/v2/<name>/referrers/<digest>` is served.
    ///
    /// Off until Phase 6, per PLAN.md. The route is wired either way and the
    /// handler is complete; this flag is the switch. Note the spec's rule that
    /// once the API is on it MUST NOT `404` for an unknown subject - the
    /// handler honours that, so turning this on is safe as soon as the `F`
    /// edges are being written.
    pub referrers_enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            max_manifest_bytes: 8 * 1024 * 1024,
            max_upload_chunk_bytes: 1024 * 1024 * 1024,
            default_page_size: 1000,
            max_page_size: 1000,
            max_tag_params: 32,
            chunk_min_length: None,
            referrers_enabled: false,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "summ", version, about = "An OCI Distribution Spec registry")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the registry.
    Serve(ServeArgs),
}

#[derive(Debug, Parser)]
pub struct ServeArgs {
    /// Address to listen on, as `<host>:<port>`.
    ///
    /// `127.0.0.1:5000` for this machine only, `0.0.0.0:5000` for every IPv4
    /// interface, `[::]:5000` for every interface on both families. Port `0`
    /// binds an ephemeral port, which the startup banner then reports.
    ///
    /// One value rather than separate host and port flags, because a bind
    /// address is one thing: it maps onto a `SocketAddr` with no reassembly
    /// step, it is how an address is quoted between people, and it is the only
    /// shape that stays expressible if this ever has to accept more than one.
    ///
    /// A host, not a hostname: a name can resolve to several addresses and a
    /// listener binds exactly one, so resolving here would only hide which one
    /// we picked.
    #[arg(long, default_value = "127.0.0.1:5000", env = "SUMM_LISTEN")]
    pub listen: SocketAddr,

    /// Directory for blobs and metadata.
    ///
    /// `meta/` holds the metadata engine and `blobs/` the content-addressed
    /// store. They share a directory because they must share a filesystem: an
    /// upload is committed by renaming its staging file into the blob tree, and
    /// a rename across devices is not a rename.
    #[arg(long, default_value = "./data", env = "SUMM_DATA_DIR")]
    pub data_dir: PathBuf,

    /// Metadata engine.
    ///
    /// RocksDB is the v1 decision. redb is the second implementation that keeps
    /// `MetaEngine` honest, and running the whole binary on it is a stronger
    /// check of that boundary than running the trait's tests against it.
    #[arg(long, value_enum, default_value = "rocks", env = "SUMM_ENGINE")]
    pub engine: Engine,

    /// Accept a manifest whose layers or child manifests are not present yet.
    ///
    /// Validation defaults on: a registry that accepts a manifest it cannot
    /// serve has traded a push-time 400 for a pull-time 404, and the pull-time
    /// failure is the one nobody can diagnose. It is optional per spec and
    /// R1 recommends against it for exactly one caller - the conformance
    /// suite's `OCI_DATA_SPARSE` sets push a manifest and its layers
    /// concurrently, which is the shape validation rejects. This flag is how
    /// the harness turns it off without the server arguing with it.
    #[arg(long, env = "SUMM_ALLOW_MISSING_REFERENCES")]
    pub allow_missing_references: bool,

    /// Maximum manifest size in bytes.
    #[arg(long, default_value_t = 8 * 1024 * 1024, env = "SUMM_MAX_MANIFEST_BYTES")]
    pub max_manifest_bytes: usize,

    /// Default number of results for a list endpoint with no `?n=`.
    #[arg(long, default_value_t = 1000, env = "SUMM_DEFAULT_PAGE_SIZE")]
    pub default_page_size: usize,

    /// Ceiling for `?n=`; larger requests are clamped to it.
    #[arg(long, default_value_t = 1000, env = "SUMM_MAX_PAGE_SIZE")]
    pub max_page_size: usize,

    /// Serve `/v2/<name>/referrers/<digest>` instead of answering `404`.
    #[arg(long, env = "SUMM_REFERRERS")]
    pub referrers: bool,
}

impl ServeArgs {
    /// The ops layer's own limits, which are not the HTTP layer's.
    ///
    /// `max_manifest_bytes` appears in both on purpose and is deliberately the
    /// same number: the handler's copy decides a `413` before the body is read,
    /// and the ops layer's is the backstop for any caller that did not come
    /// through HTTP.
    pub fn registry_options(&self) -> summ_registry::RegistryOptions {
        summ_registry::RegistryOptions {
            validate_references: !self.allow_missing_references,
            max_manifest_bytes: self.max_manifest_bytes,
        }
    }

    pub fn server_config(&self) -> ServerConfig {
        ServerConfig {
            max_manifest_bytes: self.max_manifest_bytes,
            default_page_size: self.default_page_size,
            max_page_size: self.max_page_size,
            referrers_enabled: self.referrers,
            ..ServerConfig::default()
        }
    }
}
