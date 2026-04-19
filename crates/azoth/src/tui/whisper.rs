//! Whisper — a single-row narrator above the composer.
//!
//! Present-tense descriptions of what the agent is doing right now,
//! plus a rotating view of the latest system notes. Replaces the
//! `[thinking...]` placeholder that used to pollute the transcript.

use std::time::Instant;

use ratatui::text::{Line, Span};

use super::card::{Note, NoteKind};
use super::motion;
use super::theme::{Palette, Theme};

#[derive(Debug, Clone)]
pub struct Whisper {
    narration: Option<String>,
    narration_started: Option<Instant>,
    ready_since: Option<Instant>,
}

impl Default for Whisper {
    fn default() -> Self {
        Self {
            narration: None,
            narration_started: None,
            ready_since: Some(Instant::now()),
        }
    }
}

impl Whisper {
    pub fn set(&mut self, text: impl Into<String>) {
        self.narration = Some(text.into());
        self.narration_started = Some(Instant::now());
        self.ready_since = None;
    }

    pub fn clear(&mut self) {
        self.narration = None;
        self.narration_started = None;
        self.ready_since = Some(Instant::now());
    }

    pub fn is_narrating(&self) -> bool {
        self.narration.is_some()
    }

    /// Build the whisper line. When narrating, shows the narration +
    /// elapsed seconds. When idle, shows the most recent note (if
    /// any, and if <5s old), else "ready · ⌃K for commands" on zero
    /// state.
    pub fn render_line(&self, theme: &Theme, latest_note: Option<&Note>) -> Line<'static> {
        if let (Some(text), Some(started)) = (self.narration.as_ref(), self.narration_started) {
            let elapsed_f = started.elapsed().as_secs_f32();
            let elapsed_ms = started.elapsed().as_millis();
            let spinner = motion::spinner_frame(theme, elapsed_ms);
            return Line::from(vec![
                Span::raw("      "),
                Span::styled(spinner.to_string(), theme.accent()),
                Span::raw(" "),
                Span::styled("azoth".to_string(), theme.bold()),
                Span::raw(" "),
                Span::styled(text.clone(), theme.italic_dim()),
                Span::styled(format!(" · {elapsed_f:.1}s"), theme.dim()),
            ]);
        }

        if let Some(note) = latest_note {
            if note.at.elapsed().as_secs_f32() < 5.0 {
                let (prefix, style) = match note.kind {
                    NoteKind::Info => ("·", theme.dim()),
                    NoteKind::Help => ("?", theme.ink(Palette::ACCENT)),
                    NoteKind::Warn => ("!", theme.ink(Palette::AMBER)),
                    NoteKind::Error => ("!", theme.ink(Palette::ABORT)),
                };
                return Line::from(vec![
                    Span::raw("      "),
                    Span::styled(prefix.to_string(), style),
                    Span::raw(" "),
                    Span::styled(note.text.clone(), theme.italic_dim()),
                ]);
            }
        }

        // Default zero-state hint.
        Line::from(vec![
            Span::raw("      "),
            Span::styled("ready".to_string(), theme.dim()),
            Span::raw(" "),
            Span::styled("· ⌃K for commands".to_string(), theme.italic_dim()),
        ])
    }
}
