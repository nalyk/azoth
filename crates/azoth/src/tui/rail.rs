//! Turn rail — an 8-column left drawer showing every turn as a
//! single-line miniature. Toggled via `⌃1`.

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
