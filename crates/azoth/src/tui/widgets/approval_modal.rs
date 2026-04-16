//! Centered approval modal overlay. Drawn after the scrollback pass when
//! `AppState.pending_approval` is `Some`.

use azoth_core::authority::ApprovalRequestMsg;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

pub fn render(f: &mut Frame, area: Rect, req: &ApprovalRequestMsg) {
    let rect = centered_rect(area, 60, 30, 44, 9);
    f.render_widget(Clear, rect);

    let lines = vec![
        Line::from(vec![
            Span::styled("tool:    ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(req.tool_name.clone()),
        ]),
        Line::from(vec![
            Span::styled("effect:  ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(format!("{:?}", req.effect_class)),
        ]),
        Line::from(vec![
            Span::styled("summary: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(req.summary.clone()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "[y] grant once   [s] grant session   [n] deny   [esc] deny",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" approval required ");
    let body = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(body, rect);
}

fn centered_rect(area: Rect, pct_w: u16, pct_h: u16, min_w: u16, min_h: u16) -> Rect {
    let w = (area.width * pct_w / 100).max(min_w).min(area.width);
    let h = (area.height * pct_h / 100).max(min_h).min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}
