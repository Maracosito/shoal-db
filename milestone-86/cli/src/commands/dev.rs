use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum DevCommand {
    /// Start the local stack (API server, indexer worker, MinIO) in the background
    Up {
        /// Path to a docker-compose file (default: deploy/docker-compose.yml)
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        /// Rebuild images before starting
        #[arg(long)]
        build: bool,
        /// Do not wait for the API server to become healthy
        #[arg(long)]
        no_wait: bool,
    },
    /// Stop the local stack
    Down {
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        /// Also remove data volumes (MinIO buckets, caches) — destroys local data
        #[arg(long)]
        volumes: bool,
    },
    /// Show the status of the local stack's containers
    Status {
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
    },
    /// Tail logs from the local stack (optionally a single service)
    Logs {
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        /// Service name (api, worker, minio, example-app)
        service: Option<String>,
        /// Follow log output
        #[arg(long)]
        follow: bool,
    },
}

pub async fn run(cmd: DevCommand, api_url: &str) -> Result<()> {
    match cmd {
        DevCommand::Up { file, build, no_wait } => {
            let file = compose_file(file)?;
            let mut args = vec!["up", "-d"];
            if build {
                args.push("--build");
            }
            compose(&file, &args)?;
            if !no_wait {
                println!("waiting for API server at {api_url} ...");
                wait_healthy(api_url, Duration::from_secs(90)).await?;
                println!("lakefin is up — try `lakefin health`");
            }
        }
        DevCommand::Down { file, volumes } => {
            let file = compose_file(file)?;
            let mut args = vec!["down"];
            if volumes {
                args.push("--volumes");
            }
            compose(&file, &args)?;
        }
        DevCommand::Status { file } => {
            let file = compose_file(file)?;
            compose(&file, &["ps"])?;
        }
        DevCommand::Logs { file, service, follow } => {
            let file = compose_file(file)?;
            let mut args = vec!["logs".to_string()];
            if follow {
                args.push("--follow".to_string());
            }
            if let Some(s) = service {
                args.push(s);
            }
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            compose(&file, &arg_refs)?;
        }
    }
    Ok(())
}

fn compose_file(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(f) = explicit {
        if !f.exists() {
            bail!("compose file not found: {}", f.display());
        }
        return Ok(f);
    }
    for candidate in [
        "deploy/docker-compose.yml",
        "deploy/docker-compose.yaml",
        "docker-compose.yml",
        "docker-compose.yaml",
    ] {
        if Path::new(candidate).exists() {
            return Ok(PathBuf::from(candidate));
        }
    }
    bail!(
        "no docker-compose file found (looked for deploy/docker-compose.yml and \
         ./docker-compose.yml); pass --file or run from the repository root"
    )
}

fn compose(file: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(file)
        .args(args)
        .status()
        .context("running `docker compose` — is Docker installed and on PATH?")?;
    if !status.success() {
        bail!("docker compose exited with status {status}");
    }
    Ok(())
}

async fn wait_healthy(api_url: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("building health-check client")?;
    let url = format!("{}/v1/health", api_url.trim_end_matches('/'));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "API server did not become healthy within {}s; check `lakefin dev logs api`",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}
