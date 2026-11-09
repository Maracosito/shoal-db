//! `gannet dev` — manage the local Docker Compose development stack.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct ComposeArgs {
    /// Path to the Compose file describing the dev stack.
    #[arg(long, default_value = "docker-compose.yml")]
    pub file: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum DevAction {
    /// Start MinIO and gannet-server in the background (builds images as needed).
    Up(ComposeArgs),
    /// Stop the dev stack.
    Down {
        #[command(flatten)]
        compose: ComposeArgs,
        /// Also remove named volumes (deletes all locally stored data).
        #[arg(long)]
        volumes: bool,
    },
    /// Show the status of dev stack services.
    Status(ComposeArgs),
    /// Tail logs from dev stack services.
    Logs {
        #[command(flatten)]
        compose: ComposeArgs,
        /// Follow log output.
        #[arg(long, short = 'f')]
        follow: bool,
        /// Restrict to a single service (e.g. `gannet-server`, `minio`).
        service: Option<String>,
    },
}

pub fn run(action: DevAction) -> Result<()> {
    match action {
        DevAction::Up(compose) => {
            compose_command(&compose.file, &["up", "-d", "--build"])?;
            println!();
            println!("Dev stack is up:");
            println!("  gannet API     http://127.0.0.1:8080        (health: /v1/health, metrics: /metrics)");
            println!("  MinIO S3       http://127.0.0.1:9000        (bucket: gannet-dev)");
            println!("  MinIO console  http://127.0.0.1:9001        (user: gannet / gannet-dev-secret)");
            println!();
            println!("Try: gannet health");
            Ok(())
        }
        DevAction::Down { compose, volumes } => {
            let mut args = vec!["down"];
            if volumes {
                args.push("-v");
            }
            compose_command(&compose.file, &args)?;
            println!("Dev stack stopped{}.", if volumes { " and volumes removed" } else { "" });
            Ok(())
        }
        DevAction::Status(compose) => compose_command(&compose.file, &["ps"]),
        DevAction::Logs {
            compose,
            follow,
            service,
        } => {
            let mut args = vec!["logs".to_string()];
            if follow {
                args.push("-f".to_string());
            }
            if let Some(service) = service {
                args.push(service);
            }
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            compose_command(&compose.file, &arg_refs)
        }
    }
}

/// Run `docker compose -f <file> <args...>`, inheriting stdio so the user sees
/// Compose output directly.
fn compose_command(file: &Path, args: &[&str]) -> Result<()> {
    if !file.exists() {
        bail!(
            "compose file {} not found. Run this from the repository root, or pass --file.",
            file.display()
        );
    }

    let status = std::process::Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(file)
        .args(args)
        .status()
        .map_err(|e| {
            anyhow!("failed to invoke `docker`: {e}. Is Docker installed and on your PATH?")
        })
        .context("could not start docker compose")?;

    if !status.success() {
        bail!(
            "`docker compose {}` exited with {}",
            args.join(" "),
            status
        );
    }
    Ok(())
}
