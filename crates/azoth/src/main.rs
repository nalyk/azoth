#![allow(dead_code)]
//! azoth CLI binary entrypoint.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod eval;
mod eval_live;
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
        /// Chronon CP-5: resume in read-only mode at a wall-clock cutoff.
        /// Only turns terminated at or before `<ISO8601>` are hydrated;
        /// new turns are suppressed. Format: RFC3339 UTC, e.g.
        /// `2026-04-20T15:42:00Z`.
        #[arg(long, value_name = "ISO8601")]
        as_of: Option<String>,
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
    /// Eval plane (v2 Sprint 6). Sweeps a seed task set, computes
    /// localization@k, prints a report, and emits `EvalSampled`
    /// events to `.azoth/sessions/<run_id>.jsonl` for SQLite mirror
    /// projection.
    Eval {
        #[command(subcommand)]
        sub: EvalCommand,
    },
}

#[derive(Subcommand, Debug)]
enum EvalCommand {
    /// Score every task in a seed JSON file against its
    /// `relevant_files` ground truth.
    Run {
        /// Path to the seed task JSON file. Schema:
        /// `[{id, prompt, relevant_files[], predicted_files[], notes}, ...]`.
        #[arg(long)]
        seed: PathBuf,
        /// Cut-off k for precision@k. Defaults to 5 (plan §Verification
        /// gate 8).
        #[arg(long, default_value_t = 5)]
        k: u32,
        /// Write the full `EvalReport` as JSON to this path. When
        /// omitted, only the human-readable report lands on stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Directory for the synthetic session JSONL. Defaults to
        /// `.azoth/sessions`.
        #[arg(long)]
        sessions_dir: Option<PathBuf>,
        /// Override the synthesised run_id. Default: `eval_<digest>`
        /// where `<digest>` is the first 12 hex chars of the seed
        /// file's sha256.
        #[arg(long)]
        run_id: Option<String>,
        /// Run a real composite retrieval pass against `<repo>` and
        /// overwrite each task's `predicted_files` before scoring.
        /// Flips the emitted metric to
        /// `localization_precision_at_k_live` so forensic consumers
        /// can split seed-vs-seed and live-retrieval runs.
        #[arg(long, value_name = "REPO")]
        live_retrieval: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();

    // v2.1-H: pre-warm the user-ns probe cache from single-threaded
    // startup context. Every downstream sandbox call site
    // (`SandboxPolicy::from_env`, `bash::build_bash_command`) reads
    // through the cache, so no tokio worker thread ever pays the
    // fork — respecting the probe's SAFETY precondition.
    // First call pays one fork; subsequent calls are atomic loads.
    azoth_core::sandbox::warm_userns_cache();

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
        Command::Tui => run_tui(None, None),
        Command::Resume { run_id, as_of } => run_tui(Some(run_id), as_of),
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
        Command::Eval { sub } => match sub {
            EvalCommand::Run {
                seed,
                k,
                out,
                sessions_dir,
                run_id,
                live_retrieval,
            } => {
                let sessions_dir =
                    sessions_dir.unwrap_or_else(|| PathBuf::from(".azoth").join("sessions"));
                let args = eval::Args {
                    seed,
                    k,
                    out,
                    sessions_dir,
                    run_id,
                    live_retrieval,
                };
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                if let Err(e) = eval::run(args, &mut lock) {
                    eprintln!("eval error: {e}");
                    std::process::exit(1);
                }
            }
        },
    }
}

fn run_tui(resume: Option<String>, as_of: Option<String>) {
    if as_of.is_some() && resume.is_none() {
        eprintln!("--as-of requires a run_id (use: azoth resume <run_id> --as-of <iso8601>)");
        std::process::exit(2);
    }
    // Validate RFC3339 shape up front: the forensic projection compares
    // timestamps chronologically (not lexicographically), and a malformed
    // cutoff would otherwise silently exclude every turn. Surfacing here
    // gives the operator an actionable error before the TUI even opens.
    if let Some(t) = as_of.as_deref() {
        if time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).is_err() {
            eprintln!("malformed --as-of {t:?}: expected RFC3339 (e.g. 2026-04-20T10:00:00Z)");
            std::process::exit(2);
        }
    }
    #[cfg(feature = "tui")]
    {
        if let Err(e) = tui::run(resume, as_of) {
            eprintln!("tui error: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = (resume, as_of);
        eprintln!("this build was compiled without the `tui` feature");
        std::process::exit(2);
    }
}

#[cfg(feature = "tui")]
mod tui;
