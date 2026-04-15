#![allow(dead_code)]
//! azoth CLI binary entrypoint.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "azoth", version, about = "Azoth coding-first agent runtime")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Launch the interactive TUI (default).
    Tui,
    /// Resume a prior session by `run_id` from `.azoth/sessions/<run_id>.jsonl`.
    Resume {
        /// The `run_id` of the session file to reopen.
        run_id: String,
    },
    /// Dump version + build info.
    Version,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => run_tui(None),
        Command::Resume { run_id } => run_tui(Some(run_id)),
        Command::Version => {
            println!("azoth {}", env!("CARGO_PKG_VERSION"));
        }
    }
}

fn run_tui(resume: Option<String>) {
    #[cfg(feature = "tui")]
    {
        if let Err(e) = tui::run(resume) {
            eprintln!("tui error: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = resume;
        eprintln!("this build was compiled without the `tui` feature");
        std::process::exit(2);
    }
}

#[cfg(feature = "tui")]
mod tui;
