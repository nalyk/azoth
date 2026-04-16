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
    let cli = Cli::parse();
    let is_tui = !matches!(cli.command, Some(Command::Version));

    if is_tui {
        // TUI mode: tracing goes to .azoth/azoth.log so it doesn't corrupt
        // the ratatui alternate screen. Create the dir if needed.
        let log_dir = std::path::Path::new(".azoth");
        let _ = std::fs::create_dir_all(log_dir);
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("azoth.log"))
            .expect("open .azoth/azoth.log");
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_target(false)
            .with_writer(std::sync::Mutex::new(log_file))
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_target(false)
            .init();
    }

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
