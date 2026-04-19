//! Turn cards — the atomic visual unit of PAPER.
//!
//! A card replaces the flat `Vec<String>` transcript line. Each card
//! owns its own role, state, prose, and tool cells. The render path
//! iterates `Vec<TurnCard>` and produces pre-styled `Line<'static>`
//! values, avoiding the per-frame `String::clone` tax the old
//! `Vec<String>` + `Line::from(s.clone())` loop paid.

use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::markdown;
use super::motion;
use super::theme::{Palette, Theme};

/// Who owns the card's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardRole {
    User,
    Agent,
    System,
}

impl CardRole {
    pub fn label(&self) -> &'static str {
        match self {
            CardRole::User => "you",
            CardRole::Agent => "azoth",
            CardRole::System => "system",
        }
    }
}

/// The card's lifecycle state. Drives the accent bar glyph + style.
#[derive(Debug, Clone)]
pub enum CardState {
    /// Fresh card, streaming in progress — bar pulses.
    Live,
    /// Awaiting user approval — bar pulses amber.
    AwaitingApproval,
    /// Committed. Solid bar, normal prose.
    Committed,
    /// Aborted (validator, budget, runtime). Struck-through body.
    Aborted { reason: String, detail: String },
    /// Interrupted (user cancel, crash). Dashed bar.
    Interrupted { reason: String },
}

impl CardState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            CardState::Committed | CardState::Aborted { .. } | CardState::Interrupted { .. }
        )
    }
}

/// A tool cell rendered inline within an agent card.
#[derive(Debug, Clone)]
pub struct ToolCell {
    pub tool_use_id: String,
    pub name: String,
    pub summary: String,
    pub expanded: bool,
    pub result: CellResult,
    pub preview_lines: Vec<String>, // first 4 lines of result, for collapsed view
    pub full_lines: Vec<String>,
    /// Creation timestamp — drives the sweep/spinner progression.
    /// Before this field existed, all pending cells shared a
    /// process-wide boot clock which left stuck cells (turn committed
    /// without a ToolResult event) animating forever on scroll-back.
    pub created_at: Instant,
    /// Lazy cache of the rendered preview lines (collapsed view).
    /// `None` means dirty; populated on first paint, invalidated by
    /// `set_preview_lines`. Pre-cache, every paint re-ran the
    /// path-link / diff-prefix heuristics on every line.
    pub cached_preview_render: Option<Vec<Line<'static>>>,
    /// Same lazy cache for the expanded view (`full_lines`).
    pub cached_full_render: Option<Vec<Line<'static>>>,
}

impl ToolCell {
    /// Replace the preview lines and drop the rendered cache so the
    /// next paint recomputes once.
    pub fn set_preview_lines(&mut self, lines: Vec<String>) {
        self.preview_lines = lines;
        self.cached_preview_render = None;
    }

    /// Replace the full lines and drop the rendered cache so the next
    /// paint recomputes once.
    pub fn set_full_lines(&mut self, lines: Vec<String>) {
        self.full_lines = lines;
        self.cached_full_render = None;
    }

    /// Return the rendered preview lines (collapsed view), refreshing
    /// the cache on a miss. Bounded at 4 lines per the visual design.
    pub fn render_preview(&mut self, theme: &Theme) -> &[Line<'static>] {
        if self.cached_preview_render.is_none() {
            self.cached_preview_render = Some(
                self.preview_lines
                    .iter()
                    .take(4)
                    .map(|p| render_cell_preview_line(p, theme))
                    .collect(),
            );
        }
        self.cached_preview_render.as_deref().unwrap_or(&[])
    }

    /// Return the rendered full lines (expanded view), refreshing the
    /// cache on a miss. Bounded at 24 lines per the visual design.
    pub fn render_full(&mut self, theme: &Theme) -> &[Line<'static>] {
        if self.cached_full_render.is_none() {
            self.cached_full_render = Some(
                self.full_lines
                    .iter()
                    .take(24)
                    .map(|p| render_cell_preview_line(p, theme))
                    .collect(),
            );
        }
        self.cached_full_render.as_deref().unwrap_or(&[])
    }
}

/// Tool cell outcome.
#[derive(Debug, Clone)]
pub enum CellResult {
    Pending,
    Ok { count_hint: Option<String> },
    Err { message: String },
}

/// One system note / toast, used for slash-command feedback and
/// session banners. Notes live outside the card stream.
#[derive(Debug, Clone)]
pub struct Note {
    pub kind: NoteKind,
    pub text: String,
    pub at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Info,
    Warn,
    Error,
    Help,
}

impl Note {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            kind: NoteKind::Info,
            text: text.into(),
            at: Instant::now(),
        }
    }
    pub fn warn(text: impl Into<String>) -> Self {
        Self {
            kind: NoteKind::Warn,
            text: text.into(),
            at: Instant::now(),
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: NoteKind::Error,
            text: text.into(),
            at: Instant::now(),
        }
    }
    pub fn help(text: impl Into<String>) -> Self {
        Self {
            kind: NoteKind::Help,
            text: text.into(),
            at: Instant::now(),
        }
    }
}

/// Cached markdown render of a card's prose. Invalidated when the
/// prose revision bumps (any append) or when the theme's unicode flag
/// flips (glyph fallbacks differ). Eliminates the per-frame
/// `pulldown_cmark` re-parse, which was the dominant render cost on
/// long agent cards.
#[derive(Debug, Clone)]
pub struct CachedProse {
    revision: u64,
    unicode: bool,
    lines: Vec<Line<'static>>,
}

/// The card itself.
#[derive(Debug, Clone)]
pub struct TurnCard {
    pub turn_id: String,
    pub role: CardRole,
    pub state: CardState,
    /// Model prose — raw markdown source. Rendered via `markdown::render`
    /// at paint time so inline bold/italic/code, fenced code islands,
    /// headings, and bullets become real typography. Stored as a
    /// single owned `String` so streaming appends are `push_str`
    /// (no Vec growth) and the markdown cache miss path passes
    /// `&self.prose` directly without materialising a fresh joined
    /// copy on every invalidation.
    pub prose: String,
    /// Bumped on every `append_prose`. Drives `cached_prose` invalidation.
    pub prose_revision: u64,
    /// Cached markdown render of `prose`. None when never rendered or
    /// when the previous render's `(revision, unicode)` no longer match
    /// the current pair. Recomputed on demand inside `render_rows`.
    pub cached_prose: Option<CachedProse>,
    /// Extended-thinking content (from Anthropic reasoning blocks).
    /// Rendered as a collapsible block above the prose. Hidden by
    /// default; `⇥` on the live card toggles.
    pub thoughts: Vec<String>,
    /// Bumped on every `append_thought`. Drives `cached_thoughts`
    /// invalidation in the same shape as `prose_revision`.
    pub thoughts_revision: u64,
    /// Lazy cache of the rendered thought lines as `(revision, lines)`.
    /// Mirrors `cached_prose` — populated on first paint, invalidated
    /// when `thoughts_revision` changes. Eliminates per-frame
    /// `String::clone` for every thought line on long sessions where
    /// the user keeps `⌃T thoughts` expanded.
    pub cached_thoughts: Option<(u64, Vec<Line<'static>>)>,
    pub thoughts_expanded: bool,
    pub cells: Vec<ToolCell>,
    pub usage: Option<UsageChip>,
    pub started: Instant,
    /// Set when the card transitions to `Committed`. Used to drive
    /// the commit-bloom effect (accent bar glows brighter for ~600ms).
    pub committed_at: Option<Instant>,
    /// Last time prose was appended — drives the streaming shimmer
    /// (trailing accent glow on newly-typed characters).
    pub last_append: Option<Instant>,
    pub contract_goal: Option<String>,
    pub cell_focus: Option<usize>,
    /// Number of rows the card produced on its most-recent render.
    /// `0` means "never rendered" — the canvas treats that as dirty
    /// and forces a full render to learn the height. Used for the
    /// viewport-virtualisation pass: cards entirely outside the
    /// visible window get blank-line placeholders matching this
    /// count, so off-screen cards skip `render_rows` entirely on
    /// long sessions.
    pub last_rendered_rows: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct UsageChip {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Annotation emitted alongside each rendered line so the frame
/// orchestrator can register click targets without re-computing card
/// layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowHint {
    ThoughtsHeader,
    CellHeader { cell_idx: usize },
}

impl TurnCard {
    pub fn user(turn_id: impl Into<String>, text: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            turn_id: turn_id.into(),
            role: CardRole::User,
            state: CardState::Committed,
            prose: text.into(),
            prose_revision: 1,
            cached_prose: None,
            thoughts: Vec::new(),
            thoughts_revision: 0,
            cached_thoughts: None,
            thoughts_expanded: false,
            cells: Vec::new(),
            usage: None,
            started: now,
            committed_at: Some(now),
            last_append: None,
            contract_goal: None,
            cell_focus: None,
            last_rendered_rows: 0,
        }
    }
    pub fn agent(turn_id: impl Into<String>) -> Self {
        Self {
            turn_id: turn_id.into(),
            role: CardRole::Agent,
            state: CardState::Live,
            prose: String::new(),
            prose_revision: 0,
            cached_prose: None,
            thoughts: Vec::new(),
            thoughts_revision: 0,
            cached_thoughts: None,
            thoughts_expanded: false,
            cells: Vec::new(),
            usage: None,
            started: Instant::now(),
            committed_at: None,
            last_append: None,
            contract_goal: None,
            cell_focus: None,
            last_rendered_rows: 0,
        }
    }

    pub fn append_prose(&mut self, text: &str) {
        // Single push_str — newlines in the streamed chunk land
        // verbatim inside `prose`. Pre-refactor we stored prose as
        // `Vec<String>` and re-joined with `\n` on every cache miss;
        // a long agent reply (~10kB) paid that join cost per frame.
        self.prose.push_str(text);
        self.last_append = Some(Instant::now());
        self.prose_revision = self.prose_revision.wrapping_add(1);
    }

    pub fn append_thought(&mut self, text: &str) {
        // Streaming reasoning chunks arrive without trailing newlines,
        // so the first chunk of each new logical line gets joined
        // onto the last existing entry. Earlier code split + push
        // unconditionally, which fragmented "Hello," + " world!" into
        // two separate Vec entries (visible as two lines) instead of
        // one. Mirrors the round-5 prose `push_str` semantics — the
        // render path depends on `self.thoughts.len()` so we keep
        // the Vec<String> shape, just join the chunks correctly.
        if self.thoughts.is_empty() {
            self.thoughts.push(String::new());
        }
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            // `last_mut()` is guaranteed Some by the is_empty check
            // above; the unwrap is unreachable in practice but keeps
            // the borrow ergonomic.
            if let Some(last) = self.thoughts.last_mut() {
                last.push_str(first);
            }
        }
        for line in parts {
            self.thoughts.push(line.to_string());
        }
        self.thoughts_revision = self.thoughts_revision.wrapping_add(1);
    }

    pub fn add_cell(&mut self, cell: ToolCell) {
        self.cells.push(cell);
    }

    pub fn cell_by_id_mut(&mut self, id: &str) -> Option<&mut ToolCell> {
        self.cells.iter_mut().find(|c| c.tool_use_id == id)
    }

    pub fn is_live(&self) -> bool {
        matches!(self.state, CardState::Live | CardState::AwaitingApproval)
    }

    /// Convenience wrapper — renders the card and drops click hints.
    /// Kept for tests and simpler callers. Takes `&mut self` so the
    /// markdown cache populated by `render_rows` survives across
    /// frames; without that, every paint re-runs `pulldown_cmark`.
    pub fn render_lines(
        &mut self,
        theme: &Theme,
        live_cursor_phase: bool,
        pulse_phase_a: bool,
    ) -> Vec<Line<'static>> {
        self.render_rows(theme, live_cursor_phase, pulse_phase_a)
            .into_iter()
            .map(|(l, _)| l)
            .collect()
    }

    /// Render this card into styled `Line` values paired with optional
    /// `RowHint`s for mouse-click routing. The caller provides pulse/
    /// cursor phase from a single monotonic clock; this fn reads
    /// `Instant::now()` only for the commit-bloom and shimmer decay
    /// windows. Mutates `cached_prose` so the markdown re-parse only
    /// runs when prose actually changes (or when the theme's unicode
    /// flag flips).
    pub fn render_rows(
        &mut self,
        theme: &Theme,
        live_cursor_phase: bool,
        pulse_phase_a: bool,
    ) -> Vec<(Line<'static>, Option<RowHint>)> {
        // Capacity hint from the prior render — `prose.lines().count()`
        // would walk the entire body every frame. `last_rendered_rows`
        // is exactly the count we observed last paint; .max(8) gives
        // a sensible floor for never-rendered cards (the canvas's
        // never_rendered branch forces a real render anyway, so the
        // exact number doesn't matter — only the alloc count does).
        let mut out: Vec<(Line<'static>, Option<RowHint>)> =
            Vec::with_capacity(self.last_rendered_rows.max(8));

        let bar = self.bar_glyph(theme, pulse_phase_a);
        let bar_style = self.effective_bar_style();
        let role_style = match self.role {
            CardRole::User => theme.bold().fg(Palette::ACCENT),
            CardRole::Agent => theme.bold(),
            CardRole::System => theme.italic_dim(),
        };

        // Header: bar + role + usage chip + timestamp.
        let elapsed = self.started.elapsed().as_secs_f32();
        let ts_label = if elapsed < 10.0 {
            format!("t+{elapsed:.1}s")
        } else {
            format!("t+{:.0}s", elapsed)
        };
        // `bar` and `role.label()` are `&'static str` — pass them
        // directly to Span::styled (Cow::Borrowed) instead of
        // allocating String per frame per visible card.
        let mut header = vec![
            Span::styled(bar, bar_style),
            Span::raw(" "),
            Span::styled(self.role.label(), role_style),
        ];
        if let Some(usage) = self.usage {
            header.push(Span::styled(
                format!(
                    "   {}↓ {}↑",
                    chip_num(usage.input_tokens),
                    chip_num(usage.output_tokens)
                ),
                theme.dim(),
            ));
        }
        header.push(Span::styled(format!("    {ts_label}"), theme.dim()));
        out.push((Line::from(header), None));
        out.push((Line::from(""), None));

        // Thoughts block — collapsible, above the prose.
        if !self.thoughts.is_empty() {
            let diamond = if theme.unicode { "◇" } else { "*" };
            let (disclosure, fold_hint) = if self.thoughts_expanded {
                (
                    if theme.unicode { "▾" } else { "-" },
                    "⇥ fold · click to close",
                )
            } else {
                (
                    if theme.unicode { "▸" } else { "+" },
                    "⇥ unfold · click to open",
                )
            };
            let header_text = format!(
                "{disclosure} {diamond} thoughts ({} lines · {fold_hint})",
                self.thoughts.len()
            );
            // Hint: clicking this row toggles thoughts on this card.
            out.push((
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled(header_text, theme.italic_dim()),
                ]),
                Some(RowHint::ThoughtsHeader),
            ));
            if self.thoughts_expanded {
                // Round-19 fix: cache the rendered thought lines so
                // `line.clone()` per thought per frame doesn't run on
                // every paint. Thoughts are immutable once received;
                // invalidate via `thoughts_revision` (bumped in
                // `append_thought`).
                let want_rev = self.thoughts_revision;
                let needs_refresh = self
                    .cached_thoughts
                    .as_ref()
                    .map(|(rev, _)| *rev != want_rev)
                    .unwrap_or(true);
                if needs_refresh {
                    let italic = theme.italic_dim();
                    let lines: Vec<Line<'static>> = self
                        .thoughts
                        .iter()
                        .map(|t| {
                            Line::from(vec![Span::raw("     "), Span::styled(t.clone(), italic)])
                        })
                        .collect();
                    self.cached_thoughts = Some((want_rev, lines));
                }
                if let Some((_, cached)) = self.cached_thoughts.as_ref() {
                    for line in cached {
                        out.push((line.clone(), None));
                    }
                }
            }
            out.push((Line::from(""), None));
        }

        // Prose — markdown-rendered for agents; plain for users.
        // Cached on the card; recomputed only when prose mutates or
        // theme.unicode flips. Pre-cache, every paint re-ran
        // `pulldown_cmark::Parser` over the full message body — the
        // dominant render cost on long replies.
        if !self.prose.is_empty() {
            let needs_refresh = self
                .cached_prose
                .as_ref()
                .map(|c| c.revision != self.prose_revision || c.unicode != theme.unicode)
                .unwrap_or(true);
            if needs_refresh {
                // No `join("\n")` — `self.prose` is already the source.
                // For long agent replies this skips an O(N) String alloc
                // on every cache miss (e.g. each new streamed chunk).
                let lines: Vec<Line<'static>> = match self.role {
                    CardRole::Agent => markdown::render(&self.prose, theme),
                    _ => self
                        .prose
                        .lines()
                        .map(|l| Line::from(Span::styled(l.to_string(), theme.ink(Palette::INK_0))))
                        .collect(),
                };
                self.cached_prose = Some(CachedProse {
                    revision: self.prose_revision,
                    unicode: theme.unicode,
                    lines,
                });
            }
            let prose_lines: &[Line<'static>] = self
                .cached_prose
                .as_ref()
                .map(|c| c.lines.as_slice())
                .unwrap_or(&[]);
            // Aborted cards used to strike-through every prose span, which
            // rendered the reply unreadable (especially brutal when the
            // model had already streamed a full useful answer before
            // the abort fired). The abort signal now lives entirely in
            // the bar color + the explicit "aborted · <reason>" footer;
            // prose keeps its real styling so you can read it.
            let tail_chars = match (&self.state, self.last_append) {
                (CardState::Live, Some(at)) => motion::shimmer_chars(at.elapsed().as_millis()),
                _ => 0,
            };
            let last_non_blank_idx = prose_lines
                .iter()
                .rposition(|l| l.spans.iter().any(|s| !s.content.trim().is_empty()));

            for (i, line) in prose_lines.iter().enumerate() {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
                spans.push(Span::raw("   "));
                for s in &line.spans {
                    // The clone is INHERENT to the canvas aggregation
                    // pattern, not a missed optimisation. `render_canvas`
                    // walks N cards in pass 2 and accumulates their
                    // lines into a single `Paragraph::new(Vec<Line>)`.
                    // Borrowing span content from `cached_prose` would
                    // require `Vec<Line<'a>>` where 'a borrows from
                    // `&state.cards[N]` — but the next iteration's
                    // `render_rows` call needs `&mut state.cards[N+1]`,
                    // and both go through the same `state.cards` Vec,
                    // so the borrow checker rejects the aggregate.
                    // Cow<'static, str>::clone() is pointer-copy for
                    // Borrowed and a String memcpy for Owned — both
                    // dominated by the saved markdown re-parse.
                    spans.push(Span::styled(s.content.clone(), s.style));
                }
                if Some(i) == last_non_blank_idx && tail_chars > 0 {
                    if let Some(last_span) = spans.pop() {
                        let base = last_span.style;
                        let content = last_span.content.into_owned();
                        let shim = motion::shimmer_spans(&content, tail_chars, base);
                        spans.extend(shim);
                    }
                }
                if Some(i) == last_non_blank_idx && matches!(self.state, CardState::Live) {
                    let cursor_glyph = theme.glyph(if live_cursor_phase {
                        Theme::CURSOR_A
                    } else {
                        Theme::CURSOR_B
                    });
                    spans.push(Span::styled(cursor_glyph, theme.accent()));
                }
                out.push((Line::from(spans), None));
            }
        } else if matches!(self.state, CardState::Live) {
            let dots = motion::typing_frame(theme, self.started.elapsed().as_millis());
            out.push((
                Line::from(vec![Span::raw("   "), Span::styled(dots, theme.accent())]),
                None,
            ));
        }

        // Cells — indented, collapsed by default.
        let cell_focus = self.cell_focus;
        let card_live = matches!(self.state, CardState::Live | CardState::AwaitingApproval);
        for (i, cell) in self.cells.iter_mut().enumerate() {
            out.push((Line::from(""), None));
            let prefix = theme.glyph(Theme::CELL_PREFIX);
            let focus_marker = if cell_focus == Some(i) { "·" } else { " " };
            // Disclosure triangle — makes the cell obviously clickable
            // and its state obviously toggleable. ▾ when expanded,
            // ▸ when collapsed, ASCII `+/-` on non-Unicode terminals.
            let has_content = !cell.preview_lines.is_empty() || !cell.full_lines.is_empty();
            let disclosure = if !has_content {
                " "
            } else if cell.expanded {
                if theme.unicode {
                    "▾"
                } else {
                    "-"
                }
            } else if theme.unicode {
                "▸"
            } else {
                "+"
            };
            let result_chip = match &cell.result {
                CellResult::Pending if card_live => {
                    let age_ms = cell_pending_age(cell);
                    if age_ms > 1500 {
                        Span::styled(
                            format!(" {}", motion::sweep_frame(theme, age_ms)),
                            theme.accent(),
                        )
                    } else {
                        Span::styled(
                            format!(" {}", motion::spinner_frame(theme, age_ms)),
                            theme.accent(),
                        )
                    }
                }
                CellResult::Pending => Span::styled(" —", theme.dim()),
                CellResult::Ok { count_hint } => {
                    let c = count_hint
                        .clone()
                        .unwrap_or_else(|| theme.glyph(Theme::CHECK).to_string());
                    Span::styled(format!("   {c}"), theme.dim())
                }
                CellResult::Err { .. } => {
                    let w = theme.glyph(Theme::WARN);
                    Span::styled(format!("   {w}"), theme.ink(Palette::ABORT))
                }
            };
            let cell_line = Line::from(vec![
                Span::styled(format!("   {focus_marker}{disclosure} "), theme.accent()),
                Span::styled(format!("{prefix} "), theme.dim()),
                // `cell.name.clone()` — same architectural constraint
                // as the prose span clone (see render_rows comment
                // ~line 530): `out: Vec<Line<'static>>` requires
                // owned content; borrowing from `cell` ties the
                // lifetime to &mut self and breaks the canvas
                // aggregation in render_canvas.
                //
                // Round-22 follow-up: gemini suggested switching to
                // `Arc<str>` for cached span content to make clones
                // O(1) refcount bumps. Doesn't apply: `Span<'a>.content`
                // is `Cow<'a, str>`, which has only Owned(String) and
                // Borrowed(&str) variants. Wrapping an Arc<str> in
                // `Cow::Borrowed(&*arc)` reintroduces the same
                // borrow-checker conflict (lifetime ties to the Arc's
                // location, which lives in `&state.cards[N]`).
                // `Cow::Owned(arc.to_string())` would still allocate.
                // No win.
                Span::styled(cell.name.clone(), theme.bold()),
                Span::styled(format!("  {}", truncate(&cell.summary, 56)), theme.dim()),
                result_chip,
            ]);
            // Hint: clicking this row toggles the cell's expansion.
            out.push((cell_line, Some(RowHint::CellHeader { cell_idx: i })));

            // Render preview/full from the per-cell cache. The cache is
            // populated lazily on first paint and invalidated by
            // `set_preview_lines` / `set_full_lines` — eliminates the
            // per-frame path-link / diff-prefix heuristics that used
            // to run on every visible cell line every frame.
            let cached = if cell.expanded {
                cell.render_full(theme)
            } else {
                cell.render_preview(theme)
            };
            for line in cached {
                out.push((line.clone(), None));
            }
            // Borrow `cell.result` after the &mut borrow above ends.
            if let CellResult::Err { message } = &cell.result {
                out.push((
                    Line::from(vec![
                        Span::styled("     ", theme.dim()),
                        Span::styled(
                            truncate(message, 80).into_owned(),
                            theme.ink(Palette::ABORT),
                        ),
                    ]),
                    None,
                ));
            }
        }

        // Terminal-state footer.
        match &self.state {
            CardState::Aborted { reason, detail } => {
                out.push((Line::from(""), None));
                out.push((
                    Line::from(vec![
                        Span::styled("   aborted · ", theme.dim()),
                        Span::styled(reason.clone(), theme.ink(Palette::ABORT)),
                        if detail.is_empty() {
                            Span::raw("")
                        } else {
                            Span::styled(format!(" · {detail}"), theme.dim())
                        },
                    ]),
                    None,
                ));
            }
            CardState::Interrupted { reason } => {
                out.push((Line::from(""), None));
                out.push((
                    Line::from(vec![
                        Span::styled("   interrupted · ", theme.dim()),
                        Span::styled(reason.clone(), theme.italic_dim()),
                    ]),
                    None,
                ));
            }
            _ => {}
        }

        out.push((Line::from(""), None));
        // Record the actual row count for the canvas's viewport
        // virtualisation pass — see `render_canvas`.
        self.last_rendered_rows = out.len();
        out
    }

    /// Bar style with commit-bloom overlay applied when applicable.
    fn effective_bar_style(&self) -> Style {
        if let Some(at) = self.committed_at {
            if matches!(self.state, CardState::Committed) {
                let intensity = motion::bloom_phase(at.elapsed().as_millis());
                if intensity > 0.0 {
                    return motion::bloom_bar_style(intensity);
                }
            }
        }
        self.bar_style()
    }

    pub fn bar_glyph(&self, theme: &Theme, phase_a: bool) -> &'static str {
        use super::theme::Theme as T;
        match &self.state {
            CardState::Committed => theme.glyph(T::BAR_COMMITTED),
            CardState::Live => theme.glyph(if phase_a {
                T::BAR_LIVE_A
            } else {
                T::BAR_LIVE_B
            }),
            CardState::AwaitingApproval => theme.glyph(if phase_a {
                T::BAR_AWAIT_A
            } else {
                T::BAR_AWAIT_B
            }),
            CardState::Aborted { .. } => theme.glyph(T::BAR_ABORTED),
            CardState::Interrupted { .. } => theme.glyph(T::BAR_INTERRUPTED),
        }
    }

    pub fn bar_style(&self) -> Style {
        match &self.state {
            CardState::Committed => Style::default().fg(Palette::INK_2),
            CardState::Live => Style::default()
                .fg(Palette::ACCENT)
                .add_modifier(Modifier::BOLD),
            CardState::AwaitingApproval => Style::default()
                .fg(Palette::AMBER)
                .add_modifier(Modifier::BOLD),
            CardState::Aborted { .. } => Style::default().fg(Palette::ABORT),
            CardState::Interrupted { .. } => Style::default().fg(Palette::INK_3),
        }
    }

    /// A one-line miniature for the rail.
    pub fn miniature(&self, theme: &Theme, phase_a: bool) -> Line<'static> {
        let bar = self.bar_glyph(theme, phase_a);
        let role = self.role.label();
        let first_prose = self.prose.lines().next().unwrap_or("…");
        let excerpt = truncate(first_prose, 18).to_string();
        Line::from(vec![
            Span::styled(bar, self.bar_style()),
            Span::raw(" "),
            Span::styled(role, theme.bold()),
            Span::raw(" "),
            Span::styled(excerpt, theme.dim()),
        ])
    }
}

/// Age in milliseconds since a tool cell was created. Per-cell now
/// (was process-wide), so a stuck Pending cell from a committed turn
/// no longer keeps animating against the app's boot clock.
fn cell_pending_age(cell: &ToolCell) -> u128 {
    cell.created_at.elapsed().as_millis()
}

/// Render a single preview line from a tool cell, with diff and
/// path:line tinting applied.
fn render_cell_preview_line(line: &str, theme: &Theme) -> Line<'static> {
    // Diff markers.
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix('+') {
        if !rest.starts_with('+') {
            return Line::from(vec![
                Span::styled("     ", theme.dim()),
                Span::styled(
                    line.to_string(),
                    Style::default()
                        .fg(Color::Indexed(108))
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
        }
    }
    if let Some(rest) = trimmed.strip_prefix('-') {
        if !rest.starts_with('-') {
            return Line::from(vec![
                Span::styled("     ", theme.dim()),
                Span::styled(
                    line.to_string(),
                    Style::default()
                        .fg(Color::Indexed(167))
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
        }
    }

    // Path:line detection — path starts near the left, followed by
    // `:<digits>` or `:<digits>:<digits>`. Apply accent + underline.
    // Use `split_whitespace().next()` directly + `line.find(first)`
    // so leading whitespace doesn't shift the slice. Earlier
    // `&line[first.len()..]` assumed the path token started at index
    // 0 — on lines like "  src/foo.rs:42 ctx" it sliced "rs:42 ctx"
    // (chopping the path tail + including leftover ctx).
    if let Some(first) = line.split_whitespace().next() {
        if looks_like_path_line(first) {
            let first_idx = line.find(first).unwrap_or(0);
            let rest = &line[first_idx + first.len()..];
            return Line::from(vec![
                Span::styled("     ", theme.dim()),
                Span::styled(
                    first.to_string(),
                    Style::default()
                        .fg(Palette::ACCENT)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                Span::styled(rest.to_string(), theme.dim()),
            ]);
        }
    }

    Line::from(vec![
        Span::styled("     ", theme.dim()),
        Span::styled(line.to_string(), theme.dim()),
    ])
}

/// Heuristic: does this token look like `path/with.ext:42` or
/// `path:42:7`?
fn looks_like_path_line(s: &str) -> bool {
    let Some(last_colon) = s.rfind(':') else {
        return false;
    };
    let tail = &s[last_colon + 1..];
    if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit()) {
        // Maybe path:line:col — strip tail and try once more.
        if let Some(prior) = s[..last_colon].rfind(':') {
            let middle = &s[prior + 1..last_colon];
            if middle.chars().all(|c| c.is_ascii_digit())
                && tail.chars().all(|c| c.is_ascii_digit())
            {
                return s[..prior].contains('.') || s[..prior].contains('/');
            }
        }
        return false;
    }
    s[..last_colon].contains('.') || s[..last_colon].contains('/')
}

fn chip_num(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f32 / 1000.0)
    } else {
        n.to_string()
    }
}

use super::util::truncate;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_card_holds_text() {
        let c = TurnCard::user("t1", "hello\nworld");
        assert_eq!(c.prose, "hello\nworld");
        assert_eq!(c.role, CardRole::User);
    }

    #[test]
    fn agent_card_is_live_by_default() {
        let c = TurnCard::agent("t2");
        assert!(matches!(c.state, CardState::Live));
        assert!(c.is_live());
    }

    #[test]
    fn append_prose_joins_first_into_last() {
        let mut c = TurnCard::agent("t3");
        c.append_prose("hello ");
        c.append_prose("world\nsecond");
        assert_eq!(c.prose, "hello world\nsecond");
    }

    #[test]
    fn append_thought_joins_streaming_chunks_into_one_line() {
        let mut c = TurnCard::agent("t-thought");
        // Three streaming chunks of one logical line. Earlier
        // implementation would push them as 3 separate Vec entries,
        // visible to the user as 3 fragmented "lines".
        c.append_thought("Let me think about ");
        c.append_thought("the auth flow ");
        c.append_thought("specifically.");
        assert_eq!(
            c.thoughts,
            vec!["Let me think about the auth flow specifically.".to_string()],
            "streaming chunks must coalesce into a single line"
        );
        // Newline in a chunk creates a new line and continues
        // accumulating into it.
        c.append_thought("\nNext: token refresh ");
        c.append_thought("logic.");
        assert_eq!(
            c.thoughts,
            vec![
                "Let me think about the auth flow specifically.".to_string(),
                "Next: token refresh logic.".to_string(),
            ]
        );
    }

    #[test]
    fn cell_expand_toggle_changes_state() {
        let mut c = TurnCard::agent("t4");
        c.add_cell(ToolCell {
            tool_use_id: "tu1".into(),
            name: "repo_search".into(),
            summary: "query".into(),
            expanded: false,
            result: CellResult::Pending,
            preview_lines: vec!["p1".into()],
            full_lines: vec!["p1".into(), "p2".into()],
            created_at: Instant::now(),
            cached_preview_render: None,
            cached_full_render: None,
        });
        assert_eq!(c.cells.len(), 1);
        c.cells[0].expanded = true;
        assert!(c.cells[0].expanded);
    }

    #[test]
    fn render_produces_header_prose_and_cell_lines() {
        let mut c = TurnCard::agent("t5");
        c.append_prose("fixing a bug");
        c.add_cell(ToolCell {
            tool_use_id: "tu".into(),
            name: "bash".into(),
            summary: "ls".into(),
            expanded: false,
            result: CellResult::Ok { count_hint: None },
            preview_lines: vec!["file1".into()],
            full_lines: vec!["file1".into()],
            created_at: Instant::now(),
            cached_preview_render: None,
            cached_full_render: None,
        });
        let theme = Theme { unicode: true };
        let lines = c.render_lines(&theme, true, true);
        assert!(!lines.is_empty());
        // Header + blank + 1 prose + blank + cell line + 1 preview + trailing blank
        assert!(lines.len() >= 6);
    }

    #[test]
    fn append_prose_bumps_revision_and_invalidates_cache() {
        let mut c = TurnCard::agent("t-cache");
        c.append_prose("hello");
        let theme = Theme { unicode: true };
        let _ = c.render_lines(&theme, true, true);
        let cached_rev = c
            .cached_prose
            .as_ref()
            .expect("first render fills cache")
            .revision;
        assert_eq!(cached_rev, c.prose_revision);

        // Stamp the cache with a sentinel so we can prove it gets replaced.
        c.cached_prose
            .as_mut()
            .unwrap()
            .lines
            .push(Line::from(Span::raw("SENTINEL")));
        let stamped_revision = c
            .cached_prose
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.as_ref() == "SENTINEL"));
        assert!(stamped_revision);

        // Append → revision bumps → render rebuilds → sentinel evicted.
        c.append_prose(" world");
        let _ = c.render_lines(&theme, true, true);
        let still_has_sentinel = c
            .cached_prose
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.as_ref() == "SENTINEL"));
        assert!(
            !still_has_sentinel,
            "cache must be rebuilt on append, not patched in place"
        );
    }

    #[test]
    fn cached_prose_invalidates_on_unicode_flip() {
        let mut c = TurnCard::agent("t-flip");
        c.append_prose("fence test");
        let unicode_theme = Theme { unicode: true };
        let _ = c.render_lines(&unicode_theme, true, true);
        let cached_unicode = c.cached_prose.as_ref().unwrap().unicode;
        assert!(cached_unicode);
        let ascii_theme = Theme { unicode: false };
        let _ = c.render_lines(&ascii_theme, true, true);
        let cached_after = c.cached_prose.as_ref().unwrap().unicode;
        assert!(
            !cached_after,
            "unicode flip must invalidate the markdown cache"
        );
    }

    #[test]
    fn render_rows_records_actual_row_count() {
        let mut c = TurnCard::agent("t-rowcount");
        c.append_prose("hi");
        let theme = Theme { unicode: true };
        assert_eq!(c.last_rendered_rows, 0, "fresh card has no cached count");
        let lines = c.render_lines(&theme, true, true);
        assert!(c.last_rendered_rows > 0);
        assert_eq!(
            c.last_rendered_rows,
            lines.len(),
            "cached count must match what the canvas will see"
        );
    }

    #[test]
    fn cell_render_caches_preview_after_first_paint() {
        let mut c = TurnCard::agent("t-cellcache");
        c.add_cell(ToolCell {
            tool_use_id: "tu".into(),
            name: "bash".into(),
            summary: "ls".into(),
            expanded: false,
            result: CellResult::Ok { count_hint: None },
            preview_lines: vec!["src/foo.rs:42".into(), "+ added".into()],
            full_lines: vec!["src/foo.rs:42".into(), "+ added".into()],
            created_at: Instant::now(),
            cached_preview_render: None,
            cached_full_render: None,
        });
        let theme = Theme { unicode: true };
        // First paint populates the cache.
        let _ = c.render_lines(&theme, true, true);
        assert!(c.cells[0].cached_preview_render.is_some());
        // Stamp the cache; second paint must NOT recompute (sentinel survives).
        c.cells[0]
            .cached_preview_render
            .as_mut()
            .unwrap()
            .push(Line::from(Span::raw("CELL_SENTINEL")));
        let _ = c.render_lines(&theme, true, true);
        let still = c.cells[0]
            .cached_preview_render
            .as_ref()
            .unwrap()
            .iter()
            .any(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.as_ref() == "CELL_SENTINEL")
            });
        assert!(
            still,
            "second paint must reuse the cell render cache, not rebuild"
        );
        // set_preview_lines invalidates.
        c.cells[0].set_preview_lines(vec!["new".into()]);
        assert!(c.cells[0].cached_preview_render.is_none());
    }
}
