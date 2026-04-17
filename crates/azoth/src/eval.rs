//! `azoth eval run` — sweep a seed task set, compute localization@k,
//! and print a summary report.
//!
//! The subcommand is deliberately retrieval-backend-agnostic: it
//! consumes `predicted_files` directly from the seed JSON, so CI can
//! exercise the metric without any live index. Sprint 7 will add a
//! flag that overrides `predicted_files` with a live retrieval pass
//! — at which point the same reporter covers both paths.
//!
//! Invariant 1 discipline: every seed task emits an `EvalSampled`
//! event to an append-only `.azoth/sessions/<run_id>.jsonl` so the
//! SQLite mirror's `eval_runs` table gets populated alongside the
//! live stream. The `run_id` and per-task `turn_id` are synthesised
//! deterministically from the seed file digest + `k` + task index —
//! two back-to-back invocations against the same seed AND same `k`
//! produce identical JSONL (byte-for-byte on the event stream minus
//! `sampled_at`), so `rebuild_from` stays idempotent.
//!
//! ## Rerun semantics
//!
//! Two PR-#10 fixes shape the rerun contract:
//! - **codex P1** — the default `run_id` folds in `k` (`eval_<digest12>_k<K>`)
//!   so sweeping the same seed under different `--k` values produces
//!   distinct `run_id`s. Otherwise the mirror's composite PK
//!   `(run_id, turn_id, metric, task_id)` would silently overwrite
//!   prior measurements when `--k` changed.
//! - **codex P2** — the synthetic session JSONL is deleted before
//!   writing so reruns under the same `run_id` produce a fresh
//!   file rather than appending more `RunStarted` / `EvalSampled`
//!   lines onto the previous run. A seed with fewer tasks on
//!   rerun must not carry stale rows from the prior larger sweep.

use std::io::Write;
use std::path::{Path, PathBuf};

use azoth_core::eval::{mean_precision, score_tasks, EvalReport, SeedTask, TaskScore};
use azoth_core::event_store::JsonlWriter;
use azoth_core::schemas::{ContractId, RunId, SessionEvent, TurnId};

pub struct Args {
    pub seed: PathBuf,
    pub k: u32,
    pub out: Option<PathBuf>,
    pub sessions_dir: PathBuf,
    pub run_id: Option<String>,
}

#[derive(Debug)]
pub enum EvalError {
    Io(std::io::Error),
    Parse(String),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::Io(e) => write!(f, "io: {e}"),
            EvalError::Parse(s) => write!(f, "parse: {s}"),
        }
    }
}

impl std::error::Error for EvalError {}

impl From<std::io::Error> for EvalError {
    fn from(e: std::io::Error) -> Self {
        EvalError::Io(e)
    }
}

pub fn run<W: Write>(args: Args, out: &mut W) -> Result<EvalReport, EvalError> {
    let bytes = std::fs::read(&args.seed).map_err(|e| {
        EvalError::Io(std::io::Error::new(
            e.kind(),
            format!("open seed {}: {e}", args.seed.display()),
        ))
    })?;
    let seed_digest = seed_digest(&bytes);
    let tasks: Vec<SeedTask> =
        serde_json::from_slice(&bytes).map_err(|e| EvalError::Parse(e.to_string()))?;

    let scores = score_tasks(&tasks, args.k);
    let mean = mean_precision(&scores);
    let sampled_at = now_iso();

    // PR #10 codex P1: fold `k` into the default run_id so sweeping
    // the same seed under different `--k` values does not collide on
    // the mirror's composite PK and silently overwrite prior samples.
    let run_id = args
        .run_id
        .clone()
        .unwrap_or_else(|| format!("eval_{}_k{}", &seed_digest[..12], args.k));
    write_eval_session(&args.sessions_dir, &run_id, &scores, &sampled_at)?;

    let report = EvalReport {
        localization_precision_at_k: mean,
        regression_rate: None,
        sampled_at: sampled_at.clone(),
        k: args.k,
        tasks_scored: scores.len() as u32,
        tasks: scores.clone(),
    };

    if let Some(path) = &args.out {
        let json =
            serde_json::to_string_pretty(&report).map_err(|e| EvalError::Parse(e.to_string()))?;
        std::fs::write(path, json).map_err(|e| {
            EvalError::Io(std::io::Error::new(
                e.kind(),
                format!("write report {}: {e}", path.display()),
            ))
        })?;
    }

    render_report(&report, out)?;
    Ok(report)
}

fn render_report<W: Write>(report: &EvalReport, out: &mut W) -> Result<(), EvalError> {
    writeln!(out, "azoth eval report (sampled_at={})", report.sampled_at)?;
    writeln!(out, "  k                = {}", report.k)?;
    writeln!(out, "  tasks_scored     = {}", report.tasks_scored)?;
    writeln!(
        out,
        "  localization@{}   = {}",
        report.k,
        fmt_opt(report.localization_precision_at_k)
    )?;
    writeln!(
        out,
        "  regression_rate  = {}",
        fmt_opt(report.regression_rate)
    )?;
    writeln!(out, "per-task:")?;
    for s in &report.tasks {
        writeln!(
            out,
            "  {:<8} precision@{:<2} = {:.4}  matched={} considered={} relevant_total={}",
            s.task_id, s.k, s.precision_at_k, s.matched, s.predicted_considered, s.relevant_total
        )?;
    }
    Ok(())
}

fn fmt_opt(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.4}"),
        None => "n/a".to_string(),
    }
}

fn write_eval_session(
    sessions_dir: &Path,
    run_id: &str,
    scores: &[TaskScore],
    sampled_at: &str,
) -> Result<(), EvalError> {
    std::fs::create_dir_all(sessions_dir)?;
    let path = sessions_dir.join(format!("{run_id}.jsonl"));
    // PR #10 codex P2: `JsonlWriter::open` appends. An eval rerun
    // with the same `run_id` would otherwise accumulate duplicate
    // `RunStarted` + stale `EvalSampled` rows, double-counting in
    // forensic consumers and keeping old tasks around when the seed
    // shrinks. A synthetic eval session has no replay-history
    // semantics to preserve — delete-then-open is the right
    // primitive here.
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| {
            EvalError::Io(std::io::Error::other(format!(
                "truncate stale session {}: {e}",
                path.display()
            )))
        })?;
    }
    let mut writer = JsonlWriter::open(&path).map_err(|e| {
        EvalError::Io(std::io::Error::other(format!(
            "open session {}: {e}",
            path.display()
        )))
    })?;

    writer
        .append(&SessionEvent::RunStarted {
            run_id: RunId::from(run_id.to_string()),
            contract_id: ContractId::from(format!("ctr_{run_id}")),
            timestamp: sampled_at.to_string(),
        })
        .map_err(EvalError::Io)?;

    for (i, s) in scores.iter().enumerate() {
        let turn_id = TurnId::from(format!("t_{i:03}"));
        writer
            .append(&SessionEvent::EvalSampled {
                turn_id,
                metric: "localization_precision_at_k".into(),
                value: s.precision_at_k,
                k: s.k,
                sampled_at: sampled_at.to_string(),
                task_id: s.task_id.clone(),
            })
            .map_err(EvalError::Io)?;
    }
    Ok(())
}

fn seed_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn now_iso_parses_as_rfc3339() {
        let s = now_iso();
        time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
            .expect("now_iso emits valid RFC3339");
    }

    #[test]
    fn end_to_end_writes_session_and_report() {
        let dir = tempdir().unwrap();
        let seed_path = dir.path().join("seed.json");
        let seed = r#"[
            {"id":"t1","prompt":"p","relevant_files":["a"],"predicted_files":["a","z"]},
            {"id":"t2","prompt":"p","relevant_files":["b"],"predicted_files":["b"]}
        ]"#;
        std::fs::write(&seed_path, seed).unwrap();

        let sessions_dir = dir.path().join(".azoth").join("sessions");
        let args = Args {
            seed: seed_path,
            k: 5,
            out: None,
            sessions_dir: sessions_dir.clone(),
            run_id: Some("eval_unit".into()),
        };

        let mut buf: Vec<u8> = Vec::new();
        let report = run(args, &mut buf).unwrap();
        assert_eq!(report.tasks_scored, 2);
        assert_eq!(report.k, 5);
        let mean = report.localization_precision_at_k.unwrap();
        assert!((mean - 0.75).abs() < 1e-9);
        assert!(
            sessions_dir.join("eval_unit.jsonl").exists(),
            "session file written"
        );
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("localization@5"));
        assert!(output.contains("t1"));
        assert!(output.contains("t2"));
    }

    /// PR #10 codex P2 regression guard: rerunning the CLI against
    /// the same `run_id` must produce a fresh synthetic session
    /// rather than appending. Otherwise forensic consumers would
    /// double-count historical `EvalSampled` rows and see two
    /// `RunStarted` lines per run.
    #[test]
    fn rerun_truncates_session_instead_of_appending() {
        let dir = tempdir().unwrap();
        let seed_path = dir.path().join("seed.json");
        std::fs::write(
            &seed_path,
            r#"[
                {"id":"t1","prompt":"p","relevant_files":["a"],"predicted_files":["a"]},
                {"id":"t2","prompt":"p","relevant_files":["b"],"predicted_files":["b"]}
            ]"#,
        )
        .unwrap();

        let sessions_dir = dir.path().join(".azoth").join("sessions");
        let make_args = || Args {
            seed: seed_path.clone(),
            k: 5,
            out: None,
            sessions_dir: sessions_dir.clone(),
            run_id: Some("eval_rerun".into()),
        };

        let mut buf: Vec<u8> = Vec::new();
        run(make_args(), &mut buf).unwrap();
        let path = sessions_dir.join("eval_rerun.jsonl");
        let first_len = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(first_len, 3, "1 RunStarted + 2 EvalSampled");

        // Second run under the same run_id must NOT append — the
        // file should be regenerated, not grown.
        let mut buf2: Vec<u8> = Vec::new();
        run(make_args(), &mut buf2).unwrap();
        let second_len = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(
            second_len, 3,
            "rerun must truncate stale session, got {second_len} lines"
        );

        // Count RunStarted events explicitly — that's the tell-tale
        // accumulation signal a forensic consumer would trip over.
        let run_started_count = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| l.contains(r#""type":"run_started""#))
            .count();
        assert_eq!(run_started_count, 1, "exactly one RunStarted per session");
    }

    /// PR #10 codex P1 regression guard: the default `run_id` must
    /// differ across `--k` values so the mirror's composite PK does
    /// not silently overwrite prior measurements when the same seed
    /// is swept at multiple cut-offs.
    #[test]
    fn default_run_id_distinguishes_k() {
        let dir = tempdir().unwrap();
        let seed_path = dir.path().join("seed.json");
        std::fs::write(
            &seed_path,
            r#"[
                {"id":"t1","prompt":"p","relevant_files":["a"],"predicted_files":["a"]}
            ]"#,
        )
        .unwrap();

        let sessions_dir = dir.path().join(".azoth").join("sessions");
        let run = |k: u32| {
            let args = Args {
                seed: seed_path.clone(),
                k,
                out: None,
                sessions_dir: sessions_dir.clone(),
                run_id: None, // exercise default path
            };
            let mut buf: Vec<u8> = Vec::new();
            super::run(args, &mut buf).unwrap();
        };

        run(5);
        run(10);

        let jsonl_files: Vec<_> = std::fs::read_dir(&sessions_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        assert_eq!(
            jsonl_files.len(),
            2,
            "--k 5 and --k 10 must produce distinct run_ids; got {:?}",
            jsonl_files
                .iter()
                .map(|e| e.file_name())
                .collect::<Vec<_>>()
        );
    }
}
