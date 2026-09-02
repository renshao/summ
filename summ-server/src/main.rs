//! The `summ` binary.
//!
//! `serve` currently backs the API with [`MemoryRegistry`]: `summ-registry` and
//! `summ-storage` are being built alongside this crate, and the HTTP layer
//! reaches them only through [`summ_server::seam::Registry`], so swapping the
//! implementation is one line here.

use std::sync::Arc;

use clap::Parser;
use summ_server::config::{Cli, Command, ServeArgs};
use summ_server::memory::MemoryRegistry;
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
                .unwrap_or_else(|_| EnvFilter::new("summ_server=info,tower_http=info")),
        )
        .init();

    let state = AppState::new(Arc::new(MemoryRegistry::new()), args.server_config());
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(
        listen = %listener.local_addr()?,
        data_dir = %args.data_dir.display(),
        "summ listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
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
