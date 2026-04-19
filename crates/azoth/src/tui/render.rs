//! PAPER canvas — the frame orchestrator.
//!
//! Layout (rows top-to-bottom):
//!
//! 1.  status strip (1 row, no border)
//! 2.  hairline separator (1 row)
//! 3.  canvas row: optional rail (left), canvas (flex), optional inspector (right)
//! 4.  whisper row (1 row, pre-composer narrator)
//! 5.  hairline separator (1 row)
//! 6.  composer (3 rows, rounded)
//!
//! When the terminal is narrow (<100 cols), the inspector auto-hides.
//! When Focus Mode is on, all turns except the active one are hidden.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::app::{AppState, ClickTarget};
use super::theme::{pulse_phase, Palette as Colors, Theme};
use super::{inspector, palette, rail, sheet, splash};

pub fn frame(f: &mut Frame, state: &mut AppState) {
    let size = f.area();
    let theme = state.theme;
    let elapsed_ms = state.boot.elapsed().as_millis();
    let bar_phase = pulse_phase(elapsed_ms, 600);
    let cursor_phase = pulse_phase(elapsed_ms, 500);

    // Reset click map every frame; render paths register hit regions
    // by absolute terminal Y. Sized to exactly the canvas height.
    // Each row holds a list of (x_range, target) so multiple buttons
    // on one row (sheet action bar, status row toggles) are routable.
    state.click_map.clear();
    state.click_map.resize_with(size.height as usize, Vec::new);

    // Splashscreen takes the whole canvas while the worker boots.
    if state.booting {
        splash::render(f, size, &theme, &state.boot_phase, elapsed_ms);
        return;
    }

    let show_rail = state.rail_open;
    let show_inspector = state.inspector_open && size.width >= 100;

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status
            Constraint::Length(1), // hairline
            Constraint::Min(3),    // canvas row
            Constraint::Length(1), // whisper
            Constraint::Length(1), // hairline
            Constraint::Length(3), // composer
        ])
        .split(size);

    render_status(f, vertical[0], state, &theme);
    // Status row's "azoth" word opens the palette on click — gives
    // mouse users a hit target without adding visible button chrome.
    // Both ranges are anchored to `vertical[0].x` so they survive
    // any future layout that nests the canvas under a non-zero
    // horizontal offset (today the top-level split has area.x=0,
    // but the right answer is still relative).
    let status_y = vertical[0].y as usize;
    if status_y < state.click_map.len() {
        let row_x = vertical[0].x;
        let row_w = vertical[0].width;
        // "  azoth" — leading 2 spaces + 5-letter brand.
        state.click_map[status_y].push((row_x + 2..row_x + 7, ClickTarget::PaletteOpen));
        // "ctx 45%" lives on the right side; clicking toggles
        // inspector. Width-conditional fallback: if status row is
        // narrower than ~40 cols, the ctx label may be off-screen,
        // but the click range simply registers no hits in that case.
        if row_w > 12 {
            let start = row_x + row_w.saturating_sub(12);
            state.click_map[status_y].push((start..row_x + row_w, ClickTarget::InspectorToggle));
        }
    }
    render_hairline(f, vertical[1], &theme);

    // Middle row: optional rail + canvas + optional inspector.
    let mut mid_constraints: Vec<Constraint> = Vec::new();
    if show_rail {
        mid_constraints.push(Constraint::Length(14));
    }
    mid_constraints.push(Constraint::Min(20));
    if show_inspector {
        mid_constraints.push(Constraint::Length(30));
    }
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(mid_constraints)
        .split(vertical[2]);

    let mut idx = 0;
    if show_rail {
        let rail_area = mid[idx];
        let selected = state.cards.len().saturating_sub(1);
        rail::render(f, rail_area, &state.cards, &theme, bar_phase, selected);
        idx += 1;
    }
    let canvas_area = mid[idx];
    render_canvas(f, canvas_area, state, &theme, bar_phase, cursor_phase);
    idx += 1;
    if show_inspector {
        let inspector_area = mid[idx];
        inspector::render(f, inspector_area, &state.inspector_data, &theme);
    }

    render_whisper(f, vertical[3], state, &theme);
    render_hairline(f, vertical[4], &theme);
    render_composer(f, vertical[5], state, &theme);

    // Overlays.
    if state.palette.open {
        palette::render(f, size, &state.palette, &theme, state.cards.len());
    }
    if let Some(req) = state.pending_approval.as_ref() {
        sheet::render(f, canvas_area, req, &theme, &mut state.click_map);
    }
}

fn render_status(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut spans: Vec<Span<'static>> = vec![
        Span::raw("  "),
        Span::styled("azoth".to_string(), theme.bold()),
        Span::styled(" · ".to_string(), theme.dim()),
    ];
    let contract_label = state
        .inspector_data
        .contract_goal
        .as_deref()
        .map(|g| trunc(g, 40))
        .unwrap_or_else(|| "no contract yet".to_string());
    spans.push(Span::styled(contract_label, theme.ink(Colors::INK_1)));
    // Model / profile label (from AppState.status, set to
    // "<profile> · <model_id>" at worker init).
    if !state.status.is_empty() && state.status != "ready" {
        spans.push(Span::styled(" · ".to_string(), theme.dim()));
        spans.push(Span::styled(
            trunc(&state.status, 48),
            theme.ink(Colors::INK_2),
        ));
    }
    // Turn count.
    spans.push(Span::styled(
        format!("  ·  {} turns", state.committed_turns),
        theme.dim(),
    ));
    // Context percentage — no broken clock glyph; the label + color
    // carries the meaning.
    let ctx_style = if state.ctx_pct >= 80 {
        theme.ink(Colors::ABORT).add_modifier(Modifier::BOLD)
    } else {
        theme.ink(Colors::ACCENT).add_modifier(Modifier::BOLD)
    };
    spans.push(Span::styled("     ctx ".to_string(), theme.dim()));
    spans.push(Span::styled(format!("{}%", state.ctx_pct), ctx_style));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_hairline(f: &mut Frame, area: Rect, theme: &Theme) {
    let w = area.width as usize;
    let line = Line::from(Span::styled(
        theme.glyph(Theme::HAIRLINE_CHAR).repeat(w),
        theme.hairline(),
    ));
    f.render_widget(Paragraph::new(line), area);
}

fn render_canvas(
    f: &mut Frame,
    area: Rect,
    state: &mut AppState,
    theme: &Theme,
    bar_phase: bool,
    cursor_phase: bool,
) {
    if state.cards.is_empty() {
        render_zero_state(f, area, theme);
        return;
    }

    // Which card indices map into the visible iterator.
    let visible_indices: Vec<usize> = if state.focus_mode {
        // The last live card, else the last card overall.
        let live = state
            .cards
            .iter()
            .enumerate()
            .rev()
            .find(|(_, c)| c.is_live())
            .map(|(i, _)| i);
        match live {
            Some(i) => vec![i],
            None => state
                .cards
                .len()
                .checked_sub(1)
                .map(|i| vec![i])
                .unwrap_or_default(),
        }
    } else {
        (0..state.cards.len()).collect()
    };

    let visible_height = area.height;
    let visible_h_usize = visible_height as usize;

    // Pass 1 — estimate the total height by streaming cards through
    // the same per-card height function. Earlier code collected into
    // a Vec<usize> just to .sum() it, paying an N-sized allocation
    // every frame; the helper closure below lets pass 2 read the
    // same per-card estimate without re-materialising the Vec.
    //
    // Future scaling: this is O(visible cards) per frame. For sessions
    // with >>10k cards a Fenwick/segment tree on `last_rendered_rows`
    // would give O(log N) lookup. Defer until we see real workloads
    // that hit the wall — until then the constant factors dominate
    // and a Vec walk is faster than tree pointer-chasing.
    const UNRENDERED_HEIGHT_HINT: usize = 4;
    fn est_h(rows: usize) -> usize {
        if rows == 0 {
            UNRENDERED_HEIGHT_HINT
        } else {
            rows
        }
    }
    let est_total: usize = visible_indices
        .iter()
        .map(|&i| est_h(state.cards[i].last_rendered_rows))
        .sum();
    let est_max_scroll = est_total.saturating_sub(visible_h_usize);
    let scroll_offset = state.scroll_offset as usize;
    let est_target_top = if state.scroll_locked {
        est_max_scroll.saturating_sub(scroll_offset)
    } else {
        state.scroll_offset = 0;
        est_max_scroll
    };
    let est_target_bot = est_target_top + visible_h_usize;

    // Pass 2 — for each card, render fully when it intersects the
    // estimated viewport (or has never been painted). Cards entirely
    // off-screen get blank-line placeholders matching their cached
    // height so scroll math + click_map indices stay consistent
    // without paying `render_rows` cost (which includes markdown
    // restyling, cell preview restyling, etc.). On long-running
    // sessions this turns O(N) per-frame work into O(rows on screen).
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut row_hints: Vec<Option<(usize, super::card::RowHint)>> = Vec::new();
    let mut cursor_y: usize = 0;
    for &card_idx in visible_indices.iter() {
        let est_h_val = est_h(state.cards[card_idx].last_rendered_rows);
        let card_y_end = cursor_y + est_h_val;
        let intersects = card_y_end >= est_target_top && cursor_y <= est_target_bot;
        let never_rendered = state.cards[card_idx].last_rendered_rows == 0;
        if intersects || never_rendered {
            let rows = state.cards[card_idx].render_rows(theme, cursor_phase, bar_phase);
            cursor_y += rows.len();
            for (line, hint) in rows {
                lines.push(line);
                row_hints.push(hint.map(|h| (card_idx, h)));
            }
        } else {
            for _ in 0..est_h_val {
                lines.push(Line::from(""));
                row_hints.push(None);
            }
            cursor_y += est_h_val;
        }
    }

    // Recompute scroll from actual line count — estimates may be off
    // by a few rows after a card streams new prose, and the actual
    // total is what `Paragraph::scroll` consumes.
    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(visible_height);
    let scroll_pos = if state.scroll_locked {
        max_scroll
            .saturating_sub(state.scroll_offset)
            .min(max_scroll)
    } else {
        max_scroll
    };

    // Populate click_map by mapping each visible row to its absolute
    // terminal Y. Iterating only the visible window keeps this O(rows
    // on screen) instead of O(total transcript lines), which mattered
    // once long-running sessions accumulated thousands of lines.
    let first_visible = scroll_pos as usize;
    for (relative_y, hint) in row_hints
        .iter()
        .enumerate()
        .skip(first_visible)
        .take(visible_height as usize)
        .map(|(line_idx, hint)| (line_idx - first_visible, hint))
    {
        let absolute_y = area.y as usize + relative_y;
        if let Some((card_idx, h)) = hint {
            let target = match h {
                super::card::RowHint::ThoughtsHeader => ClickTarget::ThoughtsToggle {
                    card_idx: *card_idx,
                },
                super::card::RowHint::CellHeader { cell_idx } => ClickTarget::CellToggle {
                    card_idx: *card_idx,
                    cell_idx: *cell_idx,
                },
            };
            if absolute_y < state.click_map.len() {
                // Card hits are constrained to the canvas X bounds —
                // earlier `0..u16::MAX` leaked through rail/inspector
                // panels on the same Y, toggling cards from clicks
                // landing inside side drawers.
                let x_range = area.x..(area.x + area.width);
                state.click_map[absolute_y].push((x_range, target));
            }
        }
    }

    let body = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_pos, 0));
    f.render_widget(body, area);
}

fn render_zero_state(f: &mut Frame, area: Rect, theme: &Theme) {
    let inner_y = area.y + area.height / 3;
    let bar = theme.glyph(Theme::BAR_COMMITTED);
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("     "),
            Span::styled(bar.to_string(), theme.accent()),
            Span::raw("  "),
            Span::styled("what are we building?".to_string(), theme.bold()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("        "),
            Span::styled(
                "tell azoth what you want. it will plan, then ask before touching anything."
                    .to_string(),
                theme.italic_dim(),
            ),
        ]),
    ];
    let rect = Rect {
        x: area.x,
        y: inner_y,
        width: area.width,
        height: lines.len() as u16,
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rect);
}

fn render_whisper(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let latest_note = state.notes.last();
    let line = state.whisper.render_line(theme, latest_note);
    f.render_widget(Paragraph::new(line), area);
}

fn render_composer(f: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.ink(Colors::INK_3))
        .title(Line::from(vec![
            Span::styled(" write".to_string(), theme.bold()),
            Span::styled(" ".to_string(), Style::default()),
        ]))
        .title_bottom(Line::from(vec![
            Span::styled(" ⌃K ".to_string(), theme.accent()),
            Span::styled("palette · ".to_string(), theme.dim()),
            Span::styled("↵ ".to_string(), theme.accent()),
            Span::styled("send · ".to_string(), theme.dim()),
            Span::styled("⇧↵ ".to_string(), theme.accent()),
            Span::styled("newline ".to_string(), theme.dim()),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(&state.textarea, inner);
}

fn trunc(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(limit.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}
