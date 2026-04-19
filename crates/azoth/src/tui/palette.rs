//! Command palette — the one surface for all commands and navigation.
//!
//! `⌃K` opens it. Typing fuzzy-filters a static command list plus
//! session-aware commands (jump to turn N, show context, etc.). `↵`
//! fires the selection, `⎋` dismisses. Slash-prefixed queries
//! (`/help`) still work — the parser strips the leading `/`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::theme::{Palette as Colors, Theme};

/// One palette action — what fires when the user presses `↵`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    ShowContext,
    ShowContract,
    ShowTools,
    ShowEvidence,
    OpenRail,
    OpenInspector,
    FocusMode,
    Resume,
    Continue,
    Quit,
    Approve(Option<String>),
    DraftContract(Option<String>),
    JumpToTurn(usize),
    UnknownSlash(String),
}

#[derive(Debug, Clone)]
pub struct PaletteEntry {
    pub label: &'static str,
    pub hint: &'static str,
    pub action: PaletteAction,
}

type StaticEntry = (&'static str, &'static str, fn() -> PaletteAction);

const STATIC_ENTRIES: &[StaticEntry] = &[
    ("show context", "packet · digest · budget", || {
        PaletteAction::ShowContext
    }),
    ("show contract", "active contract + criteria", || {
        PaletteAction::ShowContract
    }),
    ("show tools", "registered tools", || {
        PaletteAction::ShowTools
    }),
    ("show evidence", "last compiled lanes", || {
        PaletteAction::ShowEvidence
    }),
    ("toggle rail", "⌃1 · turn miniatures", || {
        PaletteAction::OpenRail
    }),
    ("toggle inspector", "⌃2 · session dossier", || {
        PaletteAction::OpenInspector
    }),
    ("focus mode", "⌃\\ · current card only", || {
        PaletteAction::FocusMode
    }),
    ("continue", "resume a truncated turn", || {
        PaletteAction::Continue
    }),
    ("quit", "leave azoth (⌃D)", || PaletteAction::Quit),
];

#[derive(Debug, Clone, Default)]
pub struct PaletteState {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    /// Cached `match_entries` result keyed by `(query, turn_count)`.
    /// `match_entries` runs every frame while the palette is open
    /// (60fps), allocating + filtering up to N entries; caching keeps
    /// it at one compute per actual change. Cleared on close.
    pub cached_entries: Option<(String, usize, Vec<PaletteEntry>)>,
}

impl PaletteState {
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.cached_entries = None;
    }
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = 0;
        self.cached_entries = None;
    }
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }
    pub fn pop_char(&mut self) {
        self.query.pop();
        self.selected = 0;
    }
    pub fn cursor_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
    pub fn cursor_down(&mut self, max: usize) {
        if self.selected + 1 < max {
            self.selected += 1;
        }
    }

    /// Resolve the current query to a concrete action. Called on ↵.
    pub fn fire(&self, turn_count: usize) -> Option<PaletteAction> {
        let results = match_entries(&self.query, turn_count);
        results.get(self.selected).map(|e| e.action.clone())
    }
}

/// Fuzzy-match entries against a query. Typed commands (`contract
/// <goal>`, `approve <tool>`, `jump N`, `resume`) are recognised as
/// "intent entries" and pinned at the top, bypassing the fuzzy filter.
/// All other entries pass through subsequence matching on label + hint.
pub fn match_entries(query: &str, turn_count: usize) -> Vec<PaletteEntry> {
    let q = query.trim().trim_start_matches('/').to_lowercase();

    // Round-25 fix: build PaletteEntry on demand from STATIC_ENTRIES
    // rather than materialising a fresh `Vec<PaletteEntry>` on every
    // call. The two consumers below (extend + filter_map.collect)
    // each iterate STATIC_ENTRIES independently — same per-entry
    // alloc cost, one less wrapping Vec allocation per call.
    let static_iter = || {
        STATIC_ENTRIES
            .iter()
            .map(|(label, hint, builder)| PaletteEntry {
                label,
                hint,
                action: builder(),
            })
    };

    let mut pinned: Vec<PaletteEntry> = Vec::new();

    if let Some(rest) = q.strip_prefix("contract ").map(|s| s.trim().to_string()) {
        if !rest.is_empty() {
            pinned.push(PaletteEntry {
                label: "draft contract · use typed text as goal",
                hint: "contract <goal>",
                action: PaletteAction::DraftContract(Some(rest)),
            });
        }
    } else if q == "contract" {
        pinned.push(PaletteEntry {
            label: "draft contract · usage",
            hint: "contract <goal>",
            action: PaletteAction::DraftContract(None),
        });
    }

    if let Some(rest) = q.strip_prefix("approve ").map(|s| s.trim().to_string()) {
        if !rest.is_empty() {
            pinned.push(PaletteEntry {
                label: "grant session-scope token",
                hint: "approve <tool>",
                action: PaletteAction::Approve(Some(rest)),
            });
        }
    } else if q == "approve" {
        pinned.push(PaletteEntry {
            label: "approve · usage",
            hint: "approve <tool>",
            action: PaletteAction::Approve(None),
        });
    }

    if q == "resume" {
        pinned.push(PaletteEntry {
            label: "resume · usage",
            hint: "CLI: azoth resume <run_id>",
            action: PaletteAction::Resume,
        });
    }

    if let Some(rest) = q.strip_prefix("jump ").map(|s| s.trim().to_string()) {
        if let Ok(n) = rest.parse::<usize>() {
            if n > 0 && n <= turn_count {
                pinned.push(PaletteEntry {
                    label: "jump to turn",
                    hint: "",
                    action: PaletteAction::JumpToTurn(n.saturating_sub(1)),
                });
            }
        }
    }

    // When there's a pinned intent, it dominates — show it + let the
    // fuzzy-matched statics fill below.
    let mut out = pinned;
    if q.is_empty() {
        out.extend(static_iter());
        return out;
    }
    let mut scored: Vec<(i32, PaletteEntry)> = static_iter()
        .filter_map(|e| {
            let label_score = score(&q, e.label);
            let hint_score = score(&q, e.hint);
            label_score.or(hint_score.map(|s| s - 20)).map(|s| (s, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    out.extend(scored.into_iter().map(|(_, e)| e));

    // `/help` is a no-op intent — users should discover the palette.
    // Ensure we return at least one entry for any `/`-prefixed query so
    // muscle memory doesn't hit an empty sheet.
    if out.is_empty() && query.trim().starts_with('/') {
        out.push(PaletteEntry {
            label: "no matching command",
            hint: "type a command name or press ⎋ to dismiss",
            action: PaletteAction::UnknownSlash(q),
        });
    }
    out
}

/// Simple subsequence-match score. Returns None if `query` isn't a
/// subsequence of `target`. Higher is better. Prefix matches get a
/// big boost.
fn score(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let t = target.to_lowercase();
    let mut q_iter = query.chars();
    let mut q_cur = q_iter.next()?;
    let mut matched = 0i32;
    let mut last_match: Option<usize> = None;
    let mut proximity = 0i32;
    for (i, ch) in t.chars().enumerate() {
        if ch == q_cur {
            matched += 1;
            if let Some(last) = last_match {
                proximity -= (i - last - 1) as i32;
            }
            last_match = Some(i);
            match q_iter.next() {
                Some(c) => q_cur = c,
                None => {
                    let prefix_bonus = if t.starts_with(query) { 50 } else { 0 };
                    return Some(matched * 10 + proximity + prefix_bonus);
                }
            }
        }
    }
    None
}

pub fn render(
    f: &mut Frame,
    area: Rect,
    state: &mut PaletteState,
    theme: &Theme,
    turn_count: usize,
) {
    let w = (area.width.saturating_mul(60) / 100).clamp(40, 100);
    let h = (area.height.saturating_mul(55) / 100).clamp(10, 24);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 3;
    let rect = Rect::new(x, y, w, h);
    f.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.ink(Colors::INK_3))
        .title(Line::from(vec![
            Span::styled(
                format!(" {} ", theme.glyph(Theme::MAGNIFIER)),
                theme.accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled("palette".to_string(), theme.bold()),
            Span::styled(
                "  · ↵ fire · ⎋ dismiss · ↑↓ navigate ".to_string(),
                theme.dim(),
            ),
        ]));
    f.render_widget(block, rect);

    let inner = Rect {
        x: rect.x + 2,
        y: rect.y + 1,
        width: rect.width.saturating_sub(4),
        height: rect.height.saturating_sub(2),
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    // Query line. Cursor glyph routes through `Theme::glyph` so the
    // ASCII-fallback theme (LC_ALL=C / non-UTF-8 terminal) gets a
    // plain `_` instead of a Unicode block — earlier code emitted
    // `▋` unconditionally, defeating the round-1 ASCII-fallback path.
    let q_line = Line::from(vec![
        Span::styled("› ".to_string(), theme.accent()),
        Span::styled(state.query.clone(), theme.bold()),
        Span::styled(theme.glyph(Theme::CURSOR_A).to_string(), theme.accent()),
    ]);
    f.render_widget(Paragraph::new(q_line), chunks[0]);

    // Separator.
    let sep_w = chunks[1].width as usize;
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            theme.glyph(Theme::HAIRLINE_CHAR).repeat(sep_w),
            theme.hairline(),
        ))),
        chunks[1],
    );

    // Results — cached by (query, turn_count). `match_entries`
    // allocates + filters per call; without cache it ran every frame
    // while the palette was open (60fps). Cache populates lazily,
    // invalidates when either key component changes.
    let needs_recompute = state
        .cached_entries
        .as_ref()
        .map(|(q, n, _)| q != &state.query || *n != turn_count)
        .unwrap_or(true);
    if needs_recompute {
        let entries = match_entries(&state.query, turn_count);
        state.cached_entries = Some((state.query.clone(), turn_count, entries));
    }
    let entries = state
        .cached_entries
        .as_ref()
        .map(|(_, _, e)| e.as_slice())
        .unwrap_or(&[]);
    let visible = entries.len().min(chunks[2].height as usize);
    let result_lines: Vec<Line<'static>> = entries
        .iter()
        .take(visible)
        .enumerate()
        .map(|(i, e)| {
            let selected = i == state.selected;
            let marker: String = if selected {
                format!("{} ", theme.glyph(Theme::CHEVRON))
            } else {
                "  ".to_string()
            };
            let marker_style = if selected {
                theme.accent().add_modifier(Modifier::BOLD)
            } else {
                theme.dim()
            };
            let label_style = if selected {
                Style::default()
                    .fg(Colors::INK_0)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.ink(Colors::INK_1)
            };
            let hint = if e.hint.is_empty() {
                String::new()
            } else {
                format!("     {}", e.hint)
            };
            Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(e.label.to_string(), label_style),
                Span::styled(hint, theme.dim()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(result_lines), chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_all_static_entries() {
        let entries = match_entries("", 0);
        assert!(entries.len() >= STATIC_ENTRIES.len());
    }

    #[test]
    fn fuzzy_matches_on_subsequence() {
        let entries = match_entries("ctx", 0);
        assert!(
            entries
                .iter()
                .any(|e| matches!(e.action, PaletteAction::ShowContext)),
            "expected 'show context' to be matched by 'ctx'"
        );
    }

    #[test]
    fn contract_with_goal_creates_draft_entry() {
        let entries = match_entries("contract fix token refresh", 0);
        assert!(entries.iter().any(|e| matches!(
            &e.action,
            PaletteAction::DraftContract(Some(goal)) if goal == "fix token refresh"
        )));
    }

    #[test]
    fn approve_with_arg_creates_grant_entry() {
        let entries = match_entries("approve fs_write", 0);
        assert!(entries.iter().any(|e| matches!(
            &e.action,
            PaletteAction::Approve(Some(tool)) if tool == "fs_write"
        )));
    }

    #[test]
    fn leading_slash_is_stripped() {
        let entries = match_entries("/help", 0);
        assert!(!entries.is_empty());
    }

    #[test]
    fn jump_matches_existing_turn() {
        let entries = match_entries("jump 2", 3);
        assert!(entries
            .iter()
            .any(|e| matches!(e.action, PaletteAction::JumpToTurn(1))));
    }

    #[test]
    fn palette_state_fire_resolves_selected_entry() {
        let mut s = PaletteState::default();
        s.open();
        // First static entry should be "show context"
        s.query.clear();
        let action = s.fire(0);
        assert!(matches!(action, Some(PaletteAction::ShowContext)));
    }
}
