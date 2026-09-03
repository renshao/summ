//! The `summ` binary.
//!
//! `serve` backs the API with [`Backend`]: `summ-registry` over `summ-meta`,
//! with `summ-storage` holding the bytes. The HTTP layer reaches all three only
//! through [`summ_server::seam::Registry`], which is why choosing an
//! implementation is one line here.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use clap::Parser;
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
    let backend = Backend::open(&args.data_dir, args.engine, args.registry_options())?;
    let state = AppState::new(Arc::new(backend), args.server_config());
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

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
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
