//! `gannet health` — query a running server's health endpoint.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;

#[derive(Debug, Args)]
pub struct HealthArgs {
    /// Base URL of the gannet-server to check.
    #[arg(long, default_value = "http://127.0.0.1:8080", env = "GANNET_URL")]
    pub url: String,

    /// Request timeout in seconds.
    #[arg(long, default_value_t = 5)]
    pub timeout_secs: u64,
}

pub async fn run(args: HealthArgs) -> Result<()> {
    let base = args.url.trim_end_matches('/');
    let endpoint = format!("{base}/v1/health");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(args.timeout_secs))
        .build()
        .context("failed to construct HTTP client")?;

    let response = client
        .get(&endpoint)
        .send()
        .await
        .with_context(|| format!("could not reach {endpoint} — is the server running? (try `gannet dev up`)"))?;

    let http_status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .context("health endpoint returned a non-JSON body")?;

    let status = body["status"].as_str().unwrap_or("unknown");
    let version = body["version"].as_str().unwrap_or("unknown");
    let uptime = body["uptime_seconds"].as_u64().unwrap_or(0);
    let storage = body["storage_backend"].as_str().unwrap_or("unknown");

    let healthy = http_status.is_success() && status == "ok";
    println!(
        "{} gannet-server at {base}",
        if healthy { "✓" } else { "✗" }
    );
    println!("  status:   {status}");
    println!("  version:  {version}");
    println!("  uptime:   {uptime}s");
    println!("  storage:  {storage}");

    if !healthy {
        bail!("server reported unhealthy state (HTTP {http_status}, status={status})");
    }
    Ok(())
}
