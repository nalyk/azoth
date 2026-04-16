//! ratatui frame builder.

use super::app::AppState;
use super::widgets::approval_modal;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Frame;

pub fn frame(f: &mut Frame, state: &mut AppState) {
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

    // Transcript with auto-scroll to bottom and manual scroll-back.
    let body_text: Vec<Line> = state
        .transcript
        .iter()
        .map(|s| Line::from(s.clone()))
        .collect();
    let total_lines = body_text.len() as u16;
    let visible_height = chunks[1].height.saturating_sub(2); // borders

    // Compute the scroll position: when not locked, pin to bottom.
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll_pos = if state.scroll_locked {
        max_scroll.saturating_sub(state.scroll_offset).min(max_scroll)
    } else {
        // Auto-scroll: always show the latest lines.
        state.scroll_offset = 0;
        max_scroll
    };

    let body = Paragraph::new(body_text)
        .block(Block::default().borders(Borders::ALL).title(" transcript "))
        .wrap(Wrap { trim: false })
        .scroll((scroll_pos, 0));
    f.render_widget(body, chunks[1]);

    // Scrollbar indicator.
    if total_lines > visible_height {
        let mut scrollbar_state = ScrollbarState::new(max_scroll as usize)
            .position(scroll_pos as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            chunks[1],
            &mut scrollbar_state,
        );
    }

    f.render_widget(&state.textarea, chunks[2]);

    if let Some(req) = state.pending_approval.as_ref() {
        approval_modal::render(f, size, req);
    }
}
