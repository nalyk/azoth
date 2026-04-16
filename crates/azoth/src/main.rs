#![allow(dead_code)]
//! azoth CLI binary entrypoint.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod export;
mod replay;

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
    /// Render a prior session's JSONL log to stdout. Replayable projection by
    /// default; pass `--forensic` to include aborted/interrupted turns.
    Replay {
        /// The `run_id` of the session file to read.
        run_id: String,
        /// Include non-replayable events (aborted, interrupted, dangling).
        #[arg(long)]
        forensic: bool,
        /// Output format.
        #[arg(long, value_enum, default_value_t = replay::Format::Text)]
        format: replay::Format,
        /// Directory containing `<run_id>.jsonl`. Defaults to `.azoth/sessions`.
        #[arg(long)]
        sessions_dir: Option<PathBuf>,
    },
    /// Render a committed-only conversation transcript from a prior session.
    /// Markdown by default (human-shareable); `--format json` passes through
    /// the replayable `SessionEvent` stream as line-delimited JSON.
    Export {
        /// The `run_id` of the session file to read.
        run_id: String,
        /// Output format.
        #[arg(long, value_enum, default_value_t = export::Format::Markdown)]
        format: export::Format,
        /// Directory containing `<run_id>.jsonl`. Defaults to `.azoth/sessions`.
        #[arg(long)]
        sessions_dir: Option<PathBuf>,
        /// Write to this file instead of stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Dump version + build info.
    Version,
}

fn main() {
    let cli = Cli::parse();
    let is_tui = matches!(
        cli.command,
        None | Some(Command::Tui) | Some(Command::Resume { .. })
    );

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
        Command::Replay {
            run_id,
            forensic,
            format,
            sessions_dir,
        } => {
            let sessions_dir =
                sessions_dir.unwrap_or_else(|| PathBuf::from(".azoth").join("sessions"));
            let args = replay::Args {
                run_id,
                sessions_dir,
                forensic,
                format,
            };
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            if let Err(e) = replay::run(args, &mut lock) {
                eprintln!("replay error: {e}");
                std::process::exit(1);
            }
        }
        Command::Export {
            run_id,
            format,
            sessions_dir,
            output,
        } => {
            let sessions_dir =
                sessions_dir.unwrap_or_else(|| PathBuf::from(".azoth").join("sessions"));
            let args = export::Args {
                run_id,
                sessions_dir,
                format,
            };
            let result = match output {
                Some(path) => match std::fs::File::create(&path) {
                    Ok(mut f) => export::run(args, &mut f),
                    Err(e) => {
                        eprintln!("export: create {}: {e}", path.display());
                        std::process::exit(1);
                    }
                },
                None => {
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    export::run(args, &mut lock)
                }
            };
            if let Err(e) = result {
                eprintln!("export error: {e}");
                std::process::exit(1);
            }
        }
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
