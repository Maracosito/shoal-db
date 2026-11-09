use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;

use crate::api::ApiClient;

#[derive(Args)]
pub struct ExportArgs {
    /// Namespace to export
    #[arg(short = 'n', long)]
    pub namespace: String,

    /// Output file (JSONL). Writes to stdout when omitted.
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// Documents fetched per page
    #[arg(long, default_value_t = 1000)]
    pub page_size: usize,

    /// Include dense vectors in the export
    #[arg(long)]
    pub include_vectors: bool,
}

pub async fn run(client: &ApiClient, args: ExportArgs) -> Result<()> {
    let mut writer: Box<dyn Write> = match &args.output {
        Some(path) => Box::new(std::io::BufWriter::new(
            std::fs::File::create(path)
                .with_context(|| format!("creating {}", path.display()))?,
        )),
        None => Box::new(std::io::BufWriter::new(std::io::stdout())),
    };

    let mut cursor: Option<String> = None;
    let mut total: u64 = 0;
    loop {
        let page = client
            .export_page(
                &args.namespace,
                cursor.as_deref(),
                args.page_size,
                args.include_vectors,
            )
            .await?;
        for doc in &page.documents {
            serde_json::to_writer(&mut writer, doc).context("writing document")?;
            writer.write_all(b"\n")?;
            total += 1;
        }
        match page.next_cursor {
            Some(next) if !page.documents.is_empty() => cursor = Some(next),
            _ => break,
        }
    }
    writer.flush()?;

    if args.output.is_some() {
        eprintln!("exported {total} documents from '{}'", args.namespace);
    }
    Ok(())
}
