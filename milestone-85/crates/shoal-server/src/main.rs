//! `shoal-server` binary entrypoint.
//!
//! Configuration is loaded by `ServerConfig::load()`, which reads the file
//! pointed to by `SHOAL_CONFIG` (if set) and then applies `SHOAL_*`
//! environment-variable overrides, so the binary runs with zero arguments in
//! containers. Telemetry (structured logs + optional OTLP traces) is
//! initialised before the engine so startup itself is observable.

use anyhow::Context;
use shoal_server::config::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = ServerConfig::load().context("loading configuration")?;

    // Keep the guard alive for the lifetime of the process; dropping it
    // flushes any buffered spans on shutdown.
    let _telemetry_guard =
        shoal_server::telemetry::init(&config).context("initialising telemetry")?;

    shoal_server::run(config).await
}
