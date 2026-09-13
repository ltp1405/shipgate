mod authorship;
mod config;
mod db;
mod gate;
mod gh;
mod git;
mod llm;
mod triage;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// A quiz you run before marking a pull request ready for review.
#[derive(Parser)]
#[command(name = "shipgate", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Quiz the AI-authored parts of this PR, then write the description and mark it ready.
    Ready {
        /// Run the quiz but touch nothing on GitHub.
        #[arg(long)]
        dry_run: bool,
    },
    /// PRs that went ready without a gate, plus open obligations.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    match cli.command {
        Command::Ready { dry_run } => gate::Ready { dry_run }.run(&cwd),
        Command::Status => gate::status(&cwd),
    }
}
