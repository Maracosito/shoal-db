use anyhow::{bail, Result};
use clap::Subcommand;
use serde_json::Value;

use crate::api::ApiClient;
use crate::output;

#[derive(Subcommand)]
pub enum NsCommand {
    /// Create a namespace
    Create {
        /// Namespace name
        name: String,
        /// Dense vector dimensionality (optional; inferred from first write if omitted)
        #[arg(long)]
        dims: Option<usize>,
        /// Distance metric: cosine | dot | euclidean
        #[arg(long, default_value = "cosine")]
        metric: String,
    },
    /// List all namespaces visible to this API key
    List,
    /// Show metadata for a namespace (document count, segments, WAL size, pinning)
    Info { name: String },
    /// Delete a namespace and all its data
    Delete {
        name: String,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Copy a namespace into a new, fully independent namespace
    Copy { source: String, target: String },
    /// Branch a namespace (copy-on-write; shares immutable segments with source)
    Branch { source: String, target: String },
    /// Warm a namespace's disk/memory caches so first queries are fast
    Warm {
        name: String,
        /// Also pin the namespace so caches are never evicted
        #[arg(long)]
        pin: bool,
    },
    /// Remove a namespace's cache pin (it stays warm until evicted)
    Unpin { name: String },
}

pub async fn run(client: &ApiClient, cmd: NsCommand, json_mode: bool) -> Result<()> {
    match cmd {
        NsCommand::Create { name, dims, metric } => {
            let v = client
                .create_namespace(&name, dims, Some(metric.as_str()))
                .await?;
            if json_mode {
                output::print_json(&v);
            } else {
                println!("created namespace '{name}'");
            }
        }
        NsCommand::List => {
            let v = client.list_namespaces().await?;
            // Server returns {"namespaces": [...]}; tolerate a bare array too.
            let rows = v
                .get("namespaces")
                .cloned()
                .unwrap_or_else(|| v.clone());
            output::print_value(json_mode, &rows);
        }
        NsCommand::Info { name } => {
            let v = client.namespace_info(&name).await?;
            output::print_value(json_mode, &v);
        }
        NsCommand::Delete { name, yes } => {
            if !yes && !confirm(&format!("Delete namespace '{name}' and ALL its data?"))? {
                bail!("aborted");
            }
            client.delete_namespace(&name).await?;
            if json_mode {
                output::print_json(&serde_json::json!({ "deleted": name }));
            } else {
                println!("deleted namespace '{name}'");
            }
        }
        NsCommand::Copy { source, target } => {
            let v = client.copy_namespace(&source, &target).await?;
            report(json_mode, &v, &format!("copied '{source}' -> '{target}'"));
        }
        NsCommand::Branch { source, target } => {
            let v = client.branch_namespace(&source, &target).await?;
            report(
                json_mode,
                &v,
                &format!("branched '{source}' -> '{target}' (copy-on-write)"),
            );
        }
        NsCommand::Warm { name, pin } => {
            let v = client
                .warm_namespace(&name, if pin { Some(true) } else { None })
                .await?;
            let suffix = if pin { " and pinned" } else { "" };
            report(json_mode, &v, &format!("warmed namespace '{name}'{suffix}"));
        }
        NsCommand::Unpin { name } => {
            let v = client.warm_namespace(&name, Some(false)).await?;
            report(json_mode, &v, &format!("unpinned namespace '{name}'"));
        }
    }
    Ok(())
}

fn report(json_mode: bool, v: &Value, human: &str) {
    if json_mode {
        output::print_json(v);
    } else {
        println!("{human}");
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}
