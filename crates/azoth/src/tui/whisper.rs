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

    /// Build the whisper line. Priority order (highest first):
    ///
    /// 1. `pending_approval` — an approval sheet is up and the worker is
    ///    blocked on the user. F1 2026-04-24: previously the whisper
    ///    continued to show "running <tool>" with an elapsed timer
    ///    ticking, which convinced an operator the model was stuck
    ///    (the sheet can be off-screen under narrow terminals).
    /// 2. narration — the worker is actively doing something.
    /// 3. the most recent note (if <5s old).
    /// 4. zero-state hint.
    pub fn render_line(
        &self,
        theme: &Theme,
        latest_note: Option<&Note>,
        pending_approval: Option<&azoth_core::authority::ApprovalRequestMsg>,
    ) -> Line<'static> {
        if let Some(req) = pending_approval {
            let tool = req.tool_name.clone();
            let cls = req.effect_class.to_string();
            return Line::from(vec![
                Span::raw("      "),
                Span::styled("⏸", theme.ink(Palette::AMBER)),
                Span::raw(" "),
                Span::styled("azoth", theme.bold()),
                Span::raw(" "),
                Span::styled("awaiting approval", theme.bold()),
                Span::styled(format!(" · {tool} → {cls}"), theme.italic_dim()),
            ]);
        }
        if let (Some(text), Some(started)) = (self.narration.as_ref(), self.narration_started) {
            let elapsed_f = started.elapsed().as_secs_f32();
            let elapsed_ms = started.elapsed().as_millis();
            let spinner = motion::spinner_frame(theme, elapsed_ms);
            return Line::from(vec![
                Span::raw("      "),
                Span::styled(spinner, theme.accent()),
                Span::raw(" "),
                Span::styled("azoth", theme.bold()),
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
                    Span::styled(prefix, style),
                    Span::raw(" "),
                    Span::styled(note.text.clone(), theme.italic_dim()),
                ]);
            }
        }

        // Default zero-state hint.
        Line::from(vec![
            Span::raw("      "),
            Span::styled("ready", theme.dim()),
            Span::raw(" "),
            Span::styled("· ⌃K for commands", theme.italic_dim()),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;
    use azoth_core::authority::ApprovalRequestMsg;
    use azoth_core::schemas::{ApprovalId, EffectClass, TurnId};
    use tokio::sync::oneshot;

    fn flatten(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn pending_for(tool: &str, cls: EffectClass) -> ApprovalRequestMsg {
        let (tx, _rx) = oneshot::channel();
        ApprovalRequestMsg {
            turn_id: TurnId::new(),
            approval_id: ApprovalId::new(),
            tool_name: tool.into(),
            effect_class: cls,
            summary: format!("{tool} → example summary"),
            responder: tx,
            budget_extension: None,
        }
    }

    #[test]
    fn pending_approval_overrides_narration_with_awaiting_text() {
        // F1: when the approval sheet is up, the whisper must NOT
        // read "running bash · 30s" — that lies about worker state.
        let mut w = Whisper::default();
        w.set("running bash");
        let theme = Theme::detect();
        let req = pending_for("bash", EffectClass::ApplyLocal);
        let line = w.render_line(&theme, None, Some(&req));
        let flat = flatten(&line);
        assert!(
            flat.contains("awaiting approval"),
            "must surface awaiting state; got: {flat:?}"
        );
        assert!(
            flat.contains("bash"),
            "must include tool name; got: {flat:?}"
        );
        assert!(
            flat.contains("apply_local"),
            "must include effect_class in snake_case (F8); got: {flat:?}"
        );
        assert!(
            !flat.contains("running bash"),
            "narration MUST be suppressed while blocked on approval; got: {flat:?}"
        );
    }

    #[test]
    fn pending_approval_overrides_recent_note() {
        // Priority: approval > note. A recent error note shouldn't
        // leak through when the user owes a decision.
        let w = Whisper::default();
        let note = Note::error("something exploded");
        let theme = Theme::detect();
        let req = pending_for("fs_write", EffectClass::ApplyLocal);
        let line = w.render_line(&theme, Some(&note), Some(&req));
        let flat = flatten(&line);
        assert!(flat.contains("awaiting approval"), "got: {flat:?}");
        assert!(
            !flat.contains("something exploded"),
            "note must not leak past approval gate; got: {flat:?}"
        );
    }

    #[test]
    fn no_approval_still_narrates() {
        // Regression guard: approval plumbing must not break the
        // normal narration path.
        let mut w = Whisper::default();
        w.set("thinking");
        let theme = Theme::detect();
        let line = w.render_line(&theme, None, None);
        let flat = flatten(&line);
        assert!(flat.contains("thinking"), "got: {flat:?}");
    }
}
