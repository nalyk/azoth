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

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;
use tui_textarea::{Input as TaInput, TextArea};

use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, ApprovalResponse, CapabilityStore};
use azoth_core::context::{
    CompositeEvidenceCollector, EvidenceCollector, IdentityReranker, LexicalEvidenceCollector,
    ReciprocalRankFusion, SymbolEvidenceCollector, TokenBudget,
};
use azoth_core::event_store::{JsonlReader, JsonlWriter, SqliteMirror};
use azoth_core::execution::{ExecutionContext, ToolDispatcher};
use azoth_core::impact::{DiffSource, ImpactConfig, ImpactSelector};
use azoth_core::retrieval::{
    CoEditConfig, LexicalBackend, LexicalRetrieval, RetrievalConfig, RetrievalMode,
    RipgrepLexicalRetrieval, SymbolRetrieval,
};
use azoth_core::schemas::{
    ApprovalId, ApprovalScope, CapabilityTokenId, ContentBlock, Contract, ContractId, Message,
    RunId, SessionEvent, TurnId,
};
use azoth_core::tools::{
    BashTool, FsWriteTool, RepoReadFileTool, RepoReadSpansTool, RepoSearchTool,
};
use azoth_core::turn::TurnDriver;
use azoth_core::validators::{
    ContractGoalValidator, ImpactValidator, SelectorBackedImpactValidator, Validator,
};

use azoth_repo::history::co_edit;
use azoth_repo::{CoEditGraphRetrieval, FtsLexicalRetrieval, RepoIndexer, SqliteSymbolIndex};

use super::input::SlashCommand;
use super::render;

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize,
}

pub struct AppState {
    pub textarea: TextArea<'static>,
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
    pub last_context_summary: Option<String>,
    pending_approve: Option<String>,
    input_history: Vec<String>,
    history_cursor: usize,
    /// Last turn's input token count — the real context window pressure.
    pub last_input_tokens: u32,
    /// Max context window from the active profile. Set once at startup.
    pub max_context_tokens: u32,
    /// Scroll offset for the transcript (0 = pinned to bottom).
    pub scroll_offset: u16,
    /// Whether the user has manually scrolled up (disables auto-scroll).
    pub scroll_locked: bool,
}

impl AppState {
    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("type a message or /command…");
        Self {
            textarea,
            transcript: vec!["azoth · ready".to_string()],
            status: "ready".to_string(),
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
            last_context_summary: None,
            pending_approve: None,
            input_history: Vec::new(),
            history_cursor: 0,
            last_input_tokens: 0,
            max_context_tokens: 0,
            scroll_offset: 0,
            scroll_locked: false,
        }
    }

    fn textarea_content(&self) -> String {
        self.textarea.lines().join("\n")
    }

    fn handle_slash(&mut self, cmd: SlashCommand) {
        match cmd {
            SlashCommand::Help => {
                self.transcript.push("· help".into());
                self.transcript
                    .push("  /help              show this list".into());
                self.transcript
                    .push("  /status            run_id, session path, turn count".into());
                self.transcript
                    .push("  /context           latest compiled context packet".into());
                self.transcript
                    .push("  /contract <goal>   draft + accept a run contract".into());
                self.transcript
                    .push("  /approve [tool]    pre-approve a tool for the session".into());
                self.transcript
                    .push("  /resume <run_id>   (restart required in v1)".into());
                self.transcript.push("  /quit              exit".into());
            }
            SlashCommand::Status => {
                self.transcript.push("· status".into());
                self.transcript
                    .push(format!("  run_id        {}", self.run_id));
                self.transcript
                    .push(format!("  session_path  {}", self.session_path));
                self.transcript.push(format!(
                    "  pending_appr  {}",
                    if self.pending_approval.is_some() {
                        "yes"
                    } else {
                        "no"
                    }
                ));
                self.transcript
                    .push(format!("  turns         {}", self.committed_turns));
                self.transcript.push(format!(
                    "  contract      {}",
                    self.current_contract_id
                        .as_ref()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "(none)".to_string())
                ));
            }
            SlashCommand::Context => match &self.last_context_summary {
                Some(summary) => {
                    self.transcript.push("· context".into());
                    for line in summary.lines() {
                        self.transcript.push(format!("  {line}"));
                    }
                }
                None => {
                    self.transcript
                        .push("· context: no packet compiled yet".into());
                }
            },
            SlashCommand::Contract(rest) => match rest {
                None => {
                    self.transcript
                        .push("! usage: /contract <goal text>".into());
                }
                Some(goal) => {
                    let mut c = azoth_core::contract::draft(goal.clone());
                    c.success_criteria.push(format!("delivers: {goal}"));
                    self.transcript.push(format!("· contract drafted: {goal}"));
                    self.pending_contract = Some(c);
                }
            },
            SlashCommand::Approve(arg) => match arg {
                Some(tool_name) => {
                    self.transcript.push(format!(
                        "· approve: queuing session-scoped pre-approval for {tool_name}"
                    ));
                    self.pending_approve = Some(tool_name);
                }
                None => {
                    self.transcript.push("· approve".into());
                    self.transcript.push("  usage: /approve <tool_name>".into());
                    self.transcript
                        .push("  pre-grants a session-scoped capability token".into());
                    self.transcript
                        .push("  so the tool will not prompt for approval.".into());
                    self.transcript.push("  registered tools: fs.write, bash, repo.search, repo.read_file, repo.read_spans".into());
                }
            },
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
            InputEvent::Mouse(me) => self.handle_mouse(me),
            InputEvent::Resize => self.dirty = true,
        }
    }

    fn handle_mouse(&mut self, me: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;
        match me.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(3);
                self.scroll_locked = true;
                self.dirty = true;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
                if self.scroll_offset == 0 {
                    self.scroll_locked = false;
                }
                self.dirty = true;
            }
            _ => {}
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
                return;
            }
            (KeyCode::Enter, m) if !m.contains(KeyModifiers::ALT) => {
                let content = self.textarea_content();
                if !content.is_empty() {
                    self.input_history.push(content.clone());
                    self.history_cursor = self.input_history.len();
                    self.textarea = TextArea::default();
                    self.textarea
                        .set_placeholder_text("type a message or /command…");
                    self.transcript.push(format!("> {content}"));
                    if let Some(cmd) = SlashCommand::parse(&content) {
                        self.handle_slash(cmd);
                    } else {
                        self.pending_user_text = Some(content);
                    }
                    self.dirty = true;
                }
                return;
            }
            (KeyCode::Up, _)
                if self.textarea.lines().len() == 1
                    && self.textarea.lines()[0].is_empty()
                    && self.history_cursor > 0 =>
            {
                self.history_cursor -= 1;
                let prev = self.input_history[self.history_cursor].clone();
                self.textarea = TextArea::from(prev.lines().map(String::from).collect::<Vec<_>>());
                self.textarea
                    .set_placeholder_text("type a message or /command…");
                self.dirty = true;
                return;
            }
            (KeyCode::Down, _)
                if self.textarea.lines().len() == 1
                    && self.textarea.lines()[0].is_empty()
                    && self.history_cursor < self.input_history.len() =>
            {
                self.history_cursor += 1;
                if self.history_cursor < self.input_history.len() {
                    let next = self.input_history[self.history_cursor].clone();
                    self.textarea =
                        TextArea::from(next.lines().map(String::from).collect::<Vec<_>>());
                } else {
                    self.textarea = TextArea::default();
                }
                self.textarea
                    .set_placeholder_text("type a message or /command…");
                self.dirty = true;
                return;
            }
            (KeyCode::PageUp, _) => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                self.scroll_locked = true;
                self.dirty = true;
                return;
            }
            (KeyCode::PageDown, _) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                if self.scroll_offset == 0 {
                    self.scroll_locked = false;
                }
                self.dirty = true;
                return;
            }
            (KeyCode::Up, KeyModifiers::SHIFT) => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                self.scroll_locked = true;
                self.dirty = true;
                return;
            }
            (KeyCode::Down, KeyModifiers::SHIFT) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                if self.scroll_offset == 0 {
                    self.scroll_locked = false;
                }
                self.dirty = true;
                return;
            }
            (KeyCode::Up, KeyModifiers::CONTROL) => {
                self.scroll_offset = self.scroll_offset.saturating_add(5);
                self.scroll_locked = true;
                self.dirty = true;
                return;
            }
            (KeyCode::Down, KeyModifiers::CONTROL) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(5);
                if self.scroll_offset == 0 {
                    self.scroll_locked = false;
                }
                self.dirty = true;
                return;
            }
            (KeyCode::End, KeyModifiers::CONTROL) | (KeyCode::Home, KeyModifiers::CONTROL) => {
                self.scroll_offset = 0;
                self.scroll_locked = false;
                self.dirty = true;
                return;
            }
            _ => {}
        }
        // All other keys route to tui-textarea's built-in handling
        // (cursor movement, Alt+Enter for newline, Home/End, etc.)
        let ta_input: TaInput = key.into();
        if self.textarea.input(ta_input) {
            self.dirty = true;
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

    /// Drain any tool name the `/approve` handler queued for pre-approval.
    pub fn take_pending_approve(&mut self) -> Option<String> {
        self.pending_approve.take()
    }

    /// Render a `SessionEvent` into the transcript. Model text is shown
    /// prominently; internal lifecycle events are suppressed or shown as
    /// compact one-liners so the conversation is readable.
    pub fn handle_session_event(&mut self, ev: SessionEvent) {
        match ev {
            SessionEvent::ContractAccepted { contract, .. } => {
                self.transcript
                    .push(format!("  [contract accepted] {}", contract.goal));
                self.current_contract_id = Some(contract.id);
            }
            // Suppress noisy lifecycle events — they go to .azoth/azoth.log
            SessionEvent::TurnStarted { .. } | SessionEvent::ModelRequest { .. } => {}
            SessionEvent::ContextPacket {
                turn_id,
                packet_id,
                packet_digest,
            } => {
                self.last_context_summary = Some(format!(
                    "packet_id  {packet_id}\nturn_id    {turn_id}\ndigest     {packet_digest}"
                ));
            }
            SessionEvent::ContentBlock { block, .. } => match block {
                ContentBlock::Text { text } => {
                    // Model text — the main content the user wants to read.
                    for line in text.lines() {
                        self.transcript.push(format!("  {line}"));
                    }
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let summary = input
                        .get("command")
                        .or_else(|| input.get("path"))
                        .or_else(|| input.get("q"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("...");
                    self.transcript.push(format!("  [{name}] {summary}"));
                }
                ContentBlock::ToolResult {
                    is_error, content, ..
                } => {
                    if is_error {
                        let msg = content
                            .first()
                            .and_then(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .unwrap_or("error");
                        self.transcript.push(format!("  [error] {msg}"));
                    }
                }
                ContentBlock::Thinking { .. } => {
                    self.transcript.push("  [thinking...]".into());
                }
            },
            SessionEvent::EffectRecord { effect, .. } => {
                if effect.error.is_some() {
                    self.transcript.push(format!(
                        "  [effect error] {} {:?}",
                        effect.tool_name, effect.error
                    ));
                }
            }
            SessionEvent::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                if is_error {
                    self.transcript
                        .push(format!("  [tool error] id={tool_use_id}"));
                }
            }
            SessionEvent::ApprovalGranted { scope, .. } => {
                let scope_label = match &scope {
                    ApprovalScope::Once => "once",
                    ApprovalScope::Session => "session",
                    ApprovalScope::ScopedPaths { .. } => "scoped-paths",
                };
                self.transcript.push(format!("  [approved {scope_label}]"));
            }
            SessionEvent::TurnCommitted { usage, .. } => {
                self.last_input_tokens = usage.input_tokens;
                if self.max_context_tokens > 0 {
                    self.ctx_pct = ((usage.input_tokens as u64 * 100)
                        / self.max_context_tokens as u64)
                        .min(100) as u8;
                }
                self.transcript.push(format!(
                    "  [done] {} in / {} out tokens",
                    usage.input_tokens, usage.output_tokens
                ));
                self.committed_turns = self.committed_turns.saturating_add(1);
            }
            SessionEvent::TurnAborted { reason, detail, .. } => {
                let d = detail.unwrap_or_default();
                self.transcript.push(format!("  [aborted] {reason:?}: {d}"));
            }
            SessionEvent::TurnInterrupted { reason, .. } => {
                self.transcript.push(format!("  [interrupted] {reason:?}"));
            }
            // Suppress all other internal events (ApprovalRequest,
            // ApprovalDenied, SandboxEntered, Checkpoint, ValidatorResult,
            // RunStarted). They're in the JSONL + log file.
            _ => {}
        }
        self.dirty = true;
    }

    pub fn push_error(&mut self, msg: impl Into<String>) {
        self.transcript.push(format!("! {}", msg.into()));
        self.dirty = true;
    }
}

/// Composite-lane indexer backends materialised at worker startup.
/// Each backend owns its OWN `rusqlite::Connection` against the shared
/// `.azoth/state.sqlite` file — WAL mode (set once by any opener and
/// persisted on the file) then multiplexes concurrent reads across
/// backends, while the per-backend Mutex only serialises calls within
/// a single lane. PR #11 review: this is the pattern CLAUDE.md already
/// documented; the first pass shared one `Arc<Mutex<Connection>>`
/// across all lanes, which worked under composite's sequential `for
/// lane in lanes { ... }` loop but blocked future parallel-lane work
/// and diverged from the documented contract.
struct IndexerBackends {
    fts: Arc<FtsLexicalRetrieval>,
    symbols: Arc<SqliteSymbolIndex>,
    /// Held for liveness so Sprint 7.1 can bolt on a
    /// `GraphEvidenceCollector` without reopening the Connection.
    #[allow(dead_code)]
    graph: Arc<CoEditGraphRetrieval>,
}

/// Open the mirror DB through a writer-role `RepoIndexer`, run the
/// reindex pass, best-effort rebuild the co-edit graph, then drop
/// the writer handle. Each reader backend (FTS / symbols / graph)
/// opens its own reader Connection on the same file. Returns `None`
/// on any failure — composite falls back to ripgrep-only operation.
async fn build_indexer_backends(
    db_path: &std::path::Path,
    repo_root: &std::path::Path,
    co_edit_cfg: CoEditConfig,
) -> Option<IndexerBackends> {
    // Writer Connection — reindex + co_edit build phase. Dropped at
    // function exit so readers below aren't forced to share with a
    // long-lived writer handle.
    let indexer = match RepoIndexer::open(db_path, repo_root.to_path_buf()) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "repo indexer open failed; composite lanes will be ripgrep-only");
            return None;
        }
    };

    match indexer.reindex_incremental().await {
        Ok(stats) => {
            tracing::info!(
                walked = stats.walked,
                inserted = stats.inserted,
                updated = stats.updated,
                deleted = stats.deleted,
                symbols_extracted = stats.symbols_extracted,
                "repo indexer pass complete"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "reindex_incremental failed; lanes may serve stale data");
        }
    }

    // Co-edit graph build uses the indexer's own (writer) Connection
    // via spawn_blocking — git shell-out + SQLite writes.
    let co_edit_conn = indexer.connection();
    let co_edit_root = repo_root.to_path_buf();
    let co_edit_res = tokio::task::spawn_blocking(move || {
        co_edit::build(&co_edit_conn, &co_edit_root, co_edit_cfg)
    })
    .await;
    match co_edit_res {
        Ok(Ok(stats)) => {
            tracing::info!(
                commits_walked = stats.commits_walked,
                commits_contributed = stats.commits_contributed,
                commits_skipped_large = stats.commits_skipped_large,
                edges_written = stats.edges_written,
                elapsed_ms = stats.elapsed_ms,
                "co_edit graph built"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "co_edit graph build skipped");
        }
        Err(join_err) => {
            tracing::warn!(error = %join_err, "co_edit graph build join failed");
        }
    }
    // Release the writer handle before opening the reader trio so the
    // Connection doesn't linger unnecessarily.
    drop(indexer);

    // Reader backends — each opens its own Connection against the
    // shared file. `::open` enables WAL + synchronous=NORMAL and runs
    // migrations idempotently on entry (matches the pattern
    // `FtsLexicalRetrieval::open` established in Sprint 1).
    let fts = match FtsLexicalRetrieval::open(db_path) {
        Ok(f) => Arc::new(f),
        Err(e) => {
            tracing::warn!(error = %e, "FTS retrieval open failed; composite lanes will be ripgrep-only");
            return None;
        }
    };
    let symbols = match SqliteSymbolIndex::open(db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!(error = %e, "symbol retrieval open failed; composite lanes will be ripgrep-only");
            return None;
        }
    };
    let graph = match CoEditGraphRetrieval::open(db_path) {
        Ok(g) => Arc::new(g),
        Err(e) => {
            tracing::warn!(error = %e, "graph retrieval open failed; composite lanes will be ripgrep-only");
            return None;
        }
    };

    Some(IndexerBackends {
        fts,
        symbols,
        graph,
    })
}

pub async fn run_app(resume: Option<String>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (input_tx, mut input_rx) = mpsc::channel::<InputEvent>(128);
    let (user_tx, mut user_rx) = mpsc::channel::<String>(8);
    let (contract_tx, mut contract_rx) = mpsc::channel::<Contract>(8);
    let (session_tx, mut session_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let (error_tx, mut error_rx) = mpsc::channel::<String>(8);
    let (approval_req_tx, mut approval_req_rx) = mpsc::channel::<ApprovalRequestMsg>(8);
    let (approve_tx, mut approve_rx) = mpsc::channel::<String>(8);

    // Dedicated input task — prevents the keyboard reader from being starved
    // by model streaming in the main select loop.
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(Ok(ev)) = events.next().await {
            let to_send = match ev {
                TermEvent::Key(k) => Some(InputEvent::Key(k)),
                TermEvent::Mouse(m) => Some(InputEvent::Mouse(m)),
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
    let session_path = cwd
        .join(".azoth")
        .join("sessions")
        .join(format!("{run_id}.jsonl"));
    let artifacts_root = cwd.join(".azoth").join("artifacts");

    // Resolve the provider profile on the main thread so we can read
    // max_context_tokens for the status line before spawning the worker.
    let provider_profile = super::config::resolve_profile();
    let profile_max_ctx = provider_profile.max_context_tokens;
    let profile_status = format!("{} · {}", provider_profile.name, provider_profile.model_id);

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

        // Single binding for the shared mirror DB path. SqliteMirror,
        // RepoIndexer, FtsLexicalRetrieval, SqliteSymbolIndex, and
        // CoEditGraphRetrieval all open their own Connection on this
        // same file — WAL mode, set once at first open and persisted
        // on the file, lets the independent handles multiplex reads.
        let db_path = worker_cwd.join(".azoth").join("state.sqlite");

        // SQLite mirror: one per repo at `.azoth/state.sqlite` (draft_plan
        // line ~85). JSONL is authoritative — mirror failures log and
        // continue, never block the turn.
        match SqliteMirror::open(&db_path) {
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
        dispatcher.register(RepoReadFileTool);
        dispatcher.register(RepoReadSpansTool);
        dispatcher.register(FsWriteTool);
        dispatcher.register(BashTool);
        let dispatcher = Arc::new(dispatcher);

        // Resume amnesia fix: if we're opening an existing session, rebuild
        // the cross-turn `Vec<Message>` the prior worker had in memory from
        // the replayable JSONL projection. Fresh sessions start empty (no
        // TurnCommitted events exist yet, so `rebuild_history` would return
        // an empty Vec anyway — but skipping the read avoids a spurious
        // file-open on the brand-new path). Any read error falls back to an
        // empty history so the session at least starts cleanly instead of
        // aborting the worker.
        let mut history: Vec<Message> = if resuming {
            match JsonlReader::open(&worker_session_path).rebuild_history() {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(error = %e, "rebuild history failed, starting empty");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
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

        // Default validator set. The driver's EndTurn gate short-circuits
        // when `contract` is None, so this slice is inert on contract-less
        // runs — byte shape stays identical to pre-validators. When a
        // contract is active, `ContractGoalValidator` emits one
        // `ValidatorResult` per turn and gates the `Checkpoint`.
        let goal_validator = ContractGoalValidator;
        let validators: &[&dyn Validator] = &[&goal_validator];

        // Build the real adapter from the profile resolved on the main thread.
        tracing::info!(
            profile = %provider_profile.name,
            base_url = %provider_profile.base_url,
            model = %provider_profile.model_id,
            "resolved provider profile"
        );
        let adapter = super::config::build_adapter(&provider_profile);

        // Evidence collector. Sprint 7 ship defaults are `composite`
        // mode + `fts` lexical backend. Ripgrep stays reachable via
        // `AZOTH_LEXICAL_BACKEND=ripgrep`; legacy (single-lane) mode
        // via `AZOTH_RETRIEVAL_MODE=legacy`.
        //
        // Composite-mode slot assignment (PR #11 review fix):
        //   - `lexical`  — always ripgrep (substring / tokenised).
        //   - `fts`      — always FTS5 (BM25), when the indexer is
        //                  available.
        //   - `symbol`   — tree-sitter-Rust symbol index.
        //   - `graph`    — co-edit neighbours (built for future lanes;
        //                  `GraphEvidenceCollector` is Sprint 7.1).
        // The `lexical_backend` knob is therefore LEGACY-ONLY — in
        // composite mode the two lanes are deliberately different
        // backends so RRF's per-lane score summation (after label
        // dedupe) measures genuine cross-lane agreement. The original
        // Sprint 7 wiring put FTS into both slots when knob=fts (the
        // new default), which double-scored FTS-only matches and
        // skewed ranking vs. graph/symbol hits. Gemini MED + Codex P1
        // both flagged this.
        //
        // The composite is best-effort: indexer failures degrade to
        // a ripgrep-only composite so dogfood on fresh repos still
        // works.
        let retrieval_cfg = RetrievalConfig::from_env();
        let ripgrep_retrieval: Arc<dyn LexicalRetrieval> = Arc::new(RipgrepLexicalRetrieval {
            root: worker_cwd.clone(),
        });

        let indexer_backends =
            build_indexer_backends(&db_path, &worker_cwd, retrieval_cfg.co_edit).await;

        let (fts_retrieval, symbol_retrieval) = match indexer_backends.as_ref() {
            Some(b) => (Some(Arc::clone(&b.fts)), Some(Arc::clone(&b.symbols))),
            None => (None, None),
        };

        // Legacy-mode (single-lane) backend selection honours the
        // `lexical_backend` knob. This branch only runs when
        // `retrieval.mode = legacy`; composite mode ignores the knob
        // (see slot-assignment comment above).
        let (legacy_slot_retrieval, legacy_backend_in_use): (
            Arc<dyn LexicalRetrieval>,
            &'static str,
        ) = match (retrieval_cfg.lexical_backend, fts_retrieval.clone()) {
            (LexicalBackend::Fts, Some(fts)) => (fts as Arc<dyn LexicalRetrieval>, "fts"),
            (LexicalBackend::Fts, None) => {
                tracing::warn!(
                    "AZOTH_LEXICAL_BACKEND=fts requested but indexer unavailable; \
                         falling back to ripgrep for legacy mode"
                );
                (ripgrep_retrieval.clone(), "ripgrep_fallback")
            }
            (LexicalBackend::Ripgrep, _) => (ripgrep_retrieval.clone(), "ripgrep"),
            (LexicalBackend::Both, _) => (ripgrep_retrieval.clone(), "both"),
        };
        let legacy_collector: Arc<dyn EvidenceCollector> =
            Arc::new(LexicalEvidenceCollector::new(legacy_slot_retrieval));

        // Composite lanes — always ripgrep for lexical + FTS for fts
        // so RRF scores cross-lane agreement, not self-duplication.
        let ripgrep_lane_collector: Arc<dyn EvidenceCollector> =
            Arc::new(LexicalEvidenceCollector::new(ripgrep_retrieval.clone()));
        let fts_lane_collector: Option<Arc<dyn EvidenceCollector>> =
            fts_retrieval.as_ref().map(|fts| {
                let fts_dyn: Arc<dyn LexicalRetrieval> = fts.clone();
                Arc::new(LexicalEvidenceCollector::new(fts_dyn)) as Arc<dyn EvidenceCollector>
            });
        let symbol_lane_collector: Option<Arc<dyn EvidenceCollector>> =
            symbol_retrieval.as_ref().map(|sym| {
                let sym_dyn: Arc<dyn SymbolRetrieval> = sym.clone();
                Arc::new(SymbolEvidenceCollector::new(sym_dyn)) as Arc<dyn EvidenceCollector>
            });

        let composite_collector: Arc<dyn EvidenceCollector> = {
            let mut c = CompositeEvidenceCollector {
                graph: None, // Sprint 7.1: GraphEvidenceCollector needs a
                // seed-path policy; the co-edit graph is built but not
                // yet queried from the composite lane.
                symbol: symbol_lane_collector.clone(),
                lexical: Some(ripgrep_lane_collector),
                fts: fts_lane_collector.clone(),
                reranker: match retrieval_cfg.mode {
                    // RRF is the Sprint 4 default reranker when composite
                    // is selected. Identity stays available as a test
                    // double but the production knob is RRF.
                    RetrievalMode::Composite => Arc::new(ReciprocalRankFusion::default()),
                    RetrievalMode::Legacy => Arc::new(IdentityReranker),
                },
                budget: TokenBudget::v2_default(),
                per_lane_limit: 20,
            };
            c.budget.max_tokens = 8192;
            Arc::new(c)
        };
        let evidence_collector: &dyn EvidenceCollector = match retrieval_cfg.mode {
            RetrievalMode::Legacy => legacy_collector.as_ref(),
            RetrievalMode::Composite => composite_collector.as_ref(),
        };
        tracing::info!(
            mode = retrieval_cfg.mode.as_str(),
            legacy_backend = legacy_backend_in_use,
            fts_lane_wired = fts_lane_collector.is_some(),
            symbol_lane_wired = symbol_lane_collector.is_some(),
            indexer_ready = indexer_backends.is_some(),
            "retrieval mode resolved"
        );

        // Sprint 5 TDAD — opt-in impact selection. PR #9 codex P1:
        // the TUI worker used to hard-code `impact_validators: &[]`
        // and `diff_source: None`, which made the whole pipeline
        // unreachable outside tests. When `AZOTH_IMPACT_ENABLED=true`
        // we now construct a concrete selector + diff source at
        // worker startup — `CargoTestImpact::discover` shells out
        // to `cargo test --no-run` once, then the selector reuses
        // the universe for every turn. Default stays `false` through
        // v2 ship (plan-only); Sprint 7 will flip the default along
        // with `retrieval.lexical_backend` and `retrieval.mode`.
        let impact_cfg = ImpactConfig::from_env();
        let (impact_selector, diff_source_opt): (
            Option<Arc<SelectorBackedImpactValidator>>,
            Option<Arc<azoth_repo::GitStatusDiffSource>>,
        ) = if impact_cfg.enabled {
            match azoth_repo::CargoTestImpact::discover(worker_cwd.clone()).await {
                Ok(sel) => {
                    tracing::info!(
                        universe_size = sel.universe().len(),
                        "impact selector ready"
                    );
                    let sel: Arc<dyn ImpactSelector> = Arc::new(sel);
                    let validator =
                        Arc::new(SelectorBackedImpactValidator::new("impact:cargo_test", sel));
                    let src = Arc::new(azoth_repo::GitStatusDiffSource::new(worker_cwd.clone()));
                    (Some(validator), Some(src))
                }
                Err(e) => {
                    // Discovery failure is non-fatal — log loudly
                    // and fall back to a no-op pipeline so a broken
                    // workspace doesn't prevent `azoth` from booting.
                    // The empty slice keeps the validate phase
                    // byte-identical to pre-Sprint-5.
                    tracing::warn!(
                        error = %e,
                        "impact enabled but cargo discovery failed; pipeline disabled for this session"
                    );
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        tracing::info!(enabled = impact_cfg.enabled, "impact pipeline resolved");

        // Stash the last accepted contract from JSONL on startup/resume.
        // The writer tap replays ContractAccepted into the UI, but the
        // driver needs its own handle — the tap is one-way and never
        // loops back into the worker.
        let resume_reader = JsonlReader::open(&worker_session_path);
        let mut active_contract: Option<Contract> =
            resume_reader.last_accepted_contract().ok().flatten();
        // Rehydrate `turns_completed` and the per-class effect tally from the
        // replayable projection so the contract's `max_turns` / `effect_budget`
        // gates resume exactly where the prior session left off. Any read
        // failure falls back to a clean slate — matching the writer's
        // tolerance of a missing / fresh log.
        let (mut effects_consumed, mut turns_completed) =
            resume_reader.committed_run_progress().unwrap_or_default();

        // Has a `RunStarted` event already been appended to this session's
        // JSONL? The TUI worker emits one just before the first
        // `ContractAccepted` — which is either the user's first
        // `/contract <goal>` OR an auto-drafted contract on their first
        // message. Tracked as a single bool so resume doesn't double-emit
        // and so the auto-draft path shares the same gate as the slash
        // path.
        let mut run_started_emitted = resume_reader
            .replayable()
            .map(|events| {
                events
                    .iter()
                    .any(|e| matches!(e.0, SessionEvent::RunStarted { .. }))
            })
            .unwrap_or(false);

        loop {
            let user_text = tokio::select! {
                biased;
                maybe_contract = contract_rx.recv() => {
                    let Some(contract) = maybe_contract else { break };
                    let ts = time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());

                    // Emit RunStarted once per session, right before the
                    // first ContractAccepted. Before this, new sessions
                    // started writing mid-stream with no run-level marker.
                    if !run_started_emitted {
                        if let Err(e) = writer.append(&SessionEvent::RunStarted {
                            run_id: worker_run_id.clone(),
                            contract_id: contract.id.clone(),
                            timestamp: ts.clone(),
                        }) {
                            let _ = worker_error_tx
                                .send(format!("run_started append failed: {e}"))
                                .await;
                        } else {
                            run_started_emitted = true;
                        }
                    }

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
                maybe_approve = approve_rx.recv() => {
                    let Some(tool_name) = maybe_approve else { break };
                    // Look up the tool's effect class so we mint the right
                    // token. Unknown tools get a warning but no token.
                    if let Some(tool) = dispatcher.tool(&tool_name) {
                        let ec = tool.effect_class();
                        let tok = azoth_core::authority::mint_from_approval(
                            &tool_name,
                            ec,
                            ApprovalScope::Session,
                        );
                        caps.mint(tok);
                        let _ = worker_session_tx.send(SessionEvent::ApprovalGranted {
                            turn_id: TurnId::from("pre-approve".to_string()),
                            approval_id: ApprovalId::new(),
                            token: CapabilityTokenId::new(),
                            scope: ApprovalScope::Session,
                        });
                    } else {
                        let _ = worker_error_tx
                            .send(format!("approve: unknown tool {tool_name:?}"))
                            .await;
                    }
                    continue;
                }
                maybe_user = user_rx.recv() => {
                    let Some(t) = maybe_user else { break };
                    t
                }
            };
            // Auto-draft a contract on the user's first message if none has
            // been accepted yet. Without this, a session runs contract-less:
            // validators never fire, checkpoints never land, and the context
            // kernel has no durable state to compile from — observed as total
            // cross-turn amnesia in dogfood run_f465299c1a5e (turn 4 said
            // "I don't have any source code provided yet" after turn 3
            // analyzed the whole repo). The explicit `/contract <goal>` path
            // is still honored; this is only the fallback for users who just
            // start typing.
            if active_contract.is_none() {
                let goal = {
                    let one_line: String = user_text
                        .chars()
                        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                        .collect();
                    if one_line.chars().count() <= 200 {
                        one_line
                    } else {
                        let head: String = one_line.chars().take(200).collect();
                        format!("{head}…")
                    }
                };
                let mut draft = azoth_core::contract::draft(goal.clone());
                draft.success_criteria.push(format!("delivers: {goal}"));
                let ts = time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());

                if !run_started_emitted {
                    if let Err(e) = writer.append(&SessionEvent::RunStarted {
                        run_id: worker_run_id.clone(),
                        contract_id: draft.id.clone(),
                        timestamp: ts.clone(),
                    }) {
                        let _ = worker_error_tx
                            .send(format!("run_started append failed: {e}"))
                            .await;
                    } else {
                        run_started_emitted = true;
                    }
                }

                match azoth_core::contract::accept_and_persist(&mut writer, draft, ts) {
                    Ok(accepted) => {
                        active_contract = Some(accepted);
                    }
                    Err(e) => {
                        let _ = worker_error_tx
                            .send(format!("auto-draft contract failed: {e}"))
                            .await;
                    }
                }
            }

            let turn_id = TurnId::new();
            let ctx = ExecutionContext::builder(
                worker_run_id.clone(),
                turn_id.clone(),
                artifacts.clone(),
                worker_cwd.clone(),
            )
            .build();

            history.push(Message::user_text(user_text));

            // Materialise the TurnDriver's impact slice + diff ref
            // from the Arc-owned handles each turn. The references
            // live only for this `drive_turn` call; the Arcs own
            // the underlying objects across turns. `Option::as_slice`
            // gives us the single-or-zero element view without an
            // allocation — clippy idiom.
            let iv_opt: Option<&dyn ImpactValidator> = impact_selector
                .as_deref()
                .map(|v| v as &dyn ImpactValidator);
            let iv_slice: &[&dyn ImpactValidator] = iv_opt.as_slice();
            let diff_source_ref: Option<&dyn DiffSource> =
                diff_source_opt.as_deref().map(|s| s as &dyn DiffSource);

            let mut driver = TurnDriver {
                run_id: worker_run_id.clone(),
                adapter: adapter.as_ref(),
                dispatcher: dispatcher.as_ref(),
                writer: &mut writer,
                ctx: &ctx,
                capabilities: &mut caps,
                approval_bridge: approval_req_tx.clone(),
                contract: active_contract.as_ref(),
                turns_completed,
                kernel: Some(&kernel),
                validators,
                effects_consumed: &mut effects_consumed,
                evidence_collector: Some(evidence_collector),
                // Bind the impact validator slice at each turn —
                // `impact_selector` owns the `Arc`, we hand out a
                // reference-slice that lives for this `drive_turn`
                // call only. Empty slice when the knob is off or
                // discovery failed, matching pre-Sprint-5 wire
                // shape. No unsafe: the `Arc` outlives the borrow
                // by construction (it's held in the enclosing
                // worker task).
                impact_validators: iv_slice,
                diff_source: diff_source_ref,
            };

            let result = driver
                .drive_turn(
                    turn_id.clone(),
                    "You are azoth, a coding-first agent.".into(),
                    history.clone(),
                )
                .await;

            match result {
                Ok(outcome) => {
                    turns_completed = turns_completed.saturating_add(1);
                    // Cross-turn memory: fold the model's final response back
                    // into `history` so the next turn's model_request carries
                    // the full prior conversation, not just user messages.
                    // Before this, the TUI worker's history was user-only and
                    // the model had total amnesia across turns — a no-contract
                    // session (dogfood run_f465299c1a5e) hit this hard.
                    if let Some(assistant_content) = outcome.final_assistant {
                        history.push(Message {
                            role: azoth_core::schemas::Role::Assistant,
                            content: assistant_content,
                        });
                    }
                }
                Err(e) => {
                    let _ = worker_error_tx.send(format!("turn error: {e}")).await;
                }
            }
        }
    });

    let mut state = AppState::new();
    state.run_id = run_id.to_string();
    state.session_path = session_path.display().to_string();
    state.max_context_tokens = profile_max_ctx;
    state.status = profile_status;
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
                if let Some(tool_name) = state.take_pending_approve() {
                    if approve_tx.send(tool_name).await.is_err() {
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
            terminal.draw(|f| render::frame(f, &mut state))?;
            state.dirty = false;
        }
        if state.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    terminal.backend_mut().execute(DisableMouseCapture)?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use azoth_core::schemas::ContextPacketId;

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

    #[test]
    fn context_packet_event_populates_last_context_summary() {
        let mut state = AppState::new();
        assert!(state.last_context_summary.is_none());

        state.handle_session_event(SessionEvent::ContextPacket {
            turn_id: TurnId::new(),
            packet_id: ContextPacketId::new(),
            packet_digest: "sha256:abc123".into(),
        });

        let summary = state.last_context_summary.as_ref().expect("summary set");
        assert!(summary.contains("sha256:abc123"));
    }

    #[test]
    fn slash_context_shows_summary_when_present() {
        let mut state = AppState::new();
        state.last_context_summary = Some("packet_id  ctx_test\ndigest  sha256:ff".into());
        state.handle_slash(SlashCommand::Context);
        assert!(state.transcript.iter().any(|l| l.contains("ctx_test")));
        assert!(!state
            .transcript
            .iter()
            .any(|l| l.contains("no packet compiled yet")));
    }

    #[test]
    fn slash_context_shows_stub_when_no_packet() {
        let mut state = AppState::new();
        state.handle_slash(SlashCommand::Context);
        assert!(state
            .transcript
            .iter()
            .any(|l| l.contains("no packet compiled yet")));
    }

    #[test]
    fn slash_approve_with_arg_queues_tool_name() {
        let mut state = AppState::new();
        state.handle_slash(SlashCommand::Approve(Some("fs.write".into())));
        let tool = state.take_pending_approve().expect("pending approve");
        assert_eq!(tool, "fs.write");
        assert!(state.transcript.iter().any(|l| l.contains("fs.write")));
    }

    #[test]
    fn slash_approve_without_arg_shows_usage() {
        let mut state = AppState::new();
        state.handle_slash(SlashCommand::Approve(None));
        assert!(state.take_pending_approve().is_none());
        assert!(state
            .transcript
            .iter()
            .any(|l| l.contains("usage: /approve")));
    }
}
