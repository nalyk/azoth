//! PAPER splash — startup curtain rendered while the worker task
//! initialises (opens JSONL, opens mirror, builds retrieval indexes,
//! resolves profile, builds adapter). Hides until the worker signals
//! ready on its dedicated channel.
//!
//! Centered, minimal, animated. Single message to the user: "we're
//! building the workspace". No panels, no chrome, no commands.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::motion;
use super::theme::{Palette, Theme};

pub fn render(f: &mut Frame, area: Rect, theme: &Theme, phase: &str, elapsed_ms: u128) {
    let spinner = motion::spinner_frame(theme, elapsed_ms);
    let version = env!("CARGO_PKG_VERSION");

    // Big block-letter azoth using half-block Unicode. ASCII
    // fallback uses plain letters.
    let title_block = if theme.unicode {
        vec!["▄▀█ ▀█ █▀█ ▀█▀ █ █", "█▀█ █▄  █▄█  █  █▀█"]
    } else {
        vec!["azoth"]
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(title_block.len() + 8);

    // Vertical breathing room — push blank lines so content lands
    // roughly at the vertical third.
    let top_pad = (area.height / 4).max(2);
    for _ in 0..top_pad {
        lines.push(Line::from(""));
    }

    for row in &title_block {
        lines.push(Line::from(Span::styled(
            row.to_string(),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "contract-centric coding agent runtime".to_string(),
        theme.italic_dim(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    lines.push(Line::from(vec![
        Span::styled(spinner.to_string(), theme.accent()),
        Span::raw("  "),
        Span::styled(phase.to_string(), theme.ink(Palette::INK_1)),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(format!("v{version}"), theme.dim())));

    let para = Paragraph::new(lines).alignment(Alignment::Center);
    f.render_widget(para, area);
}
