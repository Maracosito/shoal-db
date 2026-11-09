//! `gannet config` — create and validate Gannet configuration files.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};

use gannet_server::config::ServerConfig;

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Validate a configuration file and print a secret-free summary.
    Check(CheckArgs),
    /// Write a starter configuration file.
    Init(InitArgs),
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Path to the configuration file to validate.
    #[arg(long, default_value = "gannet.toml")]
    pub file: PathBuf,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Path to write the new configuration file to.
    #[arg(long, default_value = "gannet.toml")]
    pub path: PathBuf,

    /// Storage backend to template.
    #[arg(long, value_enum, default_value_t = Backend::Fs)]
    pub backend: Backend,

    /// Overwrite an existing file.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Backend {
    Fs,
    S3,
    Memory,
}

pub fn run(action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Check(args) => check(args),
        ConfigAction::Init(args) => init(args),
    }
}

fn check(args: CheckArgs) -> Result<()> {
    let mut config = ServerConfig::load(Some(&args.file))
        .with_context(|| format!("failed to load {}", args.file.display()))?;
    config.apply_env_overrides();
    config
        .validate()
        .with_context(|| format!("{} failed validation", args.file.display()))?;

    println!("✓ {} is valid", args.file.display());
    println!("  listen:   {}", config.server.listen);
    println!("  log:      level={} format={}", config.log.level, config.log.format);
    println!("  storage:  {}", config.storage.summary());
    Ok(())
}

fn init(args: InitArgs) -> Result<()> {
    if args.path.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to overwrite",
            args.path.display()
        );
    }

    let template = match args.backend {
        Backend::Fs => FS_TEMPLATE,
        Backend::S3 => S3_TEMPLATE,
        Backend::Memory => MEMORY_TEMPLATE,
    };

    // Sanity-check the template before writing it (guards against drift
    // between templates and the config schema).
    let parsed: ServerConfig =
        toml::from_str(template).context("internal error: config template is invalid")?;
    parsed
        .validate()
        .context("internal error: config template failed validation")?;

    std::fs::write(&args.path, template)
        .with_context(|| format!("failed to write {}", args.path.display()))?;
    println!("Wrote {} ({} backend).", args.path.display(), parsed.storage.kind());
    println!("Validate any edits with: gannet config check --file {}", args.path.display());
    Ok(())
}

const FS_TEMPLATE: &str = r#"# Gannet server configuration — local filesystem backend.
# Durable single-node mode; data lives under `root`.

[server]
listen = "127.0.0.1:8080"

[log]
level = "info"     # tracing filter, e.g. "info" or "info,gannet_core=debug"
format = "text"    # "text" or "json"

[storage]
backend = "fs"
root = "./gannet-data"
"#;

const S3_TEMPLATE: &str = r#"# Gannet server configuration — S3-compatible object storage backend.
#
# Credentials are NEVER placed in this file. They are resolved from the
# standard AWS credential chain: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
# environment variables, shared credential files, or instance roles.

[server]
listen = "0.0.0.0:8080"

[log]
level = "info"
format = "json"

[storage]
backend = "s3"
bucket = "gannet-dev"
prefix = ""                          # optional key prefix inside the bucket
region = "us-east-1"
endpoint = "http://127.0.0.1:9000"   # remove for real AWS S3
force_path_style = true              # required for MinIO
"#;

const MEMORY_TEMPLATE: &str = r#"# Gannet server configuration — in-memory backend.
# NON-DURABLE: all data is lost on restart. Intended for tests only.

[server]
listen = "127.0.0.1:8080"

[log]
level = "debug"
format = "text"

[storage]
backend = "memory"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_templates_parse_and_validate() {
        for (name, template) in [
            ("fs", FS_TEMPLATE),
            ("s3", S3_TEMPLATE),
            ("memory", MEMORY_TEMPLATE),
        ] {
            let cfg: ServerConfig = toml::from_str(template)
                .unwrap_or_else(|e| panic!("{name} template must parse: {e}"));
            cfg.validate()
                .unwrap_or_else(|e| panic!("{name} template must validate: {e}"));
            assert_eq!(cfg.storage.kind(), name);
        }
    }
}
