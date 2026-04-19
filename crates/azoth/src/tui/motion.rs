//! PAPER motion — spinners, typing dots, bloom phase helpers.
//!
//! All motion is driven by a single monotonic `Instant` and phase
//! arithmetic. Zero allocations per frame; the caller picks a glyph
//! from a static table based on `elapsed_ms`.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use super::theme::{Palette, Theme};

/// Braille spinner. Eight frames at 80ms = 640ms per cycle. The
/// classic Unicode dot-cycling spinner used in every modern CLI.
pub const BRAILLE_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// ASCII fallback spinner. Four frames, rotates through `-\\|/`.
pub const ASCII_SPINNER: &[&str] = &["-", "\\", "|", "/"];

/// "Typing dots" frames — three dots that breathe. 600ms cycle.
pub const TYPING_FRAMES: &[&str] = &["·  ", "·· ", "···", " ··", "  ·", "   "];
pub const TYPING_ASCII: &[&str] = &[".  ", ".. ", "...", " ..", "  .", "   "];

/// Progress sweep for long-running tool cells. 10 frames, 200ms each.
pub const SWEEP_FRAMES: &[&str] = &[
    "▱▱▱▱▱▱▱",
    "▰▱▱▱▱▱▱",
    "▰▰▱▱▱▱▱",
    "▰▰▰▱▱▱▱",
    "▱▰▰▰▱▱▱",
    "▱▱▰▰▰▱▱",
    "▱▱▱▰▰▰▱",
    "▱▱▱▱▰▰▰",
    "▱▱▱▱▱▰▰",
    "▱▱▱▱▱▱▰",
];
pub const SWEEP_ASCII: &[&str] = &[
    "[      ]", "[=     ]", "[==    ]", "[===   ]", "[ ===  ]", "[  === ]", "[   ===]", "[    ==]",
    "[     =]", "[      ]",
];

pub fn spinner_frame(theme: &Theme, elapsed_ms: u128) -> &'static str {
    let frames = if theme.unicode {
        BRAILLE_FRAMES
    } else {
        ASCII_SPINNER
    };
    let idx = ((elapsed_ms / 80) as usize) % frames.len();
    frames[idx]
}

pub fn typing_frame(theme: &Theme, elapsed_ms: u128) -> &'static str {
    let frames = if theme.unicode {
        TYPING_FRAMES
    } else {
        TYPING_ASCII
    };
    let idx = ((elapsed_ms / 120) as usize) % frames.len();
    frames[idx]
}

pub fn sweep_frame(theme: &Theme, elapsed_ms: u128) -> &'static str {
    let frames = if theme.unicode {
        SWEEP_FRAMES
    } else {
        SWEEP_ASCII
    };
    let idx = ((elapsed_ms / 200) as usize) % frames.len();
    frames[idx]
}

/// Bloom intensity for a recently-committed card. Returns the
/// 0.0..=1.0 intensity that should tint the accent bar brighter for
/// the first ~600ms after commit, falling linearly to zero.
pub fn bloom_phase(elapsed_ms: u128) -> f32 {
    const BLOOM_WINDOW_MS: u128 = 600;
    if elapsed_ms >= BLOOM_WINDOW_MS {
        0.0
    } else {
        1.0 - (elapsed_ms as f32 / BLOOM_WINDOW_MS as f32)
    }
}

/// Shimmer — the trailing accent glow on newly-appended streaming
/// prose. Returns how many chars at the tail should be tinted with
/// the accent; decays quickly (400ms) so the glow rides the cursor
/// only while the tail is "wet".
pub fn shimmer_chars(last_append_elapsed_ms: u128) -> usize {
    const SHIMMER_WINDOW_MS: u128 = 400;
    if last_append_elapsed_ms >= SHIMMER_WINDOW_MS {
        0
    } else {
        let fade = 1.0 - (last_append_elapsed_ms as f32 / SHIMMER_WINDOW_MS as f32);
        (fade * 18.0) as usize
    }
}

/// Build the shimmer tail for a line — accent color on the last N
/// chars, normal style on the leading part. Used by the live card's
/// render path.
pub fn shimmer_spans(text: &str, tail_chars: usize, base: Style) -> Vec<Span<'static>> {
    if tail_chars == 0 || text.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let total = text.chars().count();
    if tail_chars >= total {
        return vec![Span::styled(
            text.to_string(),
            base.fg(Palette::ACCENT).add_modifier(Modifier::BOLD),
        )];
    }
    let cut = total - tail_chars;
    let head: String = text.chars().take(cut).collect();
    let tail: String = text.chars().skip(cut).collect();
    vec![
        Span::styled(head, base),
        Span::styled(tail, base.fg(Palette::ACCENT).add_modifier(Modifier::BOLD)),
    ]
}

/// Produce a bloom-tinted style — interpolates from accent bold
/// towards normal as intensity decays.
pub fn bloom_bar_style(intensity: f32) -> Style {
    if intensity <= 0.0 {
        return Style::default().fg(Palette::INK_2);
    }
    // Bright: accent bold. Dim target: INK_2. We don't interpolate
    // RGB in 256-color space — instead we step through two states
    // at 50% threshold.
    if intensity > 0.4 {
        Style::default()
            .fg(Palette::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Indexed(73)) // slightly muted accent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_cycles_over_time() {
        let theme = Theme { unicode: true };
        let f0 = spinner_frame(&theme, 0);
        let f1 = spinner_frame(&theme, 80);
        let f_same = spinner_frame(&theme, 40);
        assert_eq!(f0, f_same);
        assert_ne!(f0, f1);
    }

    #[test]
    fn bloom_decays_linearly_to_zero() {
        assert!((bloom_phase(0) - 1.0).abs() < 0.01);
        assert!((bloom_phase(300) - 0.5).abs() < 0.01);
        assert_eq!(bloom_phase(600), 0.0);
        assert_eq!(bloom_phase(1200), 0.0);
    }

    #[test]
    fn shimmer_fades_over_400ms() {
        assert!(shimmer_chars(0) > 0);
        assert!(shimmer_chars(200) > 0);
        assert_eq!(shimmer_chars(400), 0);
        assert_eq!(shimmer_chars(1000), 0);
    }

    #[test]
    fn shimmer_spans_zero_tail_yields_plain_span() {
        let spans = shimmer_spans("hello", 0, Style::default());
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn shimmer_spans_partial_tail_splits_head_and_tail() {
        let spans = shimmer_spans("hello world", 5, Style::default());
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn ascii_fallback_used_when_theme_unicode_false() {
        let ascii = Theme { unicode: false };
        let frame = spinner_frame(&ascii, 0);
        assert!(ASCII_SPINNER.contains(&frame));
    }
}
