use anyhow::Result;
use clap::Parser;

use anie_cli::{Cli, run};

#[tokio::main]
async fn main() -> Result<()> {
    run(Cli::parse()).await
}
