//! Turn rail — an 8-column left drawer showing every turn as a
//! single-line miniature. Toggled via `⌃1`.
//!
//! ## Chronon CP-5 deferral — as-of slider
//!
//! The Chronon plan (`~/.claude/plans/i-need-you-to-humming-hoare.md`
//! §CP-5) specifies a rail-level *as-of slider* for scrubbing backward
//! through committed turns, debounced 100ms, prefetching the nearest
//! checkpoint for O(1) snap-points. That interaction surface needs a
//! dedicated UX round — input grammar (Shift+PgUp / mouse drag / slash
//! command) and the slider-vs-selected-turn affordance still have to be
//! designed — and shipping it half-built would dilute the five solid
//! sub-sprints already in the CP-5 commit.
//!
//! The load-bearing CP-5 surface (bounded hydration via
//! `JsonlReader::{forensic_as_of, replayable_as_of, rebuild_history_as_of,
//! last_accepted_contract_as_of, committed_run_progress_as_of}` plus the
//! `azoth resume --as-of <ISO8601>` CLI flag that flips `AppState.
//! read_only = true`) is shipped. The slider is scoped to its own
//! follow-up round.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use super::card::TurnCard;
use super::theme::{Palette as Colors, Theme};

pub fn render(
    f: &mut Frame,
    area: Rect,
    cards: &[TurnCard],
    theme: &Theme,
    phase_a: bool,
    selected: usize,
) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_type(BorderType::Plain)
        .border_style(theme.hairline());
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(cards.len() * 2 + 1);
    lines.push(Line::from(Span::styled(
        " turns",
        theme.bold().add_modifier(Modifier::DIM),
    )));
    lines.push(Line::from(""));
    for (i, card) in cards.iter().enumerate() {
        let is_selected = i == selected;
        let number = format!("{:02} ", i + 1);
        let number_style = if is_selected {
            Style::default()
                .fg(Colors::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.dim()
        };
        let mini = card.miniature(theme, phase_a);
        let mut spans = vec![Span::styled(number, number_style)];
        spans.extend(mini.spans);
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), inner);
}
