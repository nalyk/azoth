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
//! deterministically from the seed file digest + task index — two
//! back-to-back invocations against the same seed produce identical
//! JSONL (byte-for-byte on the event stream minus `sampled_at`), so
//! `rebuild_from` stays idempotent.

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

    let run_id = args
        .run_id
        .clone()
        .unwrap_or_else(|| format!("eval_{}", &seed_digest[..12]));
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
}
