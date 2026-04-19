//! PAPER theme — colors, glyphs, and Unicode-fallback detection.
//!
//! Palette: typography is the hero. One accent (256-index 74) for live
//! interactive state. Amber (214) for approval pulse only. Red-dim (131)
//! for aborted bars only. Everything else is an ink step in greyscale.
//!
//! Glyphs: probed at startup. When the terminal can't carry UTF-8
//! (`LANG` without `UTF-8`, or `TERM=dumb`), the table falls back to
//! ASCII surrogates. Layout and semantics are unchanged; only the
//! characters degrade.

use ratatui::style::{Color, Modifier, Style};

/// Color palette. Seven colors, total.
pub struct Palette;

impl Palette {
    /// Single accent — live cursor, pulsing bar, palette highlight.
    pub const ACCENT: Color = Color::Indexed(74); // soft cyan
    /// Approval pulse only.
    pub const AMBER: Color = Color::Indexed(214);
    /// Aborted turn bars only.
    pub const ABORT: Color = Color::Indexed(131);
    /// Ink ladder — from brightest readable to faintest hairline.
    pub const INK_0: Color = Color::Reset; // default terminal fg
    pub const INK_1: Color = Color::Indexed(250);
    pub const INK_2: Color = Color::Indexed(244);
    pub const INK_3: Color = Color::Indexed(240); // dim metadata
    pub const INK_4: Color = Color::Indexed(237); // hairlines
}

/// A pair of glyphs — Unicode primary, ASCII fallback.
#[derive(Copy, Clone, Debug)]
pub struct GlyphPair {
    pub unicode: &'static str,
    pub ascii: &'static str,
}

impl GlyphPair {
    pub const fn new(unicode: &'static str, ascii: &'static str) -> Self {
        Self { unicode, ascii }
    }
}

/// The active theme: carries glyph selection and style helpers.
#[derive(Copy, Clone, Debug)]
pub struct Theme {
    pub unicode: bool,
}

impl Theme {
    /// Probe the environment once at startup. Locale precedence per
    /// POSIX: `LC_ALL` overrides everything, then `LC_CTYPE`, then
    /// `LANG`. Earlier code probed `LANG` first, so a system with
    /// `LANG=en_US.UTF-8 LC_ALL=C` would incorrectly enable Unicode
    /// glyphs (mojibake risk in cwd-locale-restricted shells).
    pub fn detect() -> Self {
        let term_ok = std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
        let utf_ok = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LC_CTYPE"))
            .or_else(|_| std::env::var("LANG"))
            .map(|v| v.to_uppercase().contains("UTF-8") || v.to_uppercase().contains("UTF8"))
            .unwrap_or(false);
        Self {
            unicode: term_ok && utf_ok,
        }
    }

    pub fn glyph(&self, pair: GlyphPair) -> &'static str {
        if self.unicode {
            pair.unicode
        } else {
            pair.ascii
        }
    }

    // --- Glyph table (public const pairs, resolved via Theme::glyph) ---

    /// Card bar: committed (solid).
    pub const BAR_COMMITTED: GlyphPair = GlyphPair::new("▍", "|");
    /// Card bar: live (pulse A).
    pub const BAR_LIVE_A: GlyphPair = GlyphPair::new("▋", "|");
    /// Card bar: live (pulse B).
    pub const BAR_LIVE_B: GlyphPair = GlyphPair::new("▍", " ");
    /// Card bar: interrupted (dashed).
    pub const BAR_INTERRUPTED: GlyphPair = GlyphPair::new("╎", "!");
    /// Card bar: aborted.
    pub const BAR_ABORTED: GlyphPair = GlyphPair::new("▍", "X");
    /// Card bar: awaiting approval (pulse).
    pub const BAR_AWAIT_A: GlyphPair = GlyphPair::new("▋", "*");
    pub const BAR_AWAIT_B: GlyphPair = GlyphPair::new("▍", " ");
    /// Tool cell prefix.
    pub const CELL_PREFIX: GlyphPair = GlyphPair::new("❯", ">");
    /// Cursor block for streaming cards.
    pub const CURSOR_A: GlyphPair = GlyphPair::new("▋", "_");
    pub const CURSOR_B: GlyphPair = GlyphPair::new(" ", " ");
    /// Status-bar hourglass.
    pub const CLOCK: GlyphPair = GlyphPair::new("⧗", "~");
    /// Palette magnifier.
    pub const MAGNIFIER: GlyphPair = GlyphPair::new("⌘", "*");
    /// Hairline row.
    pub const HAIRLINE_CHAR: GlyphPair = GlyphPair::new("─", "-");
    /// Tool cell success checkmark.
    pub const CHECK: GlyphPair = GlyphPair::new("✓", "+");
    /// Tool cell error mark.
    pub const WARN: GlyphPair = GlyphPair::new("⚠", "!");
    /// Turn arrow separators in the inspector/evidence.
    pub const BULLET: GlyphPair = GlyphPair::new("▎", "|");

    // --- Style helpers ---

    pub fn ink(&self, color: Color) -> Style {
        Style::default().fg(color)
    }
    pub fn bold(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }
    pub fn dim(&self) -> Style {
        Style::default().fg(Palette::INK_3)
    }
    pub fn hairline(&self) -> Style {
        Style::default().fg(Palette::INK_4)
    }
    pub fn italic_dim(&self) -> Style {
        Style::default()
            .fg(Palette::INK_3)
            .add_modifier(Modifier::ITALIC)
    }
    pub fn accent(&self) -> Style {
        Style::default().fg(Palette::ACCENT)
    }
    pub fn strike_dim(&self) -> Style {
        Style::default()
            .fg(Palette::INK_3)
            .add_modifier(Modifier::CROSSED_OUT)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::detect()
    }
}

/// Pulse/blink decision based on a monotonic millis clock. Returns
/// true for the "A" half of the cycle, false for the "B" half.
pub fn pulse_phase(elapsed_ms: u128, period_ms: u128) -> bool {
    (elapsed_ms / period_ms) % 2 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_alternates() {
        assert!(pulse_phase(0, 500));
        assert!(!pulse_phase(500, 500));
        assert!(pulse_phase(1000, 500));
        assert!(!pulse_phase(1500, 500));
    }

    #[test]
    fn glyph_respects_theme() {
        let utf = Theme { unicode: true };
        let ascii = Theme { unicode: false };
        assert_eq!(utf.glyph(Theme::BAR_COMMITTED), "▍");
        assert_eq!(ascii.glyph(Theme::BAR_COMMITTED), "|");
    }
}
