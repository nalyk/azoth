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
use azoth_core::event_store::{JsonlReader, JsonlWriter};
use azoth_core::execution::{CancellationToken, ExecutionContext, ToolDispatcher};
use azoth_core::schemas::{
    ApprovalScope, CommitOutcome, ContentBlock, Message, ModelTurnResponse, RunId, SessionEvent,
    StopReason, ToolUseId, TurnId, Usage,
};
use azoth_core::tools::{FsWriteTool, RepoSearchTool};
use azoth_core::turn::TurnDriver;

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
    pub pending_approval: Option<ApprovalRequestMsg>,
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
            pending_approval: None,
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
                    self.pending_user_text = Some(line);
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

    /// Render a `SessionEvent` from the TurnDriver as one or more scrollback
    /// lines. Deliberately terse: v1 wants users to see that the real event
    /// pipeline fired, not to pretty-print everything.
    pub fn handle_session_event(&mut self, ev: SessionEvent) {
        match ev {
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

        while let Some(user_text) = user_rx.recv().await {
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
            };

            let result = driver
                .drive_turn(
                    turn_id.clone(),
                    "You are azoth, a coding-first agent.".into(),
                    history.clone(),
                )
                .await;

            if let Err(e) = result {
                let _ = worker_error_tx.send(format!("turn error: {e}")).await;
            }
        }
    });

    let mut state = AppState::new();
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
