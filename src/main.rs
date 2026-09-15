mod authorship;
mod config;
mod context;
mod dash;
mod db;
mod gate;
mod gh;
mod git;
mod llm;
mod triage;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// A quiz you run before marking a pull request ready for review.
#[derive(Parser)]
#[command(name = "shipgate", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Every PR waiting on you, across every watched repository. This is what
    /// runs when shipgate is invoked with no arguments.
    Dash,
    /// Quiz the AI-authored parts of this PR, then write the description and mark it ready.
    Ready {
        /// Run the quiz but touch nothing on GitHub.
        #[arg(long)]
        dry_run: bool,
        /// Use offline stand-ins instead of the model. No cost, no network.
        #[arg(long)]
        offline: bool,
        /// Throw away the stored gate — questions and graded answers both —
        /// and generate new questions. Without it a PR that has been quizzed
        /// before replays the questions it already has, which costs nothing.
        #[arg(long)]
        force: bool,
    },
    /// PRs that went ready without a gate, plus open obligations.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    match cli.command.unwrap_or(Command::Dash) {
        Command::Dash => gate::dashboard(),
        Command::Ready { dry_run, offline, force } => {
            gate::Ready { dry_run, offline, force }.run(&cwd)
        }
        Command::Status => gate::status(&cwd),
    }
}
