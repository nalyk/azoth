//! `azoth replay` — render a prior session's JSONL log for human inspection.
//!
//! Two projections mirror `azoth_core::event_store::JsonlReader`:
//! * default (replayable): only turns that ran to `TurnCommitted`.
//! * `--forensic`: every event, with non-replayable turns visibly annotated.
//!
//! Two formats:
//! * `text` (default): per-turn grouped, human-readable timeline.
//! * `json`: one `SessionEvent` per line (passthrough of the projection).

use std::path::{Path, PathBuf};

use azoth_core::event_store::{ForensicEvent, JsonlReader, ProjectionError, ReplayableEvent};
use azoth_core::schemas::{ContentBlock, SessionEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Text,
    Json,
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("session file not found: {0}")]
    NotFound(PathBuf),
    #[error("projection error: {0}")]
    Projection(#[from] ProjectionError),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct Args {
    pub run_id: String,
    pub sessions_dir: PathBuf,
    pub forensic: bool,
    pub format: Format,
}

/// Entry point called from `main`. Writes the rendered session to `out`.
pub fn run<W: std::io::Write>(args: Args, out: &mut W) -> Result<(), ReplayError> {
    let path = args.sessions_dir.join(format!("{}.jsonl", args.run_id));
    if !path.exists() {
        return Err(ReplayError::NotFound(path));
    }
    let reader = JsonlReader::open(&path);

    if args.forensic {
        let events = reader.forensic()?;
        match args.format {
            Format::Json => render_forensic_json(&events, out),
            Format::Text => render_forensic_text(&args.run_id, &path, &events, out),
        }
    } else {
        let events = reader.replayable()?;
        match args.format {
            Format::Json => render_replayable_json(&events, out),
            Format::Text => render_replayable_text(&args.run_id, &path, &events, out),
        }
    }
}

fn render_replayable_json<W: std::io::Write>(
    events: &[ReplayableEvent],
    out: &mut W,
) -> Result<(), ReplayError> {
    for ev in events {
        serde_json::to_writer(&mut *out, &ev.0)?;
        writeln!(out)?;
    }
    Ok(())
}

fn render_forensic_json<W: std::io::Write>(
    events: &[ForensicEvent],
    out: &mut W,
) -> Result<(), ReplayError> {
    for fe in events {
        let wrapper = serde_json::json!({
            "non_replayable": fe.non_replayable,
            "event": fe.event,
        });
        serde_json::to_writer(&mut *out, &wrapper)?;
        writeln!(out)?;
    }
    Ok(())
}

fn render_replayable_text<W: std::io::Write>(
    run_id: &str,
    path: &Path,
    events: &[ReplayableEvent],
    out: &mut W,
) -> Result<(), ReplayError> {
    writeln!(out, "replay {} · {}", run_id, path.display())?;
    writeln!(out, "projection: replayable (committed turns only)")?;
    writeln!(out, "events: {}", events.len())?;
    writeln!(out, "---")?;
    for ev in events {
        write_event_line(&ev.0, false, out)?;
    }
    Ok(())
}

fn render_forensic_text<W: std::io::Write>(
    run_id: &str,
    path: &Path,
    events: &[ForensicEvent],
    out: &mut W,
) -> Result<(), ReplayError> {
    writeln!(out, "replay {} · {}", run_id, path.display())?;
    writeln!(
        out,
        "projection: forensic (all turns, non-replayable annotated)"
    )?;
    writeln!(out, "events: {}", events.len())?;
    writeln!(out, "---")?;
    for fe in events {
        write_event_line(&fe.event, fe.non_replayable, out)?;
    }
    Ok(())
}

fn write_event_line<W: std::io::Write>(
    ev: &SessionEvent,
    non_replayable: bool,
    out: &mut W,
) -> std::io::Result<()> {
    let prefix = if non_replayable { "[NR] " } else { "     " };
    let turn = ev
        .turn_id()
        .map(|t| format!("{:<8}", t.as_str()))
        .unwrap_or_else(|| "        ".to_string());
    match ev {
        SessionEvent::RunStarted {
            run_id,
            contract_id,
            timestamp,
        } => writeln!(
            out,
            "{prefix}{turn} run_started      run={} contract={} at {}",
            run_id.as_str(),
            contract_id.as_str(),
            timestamp
        ),
        SessionEvent::ContractAccepted { contract, .. } => writeln!(
            out,
            "{prefix}{turn} contract_accepted id={} goal={:?}",
            contract.id.as_str(),
            truncate(&contract.goal, 60)
        ),
        SessionEvent::TurnStarted {
            run_id, timestamp, ..
        } => writeln!(
            out,
            "{prefix}{turn} turn_started     run={} at {}",
            run_id.as_str(),
            timestamp
        ),
        SessionEvent::ContextPacket { packet_digest, .. } => writeln!(
            out,
            "{prefix}{turn} context_packet   digest={}",
            short_digest(packet_digest)
        ),
        SessionEvent::ModelRequest {
            profile_id,
            request_digest,
            ..
        } => writeln!(
            out,
            "{prefix}{turn} model_request    profile={} digest={}",
            profile_id,
            short_digest(request_digest)
        ),
        SessionEvent::ContentBlock { index, block, .. } => {
            let body = render_block(block);
            writeln!(out, "{prefix}{turn} content_block[{index}] {body}")
        }
        SessionEvent::EffectRecord { effect, .. } => writeln!(
            out,
            "{prefix}{turn} effect_record    class={:?} tool={}",
            effect.class, effect.tool_name
        ),
        SessionEvent::ToolResult {
            tool_use_id,
            is_error,
            content_artifact,
            ..
        } => writeln!(
            out,
            "{prefix}{turn} tool_result      tool_use={} error={} artifact={}",
            tool_use_id.as_str(),
            is_error,
            content_artifact.as_ref().map(|a| a.as_str()).unwrap_or("-")
        ),
        SessionEvent::ValidatorResult {
            validator,
            status,
            detail,
            ..
        } => writeln!(
            out,
            "{prefix}{turn} validator_result {} status={:?}{}",
            validator,
            status,
            detail
                .as_deref()
                .map(|d| format!(" detail={d}"))
                .unwrap_or_default()
        ),
        SessionEvent::ApprovalRequest {
            effect_class,
            tool_name,
            summary,
            ..
        } => writeln!(
            out,
            "{prefix}{turn} approval_request class={:?} tool={} summary={:?}",
            effect_class,
            tool_name,
            truncate(summary, 60)
        ),
        SessionEvent::ApprovalGranted { scope, .. } => {
            writeln!(out, "{prefix}{turn} approval_granted scope={:?}", scope)
        }
        SessionEvent::ApprovalDenied { .. } => writeln!(out, "{prefix}{turn} approval_denied"),
        SessionEvent::SandboxEntered { tier, .. } => {
            writeln!(out, "{prefix}{turn} sandbox_entered  tier={:?}", tier)
        }
        SessionEvent::Checkpoint { checkpoint_id, .. } => writeln!(
            out,
            "{prefix}{turn} checkpoint       id={}",
            checkpoint_id.as_str()
        ),
        SessionEvent::TurnCommitted { outcome, usage, .. } => writeln!(
            out,
            "{prefix}{turn} turn_committed   outcome={:?} in={} out={}",
            outcome, usage.input_tokens, usage.output_tokens
        ),
        SessionEvent::TurnAborted { reason, detail, .. } => writeln!(
            out,
            "{prefix}{turn} turn_aborted     reason={:?}{}",
            reason,
            detail
                .as_deref()
                .map(|d| format!(" detail={d}"))
                .unwrap_or_default()
        ),
        SessionEvent::TurnInterrupted { reason, .. } => {
            writeln!(out, "{prefix}{turn} turn_interrupted reason={:?}", reason)
        }
        SessionEvent::RetrievalQueried {
            backend,
            query,
            result_count,
            latency_ms,
            ..
        } => writeln!(
            out,
            "{prefix}{turn} retrieval_queried backend={} hits={} latency_ms={} query={:?}",
            backend,
            result_count,
            latency_ms,
            truncate(query, 60)
        ),
    }
}

fn render_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => format!("text {:?}", truncate(text, 80)),
        ContentBlock::ToolUse { id, name, .. } => {
            format!("tool_use {} name={}", id.as_str(), name)
        }
        ContentBlock::ToolResult {
            tool_use_id,
            is_error,
            ..
        } => {
            format!(
                "tool_result tool_use={} error={}",
                tool_use_id.as_str(),
                is_error
            )
        }
        ContentBlock::Thinking { text, .. } => {
            format!("thinking {:?}", truncate(text, 60))
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n).collect();
        format!("{head}…")
    }
}

fn short_digest(d: &str) -> String {
    let rest = d.strip_prefix("sha256:").unwrap_or(d);
    let head: String = rest.chars().take(12).collect();
    head
}

#[cfg(test)]
mod tests {
    use super::*;
    use azoth_core::schemas::{
        AbortReason, CommitOutcome, ContractId, RunId, SessionEvent, TurnId, Usage,
    };
    use std::io::Write;
    use tempfile::tempdir;

    fn synth_session() -> Vec<SessionEvent> {
        let run = RunId::from("run_abc".to_string());
        let contract = ContractId::from("ctr_xyz".to_string());
        let t1 = TurnId::from("t_001".to_string());
        let t2 = TurnId::from("t_002".to_string());
        vec![
            SessionEvent::RunStarted {
                run_id: run.clone(),
                contract_id: contract,
                timestamp: "2026-04-16T10:00:00Z".into(),
            },
            SessionEvent::TurnStarted {
                turn_id: t1.clone(),
                run_id: run.clone(),
                parent_turn: None,
                timestamp: "2026-04-16T10:00:01Z".into(),
            },
            SessionEvent::TurnCommitted {
                turn_id: t1,
                outcome: CommitOutcome::Success,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                user_input: None,
                final_assistant: None,
            },
            SessionEvent::TurnStarted {
                turn_id: t2.clone(),
                run_id: run,
                parent_turn: None,
                timestamp: "2026-04-16T10:00:10Z".into(),
            },
            SessionEvent::TurnInterrupted {
                turn_id: t2,
                reason: AbortReason::UserCancel,
                partial_usage: Default::default(),
            },
        ]
    }

    fn write_session(dir: &std::path::Path, run_id: &str, events: &[SessionEvent]) {
        let path = dir.join(format!("{run_id}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        for ev in events {
            let line = serde_json::to_string(ev).unwrap();
            writeln!(f, "{line}").unwrap();
        }
    }

    #[test]
    fn replayable_text_drops_interrupted_turn() {
        let tmp = tempdir().unwrap();
        write_session(tmp.path(), "run_abc", &synth_session());
        let args = Args {
            run_id: "run_abc".into(),
            sessions_dir: tmp.path().to_path_buf(),
            forensic: false,
            format: Format::Text,
        };
        let mut out = Vec::new();
        run(args, &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("turn_committed"),
            "committed turn must render: {s}"
        );
        assert!(
            !s.contains("turn_interrupted"),
            "interrupted turn must be filtered out in replayable projection: {s}"
        );
        assert!(
            !s.contains("[NR]"),
            "replayable projection must not annotate NR: {s}"
        );
    }

    #[test]
    fn forensic_text_keeps_interrupted_and_annotates() {
        let tmp = tempdir().unwrap();
        write_session(tmp.path(), "run_abc", &synth_session());
        let args = Args {
            run_id: "run_abc".into(),
            sessions_dir: tmp.path().to_path_buf(),
            forensic: true,
            format: Format::Text,
        };
        let mut out = Vec::new();
        run(args, &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("turn_committed"));
        assert!(s.contains("turn_interrupted"));
        assert!(
            s.contains("[NR]"),
            "forensic projection must mark NR events: {s}"
        );
    }

    #[test]
    fn json_format_is_line_delimited() {
        let tmp = tempdir().unwrap();
        write_session(tmp.path(), "run_abc", &synth_session());
        let args = Args {
            run_id: "run_abc".into(),
            sessions_dir: tmp.path().to_path_buf(),
            forensic: false,
            format: Format::Json,
        };
        let mut out = Vec::new();
        run(args, &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        for line in s.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("each line is json");
            assert!(
                v.get("type").is_some(),
                "each line is a SessionEvent: {line}"
            );
        }
    }

    #[test]
    fn missing_session_returns_not_found() {
        let tmp = tempdir().unwrap();
        let args = Args {
            run_id: "nope".into(),
            sessions_dir: tmp.path().to_path_buf(),
            forensic: false,
            format: Format::Text,
        };
        let mut out = Vec::new();
        match run(args, &mut out) {
            Err(ReplayError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
