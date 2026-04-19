//! Inspector drawer — right-side pane with context / contract /
//! evidence stacks. Toggled via `⌃2`.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Sparkline};
use ratatui::Frame;

use super::theme::{Palette as Colors, Theme};

#[derive(Debug, Clone, Default)]
pub struct InspectorData {
    pub ctx_pct: u8,
    pub ctx_history: Vec<u64>,
    pub packet_digest: Option<String>,
    pub turn_id: Option<String>,
    pub contract_goal: Option<String>,
    pub contract_budget: Option<(u32, u32)>, // (consumed, max)
    pub evidence_lanes: Vec<(String, String)>, // (lane, label)
    pub tools: Vec<String>,
}

pub fn render(f: &mut Frame, area: Rect, data: &InspectorData, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_type(BorderType::Plain)
        .border_style(theme.hairline());
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 2,
        y: area.y,
        width: area.width.saturating_sub(3),
        height: area.height,
    };

    let mut y = inner.y;

    // --- context block ---
    render_section_header(f, inner, y, "context", theme);
    y = y.saturating_add(1);
    let ctx_line = Line::from(vec![
        Span::styled("ctx  ".to_string(), theme.dim()),
        Span::styled(
            format!("{}%", data.ctx_pct),
            if data.ctx_pct >= 80 {
                theme.ink(Colors::ABORT).add_modifier(Modifier::BOLD)
            } else {
                theme.ink(Colors::ACCENT).add_modifier(Modifier::BOLD)
            },
        ),
    ]);
    f.render_widget(
        Paragraph::new(ctx_line),
        Rect::new(inner.x, y, inner.width, 1),
    );
    y = y.saturating_add(1);
    if !data.ctx_history.is_empty() {
        let spark = Sparkline::default()
            .data(&data.ctx_history)
            .style(theme.accent());
        f.render_widget(spark, Rect::new(inner.x, y, inner.width.min(18), 1));
        y = y.saturating_add(1);
    }
    if let Some(d) = &data.packet_digest {
        let digest = truncate(d, inner.width as usize);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("pkt  ".to_string(), theme.dim()),
                Span::styled(digest, theme.italic_dim()),
            ])),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
    y = y.saturating_add(1);

    // --- contract block ---
    render_section_header(f, inner, y, "contract", theme);
    y = y.saturating_add(1);
    if let Some(goal) = &data.contract_goal {
        let g = truncate(goal, inner.width as usize);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(g, theme.ink(Colors::INK_1)))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    } else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "(none accepted)".to_string(),
                theme.italic_dim(),
            ))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
    if let Some((used, max)) = data.contract_budget {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("budget ".to_string(), theme.dim()),
                Span::styled(format!("{used}/{max}"), theme.ink(Colors::INK_1)),
            ])),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
    y = y.saturating_add(1);

    // --- evidence block ---
    if !data.evidence_lanes.is_empty() {
        render_section_header(f, inner, y, "evidence", theme);
        y = y.saturating_add(1);
        for (lane, label) in &data.evidence_lanes {
            if y >= inner.y + inner.height {
                break;
            }
            let bullet = theme.glyph(Theme::BULLET);
            let lane_trunc = truncate(label, inner.width.saturating_sub(10) as usize);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!("{bullet} "), theme.accent()),
                    Span::styled(format!("{lane:<7}"), theme.dim()),
                    Span::styled(lane_trunc, theme.ink(Colors::INK_1)),
                ])),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y = y.saturating_add(1);
        }
        y = y.saturating_add(1);
    }

    // --- tools block ---
    if !data.tools.is_empty() && y < inner.y + inner.height {
        render_section_header(f, inner, y, "tools", theme);
        y = y.saturating_add(1);
        for tool in &data.tools {
            if y >= inner.y + inner.height {
                break;
            }
            // Borrow both spans from caller-owned strings — earlier
            // build allocated `"  ".to_string()` and `tool.clone()`
            // every frame for every tool. Span::styled accepts any
            // `Into<Cow<'a, str>>`, so `&str` slices land as
            // `Cow::Borrowed` with zero allocation.
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  ", theme.dim()),
                    Span::styled(tool.as_str(), theme.ink(Colors::INK_1)),
                ])),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y = y.saturating_add(1);
        }
    }
}

fn render_section_header(f: &mut Frame, inner: Rect, y: u16, label: &str, theme: &Theme) {
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label.to_string(),
            theme.bold().add_modifier(Modifier::DIM),
        ))),
        Rect::new(inner.x, y, inner.width, 1),
    );
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
