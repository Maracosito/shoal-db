use clap::Parser;

use gannet_cli::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(err) = gannet_cli::run(cli).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
