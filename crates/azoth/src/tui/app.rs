//! AppState + the biased `tokio::select!` main loop.
//!
//! Channel sizing matches draft_plan § MED-3: bounded everywhere, biased
//! branch priority so Ctrl+C / keyboard input never starves under fast
//! model streaming.
//!
//! Enter now drives a real turn end-to-end: a worker task owns a
//! `MockAdapter`, a `ToolDispatcher` with `repo.search` registered, and a
//! `JsonlWriter` writing to `.azoth/sessions/<run_id>.jsonl`. The writer's
//! tap forwards every appended `SessionEvent` to this loop so the
//! turn_started → content_block → effect_record → tool_result →
//! turn_committed sequence renders into the transcript in real time.

use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;

use azoth_core::adapter::{MockAdapter, MockScript, ProviderProfile};
use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{
    ApprovalRequestMsg, ApprovalResponse, CapabilityStore,
};
use azoth_core::event_store::{JsonlReader, JsonlWriter, SqliteMirror};
use azoth_core::execution::{CancellationToken, ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ApprovalScope, CommitOutcome, ContentBlock, Contract, ContractId, Message, ModelTurnResponse,
    RunId, SessionEvent, StopReason, ToolUseId, TurnId, Usage,
};
use azoth_core::tools::{FsWriteTool, RepoSearchTool};
use azoth_core::turn::TurnDriver;

use super::input::SlashCommand;
use super::render;

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent),
    Resize,
}

pub struct AppState {
    pub input_buffer: String,
    pub transcript: Vec<String>,
    pub status: String,
    pub ctx_pct: u8,
    pub dirty: bool,
    pub should_quit: bool,
    pending_user_text: Option<String>,
    pending_contract: Option<Contract>,
    pub pending_approval: Option<ApprovalRequestMsg>,
    pub run_id: String,
    pub session_path: String,
    pub committed_turns: u32,
    pub current_contract_id: Option<ContractId>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            input_buffer: String::new(),
            transcript: vec!["azoth · ready".to_string()],
            status: "mock adapter".to_string(),
            ctx_pct: 0,
            dirty: true,
            should_quit: false,
            pending_user_text: None,
            pending_contract: None,
            pending_approval: None,
            run_id: String::new(),
            session_path: String::new(),
            committed_turns: 0,
            current_contract_id: None,
        }
    }

    fn handle_slash(&mut self, cmd: SlashCommand) {
        match cmd {
            SlashCommand::Help => {
                self.transcript.push("· help".into());
                self.transcript.push("  /help              show this list".into());
                self.transcript.push("  /status            run_id, session path, turn count".into());
                self.transcript.push("  /context           latest compiled context packet".into());
                self.transcript.push("  /contract <goal>   draft + accept a run contract".into());
                self.transcript.push("  /approve           (not yet wired)".into());
                self.transcript.push("  /resume <run_id>   (restart required in v1)".into());
                self.transcript.push("  /quit              exit".into());
            }
            SlashCommand::Status => {
                self.transcript.push("· status".into());
                self.transcript.push(format!("  run_id        {}", self.run_id));
                self.transcript.push(format!("  session_path  {}", self.session_path));
                self.transcript.push(format!(
                    "  pending_appr  {}",
                    if self.pending_approval.is_some() { "yes" } else { "no" }
                ));
                self.transcript.push(format!("  turns         {}", self.committed_turns));
                self.transcript.push(format!(
                    "  contract      {}",
                    self.current_contract_id
                        .as_ref()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "(none)".to_string())
                ));
            }
            SlashCommand::Context => {
                self.transcript.push("· context: no packet compiled yet".into());
            }
            SlashCommand::Contract(rest) => match rest {
                None => {
                    self.transcript.push("! usage: /contract <goal text>".into());
                }
                Some(goal) => {
                    let mut c = azoth_core::contract::draft(goal.clone());
                    c.success_criteria.push(format!("delivers: {goal}"));
                    self.transcript.push(format!("· contract drafted: {goal}"));
                    self.pending_contract = Some(c);
                }
            },
            SlashCommand::Approve => {
                self.transcript.push("! /approve not yet wired".into());
            }
            SlashCommand::Quit => {
                self.should_quit = true;
            }
            SlashCommand::Resume(arg) => match arg {
                Some(id) => {
                    self.transcript.push(format!(
                        "! /resume not yet supported at runtime, restart with: azoth resume {id}"
                    ));
                    self.should_quit = true;
                }
                None => {
                    self.transcript.push("! usage: /resume <run_id>".into());
                }
            },
            SlashCommand::Unknown(name) => {
                self.transcript.push(format!("! unknown command: /{name}"));
            }
        }
    }

    pub fn handle_input(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::Key(key) => self.handle_key(key),
            InputEvent::Resize => self.dirty = true,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.pending_approval.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(req) = self.pending_approval.take() {
                        let _ = req.responder.send(ApprovalResponse::Grant {
                            scope: ApprovalScope::Once,
                        });
                        self.transcript.push("  · approval: granted once".into());
                        self.dirty = true;
                    }
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    if let Some(req) = self.pending_approval.take() {
                        let _ = req.responder.send(ApprovalResponse::Grant {
                            scope: ApprovalScope::Session,
                        });
                        self.transcript.push("  · approval: granted session".into());
                        self.dirty = true;
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(req) = self.pending_approval.take() {
                        let _ = req.responder.send(ApprovalResponse::Deny);
                        self.transcript.push("  · approval: denied".into());
                        self.dirty = true;
                    }
                }
                _ => {}
            }
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            (KeyCode::Enter, _) => {
                if !self.input_buffer.is_empty() {
                    let line = std::mem::take(&mut self.input_buffer);
                    self.transcript.push(format!("> {line}"));
                    if let Some(cmd) = SlashCommand::parse(&line) {
                        self.handle_slash(cmd);
                    } else {
                        self.pending_user_text = Some(line);
                    }
                    self.dirty = true;
                }
            }
            (KeyCode::Backspace, _) => {
                self.input_buffer.pop();
                self.dirty = true;
            }
            (KeyCode::Char(c), _) => {
                self.input_buffer.push(c);
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// Drain any user text the key handler queued for the worker.
    pub fn take_pending_user_text(&mut self) -> Option<String> {
        self.pending_user_text.take()
    }

    /// Drain any contract the slash-command handler queued for the worker.
    pub fn take_pending_contract(&mut self) -> Option<Contract> {
        self.pending_contract.take()
    }

    /// Render a `SessionEvent` from the TurnDriver as one or more scrollback
    /// lines. Deliberately terse: v1 wants users to see that the real event
    /// pipeline fired, not to pretty-print everything.
    pub fn handle_session_event(&mut self, ev: SessionEvent) {
        match ev {
            SessionEvent::ContractAccepted { contract, .. } => {
                self.transcript.push(format!(
                    "· contract_accepted {} goal={:?}",
                    contract.id, contract.goal
                ));
                self.current_contract_id = Some(contract.id);
            }
            SessionEvent::TurnStarted { turn_id, .. } => {
                self.transcript.push(format!("· turn_started {turn_id}"));
            }
            SessionEvent::ModelRequest { profile_id, .. } => {
                self.transcript.push(format!("· model_request profile={profile_id}"));
            }
            SessionEvent::ContentBlock { index, block, .. } => match block {
                ContentBlock::Text { text } => {
                    self.transcript.push(format!("  ◂ text[{index}] {text}"));
                }
                ContentBlock::ToolUse { id, name, input, .. } => {
                    self.transcript.push(format!(
                        "  ▸ tool_use[{index}] {name}({input}) id={id}"
                    ));
                }
                ContentBlock::ToolResult { tool_use_id, is_error, .. } => {
                    let tag = if is_error { "error" } else { "ok" };
                    self.transcript.push(format!(
                        "  ◂ tool_result[{index}] {tag} id={tool_use_id}"
                    ));
                }
                ContentBlock::Thinking { .. } => {
                    self.transcript.push(format!("  ◂ thinking[{index}]"));
                }
            },
            SessionEvent::EffectRecord { effect, .. } => {
                self.transcript.push(format!(
                    "  ⚙ effect_record class={:?} tool={}",
                    effect.class, effect.tool_name
                ));
            }
            SessionEvent::ToolResult { tool_use_id, is_error, .. } => {
                let tag = if is_error { "error" } else { "ok" };
                self.transcript.push(format!("  ✓ tool_result {tag} id={tool_use_id}"));
            }
            SessionEvent::TurnCommitted { turn_id, outcome, usage } => {
                let tag = match outcome {
                    CommitOutcome::Success => "success",
                    CommitOutcome::PartialSuccess => "partial",
                };
                self.transcript.push(format!(
                    "· turn_committed {turn_id} {tag} in={} out={}",
                    usage.input_tokens, usage.output_tokens
                ));
                self.committed_turns = self.committed_turns.saturating_add(1);
            }
            SessionEvent::TurnAborted { turn_id, reason, detail, .. } => {
                let d = detail.unwrap_or_default();
                self.transcript.push(format!(
                    "✗ turn_aborted {turn_id} reason={reason:?} {d}"
                ));
            }
            SessionEvent::TurnInterrupted { turn_id, reason, .. } => {
                self.transcript.push(format!(
                    "✗ turn_interrupted {turn_id} reason={reason:?}"
                ));
            }
            other => {
                if let Some(tid) = other.turn_id() {
                    self.transcript.push(format!("· event ({tid})"));
                }
            }
        }
        self.dirty = true;
    }

    pub fn push_error(&mut self, msg: impl Into<String>) {
        self.transcript.push(format!("! {}", msg.into()));
        self.dirty = true;
    }
}

/// Build a `MockScript` that exercises the full tool-use loop for one Enter:
/// first a `repo.search` tool_use, then an `EndTurn` text response that
/// acknowledges the result.
fn scripted_fs_write(query: &str) -> MockScript {
    MockScript {
        turns: vec![
            ModelTurnResponse {
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "fs.write".into(),
                    input: serde_json::json!({
                        "path": ".azoth/tmp/hello.txt",
                        "contents": format!("hello from approval path: {query}"),
                    }),
                    call_group: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage { input_tokens: 42, output_tokens: 16, ..Default::default() },
            },
            ModelTurnResponse {
                content: vec![ContentBlock::Text {
                    text: format!("(mock) wrote hello for {query:?}"),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage { input_tokens: 58, output_tokens: 24, ..Default::default() },
            },
        ],
    }
}

pub async fn run_app(resume: Option<String>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (input_tx, mut input_rx) = mpsc::channel::<InputEvent>(128);
    let (user_tx, mut user_rx) = mpsc::channel::<String>(8);
    let (contract_tx, mut contract_rx) = mpsc::channel::<Contract>(8);
    let (session_tx, mut session_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let (error_tx, mut error_rx) = mpsc::channel::<String>(8);
    let (approval_req_tx, mut approval_req_rx) = mpsc::channel::<ApprovalRequestMsg>(8);

    // Dedicated input task — prevents the keyboard reader from being starved
    // by model streaming in the main select loop.
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(Ok(ev)) = events.next().await {
            let to_send = match ev {
                TermEvent::Key(k) => Some(InputEvent::Key(k)),
                TermEvent::Resize(_, _) => Some(InputEvent::Resize),
                _ => None,
            };
            if let Some(e) = to_send {
                if input_tx.send(e).await.is_err() {
                    break;
                }
            }
        }
    });

    // Worker task: owns adapter/dispatcher/writer/ctx, runs one TurnDriver
    // per user input, and streams SessionEvents out through the writer tap.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let resuming = resume.is_some();
    let run_id = match resume {
        Some(s) => RunId::from(s),
        None => RunId::new(),
    };
    let session_path = cwd.join(".azoth").join("sessions").join(format!("{run_id}.jsonl"));
    let artifacts_root = cwd.join(".azoth").join("artifacts");

    let worker_session_tx = session_tx.clone();
    let worker_error_tx = error_tx.clone();
    let worker_run_id = run_id.clone();
    let worker_cwd = cwd.clone();
    let worker_session_path = session_path.clone();
    let worker_artifacts_root = artifacts_root.clone();

    tokio::spawn(async move {
        // Build long-lived subsystems once. On resume we open the existing
        // file (running idempotent crash recovery first), then hydrate the
        // scrollback from the replayable projection *before* attaching the
        // tap — historical events flow through the same UI sink as live
        // ones, but the tap stays clean for new turns only.
        let writer_result = if resuming {
            JsonlWriter::open_existing(&worker_session_path)
        } else {
            JsonlWriter::open(&worker_session_path)
        };
        let mut writer = match writer_result {
            Ok(w) => w,
            Err(e) => {
                let verb = if resuming { "resume" } else { "open" };
                let _ = worker_error_tx
                    .send(format!("{verb} jsonl writer failed: {e}"))
                    .await;
                return;
            }
        };
        if resuming {
            match JsonlReader::open(&worker_session_path).replayable() {
                Ok(events) => {
                    for ev in events {
                        let _ = worker_session_tx.send(ev.0);
                    }
                }
                Err(e) => {
                    let _ = worker_error_tx
                        .send(format!("hydrate replayable failed: {e}"))
                        .await;
                    return;
                }
            }
        }
        writer.set_tap(worker_session_tx.clone());

        // SQLite mirror: one per repo at `.azoth/state.sqlite` (draft_plan
        // line ~85). JSONL is authoritative — mirror failures log and
        // continue, never block the turn.
        let mirror_path = worker_cwd.join(".azoth").join("state.sqlite");
        match SqliteMirror::open(&mirror_path) {
            Ok(mirror) => writer.set_mirror(mirror),
            Err(e) => {
                tracing::warn!(error = %e, "sqlite mirror disabled: open failed");
            }
        }

        let artifacts = match ArtifactStore::open(&worker_artifacts_root) {
            Ok(a) => a,
            Err(e) => {
                let _ = worker_error_tx
                    .send(format!("open artifact store failed: {e}"))
                    .await;
                return;
            }
        };

        let mut dispatcher = ToolDispatcher::new();
        dispatcher.register(RepoSearchTool);
        dispatcher.register(FsWriteTool);
        let dispatcher = Arc::new(dispatcher);

        let mut history: Vec<Message> = Vec::new();
        let mut caps = CapabilityStore::new();

        // Per-worker ContextKernel. Reused across turns because its fields
        // are pure config (policy_version, tokenizer family, token ceiling).
        // The kernel is only consulted when an active contract exists — the
        // driver branches on `(contract, kernel)`.
        let kernel = azoth_core::context::ContextKernel {
            policy_version: "policy_v1",
            tokenizer: azoth_core::context::TokenizerFamily::Anthropic,
            max_input_tokens: 0,
        };

        // Stash the last accepted contract from JSONL on startup/resume.
        // The writer tap replays ContractAccepted into the UI, but the
        // driver needs its own handle — the tap is one-way and never
        // loops back into the worker.
        let mut active_contract: Option<Contract> =
            JsonlReader::open(&worker_session_path)
                .last_accepted_contract()
                .ok()
                .flatten();
        let mut turns_completed: u32 = 0;

        loop {
            let user_text = tokio::select! {
                biased;
                maybe_contract = contract_rx.recv() => {
                    let Some(contract) = maybe_contract else { break };
                    let ts = time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
                    match azoth_core::contract::accept_and_persist(
                        &mut writer, contract, ts,
                    ) {
                        Ok(accepted) => {
                            // Refresh the worker-side handle inline. The tap
                            // already fired ContractAccepted to the UI, but
                            // the driver reads from this local stash.
                            active_contract = Some(accepted);
                        }
                        Err(e) => {
                            let _ = worker_error_tx
                                .send(format!("contract accept failed: {e}"))
                                .await;
                        }
                    }
                    continue;
                }
                maybe_user = user_rx.recv() => {
                    let Some(t) = maybe_user else { break };
                    t
                }
            };
            let turn_id = TurnId::new();
            let adapter = MockAdapter::new(
                ProviderProfile::anthropic_default("claude-sonnet-4-6"),
                scripted_fs_write(&user_text),
            );
            let ctx = ExecutionContext {
                run_id: worker_run_id.clone(),
                turn_id: turn_id.clone(),
                artifacts: artifacts.clone(),
                cancellation: CancellationToken::new(),
                repo_root: worker_cwd.clone(),
            };

            history.push(Message::user_text(user_text));

            let mut driver = TurnDriver {
                run_id: worker_run_id.clone(),
                adapter: &adapter,
                dispatcher: dispatcher.as_ref(),
                writer: &mut writer,
                ctx: &ctx,
                capabilities: &mut caps,
                approval_bridge: approval_req_tx.clone(),
                contract: active_contract.as_ref(),
                turns_completed,
                kernel: Some(&kernel),
            };

            let result = driver
                .drive_turn(
                    turn_id.clone(),
                    "You are azoth, a coding-first agent.".into(),
                    history.clone(),
                )
                .await;

            match result {
                Ok(_) => turns_completed = turns_completed.saturating_add(1),
                Err(e) => {
                    let _ = worker_error_tx.send(format!("turn error: {e}")).await;
                }
            }
        }
    });

    let mut state = AppState::new();
    state.run_id = run_id.to_string();
    state.session_path = session_path.display().to_string();
    let banner = if resuming { "resumed" } else { "session" };
    state
        .transcript
        .push(format!("· {banner} {}", session_path.display()));
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));

    loop {
        tokio::select! {
            biased;

            Some(ev) = input_rx.recv() => {
                state.handle_input(ev);
                if let Some(text) = state.take_pending_user_text() {
                    if user_tx.send(text).await.is_err() {
                        state.push_error("worker channel closed");
                    }
                }
                if let Some(contract) = state.take_pending_contract() {
                    if contract_tx.send(contract).await.is_err() {
                        state.push_error("worker channel closed");
                    }
                }
            }
            Some(ev) = session_rx.recv() => state.handle_session_event(ev),
            Some(req) = approval_req_rx.recv() => {
                state.pending_approval = Some(req);
                state.dirty = true;
            }
            Some(err) = error_rx.recv() => state.push_error(err),
            _ = ticker.tick() => {}
            else => break,
        }

        if state.dirty {
            terminal.draw(|f| render::frame(f, &state))?;
            state.dirty = false;
        }
        if state.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_contract_with_goal_queues_draft_for_worker() {
        let mut state = AppState::new();
        state.handle_slash(SlashCommand::Contract(Some("fix token refresh".into())));
        let drafted = state.take_pending_contract().expect("drafted contract");
        assert_eq!(drafted.goal, "fix token refresh");
        assert!(drafted
            .success_criteria
            .iter()
            .any(|c| c.contains("fix token refresh")));
        // Lints clean — the worker is about to persist it.
        azoth_core::contract::lint(&drafted).expect("drafted contract lints clean");
    }

    #[test]
    fn slash_contract_without_goal_prints_usage_and_queues_nothing() {
        let mut state = AppState::new();
        state.handle_slash(SlashCommand::Contract(None));
        assert!(state.take_pending_contract().is_none());
        assert!(state
            .transcript
            .iter()
            .any(|l| l.contains("usage: /contract")));
    }

    #[test]
    fn contract_accepted_event_updates_status_line() {
        let mut state = AppState::new();
        let contract = azoth_core::contract::accept({
            let mut c = azoth_core::contract::draft("ship feature x");
            c.success_criteria.push("tests pass".into());
            c
        })
        .unwrap();
        let id = contract.id.clone();
        state.handle_session_event(SessionEvent::ContractAccepted {
            contract,
            timestamp: "2026-04-15T00:00:00Z".into(),
        });
        assert_eq!(state.current_contract_id.as_ref(), Some(&id));
    }
}
