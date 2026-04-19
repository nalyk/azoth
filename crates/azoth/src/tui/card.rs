//! Turn cards — the atomic visual unit of PAPER.
//!
//! A card replaces the flat `Vec<String>` transcript line. Each card
//! owns its own role, state, prose, and tool cells. The render path
//! iterates `Vec<TurnCard>` and produces pre-styled `Line<'static>`
//! values, avoiding the per-frame `String::clone` tax the old
//! `Vec<String>` + `Line::from(s.clone())` loop paid.

use std::time::Instant;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

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

/// The card itself.
#[derive(Debug, Clone)]
pub struct TurnCard {
    pub turn_id: String,
    pub role: CardRole,
    pub state: CardState,
    pub prose: Vec<String>,
    pub cells: Vec<ToolCell>,
    pub usage: Option<UsageChip>,
    pub started: Instant,
    pub contract_goal: Option<String>,
    pub cell_focus: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct UsageChip {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl TurnCard {
    pub fn user(turn_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            turn_id: turn_id.into(),
            role: CardRole::User,
            state: CardState::Committed,
            prose: text.into().lines().map(String::from).collect(),
            cells: Vec::new(),
            usage: None,
            started: Instant::now(),
            contract_goal: None,
            cell_focus: None,
        }
    }
    pub fn agent(turn_id: impl Into<String>) -> Self {
        Self {
            turn_id: turn_id.into(),
            role: CardRole::Agent,
            state: CardState::Live,
            prose: Vec::new(),
            cells: Vec::new(),
            usage: None,
            started: Instant::now(),
            contract_goal: None,
            cell_focus: None,
        }
    }

    pub fn append_prose(&mut self, text: &str) {
        for (i, line) in text.split('\n').enumerate() {
            if i == 0 {
                if let Some(last) = self.prose.last_mut() {
                    last.push_str(line);
                } else {
                    self.prose.push(line.to_string());
                }
            } else {
                self.prose.push(line.to_string());
            }
        }
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

    /// Render this card into a sequence of styled `Line` values for the
    /// canvas pane. `width` is the pane width; wrap is caller-managed
    /// via ratatui's `Paragraph::wrap`.
    pub fn render_lines(
        &self,
        theme: &Theme,
        live_cursor_phase: bool,
        pulse_phase_a: bool,
    ) -> Vec<Line<'static>> {
        let mut out = Vec::with_capacity(self.prose.len() + self.cells.len() * 3 + 3);

        let bar = self.bar_glyph(theme, pulse_phase_a);
        let bar_style = self.bar_style();
        let role_style = match self.role {
            CardRole::User => theme.bold(),
            CardRole::Agent => theme.bold(),
            CardRole::System => theme.italic_dim(),
        };

        // Header line: bar + role label + right-gutter metadata.
        let elapsed = self.started.elapsed().as_secs_f32();
        let ts_label = if elapsed < 10.0 {
            format!("t+{elapsed:.1}s")
        } else {
            format!("t+{:.0}s", elapsed)
        };
        let mut header = vec![
            Span::styled(bar.to_string(), bar_style),
            Span::raw(" "),
            Span::styled(self.role.label().to_string(), role_style),
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
        out.push(Line::from(header));
        out.push(Line::from(""));

        // Prose body — pre-styled, one pass.
        let prose_style = match &self.state {
            CardState::Aborted { .. } => theme.strike_dim(),
            _ => theme.ink(Palette::INK_0),
        };
        for (idx, line) in self.prose.iter().enumerate() {
            let is_last = idx + 1 == self.prose.len();
            let cursor_glyph = theme.glyph(if live_cursor_phase {
                Theme::CURSOR_A
            } else {
                Theme::CURSOR_B
            });
            let mut spans = vec![Span::raw("   "), Span::styled(line.clone(), prose_style)];
            if is_last && matches!(self.state, CardState::Live) {
                spans.push(Span::styled(cursor_glyph.to_string(), theme.accent()));
            }
            out.push(Line::from(spans));
        }

        // Cells — indented, collapsed by default.
        for (i, cell) in self.cells.iter().enumerate() {
            out.push(Line::from(""));
            let prefix = theme.glyph(Theme::CELL_PREFIX);
            let focus_marker = if self.cell_focus == Some(i) {
                "·"
            } else {
                " "
            };
            let result_chip = match &cell.result {
                CellResult::Pending => Span::styled(" …".to_string(), theme.dim()),
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
                Span::styled(format!("   {focus_marker}{prefix} "), theme.dim()),
                Span::styled(cell.name.clone(), theme.bold()),
                Span::styled(format!("  {}", truncate(&cell.summary, 56)), theme.dim()),
                result_chip,
            ]);
            out.push(cell_line);

            let preview = if cell.expanded {
                cell.full_lines.iter().take(24)
            } else {
                cell.preview_lines.iter().take(4)
            };
            for p in preview {
                out.push(Line::from(vec![
                    Span::styled("     ".to_string(), theme.dim()),
                    Span::styled(p.clone(), theme.dim()),
                ]));
            }
            if matches!(cell.result, CellResult::Err { .. }) {
                if let CellResult::Err { message } = &cell.result {
                    out.push(Line::from(vec![
                        Span::styled("     ".to_string(), theme.dim()),
                        Span::styled(truncate(message, 80).to_string(), theme.ink(Palette::ABORT)),
                    ]));
                }
            }
        }

        // Terminal-state footer.
        match &self.state {
            CardState::Aborted { reason, detail } => {
                out.push(Line::from(""));
                out.push(Line::from(vec![
                    Span::styled("   aborted · ".to_string(), theme.dim()),
                    Span::styled(reason.clone(), theme.ink(Palette::ABORT)),
                    if detail.is_empty() {
                        Span::raw("")
                    } else {
                        Span::styled(format!(" · {detail}"), theme.dim())
                    },
                ]));
            }
            CardState::Interrupted { reason } => {
                out.push(Line::from(""));
                out.push(Line::from(vec![
                    Span::styled("   interrupted · ".to_string(), theme.dim()),
                    Span::styled(reason.clone(), theme.italic_dim()),
                ]));
            }
            _ => {}
        }

        out.push(Line::from(""));
        out
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
        let first_prose = self
            .prose
            .first()
            .cloned()
            .unwrap_or_else(|| "…".to_string());
        let excerpt = truncate(&first_prose, 18).to_string();
        Line::from(vec![
            Span::styled(bar.to_string(), self.bar_style()),
            Span::raw(" "),
            Span::styled(role.to_string(), theme.bold()),
            Span::raw(" "),
            Span::styled(excerpt, theme.dim()),
        ])
    }
}

fn chip_num(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f32 / 1000.0)
    } else {
        n.to_string()
    }
}

fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(limit.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_card_holds_text() {
        let c = TurnCard::user("t1", "hello\nworld");
        assert_eq!(c.prose, vec!["hello", "world"]);
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
        assert_eq!(c.prose, vec!["hello world", "second"]);
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
        });
        let theme = Theme { unicode: true };
        let lines = c.render_lines(&theme, true, true);
        assert!(!lines.is_empty());
        // Header + blank + 1 prose + blank + cell line + 1 preview + trailing blank
        assert!(lines.len() >= 6);
    }
}
