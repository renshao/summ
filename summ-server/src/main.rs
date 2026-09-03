//! The `summ` binary.
//!
//! `serve` backs the API with [`Backend`]: `summ-registry` over `summ-meta`,
//! with `summ-storage` holding the bytes. The HTTP layer reaches all three only
//! through [`summ_server::seam::Registry`], which is why choosing an
//! implementation is one line here.

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
                .unwrap_or_else(|_| EnvFilter::new("summ_server=info,tower_http=info")),
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
    tracing::info!(
        listen = %listener.local_addr()?,
        data_dir = %args.data_dir.display(),
        engine = ?args.engine,
        validate_references = !args.allow_missing_references,
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
