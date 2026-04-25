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
    /// F7 (2026-04-25): raw last-turn input tokens kept alongside
    /// `ctx_pct` so the renderer can distinguish genuine zero from
    /// sub-integer usage. Without this, a 690-token turn on a 131k
    /// context window rendered as `ctx 0%` (integer floor).
    pub last_input_tokens: u32,
    pub packet_digest: Option<String>,
    pub turn_id: Option<String>,
    pub contract_goal: Option<String>,
    /// F3 (2026-04-25): split per EffectClass. Previously a single
    /// `(used, max)` pair summed `apply_local + apply_repo` caps —
    /// inspector read `budget 3/25` while the contract actually
    /// enforced `apply_local ≤ 20 AND apply_repo ≤ 5` separately.
    /// A user near the apply_repo=5 cap could not see they were one
    /// repo edit away from a budget abort. Each class now has its
    /// own `(used, max)` pair, rendered on its own line.
    pub contract_budget_local: Option<(u32, u32)>,
    pub contract_budget_repo: Option<(u32, u32)>,
    pub evidence_lanes: Vec<(String, String)>, // (lane, label)
    pub tools: Vec<String>,
}

/// F7 (2026-04-25): format context pressure as a display label.
///
/// - ≥1%: integer percent ("37%")
/// - >0 tokens but <1%: literal "<1%"
/// - no tokens tracked: "0%"
///
/// Shared by the status banner (render.rs) and the inspector so the
/// two surfaces can never disagree about whether context is "empty"
/// vs "0.5% full".
pub fn ctx_pct_label(ctx_pct: u8, last_input_tokens: u32) -> String {
    if ctx_pct == 0 && last_input_tokens > 0 {
        "<1%".to_string()
    } else {
        format!("{ctx_pct}%")
    }
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
        Span::styled("ctx  ", theme.dim()),
        Span::styled(
            ctx_pct_label(data.ctx_pct, data.last_input_tokens),
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
                Span::styled("pkt  ", theme.dim()),
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
                "(none accepted)",
                theme.italic_dim(),
            ))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
    // F3 (2026-04-25): render apply_local and apply_repo budgets on
    // separate lines so the user can see each cap independently. The
    // prior single-row `budget used/total` conflated the two and hid
    // impending repo-cap exhaustion behind a generous local-cap sum.
    if data.contract_budget_local.is_some() || data.contract_budget_repo.is_some() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("budget", theme.dim()))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
        if let Some((used, max)) = data.contract_budget_local {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  local  ", theme.dim()),
                    Span::styled(format!("{used}/{max}"), theme.ink(Colors::INK_1)),
                ])),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y = y.saturating_add(1);
        }
        if let Some((used, max)) = data.contract_budget_repo {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  repo   ", theme.dim()),
                    Span::styled(format!("{used}/{max}"), theme.ink(Colors::INK_1)),
                ])),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y = y.saturating_add(1);
        }
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

fn render_section_header(f: &mut Frame, inner: Rect, y: u16, label: &'static str, theme: &Theme) {
    // Round-26: `label` is always a `&'static str` literal from the
    // render body ("context"/"contract"/"evidence"/"tools"). Binding
    // the lifetime to `'static` lets Span::styled take it as
    // Cow::Borrowed, eliminating the per-frame `label.to_string()`
    // allocation that fired once per section on every render.
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            theme.bold().add_modifier(Modifier::DIM),
        ))),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

use super::util::truncate;

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn ctx_label_zero_when_no_tokens() {
        assert_eq!(ctx_pct_label(0, 0), "0%");
    }

    #[test]
    fn ctx_label_subpercent_for_small_positive_usage() {
        // 690 / 131_072 = 0.52% → stored as ctx_pct=0 but 690 tokens.
        assert_eq!(ctx_pct_label(0, 690), "<1%");
    }

    #[test]
    fn ctx_label_integer_percent_for_normal_usage() {
        assert_eq!(ctx_pct_label(37, 48_492), "37%");
    }

    #[test]
    fn ctx_label_handles_full_pressure() {
        assert_eq!(ctx_pct_label(100, 131_072), "100%");
    }
}
