//! `azoth export` — render a prior session's committed turns as a
//! shareable, human-readable conversation transcript.
//!
//! Replay's job is to show *what the runtime saw*; export's job is to show
//! *what the user and model said*. Only the replayable projection is
//! consulted (committed turns only), so exports never leak half-finished
//! or aborted turn state to an outside reader.
//!
//! Two formats:
//! * `markdown` (default): conversation-shaped, grouped per turn, safe to
//!   paste into an issue tracker or code review.
//! * `json`: the replayable `SessionEvent` stream as line-delimited JSON,
//!   for programmatic pipelines.

use std::path::{Path, PathBuf};

use azoth_core::event_store::{JsonlReader, ProjectionError, ReplayableEvent};
use azoth_core::schemas::{ContentBlock, SessionEvent, TurnId, Usage};
#[cfg(test)]
use azoth_core::schemas::{Message, Role};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Markdown,
    Json,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
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
    pub format: Format,
}

pub fn run<W: std::io::Write>(args: Args, out: &mut W) -> Result<(), ExportError> {
    let path = args.sessions_dir.join(format!("{}.jsonl", args.run_id));
    if !path.exists() {
        return Err(ExportError::NotFound(path));
    }
    let reader = JsonlReader::open(&path);
    let events = reader.replayable()?;

    match args.format {
        Format::Json => render_json(&events, out),
        Format::Markdown => render_markdown(&args.run_id, &path, &events, out),
    }
}

fn render_json<W: std::io::Write>(
    events: &[ReplayableEvent],
    out: &mut W,
) -> Result<(), ExportError> {
    for ev in events {
        serde_json::to_writer(&mut *out, &ev.0)?;
        writeln!(out)?;
    }
    Ok(())
}

/// Grouping state built up while walking the replayable projection. One
/// `TurnGroup` holds everything that happened inside a single committed turn
/// so the renderer can emit the turn as a cohesive block rather than
/// interleaving turns by event order.
struct TurnGroup {
    turn_id: TurnId,
    user_input: Option<Vec<ContentBlock>>,
    blocks: Vec<ContentBlock>,
    usage: Usage,
}

fn render_markdown<W: std::io::Write>(
    run_id: &str,
    path: &Path,
    events: &[ReplayableEvent],
    out: &mut W,
) -> Result<(), ExportError> {
    let mut contract_goal: Option<String> = None;
    let mut groups: Vec<TurnGroup> = Vec::new();
    let mut current: Option<TurnGroup> = None;

    for ReplayableEvent(ev) in events {
        match ev {
            SessionEvent::ContractAccepted { contract, .. } => {
                contract_goal = Some(contract.goal.clone());
            }
            SessionEvent::TurnStarted { turn_id, .. } => {
                if let Some(g) = current.take() {
                    groups.push(g);
                }
                current = Some(TurnGroup {
                    turn_id: turn_id.clone(),
                    user_input: None,
                    blocks: Vec::new(),
                    usage: Usage::default(),
                });
            }
            SessionEvent::ContentBlock { block, .. } => {
                if let Some(g) = current.as_mut() {
                    g.blocks.push(block.clone());
                }
            }
            SessionEvent::TurnCommitted {
                user_input, usage, ..
            } => {
                if let Some(g) = current.as_mut() {
                    g.user_input = user_input.clone();
                    g.usage = usage.clone();
                }
                if let Some(g) = current.take() {
                    groups.push(g);
                }
            }
            _ => {}
        }
    }
    if let Some(g) = current.take() {
        groups.push(g);
    }

    let total_in: u32 = groups.iter().map(|g| g.usage.input_tokens).sum();
    let total_out: u32 = groups.iter().map(|g| g.usage.output_tokens).sum();

    writeln!(out, "# azoth session · {run_id}")?;
    writeln!(out)?;
    writeln!(out, "- **Session file:** `{}`", path.display())?;
    if let Some(goal) = &contract_goal {
        writeln!(out, "- **Contract:** {}", escape_md(goal))?;
    }
    writeln!(out, "- **Committed turns:** {}", groups.len())?;
    writeln!(out, "- **Tokens used:** {total_in} in / {total_out} out")?;
    writeln!(out)?;

    for (i, g) in groups.iter().enumerate() {
        writeln!(out, "---")?;
        writeln!(out)?;
        writeln!(out, "## Turn {} · `{}`", i + 1, g.turn_id.as_str())?;
        writeln!(out)?;

        if let Some(content) = &g.user_input {
            writeln!(out, "### User")?;
            writeln!(out)?;
            write_message_body(content, out)?;
            writeln!(out)?;
        }

        if !g.blocks.is_empty() {
            writeln!(out, "### Assistant")?;
            writeln!(out)?;
            for block in &g.blocks {
                write_block(block, out)?;
            }
        }

        writeln!(
            out,
            "_tokens: {} in / {} out_",
            g.usage.input_tokens, g.usage.output_tokens
        )?;
        writeln!(out)?;
    }

    Ok(())
}

fn write_message_body<W: std::io::Write>(
    content: &[ContentBlock],
    out: &mut W,
) -> std::io::Result<()> {
    for b in content {
        if let ContentBlock::Text { text } = b {
            writeln!(out, "{text}")?;
            writeln!(out)?;
        }
    }
    Ok(())
}

fn write_block<W: std::io::Write>(block: &ContentBlock, out: &mut W) -> std::io::Result<()> {
    match block {
        ContentBlock::Text { text } => {
            writeln!(out, "{text}")?;
            writeln!(out)?;
        }
        ContentBlock::ToolUse { name, input, .. } => {
            writeln!(out, "**Tool call:** `{name}`")?;
            writeln!(out)?;
            writeln!(out, "```json")?;
            // Pretty-print tool input; falls back to compact on error.
            match serde_json::to_string_pretty(input) {
                Ok(s) => writeln!(out, "{s}")?,
                Err(_) => writeln!(out, "{input}")?,
            }
            writeln!(out, "```")?;
            writeln!(out)?;
        }
        ContentBlock::ToolResult {
            is_error, content, ..
        } => {
            let label = if *is_error {
                "Tool error"
            } else {
                "Tool result"
            };
            writeln!(out, "**{label}:**")?;
            writeln!(out)?;
            writeln!(out, "```")?;
            for inner in content {
                if let ContentBlock::Text { text } = inner {
                    writeln!(out, "{}", truncate_for_export(text, 2000))?;
                }
            }
            writeln!(out, "```")?;
            writeln!(out)?;
        }
        ContentBlock::Thinking { text, .. } => {
            writeln!(out, "> _thinking:_ {}", truncate_for_export(text, 400))?;
            writeln!(out)?;
        }
    }
    Ok(())
}

fn truncate_for_export(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n).collect();
        format!("{head}… [truncated]")
    }
}

/// Keep angle brackets / backticks from breaking the markdown frame. We
/// don't need full markdown escape here — the rendered document is for
/// human reading, not strict CommonMark round-trip.
fn escape_md(s: &str) -> String {
    s.replace('`', "\\`")
}

/// Helper used by tests: reconstruct the `[User, Assistant, …]` history a
/// live worker would hold, reading only the rehydrate fields of
/// TurnCommitted events. Kept here (vs. on `Message`) so the export binary
/// stays self-contained if we ever split the JSONL reader further.
#[cfg(test)]
pub fn rehydrated_pairs(events: &[ReplayableEvent]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for ReplayableEvent(ev) in events {
        if let SessionEvent::TurnCommitted {
            user_input: Some(u),
            final_assistant: Some(a),
            ..
        } = ev
        {
            out.push(Message {
                role: Role::User,
                content: u.clone(),
            });
            out.push(Message {
                role: Role::Assistant,
                content: a.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use azoth_core::schemas::{
        CommitOutcome, ContentBlock, Contract, ContractId, RunId, SessionEvent, ToolUseId, TurnId,
        Usage,
    };
    use std::io::Write;
    use tempfile::tempdir;

    fn synth_session() -> Vec<SessionEvent> {
        let run_id_val = RunId::from("run_exp".to_string());
        let contract = Contract {
            id: ContractId::from("ctr_exp".to_string()),
            goal: "fix the auth bug".into(),
            non_goals: vec![],
            success_criteria: vec!["tests pass".into()],
            scope: Default::default(),
            effect_budget: Default::default(),
            notes: vec![],
        };
        let t1 = TurnId::from("t_001".to_string());
        vec![
            SessionEvent::RunStarted {
                run_id: run_id_val.clone(),
                contract_id: contract.id.clone(),
                timestamp: "2026-04-16T10:00:00Z".into(),
            },
            SessionEvent::ContractAccepted {
                contract,
                timestamp: "2026-04-16T10:00:00Z".into(),
            },
            SessionEvent::TurnStarted {
                turn_id: t1.clone(),
                run_id: run_id_val,
                parent_turn: None,
                timestamp: "2026-04-16T10:00:01Z".into(),
            },
            SessionEvent::ContentBlock {
                turn_id: t1.clone(),
                index: 0,
                block: ContentBlock::ToolUse {
                    id: ToolUseId::from("tu_a".to_string()),
                    name: "repo.search".into(),
                    input: serde_json::json!({"q": "auth"}),
                    call_group: None,
                },
            },
            SessionEvent::ContentBlock {
                turn_id: t1.clone(),
                index: 1,
                block: ContentBlock::Text {
                    text: "Found the fix in auth.rs.".into(),
                },
            },
            SessionEvent::TurnCommitted {
                turn_id: t1,
                outcome: CommitOutcome::Success,
                usage: Usage {
                    input_tokens: 42,
                    output_tokens: 17,
                    ..Default::default()
                },
                user_input: Some(vec![ContentBlock::Text {
                    text: "please fix the auth bug".into(),
                }]),
                final_assistant: Some(vec![ContentBlock::Text {
                    text: "Found the fix in auth.rs.".into(),
                }]),
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
    fn markdown_renders_header_user_and_assistant() {
        let tmp = tempdir().unwrap();
        write_session(tmp.path(), "run_exp", &synth_session());
        let args = Args {
            run_id: "run_exp".into(),
            sessions_dir: tmp.path().to_path_buf(),
            format: Format::Markdown,
        };
        let mut out = Vec::new();
        run(args, &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("# azoth session · run_exp"));
        assert!(s.contains("**Contract:** fix the auth bug"));
        assert!(s.contains("**Committed turns:** 1"));
        assert!(s.contains("### User"));
        assert!(s.contains("please fix the auth bug"));
        assert!(s.contains("### Assistant"));
        assert!(s.contains("Found the fix in auth.rs."));
        assert!(s.contains("**Tool call:** `repo.search`"));
        assert!(s.contains("42 in / 17 out"));
    }

    #[test]
    fn json_format_is_line_delimited() {
        let tmp = tempdir().unwrap();
        write_session(tmp.path(), "run_exp", &synth_session());
        let args = Args {
            run_id: "run_exp".into(),
            sessions_dir: tmp.path().to_path_buf(),
            format: Format::Json,
        };
        let mut out = Vec::new();
        run(args, &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(!s.is_empty());
        for line in s.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
            assert!(v.get("type").is_some());
        }
    }

    #[test]
    fn missing_session_returns_not_found() {
        let tmp = tempdir().unwrap();
        let args = Args {
            run_id: "nope".into(),
            sessions_dir: tmp.path().to_path_buf(),
            format: Format::Markdown,
        };
        let mut out = Vec::new();
        match run(args, &mut out) {
            Err(ExportError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn aborted_turns_are_excluded_from_export() {
        use azoth_core::schemas::AbortReason;
        let run_id_val = RunId::from("run_exp2".to_string());
        let contract = Contract {
            id: ContractId::from("ctr_exp2".to_string()),
            goal: "mixed".into(),
            non_goals: vec![],
            success_criteria: vec![],
            scope: Default::default(),
            effect_budget: Default::default(),
            notes: vec![],
        };
        let t_ok = TurnId::from("t_ok".to_string());
        let t_bad = TurnId::from("t_bad".to_string());
        let events = vec![
            SessionEvent::RunStarted {
                run_id: run_id_val.clone(),
                contract_id: contract.id.clone(),
                timestamp: "2026-04-16T10:00:00Z".into(),
            },
            SessionEvent::ContractAccepted {
                contract,
                timestamp: "2026-04-16T10:00:00Z".into(),
            },
            SessionEvent::TurnStarted {
                turn_id: t_ok.clone(),
                run_id: run_id_val.clone(),
                parent_turn: None,
                timestamp: "ts".into(),
            },
            SessionEvent::ContentBlock {
                turn_id: t_ok.clone(),
                index: 0,
                block: ContentBlock::Text {
                    text: "committed text".into(),
                },
            },
            SessionEvent::TurnCommitted {
                turn_id: t_ok,
                outcome: CommitOutcome::Success,
                usage: Usage::default(),
                user_input: Some(vec![ContentBlock::Text { text: "ok".into() }]),
                final_assistant: Some(vec![ContentBlock::Text {
                    text: "committed text".into(),
                }]),
            },
            SessionEvent::TurnStarted {
                turn_id: t_bad.clone(),
                run_id: run_id_val,
                parent_turn: None,
                timestamp: "ts".into(),
            },
            SessionEvent::ContentBlock {
                turn_id: t_bad.clone(),
                index: 0,
                block: ContentBlock::Text {
                    text: "SECRET-ABORTED".into(),
                },
            },
            SessionEvent::TurnAborted {
                turn_id: t_bad,
                reason: AbortReason::ValidatorFail,
                detail: Some("nope".into()),
                usage: Usage::default(),
            },
        ];

        let tmp = tempdir().unwrap();
        write_session(tmp.path(), "run_exp2", &events);
        let args = Args {
            run_id: "run_exp2".into(),
            sessions_dir: tmp.path().to_path_buf(),
            format: Format::Markdown,
        };
        let mut out = Vec::new();
        run(args, &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("committed text"));
        assert!(
            !s.contains("SECRET-ABORTED"),
            "aborted turn content must not leak into export: {s}"
        );
    }
}
