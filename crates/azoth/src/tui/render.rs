//! PAPER canvas — the frame orchestrator.
//!
//! Layout (rows top-to-bottom):
//!
//! 1.  status strip (1 row, no border)
//! 2.  hairline separator (1 row)
//! 3.  canvas row: optional rail (left), canvas (flex), optional inspector (right)
//! 4.  whisper row (1 row, pre-composer narrator)
//! 5.  hairline separator (1 row)
//! 6.  composer (3 rows, rounded)
//!
//! When the terminal is narrow (<100 cols), the inspector auto-hides.
//! When Focus Mode is on, all turns except the active one are hidden.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::app::AppState;
use super::card::TurnCard;
use super::theme::{pulse_phase, Palette as Colors, Theme};
use super::{inspector, palette, rail, sheet};

pub fn frame(f: &mut Frame, state: &mut AppState) {
    let size = f.area();
    let theme = state.theme;
    let elapsed_ms = state.boot.elapsed().as_millis();
    let bar_phase = pulse_phase(elapsed_ms, 600);
    let cursor_phase = pulse_phase(elapsed_ms, 500);

    let show_rail = state.rail_open;
    let show_inspector = state.inspector_open && size.width >= 100;

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status
            Constraint::Length(1), // hairline
            Constraint::Min(3),    // canvas row
            Constraint::Length(1), // whisper
            Constraint::Length(1), // hairline
            Constraint::Length(3), // composer
        ])
        .split(size);

    render_status(f, vertical[0], state, &theme);
    render_hairline(f, vertical[1], &theme);

    // Middle row: optional rail + canvas + optional inspector.
    let mut mid_constraints: Vec<Constraint> = Vec::new();
    if show_rail {
        mid_constraints.push(Constraint::Length(14));
    }
    mid_constraints.push(Constraint::Min(20));
    if show_inspector {
        mid_constraints.push(Constraint::Length(30));
    }
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(mid_constraints)
        .split(vertical[2]);

    let mut idx = 0;
    if show_rail {
        let rail_area = mid[idx];
        let selected = state.cards.len().saturating_sub(1);
        rail::render(f, rail_area, &state.cards, &theme, bar_phase, selected);
        idx += 1;
    }
    let canvas_area = mid[idx];
    render_canvas(f, canvas_area, state, &theme, bar_phase, cursor_phase);
    idx += 1;
    if show_inspector {
        let inspector_area = mid[idx];
        inspector::render(f, inspector_area, &state.inspector_data, &theme);
    }

    render_whisper(f, vertical[3], state, &theme);
    render_hairline(f, vertical[4], &theme);
    render_composer(f, vertical[5], state, &theme);

    // Overlays.
    if state.palette.open {
        palette::render(f, size, &state.palette, &theme, state.cards.len());
    }
    if let Some(req) = state.pending_approval.as_ref() {
        sheet::render(f, canvas_area, req, &theme);
    }
}

fn render_status(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let clock = theme.glyph(Theme::CLOCK);
    let mut spans: Vec<Span<'static>> = vec![
        Span::raw("  "),
        Span::styled("azoth".to_string(), theme.bold()),
        Span::styled(" · ".to_string(), theme.dim()),
    ];
    let contract_label = state
        .inspector_data
        .contract_goal
        .as_deref()
        .map(|g| trunc(g, 48))
        .unwrap_or_else(|| "no contract yet".to_string());
    spans.push(Span::styled(contract_label, theme.ink(Colors::INK_1)));
    if !state.run_id.is_empty() {
        spans.push(Span::styled(
            format!(" · {}", trunc(&state.run_id, 20)),
            theme.dim(),
        ));
    }
    let ctx_style = if state.ctx_pct >= 80 {
        theme.ink(Colors::ABORT)
    } else {
        theme.ink(Colors::ACCENT)
    };
    spans.push(Span::styled(format!("        {clock}  "), theme.dim()));
    spans.push(Span::styled(format!("{}%", state.ctx_pct), ctx_style));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_hairline(f: &mut Frame, area: Rect, theme: &Theme) {
    let w = area.width as usize;
    let line = Line::from(Span::styled(
        theme.glyph(Theme::HAIRLINE_CHAR).repeat(w),
        theme.hairline(),
    ));
    f.render_widget(Paragraph::new(line), area);
}

fn render_canvas(
    f: &mut Frame,
    area: Rect,
    state: &mut AppState,
    theme: &Theme,
    bar_phase: bool,
    cursor_phase: bool,
) {
    if state.cards.is_empty() {
        render_zero_state(f, area, theme);
        return;
    }

    let visible: Vec<&TurnCard> = if state.focus_mode {
        state
            .cards
            .iter()
            .rev()
            .find(|c| c.is_live())
            .map(|c| vec![c])
            .unwrap_or_else(|| state.cards.last().map(|c| vec![c]).unwrap_or_default())
    } else {
        state.cards.iter().collect()
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for card in &visible {
        lines.push(Line::from(""));
        lines.extend(card.render_lines(theme, cursor_phase, bar_phase));
    }

    let total = lines.len() as u16;
    let visible_height = area.height;
    let max_scroll = total.saturating_sub(visible_height);
    let scroll_pos = if state.scroll_locked {
        max_scroll
            .saturating_sub(state.scroll_offset)
            .min(max_scroll)
    } else {
        state.scroll_offset = 0;
        max_scroll
    };

    let body = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_pos, 0));
    f.render_widget(body, area);
}

fn render_zero_state(f: &mut Frame, area: Rect, theme: &Theme) {
    let inner_y = area.y + area.height / 3;
    let bar = theme.glyph(Theme::BAR_COMMITTED);
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("     "),
            Span::styled(bar.to_string(), theme.accent()),
            Span::raw("  "),
            Span::styled("what are we building?".to_string(), theme.bold()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("        "),
            Span::styled(
                "tell azoth what you want. it will plan, then ask before touching anything."
                    .to_string(),
                theme.italic_dim(),
            ),
        ]),
    ];
    let rect = Rect {
        x: area.x,
        y: inner_y,
        width: area.width,
        height: lines.len() as u16,
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rect);
}

fn render_whisper(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let latest_note = state.notes.last();
    let line = state.whisper.render_line(theme, latest_note);
    f.render_widget(Paragraph::new(line), area);
}

fn render_composer(f: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.ink(Colors::INK_3))
        .title(Line::from(vec![
            Span::styled(" write".to_string(), theme.bold()),
            Span::styled(" ".to_string(), Style::default()),
        ]))
        .title_bottom(Line::from(vec![
            Span::styled(" ⌃K ".to_string(), theme.accent()),
            Span::styled("palette · ".to_string(), theme.dim()),
            Span::styled("↵ ".to_string(), theme.accent()),
            Span::styled("send · ".to_string(), theme.dim()),
            Span::styled("⇧↵ ".to_string(), theme.accent()),
            Span::styled("newline ".to_string(), theme.dim()),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(&state.textarea, inner);
}

fn trunc(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(limit.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}
