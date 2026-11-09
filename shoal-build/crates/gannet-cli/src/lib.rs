//! `gannet` — the Gannet developer CLI.
//!
//! Milestone 1 scope: manage the local Docker Compose dev stack, check server
//! health, and create/validate configuration files. Data commands (namespace
//! creation, loading, querying, export, warm, bench) land with the engine in
//! later milestones.

pub mod commands;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "gannet",
    version,
    about = "Gannet — object-storage-native vector + full-text search database",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage the local development stack (MinIO + gannet-server via Docker Compose).
    Dev {
        #[command(subcommand)]
        action: commands::dev::DevAction,
    },
    /// Check the health of a running gannet-server.
    Health(commands::health::HealthArgs),
    /// Create and validate Gannet configuration files.
    Config {
        #[command(subcommand)]
        action: commands::config_cmd::ConfigAction,
    },
}

/// Execute a parsed CLI invocation.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Dev { action } => commands::dev::run(action),
        Command::Health(args) => commands::health::run(args).await,
        Command::Config { action } => commands::config_cmd::run(action),
    }
}
