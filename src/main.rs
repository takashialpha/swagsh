use anyhow::Result;
use clap::Parser;
use swagsh::cli::Cli;

fn main() -> Result<()> {
    let cli = Cli::parse();
    println!("{:?}", cli);
    Ok(())
}
