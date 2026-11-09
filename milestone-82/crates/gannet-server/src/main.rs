//! Gannet API server binary entry point.

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;

use gannet_server::{build_app, config::ServerConfig, state::AppState, telemetry};

#[derive(Debug, Parser)]
#[command(
    name = "gannet-server",
    version,
    about = "Gannet API server — object-storage-native vector + full-text search"
)]
struct Args {
    /// Path to the TOML configuration file. Defaults to ./gannet.toml if present.
    #[arg(long, env = "GANNET_CONFIG")]
    config: Option<PathBuf>,

    /// Override the listen address from config (e.g. 0.0.0.0:8080).
    #[arg(long)]
    listen: Option<String>,

    /// Print the resolved configuration (secret-free by design) and exit.
    #[arg(long)]
    print_config: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let mut config = ServerConfig::load(args.config.as_deref())
        .context("failed to load server configuration")?;
    config.apply_env_overrides();
    if let Some(listen) = args.listen {
        config.server.listen = listen;
    }
    config.validate().context("configuration is invalid")?;

    if args.print_config {
        println!(
            "{}",
            toml::to_string_pretty(&config).context("failed to render configuration")?
        );
        return Ok(());
    }

    telemetry::init(&config.log);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        listen = %config.server.listen,
        storage = %config.storage.summary(),
        "starting gannet-server"
    );

    let addr = config.listen_addr();
    let state = AppState::new(config);
    let app = build_app(state);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    info!("shutdown complete");
    Ok(())
}

/// Resolve when SIGINT (Ctrl-C) or SIGTERM is received.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("received SIGINT, shutting down"),
        _ = terminate => info!("received SIGTERM, shutting down"),
    }
}
