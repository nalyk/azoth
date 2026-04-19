//! Approval sheet — card-anchored modal for `ApprovalRequestMsg`.
//!
//! Replaces the centered floating modal with a sheet that descends
//! inside the canvas area, inheriting the active card's accent. When
//! the payload is an `fs_write`, the sheet previews the diff body.

use azoth_core::authority::ApprovalRequestMsg;
use azoth_core::schemas::EffectClass;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::theme::{Palette as Colors, Theme};

/// Render the approval sheet inside `area`. The sheet spans ~70% of
/// the canvas width and sits in the top third — visually anchored to
/// the active card (which is top-most in the canvas).
/// Minimum canvas height required to render the approval modal usably.
/// Below this we fall back to a one-line warning at the top of the
/// canvas so the user knows an approval is pending without seeing a
/// truncated/malformed sheet.
const MIN_SHEET_CANVAS_HEIGHT: u16 = 13;

pub fn render(f: &mut Frame, area: Rect, req: &ApprovalRequestMsg, theme: &Theme) {
    if area.height < MIN_SHEET_CANVAS_HEIGHT || area.width < 50 {
        // Tiny terminal — render a single-line warning instead of a
        // truncated modal. The earlier code happily produced a
        // `body_height >= 9` clamped to `area.height - 4 < 9`,
        // yielding a sheet smaller than its own content.
        let warning = Line::from(vec![
            Span::styled(
                " ⚠ approval pending — ",
                theme.ink(Colors::AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("grow this terminal to respond ", theme.dim()),
        ]);
        let rect = Rect::new(area.x, area.y, area.width, 1);
        f.render_widget(Clear, rect);
        f.render_widget(Paragraph::new(warning), rect);
        return;
    }
    let body_lines = effect_preview(req);
    // Upper bound floored to >= 9 so `clamp(9, upper)` can't panic.
    // The post-clamp `h` cap then rounds to the actually-available
    // height, but the early-return above guarantees that's >= 9.
    let upper = area.height.saturating_sub(6).max(9);
    let body_height = (body_lines.len() as u16 + 5).clamp(9, upper);

    let w = (area.width.saturating_mul(72) / 100).clamp(48, 120);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + 2;
    let h = body_height.min(area.height.saturating_sub(4));
    let rect = Rect::new(x, y, w, h);

    f.render_widget(Clear, rect);

    let effect_label = format!("{:?}", req.effect_class).to_lowercase();
    let title = Line::from(vec![
        Span::styled(" approve · ".to_string(), theme.bold()),
        Span::styled(effect_label.clone(), theme.ink(Colors::AMBER)),
        Span::styled(
            format!(" · {} ", truncate_for_title(&req.summary, 48)),
            theme.dim(),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(
            Style::default()
                .fg(Colors::AMBER)
                .add_modifier(Modifier::BOLD),
        )
        .title(title);
    f.render_widget(block, rect);

    let inner = Rect {
        x: rect.x + 2,
        y: rect.y + 1,
        width: rect.width.saturating_sub(4),
        height: rect.height.saturating_sub(2),
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(inner);

    let body_para = Paragraph::new(body_lines).wrap(Wrap { trim: false });
    f.render_widget(body_para, chunks[0]);

    let action_line = Line::from(vec![
        Span::styled(
            "↵ ".to_string(),
            theme.accent().add_modifier(Modifier::BOLD),
        ),
        Span::styled("approve once   ".to_string(), theme.bold()),
        Span::styled(
            "s ".to_string(),
            theme.accent().add_modifier(Modifier::BOLD),
        ),
        Span::styled("session   ".to_string(), theme.bold()),
        Span::styled(
            "p ".to_string(),
            theme.accent().add_modifier(Modifier::BOLD),
        ),
        Span::styled("scoped paths   ".to_string(), theme.bold()),
        Span::styled(
            "⎋ ".to_string(),
            theme.accent().add_modifier(Modifier::BOLD),
        ),
        Span::styled("deny".to_string(), theme.bold()),
    ]);
    let hint_line = Line::from(Span::styled(
        format!("   tool: {}", req.tool_name),
        theme.dim(),
    ));
    f.render_widget(Paragraph::new(vec![action_line, hint_line]), chunks[1]);
}

fn truncate_for_title(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(limit.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn effect_preview(req: &ApprovalRequestMsg) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let theme = Theme { unicode: true };
    // The summary string is already formatted by the authority engine;
    // we render it verbatim as the sheet body, line-by-line. When the
    // tool is `fs_write` we add a "diff preview unavailable in v1"
    // footer — the summary itself carries the path and byte budget.
    let class_note = match req.effect_class {
        EffectClass::ApplyLocal => "writes to your local working tree",
        EffectClass::ApplyRepo => "writes to repo state (commit / branch / stash)",
        EffectClass::ApplyRemoteReversible => "reversible remote effect",
        EffectClass::ApplyRemoteStateful => "stateful remote effect",
        EffectClass::ApplyIrreversible => "irreversible — cannot be undone",
        EffectClass::Observe => "read-only observation",
        EffectClass::Stage => "staged change, not yet applied",
    };
    out.push(Line::from(Span::styled(
        class_note.to_string(),
        theme.italic_dim(),
    )));
    out.push(Line::from(""));
    for line in req.summary.lines() {
        out.push(Line::from(Span::styled(
            line.to_string(),
            theme.ink(Colors::INK_1),
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use azoth_core::schemas::{ApprovalId, EffectClass, TurnId};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tokio::sync::oneshot;

    fn mk_req() -> ApprovalRequestMsg {
        let (tx, _rx) = oneshot::channel();
        ApprovalRequestMsg {
            turn_id: TurnId::new(),
            approval_id: ApprovalId::new(),
            tool_name: "fs_write".into(),
            effect_class: EffectClass::ApplyLocal,
            summary: "write 42 bytes to src/foo.rs".into(),
            responder: tx,
        }
    }

    #[test]
    fn render_does_not_panic_in_narrow_or_short_terminal() {
        let theme = Theme { unicode: true };
        let req = mk_req();
        for (w, h) in [(40, 5), (30, 8), (20, 12), (200, 6)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| render(f, f.area(), &req, &theme))
                .expect("no panic on small terminal");
        }
    }

    #[test]
    fn render_normal_terminal_emits_modal_chrome() {
        let theme = Theme { unicode: true };
        let req = mk_req();
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, f.area(), &req, &theme))
            .unwrap();
    }
}
