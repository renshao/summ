//! The `summ` binary.
//!
//! `serve` backs the API with [`Backend`]: `summ-registry` over `summ-meta`,
//! with `summ-storage` holding the bytes. The HTTP layer reaches all three only
//! through [`summ_server::seam::Registry`], which is why choosing an
//! implementation is one line here.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use clap::Parser;
use summ_server::auth::{AuthPolicy, Generated};
use summ_server::backend::Backend;
use summ_server::config::{Cli, Command, ServeArgs};
use summ_server::{router, AppState};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => serve(args).await,
    }
}

async fn serve(args: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SUMM_LOG")
                .or_else(|_| EnvFilter::try_from_default_env())
                // `summ` is this binary's own crate. Without it the startup
                // and shutdown lines below are filtered out, which is why
                // `summ serve` used to look like it had died on launch.
                .unwrap_or_else(|_| EnvFilter::new("summ=info,summ_server=info,tower_http=info")),
        )
        .init();

    // Opened before the listener binds. A store that cannot be opened - a
    // newer schema version, a directory we cannot write - must be a startup
    // failure with a message, never a server that accepts connections and
    // answers 500 to every one of them.
    // Built before the store is opened: an auth argument that contradicts
    // itself is a mistake about who may reach the registry, and it should cost
    // a second, not a RocksDB open.
    //
    // Reported through clap rather than as a returned error, because that is
    // what it is - a conflict between two arguments - and it is what makes it
    // read like one. `Err` out of `main` would print the message quoted, via
    // the `Debug` that `Termination` uses, under a bare `Error:`.
    let (config, generated) = args.server_config().unwrap_or_else(|message| {
        clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            format!("{message}\n"),
        )
        .exit()
    });

    let backend = Backend::open(&args.data_dir, args.engine, args.registry_options())?;
    // Started before the router, because the router needs the handle the pull
    // path records into. Disabled hands back a counter that discards, so
    // `--no-pull-counts` costs a branch rather than a second wiring.
    let counters = backend.spawn_pull_counters(!args.no_pull_counts);
    let auth = config.auth.clone();
    let state = AppState::with_counters(Arc::new(backend), config, counters);
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(args.listen).await?;

    // Read back from the listener rather than echoing the arguments: with
    // `--port 0` the kernel chose the port, and the number the operator needs
    // is the one that was actually bound.
    let bound = listener.local_addr()?;
    // Best-effort: the directory exists by now, so this only fails on a
    // permission or symlink problem, and a relative path is still better than
    // no path at all.
    let data_dir = std::fs::canonicalize(&args.data_dir).unwrap_or_else(|_| args.data_dir.clone());

    // Printed, not logged. This is the answer to "did it start, and where?",
    // so it must survive `SUMM_LOG=warn` and any later change to the filter.
    println!("summ {}", env!("CARGO_PKG_VERSION"));
    println!("  listening on  {}:{}", bound.ip(), bound.port());
    println!("  registry      http://{}/v2/", reachable(bound));
    println!("  data dir      {}", data_dir.display());
    println!("  engine        {:?}", args.engine);
    if args.allow_missing_references {
        println!("  references    unvalidated (--allow-missing-references)");
    }
    if args.no_pull_counts {
        println!("  pull counts   off (--no-pull-counts)");
    }
    print_auth(&auth, generated);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// The auth line of the banner, and any key that had to be invented.
///
/// A *generated* key exists nowhere else, so this is its only chance to reach
/// the operator and it is printed in full. A *supplied* key is not printed at
/// all: whoever passed it already has it, and echoing it only copies a live
/// credential into a scrollback, a log file and a CI transcript. So the banner
/// says which keys it made up, and stays silent about the rest.
fn print_auth(auth: &AuthPolicy, generated: Option<Generated>) {
    let generated = generated.unwrap_or(Generated {
        read: false,
        write: false,
    });
    match auth {
        AuthPolicy::None => {
            println!("  auth          none - anonymous read-write (--auth write to gate push)");
        }
        AuthPolicy::Write { write } => {
            // Say what is open as well as what is shut: an operator reading
            // this line is entitled to learn from it that the catalog and the
            // UI are now world-readable.
            println!("  auth          write - anonymous pull, API key to push");
            if generated.write {
                println!("  write key     {}   (generated)", write.expose());
            }
        }
        AuthPolicy::All { read, write } => {
            println!("  auth          all - API key, as an HTTP Basic password");
            if generated.read {
                println!("  read key      {}   (generated)", read.expose());
            }
            if generated.write {
                println!("  write key     {}   (generated)", write.expose());
            }
        }
    }
    if generated.read || generated.write {
        println!("  !! a generated key is printed once and is not stored anywhere.");
    }
}

/// A bound address rewritten into one a browser can actually open.
///
/// Binding `0.0.0.0` (or `::`) means "every interface", which is not an address
/// anything connects *to*; the loopback address is the one that works from the
/// host, so that is what the banner offers.
fn reachable(addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        let loopback = match addr {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        SocketAddr::new(loopback, addr.port())
    } else {
        addr
    }
}

/// Stop accepting on `SIGINT` or `SIGTERM` and let in-flight requests finish.
///
/// `SIGTERM` matters as much as `SIGINT` here: it is what a container runtime
/// sends, and without it every rolling restart would sever in-flight pulls.
async fn shutdown() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => tracing::warn!(error = %e, "cannot listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down");
}
