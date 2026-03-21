mod cli;

use crate::cli::Cli;
use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = Cli::parse();
    println!("{:?}", cli);
    Ok(())
}
