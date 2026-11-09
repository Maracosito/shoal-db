mod api;
mod commands;
mod output;

use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use commands::{bench, dev, export, load, ns, query};

#[derive(Parser)]
#[command(
    name = "lakefin",
    version,
    about = "Lakefin: object-storage-native vector + full-text search database",
    long_about = "The Lakefin CLI talks to a Lakefin API server over HTTP. Configure the\n\
                  target server with --url / LAKEFIN_URL and authenticate with\n\
                  --api-key / LAKEFIN_API_KEY.",
    propagate_version = true
)]
pub struct Cli {
    /// Base URL of the Lakefin API server
    #[arg(
        long,
        global = true,
        env = "LAKEFIN_URL",
        default_value = "http://localhost:8080"
    )]
    pub url: String,

    /// API key (sent as `Authorization: Bearer <key>`)
    #[arg(long, global = true, env = "LAKEFIN_API_KEY", hide_env_values = true)]
    pub api_key: Option<String>,

    /// Emit machine-readable JSON instead of human-readable tables
    #[arg(long, global = true)]
    pub json: bool,

    /// Per-request timeout in seconds
    #[arg(long, global = true, default_value_t = 60)]
    pub timeout: u64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check server health
    Health,
    /// Print the server's Prometheus metrics in text exposition format
    Metrics,
    /// Manage namespaces (create, list, info, delete, copy, branch, warm, pin)
    #[command(subcommand)]
    Ns(ns::NsCommand),
    /// Load documents from JSON, JSONL, or CSV files into a namespace
    Load(load::LoadArgs),
    /// Run a vector, full-text, or hybrid query against a namespace
    Query(query::QueryArgs),
    /// Export all documents in a namespace as JSONL
    Export(export::ExportArgs),
    /// Run ingest and query benchmarks against a server
    #[command(subcommand)]
    Bench(bench::BenchCommand),
    /// Manage the local development stack (Docker Compose: API, worker, MinIO)
    #[command(subcommand)]
    Dev(dev::DevCommand),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let client = api::ApiClient::new(
        &cli.url,
        cli.api_key.clone(),
        Duration::from_secs(cli.timeout),
    )?;

    match cli.command {
        Command::Health => {
            let v = client.health().await?;
            output::print_value(cli.json, &v);
        }
        Command::Metrics => {
            let text = client.metrics().await?;
            print!("{text}");
        }
        Command::Ns(cmd) => ns::run(&client, cmd, cli.json).await?,
        Command::Load(args) => load::run(&client, args, cli.json).await?,
        Command::Query(args) => query::run(&client, args, cli.json).await?,
        Command::Export(args) => export::run(&client, args).await?,
        Command::Bench(cmd) => bench::run(&client, cmd, cli.json).await?,
        Command::Dev(cmd) => dev::run(cmd, &cli.url).await?,
    }
    Ok(())
}
