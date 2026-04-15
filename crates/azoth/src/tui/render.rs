//! ratatui frame builder.

use super::app::AppState;
use super::widgets::approval_modal;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub fn frame(f: &mut Frame, state: &AppState) {
    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(size);

    let status = Line::from(vec![
        Span::styled("azoth", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" · "),
        Span::raw(state.status.clone()),
        Span::raw("  ctx "),
        Span::styled(
            format!("{}%", state.ctx_pct),
            Style::default().fg(if state.ctx_pct >= 80 { Color::Red } else { Color::Cyan }),
        ),
    ]);
    f.render_widget(Paragraph::new(status), chunks[0]);

    let body_text: Vec<Line> = state
        .transcript
        .iter()
        .map(|s| Line::from(s.clone()))
        .collect();
    let body = Paragraph::new(body_text)
        .block(Block::default().borders(Borders::ALL).title(" transcript "))
        .wrap(Wrap { trim: false });
    f.render_widget(body, chunks[1]);

    let input = Paragraph::new(format!("> {}", state.input_buffer))
        .block(Block::default().borders(Borders::ALL).title(" input "));
    f.render_widget(input, chunks[2]);

    if let Some(req) = state.pending_approval.as_ref() {
        approval_modal::render(f, size, req);
    }
}
