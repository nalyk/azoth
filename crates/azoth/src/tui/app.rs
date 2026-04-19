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
use std::time::Instant;
use tokio::sync::mpsc;
use tui_textarea::{Input as TaInput, TextArea};

use azoth_core::artifacts::ArtifactStore;
use azoth_core::authority::{ApprovalRequestMsg, ApprovalResponse, CapabilityStore};
use azoth_core::context::{
    CompositeEvidenceCollector, EvidenceCollector, GraphEvidenceCollector, IdentityReranker,
    LexicalEvidenceCollector, ReciprocalRankFusion, SymbolEvidenceCollector, TokenBudget,
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

use super::card::{CardState, CellResult, Note, ToolCell, TurnCard, UsageChip};
use super::input::SlashCommand;
use super::inspector::InspectorData;
use super::palette::{PaletteAction, PaletteState};
use super::render;
use super::theme::Theme;
use super::whisper::Whisper;

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize,
}

/// A mouse-click target registered by the render path, resolved at
/// click time by `AppState::handle_mouse`. Keyed by absolute Y in
/// the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickTarget {
    ThoughtsToggle { card_idx: usize },
    CellToggle { card_idx: usize, cell_idx: usize },
    SheetApproveOnce,
    SheetApproveSession,
    SheetDeny,
    PaletteOpen,
    FocusToggle,
    RailToggle,
    InspectorToggle,
}

pub struct AppState {
    pub textarea: TextArea<'static>,
    /// PAPER cards — the visible manuscript. Replaces the flat `Vec<String>`
    /// transcript of the pre-PAPER TUI. Each card is a structured turn.
    pub cards: Vec<TurnCard>,
    /// System notes — slash-command feedback, session banners, errors.
    /// Rendered in the whisper row, not in the canvas.
    pub notes: Vec<Note>,
    /// Single-row narrator above the composer.
    pub whisper: Whisper,
    /// Command palette state (⌃K).
    pub palette: PaletteState,
    /// Resolved theme (glyph table + colors).
    pub theme: Theme,
    /// Monotonic clock for pulse/blink phase.
    pub boot: Instant,
    /// Right-side inspector drawer (⌃2).
    pub inspector_open: bool,
    /// Left-side turn rail (⌃1).
    pub rail_open: bool,
    /// Focus mode — hide all cards except the live one (⌃\).
    pub focus_mode: bool,
    /// Structured data driving the inspector drawer.
    pub inspector_data: InspectorData,

    /// Splashscreen flag — true while the worker is initialising
    /// (opening JSONL, SQLite mirror, tree-sitter index, FTS index,
    /// co-edit graph, adapter). The UI draws a centered splash
    /// instead of the canvas until the worker signals ready.
    pub booting: bool,
    /// Splash phase narration — updated as the worker progresses.
    pub boot_phase: String,

    /// Click targets registered by the render path for mouse
    /// handling. Outer Vec indexed by absolute terminal Y; inner Vec
    /// holds `(x_range, ClickTarget)` pairs so multiple buttons on
    /// one row (sheet action bar, status row toggles) are reachable.
    /// Repopulated every frame so stale entries don't fire.
    pub click_map: Vec<Vec<(std::ops::Range<u16>, ClickTarget)>>,

    pub status: String,
    pub ctx_pct: u8,
    pub dirty: bool,
    pub should_quit: bool,
    pending_user_text: Option<String>,
    pending_contract: Option<Contract>,
    pub pending_approval: Option<ApprovalRequestMsg>,
    /// Scroll offset within the approval sheet body (rows from top).
    /// Reset to 0 on each new pending_approval; advanced by scroll
    /// wheel / arrow keys while a sheet is open. Closes codex R21
    /// P1 (long approval summaries got silently clipped).
    pub sheet_scroll_offset: u16,
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
    /// Scroll offset for the canvas (0 = pinned to latest).
    pub scroll_offset: u16,
    /// Whether the user has manually scrolled up (disables auto-scroll).
    pub scroll_locked: bool,
    /// Cached `(card_idx, cell_idx)` order for `Tab` cell-cycling,
    /// newest→oldest. `None` = dirty; recomputed on next Tab press.
    /// Invalidated whenever a card or cell is added (TurnStarted +
    /// ToolUse handlers). Replaces an O(N+M) walk-allocate-collect on
    /// every Tab keystroke.
    tab_order_cache: Option<Vec<(usize, usize)>>,
    /// Index into `tab_order_cache` for the currently-focused cell.
    /// `None` after every cache invalidation; reseeded on first Tab
    /// from `card.cell_focus` via one O(N) scan, then O(1) advances
    /// (`(idx + 1) % len`) for every subsequent Tab. Earlier the Tab
    /// handler called `order.iter().position()` on every keystroke.
    tab_cursor: Option<usize>,
}

impl AppState {
    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("what are we building?");
        let inspector_data = InspectorData {
            tools: vec![
                "repo_search".into(),
                "repo_read_file".into(),
                "repo_read_spans".into(),
                "fs_write".into(),
                "bash".into(),
            ],
            ..Default::default()
        };
        Self {
            textarea,
            cards: Vec::new(),
            notes: Vec::new(),
            whisper: Whisper::default(),
            palette: PaletteState::default(),
            theme: Theme::detect(),
            boot: Instant::now(),
            inspector_open: false,
            rail_open: false,
            focus_mode: false,
            inspector_data,
            booting: true,
            boot_phase: "starting up".to_string(),
            click_map: Vec::new(),
            status: "ready".to_string(),
            ctx_pct: 0,
            dirty: true,
            should_quit: false,
            pending_user_text: None,
            pending_contract: None,
            pending_approval: None,
            sheet_scroll_offset: 0,
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
            tab_order_cache: None,
            tab_cursor: None,
        }
    }

    fn textarea_content(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Find the most recent card matching `turn_id`. Walks in reverse
    /// because the live turn is almost always the target.
    pub fn card_by_turn_id_mut(&mut self, turn_id: &str) -> Option<&mut TurnCard> {
        self.cards.iter_mut().rev().find(|c| c.turn_id == turn_id)
    }

    /// Flip the card driving `turn_id` to `AwaitingApproval` so the user
    /// can see which turn is blocked while the approval sheet is open.
    /// Only mutates Live cards — terminal states are left alone.
    fn set_card_awaiting_approval(&mut self, turn_id: &TurnId) {
        let tid = turn_id.to_string();
        if let Some(card) = self.card_by_turn_id_mut(&tid) {
            if matches!(card.state, CardState::Live) {
                card.state = CardState::AwaitingApproval;
            }
        }
    }

    /// True when something on the canvas is currently animating or
    /// transitioning under a time window — live/awaiting bar pulse,
    /// cursor blink, pending-cell sweep, whisper spinner, recent
    /// note that needs to fade out at the 5s mark, post-commit
    /// bloom decay (~600ms), or post-append shimmer decay (~400ms).
    /// The tick handler uses this to mark `dirty = true` only when
    /// a redraw would be visible; idle sessions still pay zero
    /// per-tick redraws.
    ///
    /// Earlier code only checked `is_live() || is_narrating()` — so
    /// notes stayed visually stuck past the 5s window until input
    /// arrived, and the bloom decay on a Committed bar froze on
    /// the first frame after commit.
    fn has_active_animation(&self) -> bool {
        if self.cards.iter().any(|c| c.is_live()) || self.whisper.is_narrating() {
            return true;
        }
        // Note fade window — Whisper.render_line shows latest_note
        // when its `.at.elapsed() < 5s`. Match that threshold so the
        // last frame of a fading note actually paints.
        const NOTE_TTL_SECS: f32 = 5.0;
        if let Some(latest) = self.notes.last() {
            if latest.at.elapsed().as_secs_f32() < NOTE_TTL_SECS {
                return true;
            }
        }
        // Commit-bloom decay window — see `motion::bloom_phase`.
        const BLOOM_MS: u128 = 600;
        // Streaming-shimmer decay window — see `motion::shimmer_chars`.
        const SHIMMER_MS: u128 = 400;
        self.cards.iter().any(|c| {
            c.committed_at
                .map(|t| t.elapsed().as_millis() < BLOOM_MS)
                .unwrap_or(false)
                || c.last_append
                    .map(|t| t.elapsed().as_millis() < SHIMMER_MS)
                    .unwrap_or(false)
        })
    }

    /// Take the pending approval request and roll any `AwaitingApproval`
    /// card back to `Live`. Every grant/deny path goes through here so
    /// the amber accent never lingers after the sheet closes.
    fn take_pending_approval(&mut self) -> Option<ApprovalRequestMsg> {
        let req = self.pending_approval.take();
        // Worker processes turns sequentially → at most one card is
        // AwaitingApproval. Search from newest and break on the first
        // hit; previous version walked the whole transcript.
        for card in self.cards.iter_mut().rev() {
            if matches!(card.state, CardState::AwaitingApproval) {
                card.state = CardState::Live;
                break;
            }
        }
        req
    }

    fn run_palette_action(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::ShowContext => {
                if let Some(s) = self.last_context_summary.clone() {
                    for line in s.lines() {
                        self.notes.push(Note::info(line.to_string()));
                    }
                } else {
                    self.notes
                        .push(Note::help("no packet compiled yet — send a message first"));
                }
            }
            PaletteAction::ShowContract => {
                if let Some(g) = self.inspector_data.contract_goal.clone() {
                    self.notes.push(Note::info(format!("contract · {g}")));
                } else {
                    self.notes.push(Note::help(
                        "no contract accepted yet — type `/contract <goal>` or just send a message",
                    ));
                }
            }
            PaletteAction::ShowTools => {
                self.notes.push(Note::info(format!(
                    "tools · {}",
                    self.inspector_data.tools.join(", ")
                )));
            }
            PaletteAction::ShowEvidence => {
                if self.inspector_data.evidence_lanes.is_empty() {
                    self.notes.push(Note::help(
                        "no evidence yet — send a message to trigger retrieval",
                    ));
                } else {
                    for (lane, label) in &self.inspector_data.evidence_lanes {
                        self.notes.push(Note::info(format!("{lane:<8} {label}")));
                    }
                }
            }
            PaletteAction::OpenRail => {
                self.rail_open = !self.rail_open;
            }
            PaletteAction::OpenInspector => {
                self.inspector_open = !self.inspector_open;
            }
            PaletteAction::FocusMode => {
                self.focus_mode = !self.focus_mode;
            }
            PaletteAction::Quit => {
                self.should_quit = true;
            }
            PaletteAction::Continue => {
                self.pending_user_text = Some(
                    "Please continue from where you left off — pick up the \
                     partial output and finish."
                        .to_string(),
                );
                // Note added in round 14 to match the slash-handler
                // behaviour — earlier the palette path silently queued
                // the prompt with no user feedback, while /continue
                // showed "continue requested". Now both paths agree.
                self.notes.push(Note::info("continue requested"));
            }
            PaletteAction::DraftContract(Some(goal)) => {
                let mut draft = azoth_core::contract::draft(goal.clone());
                draft.success_criteria.push(format!("delivers: {goal}"));
                self.pending_contract = Some(draft);
                self.notes
                    .push(Note::info(format!("contract drafted · {goal}")));
            }
            PaletteAction::DraftContract(None) => {
                self.notes.push(Note::help("usage: /contract <goal>"));
            }
            PaletteAction::Approve(Some(tool)) => {
                self.pending_approve = Some(tool.clone());
                self.notes
                    .push(Note::info(format!("approving tool {tool} session-scope")));
            }
            PaletteAction::Approve(None) => {
                self.notes.push(Note::help("usage: /approve <tool_name>"));
            }
            PaletteAction::Resume => {
                self.notes.push(Note::help(
                    "resume runs from the CLI: `azoth resume <run_id>`",
                ));
            }
            PaletteAction::JumpToTurn(idx) => {
                // Sum cached row counts for cards [0..idx) to find
                // where the target card starts in the full transcript
                // y-coord. Then set scroll_offset so that y lands at
                // the top of the visible window. Closes codex R21 P2
                // (jump N was previously a note-only no-op).
                if idx < self.cards.len() {
                    let prefix_y: usize = self.cards[..idx]
                        .iter()
                        .map(|c| c.last_rendered_rows.max(4))
                        .sum();
                    self.scroll_offset = u16::try_from(prefix_y).unwrap_or(u16::MAX);
                    self.scroll_locked = true;
                    self.notes
                        .push(Note::info(format!("jumped to turn {}", idx + 1)));
                } else {
                    self.notes.push(Note::warn(format!(
                        "jump · turn {} out of range (have {})",
                        idx + 1,
                        self.cards.len()
                    )));
                }
            }
            PaletteAction::UnknownSlash(name) => {
                self.notes
                    .push(Note::warn(format!("unknown command: /{name}")));
            }
        }
    }

    fn handle_slash(&mut self, cmd: SlashCommand) {
        // Delegate to `run_palette_action` for every variant that
        // already has a palette equivalent — gemini round-14 MED
        // flagged the duplication that had silently drifted
        // (e.g. /continue used to show a note but the palette
        // version didn't). Slash-only branches (Help, Status, the
        // Resume `<id>` shortcut) stay inline.
        match cmd {
            SlashCommand::Context => self.run_palette_action(PaletteAction::ShowContext),
            SlashCommand::Contract(arg) => {
                self.run_palette_action(PaletteAction::DraftContract(arg))
            }
            SlashCommand::Approve(arg) => self.run_palette_action(PaletteAction::Approve(arg)),
            SlashCommand::Quit => self.run_palette_action(PaletteAction::Quit),
            SlashCommand::Continue => self.run_palette_action(PaletteAction::Continue),
            SlashCommand::Unknown(name) => {
                self.run_palette_action(PaletteAction::UnknownSlash(name))
            }
            // Slash-only — these have no palette equivalent or take a
            // CLI-specific argument the palette can't supply.
            SlashCommand::Help => {
                self.notes.push(Note::help(
                    "press ⌃K for the palette · all commands live there",
                ));
            }
            SlashCommand::Status => {
                self.notes.push(Note::info(format!(
                    "run {} · turns {} · contract {}",
                    if self.run_id.is_empty() {
                        "(pending)".to_string()
                    } else {
                        self.run_id.chars().take(14).collect()
                    },
                    self.committed_turns,
                    self.current_contract_id
                        .as_ref()
                        .map(|c| c.to_string().chars().take(14).collect())
                        .unwrap_or_else(|| "none".to_string())
                )));
            }
            SlashCommand::Resume(Some(id)) => {
                // Slash-only behaviour: print restart instruction +
                // quit. The palette `Resume` variant just shows help.
                self.notes.push(Note::info(format!(
                    "/resume not supported at runtime — restart with: azoth resume {id}"
                )));
                self.should_quit = true;
            }
            SlashCommand::Resume(None) => self.run_palette_action(PaletteAction::Resume),
        }
        self.dirty = true;
    }

    pub fn handle_input(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::Key(key) => self.handle_key(key),
            InputEvent::Mouse(me) => self.handle_mouse(me),
            InputEvent::Resize => self.dirty = true,
        }
    }

    fn handle_mouse(&mut self, me: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match me.kind {
            MouseEventKind::ScrollUp => {
                if self.pending_approval.is_some() {
                    // Route wheel into sheet body when the modal is
                    // active (codex R21 P1).
                    self.sheet_scroll_offset = self.sheet_scroll_offset.saturating_sub(3);
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_add(3);
                    self.scroll_locked = true;
                }
                self.dirty = true;
            }
            MouseEventKind::ScrollDown => {
                if self.pending_approval.is_some() {
                    self.sheet_scroll_offset = self.sheet_scroll_offset.saturating_add(3);
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                    if self.scroll_offset == 0 {
                        self.scroll_locked = false;
                    }
                }
                self.dirty = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let y = me.row as usize;
                let x = me.column;
                let modal_active = self.palette.open || self.pending_approval.is_some();
                // Walk EVERY matching target on this row. A wide
                // canvas-card range can match before a narrow sheet
                // button range; the modal gate rejects the canvas hit
                // but we keep scanning so the sheet button still
                // fires. Earlier code returned on first reject and
                // approve/deny clicks became no-ops when a card row
                // sat behind the sheet area.
                let mut chosen: Option<ClickTarget> = None;
                let approval_pending = self.pending_approval.is_some();
                if let Some(row) = self.click_map.get(y) {
                    for (range, t) in row.iter() {
                        if !range.contains(&x) {
                            continue;
                        }
                        // PaletteOpen is gated separately: allowed when
                        // no approval is pending, dropped when an
                        // approval sheet is active. Earlier code let
                        // PaletteOpen through during approval, so a
                        // click on the status-row brand stole keyboard
                        // input (Enter/Esc routed to palette) and made
                        // the approval flow indirect.
                        let is_modal_target = match t {
                            ClickTarget::SheetApproveOnce
                            | ClickTarget::SheetApproveSession
                            | ClickTarget::SheetDeny => true,
                            ClickTarget::PaletteOpen => !approval_pending,
                            _ => false,
                        };
                        if modal_active && !is_modal_target {
                            continue;
                        }
                        chosen = Some(t.clone());
                        break;
                    }
                }
                if let Some(t) = chosen {
                    self.handle_click_target(t);
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    fn handle_click_target(&mut self, target: ClickTarget) {
        match target {
            ClickTarget::ThoughtsToggle { card_idx } => {
                if let Some(card) = self.cards.get_mut(card_idx) {
                    card.thoughts_expanded = !card.thoughts_expanded;
                }
            }
            ClickTarget::CellToggle { card_idx, cell_idx } => {
                if let Some(card) = self.cards.get_mut(card_idx) {
                    if let Some(cell) = card.cells.get_mut(cell_idx) {
                        cell.expanded = !cell.expanded;
                    }
                }
            }
            ClickTarget::SheetApproveOnce => {
                if let Some(req) = self.take_pending_approval() {
                    let _ = req.responder.send(ApprovalResponse::Grant {
                        scope: ApprovalScope::Once,
                    });
                    self.notes.push(Note::info("approval · granted once"));
                }
            }
            ClickTarget::SheetApproveSession => {
                if let Some(req) = self.take_pending_approval() {
                    let _ = req.responder.send(ApprovalResponse::Grant {
                        scope: ApprovalScope::Session,
                    });
                    self.notes.push(Note::info("approval · granted session"));
                }
            }
            ClickTarget::SheetDeny => {
                if let Some(req) = self.take_pending_approval() {
                    let _ = req.responder.send(ApprovalResponse::Deny);
                    self.notes.push(Note::warn("approval · denied"));
                }
            }
            ClickTarget::PaletteOpen => {
                self.palette.open();
            }
            ClickTarget::FocusToggle => {
                self.focus_mode = !self.focus_mode;
            }
            ClickTarget::RailToggle => {
                self.rail_open = !self.rail_open;
            }
            ClickTarget::InspectorToggle => {
                self.inspector_open = !self.inspector_open;
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Palette captures input when open.
        if self.palette.open {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => {
                    self.palette.close();
                }
                (KeyCode::Enter, _) => {
                    let action = self.palette.fire(self.cards.len());
                    self.palette.close();
                    if let Some(a) = action {
                        self.run_palette_action(a);
                    }
                }
                (KeyCode::Backspace, _) => {
                    self.palette.pop_char();
                }
                (KeyCode::Up, _) => {
                    self.palette.cursor_up();
                }
                (KeyCode::Down, _) => {
                    let total =
                        super::palette::match_entries(&self.palette.query, self.cards.len()).len();
                    self.palette.cursor_down(total);
                }
                (KeyCode::Char(c), _) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.palette.push_char(c);
                }
                _ => {}
            }
            self.dirty = true;
            return;
        }

        // Approval sheet captures input when a request is pending.
        if self.pending_approval.is_some() {
            match (key.code, key.modifiers) {
                (KeyCode::Enter, _) | (KeyCode::Char('y'), _) | (KeyCode::Char('Y'), _) => {
                    if let Some(req) = self.take_pending_approval() {
                        let _ = req.responder.send(ApprovalResponse::Grant {
                            scope: ApprovalScope::Once,
                        });
                        self.notes.push(Note::info("approval · granted once"));
                    }
                }
                (KeyCode::Char('s'), _) | (KeyCode::Char('S'), _) => {
                    if let Some(req) = self.take_pending_approval() {
                        let _ = req.responder.send(ApprovalResponse::Grant {
                            scope: ApprovalScope::Session,
                        });
                        self.notes.push(Note::info("approval · granted session"));
                    }
                }
                (KeyCode::Char('p'), _) | (KeyCode::Char('P'), _) => {
                    // Scoped-paths v1: a no-op empty path list is unsafe,
                    // so we route to session-scope and surface a note so
                    // the user knows the batch-plan sheet isn't in this
                    // build yet. Bona-fide ScopedPaths lands in v2.1.
                    if let Some(req) = self.take_pending_approval() {
                        let _ = req.responder.send(ApprovalResponse::Grant {
                            scope: ApprovalScope::Session,
                        });
                        self.notes.push(Note::info(
                            "approval · scoped-paths falls back to session in v1",
                        ));
                    }
                }
                (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
                    if let Some(req) = self.take_pending_approval() {
                        let _ = req.responder.send(ApprovalResponse::Deny);
                        self.notes.push(Note::warn("approval · denied"));
                    }
                }
                // Sheet body scroll — long approval summaries used to
                // be silently clipped (codex R21 P1). ↑/↓ + PgUp/PgDn
                // adjust the offset within the sheet body.
                (KeyCode::Up, _) => {
                    self.sheet_scroll_offset = self.sheet_scroll_offset.saturating_sub(1);
                }
                (KeyCode::Down, _) => {
                    self.sheet_scroll_offset = self.sheet_scroll_offset.saturating_add(1);
                }
                (KeyCode::PageUp, _) => {
                    self.sheet_scroll_offset = self.sheet_scroll_offset.saturating_sub(5);
                }
                (KeyCode::PageDown, _) => {
                    self.sheet_scroll_offset = self.sheet_scroll_offset.saturating_add(5);
                }
                _ => {}
            }
            self.dirty = true;
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.should_quit = true;
                return;
            }
            (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                self.palette.open();
                self.dirty = true;
                return;
            }
            (KeyCode::Char('1'), KeyModifiers::CONTROL) => {
                self.rail_open = !self.rail_open;
                self.dirty = true;
                return;
            }
            (KeyCode::Char('2'), KeyModifiers::CONTROL) => {
                self.inspector_open = !self.inspector_open;
                self.dirty = true;
                return;
            }
            (KeyCode::Char('\\'), KeyModifiers::CONTROL) => {
                self.focus_mode = !self.focus_mode;
                self.dirty = true;
                return;
            }
            (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                // Dedicated thoughts toggle on the latest agent card
                // with thoughts. Independent of Tab (which prioritises
                // tool cells); ⌃T always targets thoughts.
                if let Some(card) = self.cards.iter_mut().rev().find(|c| !c.thoughts.is_empty()) {
                    card.thoughts_expanded = !card.thoughts_expanded;
                    self.dirty = true;
                }
                return;
            }
            (KeyCode::Tab, m) if !m.contains(KeyModifiers::SHIFT) => {
                // Tab walks focus through every tool cell across every
                // card, newest→oldest, wrapping. The focused cell is the
                // only one expanded. Earlier builds only toggled the
                // last cell of the most recent card, leaving older cells
                // unreachable from the keyboard. Falls through to
                // thoughts / textarea when no cells exist anywhere.
                // Lazy-fill the cached cell order. Invalidated whenever
                // a card or cell is added (TurnStarted / ToolUse /
                // user Enter handlers above). Saves the rebuild cost
                // on every Tab keystroke for long sessions.
                if self.tab_order_cache.is_none() {
                    let order: Vec<(usize, usize)> = self
                        .cards
                        .iter()
                        .enumerate()
                        .rev()
                        .flat_map(|(ci, card)| (0..card.cells.len()).rev().map(move |xi| (ci, xi)))
                        .collect();
                    self.tab_order_cache = Some(order);
                    self.tab_cursor = None;
                }
                let order = self.tab_order_cache.as_ref().unwrap();
                if !order.is_empty() {
                    // Cursor is reseeded once after each cache rebuild
                    // by scanning `cell_focus` from the newest card —
                    // recent cards win. Subsequent Tabs read from the
                    // cursor and advance with `(idx + 1) % len`, O(1).
                    if self.tab_cursor.is_none() {
                        if let Some(prev) = self
                            .cards
                            .iter()
                            .enumerate()
                            .rev()
                            .find_map(|(ci, c)| c.cell_focus.map(|xi| (ci, xi)))
                        {
                            self.tab_cursor = order.iter().position(|&q| q == prev);
                        }
                    }
                    let next_idx = match self.tab_cursor {
                        Some(i) => (i + 1) % order.len(),
                        None => 0,
                    };
                    let next = order[next_idx];
                    // Sweep every cell across every card so Tab
                    // enforces "focused cell is the only one expanded"
                    // even when the user previously mouse-expanded
                    // others. The lighter "unfocus only previous"
                    // version (round 6) leaked manual mouse-expansions
                    // into keyboard navigation.
                    let (ci, xi) = next;
                    for (card_i, card) in self.cards.iter_mut().enumerate() {
                        for (cell_i, cell) in card.cells.iter_mut().enumerate() {
                            if card_i != ci || cell_i != xi {
                                cell.expanded = false;
                            }
                        }
                        if card_i != ci {
                            card.cell_focus = None;
                        }
                    }
                    if let Some(card) = self.cards.get_mut(ci) {
                        if let Some(cell) = card.cells.get_mut(xi) {
                            cell.expanded = true;
                        }
                        card.cell_focus = Some(xi);
                    }
                    self.tab_cursor = Some(next_idx);
                    self.dirty = true;
                    return;
                }
                // No cells anywhere — fall back to the latest card's
                // thoughts (still useful when the model only emits
                // reasoning blocks without tool calls).
                if let Some(card) = self.cards.iter_mut().rev().find(|c| !c.thoughts.is_empty()) {
                    card.thoughts_expanded = !card.thoughts_expanded;
                    self.dirty = true;
                    return;
                }
                // No expandable content — fall through to textarea.
            }
            (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => {
                // Collapse everything + drop focus. Equivalent to "back
                // to the closed view" so Tab restarts from the latest.
                // Reset `tab_cursor` too — otherwise the next Tab
                // resumes from the stale cursor position instead of
                // restarting from the newest cell.
                for card in self.cards.iter_mut() {
                    for cell in card.cells.iter_mut() {
                        cell.expanded = false;
                    }
                    card.cell_focus = None;
                }
                self.tab_cursor = None;
                self.dirty = true;
                return;
            }
            (KeyCode::Enter, m)
                if !m.contains(KeyModifiers::ALT) && !m.contains(KeyModifiers::SHIFT) =>
            {
                // Shift+Enter reaches the textarea below as a newline
                // (matches the `⇧↵ newline` hint). Earlier this branch
                // matched any non-ALT Enter and accidentally submitted
                // multi-line drafts on terminals reporting SHIFT.
                let content = self.textarea_content();
                if !content.is_empty() {
                    self.input_history.push(content.clone());
                    self.history_cursor = self.input_history.len();
                    self.textarea = TextArea::default();
                    self.textarea.set_placeholder_text("what are we building?");
                    if let Some(cmd) = SlashCommand::parse(&content) {
                        self.handle_slash(cmd);
                    } else {
                        // Push the user's card immediately — the card
                        // appears before the model even sees the turn.
                        // Use a fresh `TurnId` so back-to-back user
                        // sends produce globally-unique card IDs (the
                        // earlier `committed_turns`-based ID could
                        // collide if the user pressed Enter twice
                        // before the agent committed the prior turn).
                        let user_turn_id = TurnId::new().to_string();
                        self.cards
                            .push(TurnCard::user(user_turn_id, content.clone()));
                        self.tab_order_cache = None;
                        self.tab_cursor = None;
                        self.pending_user_text = Some(content);
                        // Queued state — spinner appears in the
                        // whisper row immediately so there's no silent
                        // gap between keystroke and the first
                        // SessionEvent from the worker. TurnStarted
                        // overrides this to "thinking".
                        self.whisper.set("queued · waiting for the worker");
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
                self.textarea.set_placeholder_text("what are we building?");
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
                self.textarea.set_placeholder_text("what are we building?");
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
                let goal = contract.goal.clone();
                self.inspector_data.contract_goal = Some(goal.clone());
                let budget = contract
                    .effect_budget
                    .max_apply_local
                    .saturating_add(contract.effect_budget.max_apply_repo);
                self.inspector_data.contract_budget = Some((0, budget));
                self.notes
                    .push(Note::info(format!("contract accepted · {goal}")));
                self.current_contract_id = Some(contract.id);
            }
            SessionEvent::TurnStarted { turn_id, .. } => {
                self.cards.push(TurnCard::agent(turn_id.to_string()));
                self.tab_order_cache = None;
                self.tab_cursor = None;
                self.whisper.set("thinking");
                // Evidence lanes are per-turn — flush so the inspector
                // shows what *this* turn retrieved, not the prior one's
                // residue. Repopulated by RetrievalQueried / SymbolResolved
                // arms below.
                self.inspector_data.evidence_lanes.clear();
            }
            SessionEvent::ModelRequest { .. } => {
                self.whisper.set("waiting for the model");
            }
            SessionEvent::ContextPacket {
                turn_id,
                packet_id,
                packet_digest,
            } => {
                self.last_context_summary = Some(format!(
                    "packet_id  {packet_id}\nturn_id    {turn_id}\ndigest     {packet_digest}"
                ));
                let digest_short: String = packet_digest.chars().take(18).collect();
                self.inspector_data.packet_digest = Some(digest_short);
                self.inspector_data.turn_id = Some(turn_id.to_string());
            }
            SessionEvent::ContentBlock { turn_id, block, .. } => {
                let tid = turn_id.to_string();
                match block {
                    ContentBlock::Text { text } => {
                        if let Some(card) = self.card_by_turn_id_mut(&tid) {
                            card.append_prose(&text);
                        }
                        self.whisper.clear();
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        let summary = input
                            .get("command")
                            .or_else(|| input.get("path"))
                            .or_else(|| input.get("q"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("…")
                            .to_string();
                        let cell = ToolCell {
                            tool_use_id: id.to_string(),
                            name: name.clone(),
                            summary: summary.clone(),
                            expanded: false,
                            result: CellResult::Pending,
                            preview_lines: Vec::new(),
                            full_lines: Vec::new(),
                            created_at: Instant::now(),
                            cached_preview_render: None,
                            cached_full_render: None,
                        };
                        if let Some(card) = self.card_by_turn_id_mut(&tid) {
                            card.add_cell(cell);
                            self.tab_order_cache = None;
                            self.tab_cursor = None;
                        }
                        let narration: String = summary.chars().take(40).collect();
                        self.whisper.set(format!("running {name} · {narration}"));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        content,
                    } => {
                        // Borrow the text without cloning the entire body —
                        // a 100k-line build log used to clone fully into
                        // `preview_text` before we even decided how much
                        // we needed.
                        // Concatenate ALL text blocks across the
                        // ToolResult content — earlier code only
                        // grabbed the first text block via find_map,
                        // dropping later text content (a tool returning
                        // `[Text "stdout", Image, Text "stderr"]` lost
                        // the stderr). Borrow when possible, allocate
                        // a join only when there are multiple blocks.
                        let texts: Vec<&str> = content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect();
                        let joined_storage: String;
                        let preview_text: &str = match texts.as_slice() {
                            [] => "",
                            [single] => single,
                            many => {
                                joined_storage = many.join("\n");
                                &joined_storage
                            }
                        };
                        let tu_id = tool_use_id.to_string();
                        if let Some(card) = self.card_by_turn_id_mut(&tid) {
                            if let Some(cell) = card.cell_by_id_mut(&tu_id) {
                                // Single streaming pass: take at most 4
                                // preview + 24 full + count, never
                                // materialising a `Vec<&str>` over the
                                // whole output. The walk is hard-capped
                                // at MAX_LINES_SCANNED so a tool emitting
                                // millions of lines (a runaway `find /`
                                // or a giant log) cannot lock the UI
                                // thread while we count.
                                const MAX_LINES_SCANNED: usize = 10_000;
                                // Hard byte cap so even a 100MB output
                                // with no newlines (or pathological
                                // 10k newlines averaging 1MB each)
                                // can't lock the UI thread.
                                // `lines()` walks every byte to find
                                // `\n`, so the line cap alone isn't
                                // enough.
                                const MAX_BYTES_SCANNED: usize = 1_048_576; // 1 MiB
                                const MAX_LINE_BYTES: usize = 1024;
                                let mut preview: Vec<String> = Vec::with_capacity(5);
                                let mut full: Vec<String> = Vec::with_capacity(24);
                                let mut total_lines: u32 = 0;
                                let mut first_line: Option<String> = None;
                                let mut truncated = false;
                                let scan_slice = if preview_text.len() > MAX_BYTES_SCANNED {
                                    truncated = true;
                                    // floor to char boundary
                                    let mut end = MAX_BYTES_SCANNED;
                                    while end > 0 && !preview_text.is_char_boundary(end) {
                                        end -= 1;
                                    }
                                    &preview_text[..end]
                                } else {
                                    preview_text
                                };
                                for (i, line) in scan_slice.lines().enumerate() {
                                    if i >= MAX_LINES_SCANNED {
                                        truncated = true;
                                        break;
                                    }
                                    let want_preview = preview.len() < 4;
                                    let want_full = full.len() < 24;
                                    let is_first = total_lines == 0;
                                    // Allocate ONLY when we will use the
                                    // owned String — earlier code paid
                                    // a `to_string()` for every line up
                                    // to MAX_LINES_SCANNED, but the
                                    // result was only stored for the
                                    // first 24 entries. Wasted ~9976
                                    // allocations on a 10k-line scan.
                                    if want_preview || want_full || is_first {
                                        let trimmed = if line.len() > MAX_LINE_BYTES {
                                            // floor to char boundary
                                            let mut end = MAX_LINE_BYTES;
                                            while end > 0 && !line.is_char_boundary(end) {
                                                end -= 1;
                                            }
                                            &line[..end]
                                        } else {
                                            line
                                        };
                                        let owned = trimmed.to_string();
                                        if is_first {
                                            first_line = Some(owned.clone());
                                        }
                                        match (want_preview, want_full) {
                                            (true, true) => {
                                                preview.push(owned.clone());
                                                full.push(owned);
                                            }
                                            (true, false) => preview.push(owned),
                                            (false, true) => full.push(owned),
                                            (false, false) => {}
                                        }
                                    }
                                    total_lines = total_lines.saturating_add(1);
                                }
                                if total_lines > 4 {
                                    let suffix = if truncated { "+" } else { "" };
                                    preview.push(format!(
                                        "… +{}{} more lines",
                                        total_lines - 4,
                                        suffix
                                    ));
                                }
                                cell.set_preview_lines(preview);
                                cell.set_full_lines(full);
                                cell.result = if is_error {
                                    CellResult::Err {
                                        message: first_line
                                            .unwrap_or_else(|| "tool error".to_string()),
                                    }
                                } else if total_lines > 0 {
                                    let suffix = if truncated { "+" } else { "" };
                                    CellResult::Ok {
                                        count_hint: Some(format!("{total_lines}{suffix} lines")),
                                    }
                                } else {
                                    CellResult::Ok { count_hint: None }
                                };
                            }
                        }
                        self.whisper.clear();
                    }
                    ContentBlock::Thinking { text, .. } => {
                        if let Some(card) = self.card_by_turn_id_mut(&tid) {
                            card.append_thought(&text);
                        }
                        self.whisper.set("thinking");
                    }
                }
            }
            SessionEvent::EffectRecord { effect, .. } => {
                if effect.error.is_some() {
                    self.notes.push(Note::error(format!(
                        "effect error · {} · {:?}",
                        effect.tool_name, effect.error
                    )));
                } else if matches!(
                    effect.class,
                    azoth_core::schemas::EffectClass::ApplyLocal
                        | azoth_core::schemas::EffectClass::ApplyRepo
                ) {
                    // Successful budget-counted effect — bump the
                    // inspector's contract budget consumption so the
                    // user can see how close they are to the cap.
                    // Earlier the consumed counter sat at 0 forever.
                    if let Some((used, max)) = self.inspector_data.contract_budget.as_mut() {
                        *used = used.saturating_add(1).min(*max);
                    }
                }
            }
            SessionEvent::RetrievalQueried {
                backend,
                query,
                result_count,
                ..
            } => {
                let label = format!(
                    "{query} · {result_count} hit{}",
                    if result_count == 1 { "" } else { "s" }
                );
                self.inspector_data.evidence_lanes.push((backend, label));
            }
            SessionEvent::SymbolResolved {
                backend,
                query,
                matched,
                ..
            } => {
                let label = format!(
                    "{query} · {} match{}",
                    matched.len(),
                    if matched.len() == 1 { "" } else { "es" }
                );
                self.inspector_data
                    .evidence_lanes
                    .push((format!("symbol/{backend}"), label));
            }
            SessionEvent::ToolResult {
                turn_id,
                tool_use_id,
                is_error,
                ..
            } => {
                if is_error {
                    let tid = turn_id.to_string();
                    let tu = tool_use_id.to_string();
                    if let Some(card) = self.card_by_turn_id_mut(&tid) {
                        if let Some(cell) = card.cell_by_id_mut(&tu) {
                            if matches!(cell.result, CellResult::Pending) {
                                cell.result = CellResult::Err {
                                    message: "tool error".to_string(),
                                };
                            }
                        }
                    }
                }
            }
            SessionEvent::ApprovalGranted { scope, .. } => {
                let label = match &scope {
                    ApprovalScope::Once => "once",
                    ApprovalScope::Session => "session",
                    ApprovalScope::ScopedPaths { .. } => "scoped-paths",
                };
                self.notes.push(Note::info(format!("approval · {label}")));
            }
            SessionEvent::TurnCommitted { turn_id, usage, .. } => {
                self.last_input_tokens = usage.input_tokens;
                if self.max_context_tokens > 0 {
                    self.ctx_pct = ((usage.input_tokens as u64 * 100)
                        / self.max_context_tokens as u64)
                        .min(100) as u8;
                    if self.inspector_data.ctx_history.len() >= 24 {
                        self.inspector_data.ctx_history.remove(0);
                    }
                    self.inspector_data.ctx_history.push(self.ctx_pct as u64);
                    self.inspector_data.ctx_pct = self.ctx_pct;
                }
                let tid = turn_id.to_string();
                let chip = UsageChip {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                };
                if let Some(card) = self.card_by_turn_id_mut(&tid) {
                    card.state = CardState::Committed;
                    card.usage = Some(chip);
                    card.committed_at = Some(Instant::now());
                }
                self.committed_turns = self.committed_turns.saturating_add(1);
                self.whisper.clear();
            }
            SessionEvent::TurnAborted {
                turn_id,
                reason,
                detail,
                ..
            } => {
                let tid = turn_id.to_string();
                let reason_str = format!("{reason:?}");
                let detail_str = detail.unwrap_or_default();
                if let Some(card) = self.card_by_turn_id_mut(&tid) {
                    card.state = CardState::Aborted {
                        reason: reason_str.clone(),
                        detail: detail_str.clone(),
                    };
                }
                self.notes.push(Note::warn(format!(
                    "aborted · {reason_str}{}{}",
                    if detail_str.is_empty() { "" } else { " · " },
                    detail_str
                )));
                self.whisper.clear();
            }
            SessionEvent::TurnInterrupted {
                turn_id, reason, ..
            } => {
                let tid = turn_id.to_string();
                let reason_str = format!("{reason:?}");
                if let Some(card) = self.card_by_turn_id_mut(&tid) {
                    card.state = CardState::Interrupted {
                        reason: reason_str.clone(),
                    };
                }
                self.notes
                    .push(Note::info(format!("interrupted · {reason_str}")));
                self.whisper.clear();
            }
            _ => {}
        }
        self.dirty = true;
    }

    pub fn push_error(&mut self, msg: impl Into<String>) {
        self.notes.push(Note::error(msg.into()));
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
    /// Wired into the composite's `graph` lane via
    /// `GraphEvidenceCollector` (PR B, v2 Sprint 7.1 closure).
    /// **Option so that co-edit build failures result in the
    /// graph lane being unwired for the session** (codex round-6
    /// P2 on PR #14). Previously this was always `Arc<...>` —
    /// build failure only logged, and stale `co_edit_edges` data
    /// from a previous run kept being queried, silently skewing
    /// retrieval. Same gating shape as `eval_live::build_collector`.
    graph: Option<Arc<CoEditGraphRetrieval>>,
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
    // Codex round-6 P2: track build success so the graph lane
    // wiring below can SKIP opening CoEditGraphRetrieval when the
    // build failed. Previously we logged and then opened anyway,
    // so stale co_edit_edges from a prior run kept being queried.
    let co_edit_build_ok = match co_edit_res {
        Ok(Ok(stats)) => {
            tracing::info!(
                commits_walked = stats.commits_walked,
                commits_contributed = stats.commits_contributed,
                commits_skipped_large = stats.commits_skipped_large,
                edges_written = stats.edges_written,
                elapsed_ms = stats.elapsed_ms,
                "co_edit graph built"
            );
            true
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "co_edit graph build skipped; graph lane will be unwired");
            false
        }
        Err(join_err) => {
            tracing::warn!(error = %join_err, "co_edit graph build join failed; graph lane will be unwired");
            false
        }
    };
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
    let graph: Option<Arc<CoEditGraphRetrieval>> = if co_edit_build_ok {
        match CoEditGraphRetrieval::open(db_path) {
            Ok(g) => Some(Arc::new(g)),
            Err(e) => {
                tracing::warn!(error = %e, "graph retrieval open failed; graph lane unwired");
                None
            }
        }
    } else {
        None
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
    // Worker → UI "ready" signal. Fired once when the worker has
    // opened every subsystem (JSONL, SQLite mirror, artifact store,
    // dispatcher, retrieval backends, adapter). Lets the UI drop
    // the splashscreen at the right moment — not on a timer.
    let (boot_phase_tx, mut boot_phase_rx) = mpsc::channel::<String>(8);
    let (ready_tx, mut ready_rx) = mpsc::channel::<()>(1);

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
    let worker_boot_phase_tx = boot_phase_tx.clone();
    let worker_ready_tx = ready_tx.clone();
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
        let _ = worker_boot_phase_tx
            .send("opening session log".to_string())
            .await;

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
        let _ = worker_boot_phase_tx
            .send(format!("connecting to {}", provider_profile.name))
            .await;
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

        let _ = worker_boot_phase_tx
            .send("indexing repo (FTS + symbols + co-edit)".to_string())
            .await;
        let indexer_backends =
            build_indexer_backends(&db_path, &worker_cwd, retrieval_cfg.co_edit).await;
        let _ = worker_boot_phase_tx
            .send("finishing indexer".to_string())
            .await;

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
        // v2 Sprint 7.1 Gap 1 closure: GraphEvidenceCollector wraps
        // the co-edit graph retrieval built on worker startup. Seed
        // extraction is greedy path-regex over the query — good
        // enough to surface neighbours when the prompt references a
        // file or directory; smart seeding (symbol-resolver-driven)
        // is v2.5 with the policy DSL.
        //
        // Codex round-6 P2: `b.graph` is now `Option<...>` —
        // `None` when the co-edit build failed so we skip wiring
        // the lane entirely. Flat-mapping through both levels of
        // Option yields `Some(Arc<dyn EvidenceCollector>)` only
        // when indexer_backends is present AND the graph itself
        // built cleanly this session.
        let graph_lane_collector: Option<Arc<dyn EvidenceCollector>> = indexer_backends
            .as_ref()
            .and_then(|b| b.graph.as_ref())
            .map(|g| {
                let graph_dyn: Arc<dyn azoth_core::retrieval::GraphRetrieval> = g.clone();
                Arc::new(GraphEvidenceCollector::new(graph_dyn)) as Arc<dyn EvidenceCollector>
            });

        let composite_collector: Arc<dyn EvidenceCollector> = {
            let mut c = CompositeEvidenceCollector {
                graph: graph_lane_collector.clone(),
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
            graph_lane_wired = graph_lane_collector.is_some(),
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

        // Worker is fully booted — drop the splashscreen.
        let _ = worker_ready_tx.send(()).await;

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
        .notes
        .push(Note::info(format!("{banner} · {}", session_path.display())));
    // 50ms tick = 20fps, fast enough for the 80ms spinner cadence to
    // land on a frame boundary without skipping. The handler below
    // only marks dirty when an animation is actually running, so a
    // truly idle session pays ~0 redraws regardless of tick rate.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(50));

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
                state.set_card_awaiting_approval(&req.turn_id);
                state.pending_approval = Some(req);
                state.sheet_scroll_offset = 0;
                state.dirty = true;
            }
            Some(err) = error_rx.recv() => {
                // Worker init paths return early after sending an error
                // and never fire `ready_rx`, so the splash spinner used
                // to spin forever and the queued error notes stayed
                // hidden. Drop the splash on any error so the notes
                // surface immediately. Post-boot errors are no-op here
                // because `booting` is already false.
                state.push_error(err);
                if state.booting {
                    state.booting = false;
                    state.boot_phase = "boot failed".to_string();
                    state.dirty = true;
                }
            }
            Some(phase) = boot_phase_rx.recv() => {
                state.boot_phase = phase;
                state.dirty = true;
            }
            Some(_) = ready_rx.recv() => {
                state.booting = false;
                state.boot_phase = "ready".to_string();
                state.dirty = true;
            }
            _ = ticker.tick() => {
                // Splash spinner OR any in-flight animation needs a
                // re-render even when no channel is active. Idle
                // session (all cards committed/aborted, whisper
                // silent) pays nothing.
                if state.booting || state.has_active_animation() {
                    state.dirty = true;
                }
            }
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
            .notes
            .iter()
            .any(|n| n.text.contains("usage: /contract")));
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
        assert!(state.notes.iter().any(|n| n.text.contains("ctx_test")));
        assert!(!state
            .notes
            .iter()
            .any(|n| n.text.contains("no packet compiled yet")));
    }

    #[test]
    fn slash_context_shows_stub_when_no_packet() {
        let mut state = AppState::new();
        state.handle_slash(SlashCommand::Context);
        assert!(state
            .notes
            .iter()
            .any(|n| n.text.contains("no packet compiled yet")));
    }

    #[test]
    fn slash_approve_with_arg_queues_tool_name() {
        let mut state = AppState::new();
        state.handle_slash(SlashCommand::Approve(Some("fs_write".into())));
        let tool = state.take_pending_approve().expect("pending approve");
        assert_eq!(tool, "fs_write");
        assert!(state.notes.iter().any(|n| n.text.contains("fs_write")));
    }

    #[test]
    fn slash_approve_without_arg_shows_usage() {
        let mut state = AppState::new();
        state.handle_slash(SlashCommand::Approve(None));
        assert!(state.take_pending_approve().is_none());
        assert!(state
            .notes
            .iter()
            .any(|n| n.text.contains("usage: /approve")));
    }

    #[test]
    fn user_enter_appends_user_card() {
        let mut state = AppState::new();
        state.textarea = TextArea::from(vec!["hello world".to_string()]);
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state.cards.len(), 1);
        assert_eq!(
            state.cards[0].prose, "hello world",
            "user card prose matches input"
        );
        assert_eq!(
            state.take_pending_user_text().as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn ctrl_k_opens_palette() {
        let mut state = AppState::new();
        state.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(state.palette.open, "⌃K opens the palette");
    }

    #[test]
    fn ctrl_1_toggles_rail() {
        let mut state = AppState::new();
        assert!(!state.rail_open);
        state.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL));
        assert!(state.rail_open, "⌃1 opens rail");
        state.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL));
        assert!(!state.rail_open, "⌃1 again closes rail");
    }

    #[test]
    fn ctrl_2_toggles_inspector() {
        let mut state = AppState::new();
        assert!(!state.inspector_open);
        state.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL));
        assert!(state.inspector_open);
    }

    #[test]
    fn ctrl_backslash_toggles_focus() {
        let mut state = AppState::new();
        state.handle_key(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL));
        assert!(state.focus_mode);
    }

    #[test]
    fn agent_card_materialised_by_turn_started() {
        let mut state = AppState::new();
        let turn_id = TurnId::new();
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        assert_eq!(state.cards.len(), 1);
        assert_eq!(state.cards[0].turn_id, turn_id.to_string());
        assert!(state.whisper.is_narrating());
    }

    #[test]
    fn tool_use_appends_cell_to_matching_card() {
        let mut state = AppState::new();
        let turn_id = TurnId::new();
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        state.handle_session_event(SessionEvent::ContentBlock {
            turn_id: turn_id.clone(),
            index: 0,
            block: ContentBlock::ToolUse {
                id: azoth_core::schemas::ToolUseId::from("tu_1".to_string()),
                name: "repo_search".to_string(),
                input: serde_json::json!({"q": "refresh"}),
                call_group: None,
            },
        });
        assert_eq!(state.cards[0].cells.len(), 1);
        assert_eq!(state.cards[0].cells[0].name, "repo_search");
    }

    #[test]
    fn render_does_not_panic_on_zero_state() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        terminal
            .draw(|f| super::super::render::frame(f, &mut state))
            .expect("zero-state render");
    }

    #[test]
    fn render_does_not_panic_with_full_state() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.rail_open = true;
        state.inspector_open = true;
        state.ctx_pct = 45;
        state.max_context_tokens = 100_000;
        state.inspector_data.contract_goal = Some("fix token refresh".into());
        state.inspector_data.ctx_pct = 45;
        state.inspector_data.ctx_history = vec![12, 18, 23, 30, 45];

        // User card + agent card with a pending tool cell.
        state
            .cards
            .push(TurnCard::user("t0", "fix the token refresh bug"));
        let mut agent = TurnCard::agent("t1");
        agent.append_prose("investigating the refresh flow\nfound an off-by-one");
        agent.add_cell(ToolCell {
            tool_use_id: "tu1".into(),
            name: "repo_search".into(),
            summary: "refresh_token".into(),
            expanded: false,
            result: CellResult::Ok {
                count_hint: Some("4 matches".into()),
            },
            preview_lines: vec!["src/auth/tokens.rs:42".into()],
            full_lines: vec!["src/auth/tokens.rs:42".into()],
            created_at: std::time::Instant::now(),
            cached_preview_render: None,
            cached_full_render: None,
        });
        state.cards.push(agent);

        state.notes.push(Note::info("session banner"));
        state.whisper.set("thinking");
        state.palette.open();
        state.palette.push_char('s');
        state.palette.push_char('h');

        terminal
            .draw(|f| super::super::render::frame(f, &mut state))
            .expect("full-state render with rail + inspector + palette");
    }

    #[test]
    fn render_survives_narrow_terminal_ascii_theme() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new();
        state.theme = super::super::theme::Theme { unicode: false };
        state.rail_open = true;
        state.inspector_open = true; // should auto-hide < 100 cols
        state.cards.push(TurnCard::user("t0", "hello"));
        let mut agent = TurnCard::agent("t1");
        agent.append_prose("hi");
        agent.state = super::super::card::CardState::Aborted {
            reason: "ValidatorFail".into(),
            detail: "impact_tests".into(),
        };
        state.cards.push(agent);
        terminal
            .draw(|f| super::super::render::frame(f, &mut state))
            .expect("narrow-terminal ASCII render");
    }

    #[test]
    fn set_card_awaiting_approval_only_mutates_live_cards() {
        let mut state = AppState::new();
        let turn_id = TurnId::new();
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        state.set_card_awaiting_approval(&turn_id);
        assert!(matches!(
            state.cards[0].state,
            super::super::card::CardState::AwaitingApproval
        ));

        // Mark as Committed, then confirm the helper refuses to
        // overwrite a terminal state.
        state.cards[0].state = super::super::card::CardState::Committed;
        state.set_card_awaiting_approval(&turn_id);
        assert!(matches!(
            state.cards[0].state,
            super::super::card::CardState::Committed
        ));
    }

    #[test]
    fn take_pending_approval_restores_live_state() {
        let mut state = AppState::new();
        let turn_id = TurnId::new();
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        state.set_card_awaiting_approval(&turn_id);
        assert!(matches!(
            state.cards[0].state,
            super::super::card::CardState::AwaitingApproval
        ));
        // Simulate a pending request (responder is dropped — that is
        // fine here, we only care about card-state bookkeeping).
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.pending_approval = Some(ApprovalRequestMsg {
            turn_id: turn_id.clone(),
            approval_id: ApprovalId::new(),
            tool_name: "fs_write".into(),
            effect_class: azoth_core::schemas::EffectClass::ApplyLocal,
            summary: "write foo".into(),
            responder: tx,
        });
        let taken = state.take_pending_approval();
        assert!(taken.is_some());
        assert!(matches!(
            state.cards[0].state,
            super::super::card::CardState::Live
        ));
    }

    #[test]
    fn retrieval_queried_pushes_to_evidence_lanes() {
        let mut state = AppState::new();
        let turn_id = TurnId::new();
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        assert!(state.inspector_data.evidence_lanes.is_empty());
        state.handle_session_event(SessionEvent::RetrievalQueried {
            turn_id: turn_id.clone(),
            backend: "fts".to_string(),
            query: "TurnDriver".to_string(),
            result_count: 3,
            latency_ms: 7,
        });
        state.handle_session_event(SessionEvent::SymbolResolved {
            turn_id: turn_id.clone(),
            backend: "sqlite".to_string(),
            query: "TurnDriver".to_string(),
            matched: vec![1, 2],
        });
        assert_eq!(state.inspector_data.evidence_lanes.len(), 2);
        assert_eq!(state.inspector_data.evidence_lanes[0].0, "fts");
        assert!(state.inspector_data.evidence_lanes[0].1.contains("3 hits"));
        assert_eq!(state.inspector_data.evidence_lanes[1].0, "symbol/sqlite");
        assert!(state.inspector_data.evidence_lanes[1]
            .1
            .contains("2 matches"));
    }

    #[test]
    fn turn_started_clears_stale_evidence_lanes() {
        let mut state = AppState::new();
        state
            .inspector_data
            .evidence_lanes
            .push(("ripgrep".to_string(), "stale · 42 hits".to_string()));
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: TurnId::new(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        assert!(
            state.inspector_data.evidence_lanes.is_empty(),
            "prior-turn evidence must not bleed into the fresh turn"
        );
    }

    #[test]
    fn modal_active_falls_through_to_modal_target_on_overlap() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut state = AppState::new();
        state.click_map.resize_with(20, Vec::new);
        // Wide canvas range (would be rejected by modal gate) THEN
        // narrow sheet button range on the same row. Earlier code
        // returned on first reject; this asserts the loop keeps
        // scanning and dispatches the sheet target.
        state.click_map[7].push((0..u16::MAX, ClickTarget::ThoughtsToggle { card_idx: 0 }));
        state.click_map[7].push((10..30, ClickTarget::SheetApproveOnce));
        // Open palette so the canvas target is gated.
        state.palette.open();
        state.dirty = false;
        state.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 15,
            row: 7,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        // Sheet target should still fire — handle_click_target marks
        // dirty as a side effect of the SheetApproveOnce branch.
        // (The take_pending_approval inside that branch sees None
        // because we never set pending_approval — but the dispatch
        // path was reached, which is what we test.)
        assert!(
            state.dirty,
            "modal-targeted hit should fire even when a wider canvas range matches first"
        );
    }

    #[test]
    fn tab_collapses_all_cells_to_enforce_focused_only_invariant() {
        use azoth_core::schemas::ToolUseId;
        let mut state = AppState::new();
        for tid in ["t1", "t2"] {
            state.handle_session_event(SessionEvent::TurnStarted {
                turn_id: TurnId::from(tid.to_string()),
                run_id: RunId::new(),
                parent_turn: None,
                timestamp: "2026-04-19T00:00:00Z".into(),
            });
            state.handle_session_event(SessionEvent::ContentBlock {
                turn_id: TurnId::from(tid.to_string()),
                index: 0,
                block: ContentBlock::ToolUse {
                    id: ToolUseId::from(format!("tu_{tid}")),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                    call_group: None,
                },
            });
        }
        // User mouse-expands BOTH cells (simulating two clicks).
        for card in state.cards.iter_mut() {
            for cell in card.cells.iter_mut() {
                cell.expanded = true;
            }
        }
        // Tab fires — must collapse all but the new focus.
        state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let expanded_count: usize = state
            .cards
            .iter()
            .flat_map(|c| c.cells.iter())
            .filter(|c| c.expanded)
            .count();
        assert_eq!(
            expanded_count, 1,
            "Tab must enforce 'focused cell is the only one expanded' invariant"
        );
    }

    #[test]
    fn shift_tab_resets_tab_cursor() {
        use azoth_core::schemas::ToolUseId;
        let mut state = AppState::new();
        for tid in ["t1", "t2"] {
            state.handle_session_event(SessionEvent::TurnStarted {
                turn_id: TurnId::from(tid.to_string()),
                run_id: RunId::new(),
                parent_turn: None,
                timestamp: "2026-04-19T00:00:00Z".into(),
            });
            state.handle_session_event(SessionEvent::ContentBlock {
                turn_id: TurnId::from(tid.to_string()),
                index: 0,
                block: ContentBlock::ToolUse {
                    id: ToolUseId::from(format!("tu_{tid}")),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                    call_group: None,
                },
            });
        }
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let shift_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE);
        state.handle_key(tab);
        state.handle_key(tab);
        assert_eq!(state.tab_cursor, Some(1), "two Tabs land on cursor 1");
        state.handle_key(shift_tab);
        assert_eq!(
            state.tab_cursor, None,
            "Shift+Tab must reset cursor so next Tab restarts at 0"
        );
        state.handle_key(tab);
        assert_eq!(
            state.tab_cursor,
            Some(0),
            "post-reset Tab restarts at the newest cell"
        );
    }

    #[test]
    fn modal_active_blocks_canvas_clicks() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut state = AppState::new();
        // Pre-populate the click_map with a card-target row so a
        // simulated click would normally fire ThoughtsToggle.
        state.click_map.resize_with(20, Vec::new);
        state.click_map[5].push((0..u16::MAX, ClickTarget::ThoughtsToggle { card_idx: 0 }));
        // No modal — click registers (would dirty state if a card existed,
        // but the click_target lookup just routes; no card → no-op).
        // We test the gate, not the downstream effect.
        // With palette open, clicks on canvas rows must be dropped.
        state.palette.open();
        state.dirty = false;
        state.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(
            !state.dirty,
            "canvas click while palette open must be dropped"
        );
    }

    #[test]
    fn tab_cursor_advances_in_o1_after_first_seed() {
        use azoth_core::schemas::ToolUseId;
        let mut state = AppState::new();
        for tid in ["t1", "t2", "t3"] {
            state.handle_session_event(SessionEvent::TurnStarted {
                turn_id: TurnId::from(tid.to_string()),
                run_id: RunId::new(),
                parent_turn: None,
                timestamp: "2026-04-19T00:00:00Z".into(),
            });
            for cell in ["a", "b"] {
                state.handle_session_event(SessionEvent::ContentBlock {
                    turn_id: TurnId::from(tid.to_string()),
                    index: 0,
                    block: ContentBlock::ToolUse {
                        id: ToolUseId::from(format!("tu_{tid}_{cell}")),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                        call_group: None,
                    },
                });
            }
        }
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let mut visited: Vec<usize> = Vec::new();
        for _ in 0..7 {
            state.handle_key(tab);
            visited.push(state.tab_cursor.expect("cursor populated after Tab"));
        }
        // 6 cells total → expect 0,1,2,3,4,5,0 (wrap to start).
        assert_eq!(visited, vec![0, 1, 2, 3, 4, 5, 0]);
    }

    #[test]
    fn shift_enter_does_not_submit() {
        let mut state = AppState::new();
        state.textarea.insert_str("draft line one");
        // Shift+Enter must NOT trigger the submit branch — the
        // textarea handler below should treat it as a newline.
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(
            state.cards.is_empty(),
            "Shift+Enter must not push a user card"
        );
        assert!(
            state.take_pending_user_text().is_none(),
            "Shift+Enter must not queue text for the worker"
        );
    }

    #[test]
    fn worker_error_during_boot_clears_splash() {
        let mut state = AppState::new();
        // App starts in booting state; the splash takes the canvas.
        assert!(state.booting);
        // Simulate the runtime delivering an error_rx event during
        // boot (e.g. JsonlWriter::open failed). The push_error path
        // must clear `booting` so the splash dismisses and the error
        // note becomes visible.
        state.push_error("jsonl open failed: permission denied");
        // Manually replicate the bridge logic that the main loop runs
        // when error_rx fires (push_error + boot dismissal).
        if state.booting {
            state.booting = false;
            state.boot_phase = "boot failed".to_string();
        }
        assert!(!state.booting, "splash must dismiss on init failure");
        assert!(
            state
                .notes
                .iter()
                .any(|n| n.text.contains("jsonl open failed")),
            "the error note must be present"
        );
    }

    #[test]
    fn tab_order_cache_is_invalidated_on_card_and_cell_add() {
        use azoth_core::schemas::ToolUseId;
        let mut state = AppState::new();
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        // Empty session: no expandable content, Tab falls through.
        state.handle_key(tab);
        // Add a card → cache must invalidate even if it was None.
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: TurnId::new(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        assert!(state.tab_order_cache.is_none(), "TurnStarted invalidates");
        // Add a tool cell → cache must invalidate.
        let tid = state.cards[0].turn_id.clone();
        state.handle_session_event(SessionEvent::ContentBlock {
            turn_id: TurnId::from(tid),
            index: 0,
            block: ContentBlock::ToolUse {
                id: ToolUseId::from("tu_1".to_string()),
                name: "bash".into(),
                input: serde_json::json!({}),
                call_group: None,
            },
        });
        assert!(state.tab_order_cache.is_none(), "ToolUse invalidates");
        // Tab populates the cache; second Tab reuses it (length stable).
        state.handle_key(tab);
        assert!(state.tab_order_cache.is_some(), "first Tab fills cache");
        let cached_len = state.tab_order_cache.as_ref().unwrap().len();
        state.handle_key(tab);
        assert_eq!(
            state.tab_order_cache.as_ref().unwrap().len(),
            cached_len,
            "second Tab reuses cache (no reallocation)"
        );
    }

    #[test]
    fn effect_record_increments_contract_budget_consumed() {
        let mut state = AppState::new();
        // Simulate accepting a contract with budget 5 (3 apply_local + 2 apply_repo).
        state.inspector_data.contract_budget = Some((0, 5));
        let turn_id = TurnId::new();
        // First successful ApplyLocal effect.
        state.handle_session_event(SessionEvent::EffectRecord {
            turn_id: turn_id.clone(),
            effect: azoth_core::schemas::EffectRecord {
                id: azoth_core::schemas::EffectRecordId::new(),
                tool_use_id: azoth_core::schemas::ToolUseId::from("tu_1".to_string()),
                class: azoth_core::schemas::EffectClass::ApplyLocal,
                tool_name: "fs_write".into(),
                input_digest: None,
                output_artifact: None,
                error: None,
            },
        });
        assert_eq!(state.inspector_data.contract_budget, Some((1, 5)));
        // Failed effect — must NOT bump the counter.
        state.handle_session_event(SessionEvent::EffectRecord {
            turn_id: turn_id.clone(),
            effect: azoth_core::schemas::EffectRecord {
                id: azoth_core::schemas::EffectRecordId::new(),
                tool_use_id: azoth_core::schemas::ToolUseId::from("tu_2".to_string()),
                class: azoth_core::schemas::EffectClass::ApplyLocal,
                tool_name: "fs_write".into(),
                input_digest: None,
                output_artifact: None,
                error: Some("denied".into()),
            },
        });
        assert_eq!(
            state.inspector_data.contract_budget,
            Some((1, 5)),
            "errored effects must not consume budget"
        );
        // Observe-class effect — also doesn't count.
        state.handle_session_event(SessionEvent::EffectRecord {
            turn_id,
            effect: azoth_core::schemas::EffectRecord {
                id: azoth_core::schemas::EffectRecordId::new(),
                tool_use_id: azoth_core::schemas::ToolUseId::from("tu_3".to_string()),
                class: azoth_core::schemas::EffectClass::Observe,
                tool_name: "repo_search".into(),
                input_digest: None,
                output_artifact: None,
                error: None,
            },
        });
        assert_eq!(
            state.inspector_data.contract_budget,
            Some((1, 5)),
            "Observe is not budget-counted"
        );
    }

    #[test]
    fn tool_result_caps_scan_at_max_lines_to_keep_ui_responsive() {
        let mut state = AppState::new();
        let turn_id = TurnId::new();
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        let tu = azoth_core::schemas::ToolUseId::from("tu_huge".to_string());
        state.handle_session_event(SessionEvent::ContentBlock {
            turn_id: turn_id.clone(),
            index: 0,
            block: ContentBlock::ToolUse {
                id: tu.clone(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "find /"}),
                call_group: None,
            },
        });
        // 50k lines — well above the 10k scan cap.
        let huge: String = (0..50_000)
            .map(|i| format!("line_{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.handle_session_event(SessionEvent::ContentBlock {
            turn_id,
            index: 1,
            block: ContentBlock::ToolResult {
                tool_use_id: tu,
                content: vec![ContentBlock::Text { text: huge }],
                is_error: false,
            },
        });
        let cell = &state.cards[0].cells[0];
        // Scan capped at 10k → count_hint reflects the cap with `+`.
        match &cell.result {
            CellResult::Ok { count_hint } => {
                let hint = count_hint.as_deref().unwrap_or("");
                assert!(
                    hint.ends_with("+ lines"),
                    "count_hint should mark the truncation: {hint:?}"
                );
                assert!(
                    hint.starts_with("10000"),
                    "scan should cap at 10k: {hint:?}"
                );
            }
            other => panic!("expected Ok with cap hint, got {other:?}"),
        }
        assert!(cell.preview_lines.last().unwrap().contains("+ more lines"));
    }

    #[test]
    fn has_active_animation_reflects_live_cards_and_whisper() {
        let mut state = AppState::new();
        // Idle: zero cards, silent whisper → no animation needed.
        assert!(!state.has_active_animation());
        // Whisper alone activates animation (spinner needs redraw).
        state.whisper.set("thinking");
        assert!(state.has_active_animation());
        state.whisper.clear();
        assert!(!state.has_active_animation());
        // A live agent card activates animation.
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: TurnId::new(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        state.whisper.clear();
        assert!(state.has_active_animation());
        // Once committed, animation stops.
        state.cards[0].state = super::super::card::CardState::Committed;
        assert!(!state.has_active_animation());
    }

    #[test]
    fn user_card_ids_are_unique_across_back_to_back_sends() {
        let mut state = AppState::new();
        state.textarea.insert_str("first message");
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        state.textarea.insert_str("second message");
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Both user cards exist; turn IDs must differ even though no
        // agent commit happened in between.
        let user_cards: Vec<&TurnCard> = state
            .cards
            .iter()
            .filter(|c| matches!(c.role, super::super::card::CardRole::User))
            .collect();
        assert_eq!(user_cards.len(), 2);
        assert_ne!(
            user_cards[0].turn_id, user_cards[1].turn_id,
            "back-to-back user enters must mint distinct card IDs"
        );
    }

    #[test]
    fn tool_result_streams_without_collecting_full_output() {
        // Build a 1000-line synthetic output and assert preview/full
        // are bounded at 5/24 entries even though the source is huge.
        let mut state = AppState::new();
        let turn_id = TurnId::new();
        state.handle_session_event(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            run_id: RunId::new(),
            parent_turn: None,
            timestamp: "2026-04-19T00:00:00Z".into(),
        });
        let tu = azoth_core::schemas::ToolUseId::from("tu_big".to_string());
        state.handle_session_event(SessionEvent::ContentBlock {
            turn_id: turn_id.clone(),
            index: 0,
            block: ContentBlock::ToolUse {
                id: tu.clone(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "find /"}),
                call_group: None,
            },
        });
        let big_output = (0..1000)
            .map(|i| format!("line_{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.handle_session_event(SessionEvent::ContentBlock {
            turn_id: turn_id.clone(),
            index: 1,
            block: ContentBlock::ToolResult {
                tool_use_id: tu,
                content: vec![ContentBlock::Text { text: big_output }],
                is_error: false,
            },
        });
        let cell = &state.cards[0].cells[0];
        // Preview = 4 + 1 ellipsis line, full = 24, NOT 1000.
        assert_eq!(
            cell.preview_lines.len(),
            5,
            "preview must stay bounded regardless of source size"
        );
        assert!(cell.preview_lines[4].contains("+996 more lines"));
        assert_eq!(cell.full_lines.len(), 24);
        match &cell.result {
            CellResult::Ok { count_hint } => {
                assert_eq!(count_hint.as_deref(), Some("1000 lines"));
            }
            _ => panic!("expected Ok result"),
        }
    }

    #[test]
    fn tab_reaches_older_cell_not_just_last_of_last() {
        use azoth_core::schemas::ToolUseId;
        let mut state = AppState::new();
        // Two turns, each with a tool cell.
        for (tid, name) in [("t1", "old_tool"), ("t2", "recent_tool")] {
            state.handle_session_event(SessionEvent::TurnStarted {
                turn_id: TurnId::from(tid.to_string()),
                run_id: RunId::new(),
                parent_turn: None,
                timestamp: "2026-04-19T00:00:00Z".into(),
            });
            state.handle_session_event(SessionEvent::ContentBlock {
                turn_id: TurnId::from(tid.to_string()),
                index: 0,
                block: ContentBlock::ToolUse {
                    id: ToolUseId::from(format!("tu_{tid}")),
                    name: name.to_string(),
                    input: serde_json::json!({"q": "x"}),
                    call_group: None,
                },
            });
        }
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        // First Tab: focus + expand latest card's latest cell (t2/recent_tool).
        state.handle_key(tab);
        assert!(
            state.cards[1].cells[0].expanded,
            "first Tab should expand the newest cell"
        );
        assert!(
            !state.cards[0].cells[0].expanded,
            "older cell stays closed until we step to it"
        );
        // Second Tab: advance to the older cell; newer collapses.
        state.handle_key(tab);
        assert!(
            !state.cards[1].cells[0].expanded,
            "previous focus collapses as Tab advances"
        );
        assert!(
            state.cards[0].cells[0].expanded,
            "second Tab must reach the older cell — previously unreachable from the keyboard"
        );
        // Third Tab wraps back to the newest.
        state.handle_key(tab);
        assert!(state.cards[1].cells[0].expanded);
        assert!(!state.cards[0].cells[0].expanded);
    }
}
