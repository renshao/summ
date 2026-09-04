//! Server configuration and the CLI that builds it.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::auth::{AuthPolicy, Generated};
use crate::backend::Engine;

/// Default ceiling on one upload request body, in bytes.
///
/// 32 GiB: comfortably above the largest layer anyone actually pushes - the
/// CUDA layers in `pytorch/pytorch` are the usual worst case, at a few
/// gigabytes - while still bounding what one request can write to the disk.
/// `0` on the command line removes it entirely.
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 32 * 1024 * 1024 * 1024;

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
    /// Largest single request body accepted on an upload `POST`/`PATCH`/`PUT`,
    /// or `None` for no ceiling at all.
    ///
    /// In principle a per-request bound rather than a blob-size bound, since a
    /// larger blob may be pushed in chunks. In practice **no client chunks** -
    /// docker, crane and oras all send a layer as one monolithic body - so this
    /// is the largest layer the registry accepts, and a low value rejects
    /// exactly the multi-gigabyte ML images a registry gets pushed.
    ///
    /// It was 1 GiB while the skeleton buffered a chunk in memory. The body
    /// streams into the staging file now and costs one frame whatever the blob,
    /// so what is left is a guard against a runaway or malicious client filling
    /// a disk - which wants a number far above any real layer, not one near it.
    pub max_upload_bytes: Option<u64>,
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
    /// **On.** The `F` edges have been written since the first push, so the
    /// switch was only ever hiding a working endpoint. It stays a switch
    /// because turning the API off is not the same as never having had it: a
    /// client that gets a `404` here MUST fall back to the referrers tag
    /// schema, and that fallback is the only thing an operator can reach for
    /// if the edges ever need rebuilding.
    ///
    /// It also gates `OCI-Subject` on a manifest `PUT`. The spec ties the two
    /// together - the header means "this registry processed your subject", and
    /// a registry that sends it while answering `404` on `/referrers/` has told
    /// the client both that the fallback is unnecessary and that it is
    /// required.
    pub referrers_enabled: bool,
    /// Who may read and who may write.
    ///
    /// [`AuthPolicy::Anonymous`] by default, which is what makes `summ serve`
    /// with no arguments a working registry in one command. Everything the
    /// server exposes is behind this once it is not - see [`crate::auth`].
    pub auth: AuthPolicy,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            max_manifest_bytes: 8 * 1024 * 1024,
            max_upload_bytes: Some(DEFAULT_MAX_UPLOAD_BYTES),
            default_page_size: 1000,
            max_page_size: 1000,
            max_tag_params: 32,
            chunk_min_length: None,
            referrers_enabled: true,
            auth: AuthPolicy::Anonymous,
        }
    }
}

/// How the registry authenticates a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthMode {
    /// Anyone may read and write. The default.
    Anonymous,
    /// A read key and a write key, sent as an HTTP Basic password.
    #[value(name = "apikey", alias = "api-key")]
    ApiKey,
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

    /// Maximum bytes in one upload request body; `0` removes the limit.
    ///
    /// No client chunks a layer, so this is effectively the largest layer the
    /// registry will accept: below a layer's size, a push fails with `413` and
    /// `SIZE_INVALID` however many times it is retried. The body streams
    /// straight to the staging file, so raising it costs disk rather than
    /// memory.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_UPLOAD_BYTES,
        env = "SUMM_MAX_UPLOAD_BYTES"
    )]
    pub max_upload_bytes: u64,

    /// Default number of results for a list endpoint with no `?n=`.
    #[arg(long, default_value_t = 1000, env = "SUMM_DEFAULT_PAGE_SIZE")]
    pub default_page_size: usize,

    /// Ceiling for `?n=`; larger requests are clamped to it.
    #[arg(long, default_value_t = 1000, env = "SUMM_MAX_PAGE_SIZE")]
    pub max_page_size: usize,

    /// Answer `404` on `/v2/<name>/referrers/<digest>` instead of serving it.
    ///
    /// The endpoint is on by default. This turns it off, which also drops
    /// `OCI-Subject` from manifest `PUT` responses - a client must not be told
    /// its subject was processed by a registry that will not list it.
    #[arg(long, env = "SUMM_NO_REFERRERS")]
    pub no_referrers: bool,

    /// Authentication mode.
    ///
    /// `anonymous`, the default, is an open read-write registry - which is the
    /// right default for the thing this is: a single binary someone runs to
    /// have a registry a minute later, usually on a laptop or inside a network
    /// that is already the boundary. `apikey` turns on the two-key scheme in
    /// [`crate::auth`] and applies it to everything the server serves.
    #[arg(long, value_enum, default_value = "anonymous", env = "SUMM_AUTH")]
    pub auth: AuthMode,

    /// API key admitting `GET` and `HEAD` - pull, list, browse.
    ///
    /// Only meaningful with `--auth apikey`, where an absent key is generated
    /// and printed once at startup. A key given here is never printed back.
    #[arg(long, env = "SUMM_READ_APIKEY")]
    pub read_apikey: Option<String>,

    /// API key admitting everything, reads included - push, delete.
    ///
    /// One key that can do everything is what a CI job wants, and making the
    /// write key also read is what stops that job needing both.
    #[arg(long, env = "SUMM_WRITE_APIKEY")]
    pub write_apikey: Option<String>,
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

    /// The HTTP layer's configuration, plus which API keys had to be invented.
    ///
    /// Fallible because the auth arguments can contradict each other, and the
    /// contradiction has to be fatal. A key supplied without `--auth apikey` is
    /// somebody who believes they have locked the registry; the alternatives to
    /// failing are to ignore the key, which serves an open registry to someone
    /// who thinks otherwise, or to infer the mode from the key's presence,
    /// which makes deleting an environment variable silently disable
    /// authentication. Neither is a failure an operator can see, so this one is
    /// made loud and made at startup, before the listener binds.
    pub fn server_config(&self) -> Result<(ServerConfig, Option<Generated>), String> {
        let (auth, generated) = match self.auth {
            AuthMode::Anonymous => {
                if self.read_apikey.is_some() || self.write_apikey.is_some() {
                    return Err("an API key was supplied but --auth is `anonymous`; pass \
                         `--auth apikey` (SUMM_AUTH=apikey) to require it"
                        .to_owned());
                }
                (AuthPolicy::Anonymous, None)
            }
            AuthMode::ApiKey => {
                let (policy, generated) =
                    AuthPolicy::new(self.read_apikey.clone(), self.write_apikey.clone())?;
                (policy, Some(generated))
            }
        };
        let config = ServerConfig {
            max_manifest_bytes: self.max_manifest_bytes,
            // Zero is "no ceiling" rather than "accept nothing": a limit flag
            // is set to zero by someone turning the limit off, and the other
            // reading makes every push fail.
            max_upload_bytes: (self.max_upload_bytes > 0).then_some(self.max_upload_bytes),
            default_page_size: self.default_page_size,
            max_page_size: self.max_page_size,
            referrers_enabled: !self.no_referrers,
            auth,
            ..ServerConfig::default()
        };
        Ok((config, generated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a `serve` argument list. The auth arguments are always passed
    /// explicitly, so a `SUMM_*` variable in the environment running the tests
    /// cannot change what is under test.
    fn args(extra: &[&str]) -> Result<ServeArgs, clap::Error> {
        let mut argv = vec!["serve"];
        argv.extend_from_slice(extra);
        ServeArgs::try_parse_from(argv)
    }

    #[test]
    fn the_default_is_an_open_registry() {
        assert!(!ServerConfig::default().auth.is_enabled());
        let args = args(&["--auth", "anonymous"]).expect("parses");
        let (config, generated) = args.server_config().expect("valid");
        assert!(!config.auth.is_enabled());
        assert!(generated.is_none(), "nothing to generate when auth is off");
    }

    #[test]
    fn a_key_without_the_mode_is_a_startup_error() {
        // The failure this prevents is silent: an operator who set
        // SUMM_READ_APIKEY and believes the registry is closed.
        let args = args(&["--auth", "anonymous", "--read-apikey", "k"]).expect("parses");
        let message = args.server_config().expect_err("must not start");
        assert!(message.contains("--auth apikey"), "{message}");
    }

    #[test]
    fn apikey_mode_generates_what_was_not_supplied() {
        let args = args(&["--auth", "apikey", "--write-apikey", "mine"]).expect("parses");
        let (config, generated) = args.server_config().expect("valid");
        assert!(config.auth.is_enabled());
        let generated = generated.expect("reported");
        assert!(generated.read, "the read key had to be invented");
        assert!(!generated.write);

        let args = args_both();
        let (_, generated) = args.server_config().expect("valid");
        let generated = generated.expect("reported");
        assert!(!generated.read);
        assert!(!generated.write, "nothing to print when both were supplied");
    }

    fn args_both() -> ServeArgs {
        args(&[
            "--auth",
            "apikey",
            "--read-apikey",
            "r",
            "--write-apikey",
            "w",
        ])
        .expect("parses")
    }

    #[test]
    fn an_empty_key_is_rejected_rather_than_matching_an_empty_credential() {
        let args = args(&["--auth", "apikey", "--read-apikey", " "]).expect("parses");
        assert!(args.server_config().is_err());
    }

    #[test]
    fn the_upload_ceiling_defaults_high_and_can_be_removed() {
        // The failure this guards against is silent in the worst way: a limit
        // below a real layer turns every push of a big image into a `413` that
        // no retry can fix, and the client's message names a byte count rather
        // than the flag that produced it.
        let ceiling = |extra: &[&str]| {
            let (config, _) = args(extra).expect("parses").server_config().expect("valid");
            config.max_upload_bytes
        };

        assert_eq!(ceiling(&[]), Some(DEFAULT_MAX_UPLOAD_BYTES));
        const {
            assert!(
                DEFAULT_MAX_UPLOAD_BYTES > 8 * 1024 * 1024 * 1024,
                "the default must clear the largest layers people actually push",
            )
        };
        assert_eq!(
            ceiling(&["--max-upload-bytes", "0"]),
            None,
            "zero removes the ceiling rather than rejecting every body",
        );
        assert_eq!(ceiling(&["--max-upload-bytes", "4096"]), Some(4096));
    }

    #[test]
    fn the_mode_is_spelled_both_ways() {
        for spelling in ["apikey", "api-key"] {
            let args = args(&["--auth", spelling]).expect("parses");
            assert_eq!(args.auth, AuthMode::ApiKey);
        }
    }
}
