//! Markdown → styled `Line` rendering for PAPER prose.
//!
//! Built on `pulldown-cmark`. The model writes real markdown; PAPER
//! renders it as real typography instead of plaintext.
//!
//! Supported:
//! - Fenced code blocks (```lang ... ```) rendered as **code islands**
//!   with a language label, a left accent bar, 2-col gutter, and
//!   minimal syntax tinting (keywords + strings + comments) for a few
//!   languages.
//! - Inline code (`code`)
//! - Bold (**text**) and italic (*text*)
//! - Headings (# through ######)
//! - Bulleted + numbered lists
//! - Blockquotes (> …)
//! - Links — rendered as `text` with the URL underneath, dim.
//! - Paragraphs separated by blank lines.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::{Palette, Theme};

/// Parse `md` and return a sequence of pre-styled `Line`s that can
/// be dropped into a ratatui `Paragraph`.
pub fn render(md: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::with_capacity(md.lines().count() + 4);
    // GFM extensions — without ENABLE_TABLES, pulldown-cmark emits
    // raw text for pipe-syntax tables, producing jumbled output.
    // Strikethrough + task lists are cheap wins on the same shape.
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);

    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![theme.ink(Palette::INK_0)];
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;
    let mut code_buffer = String::new();
    let mut list_depth: usize = 0;
    let mut list_counters: Vec<Option<u64>> = Vec::new();
    let mut in_blockquote: bool = false;
    let mut pending_link_url: Option<String> = None;
    let mut heading_level: Option<HeadingLevel> = None;
    let mut table_buf = TableBuf::default();
    let mut in_table: bool = false;
    let mut in_cell: bool = false;

    for ev in parser {
        match ev {
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush(&mut out, &mut current_spans);
                out.push(Line::from(""));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut out, &mut current_spans);
                heading_level = Some(level);
                let style = heading_style(level, theme);
                style_stack.push(style);
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut out, &mut current_spans);
                style_stack.pop();
                heading_level = None;
                out.push(Line::from(""));
            }
            Event::Start(Tag::Emphasis) => {
                let top = *style_stack.last().unwrap_or(&Style::default());
                style_stack.push(top.add_modifier(Modifier::ITALIC));
            }
            Event::End(TagEnd::Emphasis) => {
                style_stack.pop();
            }
            Event::Start(Tag::Strong) => {
                let top = *style_stack.last().unwrap_or(&Style::default());
                style_stack.push(top.add_modifier(Modifier::BOLD));
            }
            Event::End(TagEnd::Strong) => {
                style_stack.pop();
            }
            Event::Start(Tag::Strikethrough) => {
                let top = *style_stack.last().unwrap_or(&Style::default());
                style_stack.push(top.add_modifier(Modifier::CROSSED_OUT));
            }
            Event::End(TagEnd::Strikethrough) => {
                style_stack.pop();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                flush(&mut out, &mut current_spans);
                in_blockquote = true;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut out, &mut current_spans);
                in_blockquote = false;
            }
            Event::Start(Tag::List(start)) => {
                flush(&mut out, &mut current_spans);
                list_depth = list_depth.saturating_add(1);
                list_counters.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                flush(&mut out, &mut current_spans);
                list_depth = list_depth.saturating_sub(1);
                list_counters.pop();
            }
            Event::Start(Tag::Item) => {
                flush(&mut out, &mut current_spans);
                let indent = "  ".repeat(list_depth.saturating_sub(1));
                let marker = match list_counters.last_mut() {
                    Some(Some(n)) => {
                        let s = format!("{n}. ");
                        *n = n.saturating_add(1);
                        s
                    }
                    _ => "• ".to_string(),
                };
                current_spans.push(Span::raw(indent));
                current_spans.push(Span::styled(marker, theme.ink(Palette::ACCENT)));
            }
            Event::End(TagEnd::Item) => {
                flush(&mut out, &mut current_spans);
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut out, &mut current_spans);
                in_code_block = true;
                code_buffer.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => Some(lang.to_string()),
                    CodeBlockKind::Indented => None,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                render_code_island(&mut out, theme, code_lang.as_deref(), &code_buffer);
                code_buffer.clear();
                code_lang = None;
                in_code_block = false;
                out.push(Line::from(""));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                pending_link_url = Some(dest_url.to_string());
                let top = *style_stack.last().unwrap_or(&Style::default());
                style_stack.push(top.fg(Palette::ACCENT).add_modifier(Modifier::UNDERLINED));
            }
            Event::End(TagEnd::Link) => {
                style_stack.pop();
                if let Some(url) = pending_link_url.take() {
                    current_spans.push(Span::styled(format!(" ({url})"), theme.italic_dim()));
                }
            }
            Event::Start(Tag::Table(_)) => {
                flush(&mut out, &mut current_spans);
                in_table = true;
                table_buf = TableBuf::default();
            }
            Event::End(TagEnd::Table) => {
                render_table(&mut out, &table_buf, theme);
                out.push(Line::from(""));
                in_table = false;
            }
            Event::Start(Tag::TableHead) => {
                table_buf.in_header = true;
            }
            Event::End(TagEnd::TableHead) => {
                table_buf.header = std::mem::take(&mut table_buf.current_row);
                table_buf.in_header = false;
            }
            Event::Start(Tag::TableRow) => {
                // Cells accumulate via TableCell end; nothing to do here.
            }
            Event::End(TagEnd::TableRow) => {
                if !table_buf.in_header {
                    let row = std::mem::take(&mut table_buf.current_row);
                    table_buf.body.push(row);
                }
            }
            Event::Start(Tag::TableCell) => {
                in_cell = true;
                table_buf.current_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                in_cell = false;
                let cell = std::mem::take(&mut table_buf.current_cell);
                table_buf.current_row.push(cell);
            }
            Event::Code(text) => {
                if in_table && in_cell {
                    table_buf.current_cell.push_str(&text);
                } else {
                    let top = *style_stack.last().unwrap_or(&Style::default());
                    current_spans.push(Span::styled(
                        text.to_string(),
                        top.fg(Palette::ACCENT)
                            .bg(Color::Indexed(236))
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            }
            Event::Text(text) => {
                if in_table && in_cell {
                    table_buf.current_cell.push_str(&text);
                } else if in_code_block {
                    code_buffer.push_str(&text);
                } else if in_blockquote {
                    for line in text.lines() {
                        out.push(Line::from(vec![
                            Span::styled("│ ".to_string(), theme.ink(Palette::ACCENT)),
                            Span::styled(line.to_string(), theme.italic_dim()),
                        ]));
                    }
                } else {
                    let style = if let Some(level) = heading_level {
                        heading_style(level, theme)
                    } else {
                        *style_stack.last().unwrap_or(&Style::default())
                    };
                    current_spans.push(Span::styled(text.to_string(), style));
                }
            }
            Event::SoftBreak => {
                current_spans.push(Span::raw(" "));
            }
            Event::HardBreak => {
                flush(&mut out, &mut current_spans);
            }
            Event::Rule => {
                flush(&mut out, &mut current_spans);
                let w = 40usize;
                out.push(Line::from(Span::styled(
                    theme.glyph(Theme::HAIRLINE_CHAR).repeat(w),
                    theme.hairline(),
                )));
                out.push(Line::from(""));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                current_spans.push(Span::styled(html.to_string(), theme.dim()));
            }
            _ => {}
        }
    }
    flush(&mut out, &mut current_spans);
    // Drop a trailing blank line if present so the card's own
    // spacing rules take over.
    while out.last().map(is_blank_line).unwrap_or(false) {
        out.pop();
    }
    out
}

/// Accumulator for a GFM table during parsing. Cells are buffered as
/// plain strings; styling (header vs body) is applied at
/// `render_table` time.
#[derive(Default)]
struct TableBuf {
    header: Vec<String>,
    body: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_header: bool,
}

/// Render a parsed GFM table as whitespace-aligned PAPER-style prose.
/// Intentionally no vertical dividers — the aesthetic relies on
/// alignment + a single hairline under the header, matching the rest
/// of the canvas typography.
fn render_table(out: &mut Vec<Line<'static>>, t: &TableBuf, theme: &Theme) {
    let col_count = t
        .header
        .len()
        .max(t.body.iter().map(|r| r.len()).max().unwrap_or(0));
    if col_count == 0 {
        return;
    }
    const MAX_COL: usize = 48;
    const GAP: usize = 2;

    use unicode_width::UnicodeWidthStr;
    let mut widths = vec![0usize; col_count];
    for (i, cell) in t.header.iter().enumerate().take(col_count) {
        widths[i] = widths[i].max(UnicodeWidthStr::width(cell.as_str()));
    }
    for row in &t.body {
        for (i, cell) in row.iter().enumerate().take(col_count) {
            widths[i] = widths[i].max(UnicodeWidthStr::width(cell.as_str()));
        }
    }
    for w in &mut widths {
        *w = (*w).min(MAX_COL);
    }

    let header_style = theme.bold().fg(Palette::ACCENT);
    let body_style = theme.ink(Palette::INK_1);
    let gap = " ".repeat(GAP);

    let mut header_spans: Vec<Span<'static>> = vec![Span::raw("  ")];
    for (i, width) in widths.iter().enumerate().take(col_count) {
        let cell = t.header.get(i).cloned().unwrap_or_default();
        header_spans.push(Span::styled(pad_to(&cell, *width), header_style));
        if i + 1 < col_count {
            header_spans.push(Span::styled(gap.clone(), theme.dim()));
        }
    }
    out.push(Line::from(header_spans));

    let total_width: usize = widths.iter().sum::<usize>() + col_count.saturating_sub(1) * GAP;
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            theme.glyph(Theme::HAIRLINE_CHAR).repeat(total_width),
            theme.hairline(),
        ),
    ]));

    for row in &t.body {
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
        for (i, width) in widths.iter().enumerate().take(col_count) {
            let cell = row.get(i).cloned().unwrap_or_default();
            spans.push(Span::styled(pad_to(&cell, *width), body_style));
            if i + 1 < col_count {
                spans.push(Span::styled(gap.clone(), theme.dim()));
            }
        }
        out.push(Line::from(spans));
    }
}

/// Pad or truncate a string to exactly `width` display columns. Uses
/// `unicode-width` so CJK double-wide characters and wide emoji align
/// correctly; ASCII / Latin pay no cost (cached single-char widths).
/// Preserves content intact when it fits; appends `…` on truncation
/// so the result never overruns `width`.
fn pad_to(s: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let total_w = UnicodeWidthStr::width(s);
    match total_w.cmp(&width) {
        std::cmp::Ordering::Greater => {
            // Truncate chars until we're within `width - 1`, then
            // append ellipsis.
            let target = width.saturating_sub(1);
            let mut taken = String::with_capacity(s.len());
            let mut acc = 0usize;
            for c in s.chars() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if acc + cw > target {
                    break;
                }
                taken.push(c);
                acc += cw;
            }
            taken.push('…');
            // Ellipsis is width=1; we already tracked the truncated
            // body width as `acc`, so the total is acc + 1 — no need
            // to walk the string again with `UnicodeWidthStr::width`.
            let mut out = taken;
            for _ in (acc + 1)..width {
                out.push(' ');
            }
            out
        }
        std::cmp::Ordering::Less => {
            let mut out = s.to_string();
            for _ in 0..(width - total_w) {
                out.push(' ');
            }
            out
        }
        std::cmp::Ordering::Equal => s.to_string(),
    }
}

fn flush(out: &mut Vec<Line<'static>>, current: &mut Vec<Span<'static>>) {
    if !current.is_empty() {
        let taken = std::mem::take(current);
        out.push(Line::from(taken));
    }
}

fn is_blank_line(line: &Line<'static>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

fn heading_style(level: HeadingLevel, _theme: &Theme) -> Style {
    match level {
        HeadingLevel::H1 => Style::default()
            .fg(Palette::ACCENT)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H2 => Style::default().add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default().add_modifier(Modifier::BOLD),
        _ => Style::default()
            .fg(Palette::INK_1)
            .add_modifier(Modifier::BOLD),
    }
}

// --- Code islands ---

fn render_code_island(out: &mut Vec<Line<'static>>, theme: &Theme, lang: Option<&str>, body: &str) {
    // Header line: language chip in dim, above the bar.
    let lang_label = lang.unwrap_or("").trim();
    if !lang_label.is_empty() {
        out.push(Line::from(vec![
            Span::styled("  ".to_string(), theme.dim()),
            Span::styled(
                lang_label.to_lowercase(),
                theme.dim().add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    let bar = theme.glyph(Theme::BAR_COMMITTED);
    let bar_style = Style::default().fg(Palette::ACCENT);

    for line in body.lines() {
        let code_spans = tint_code(line, lang_label, theme);
        let mut spans = vec![Span::styled(bar.to_string(), bar_style), Span::raw("  ")];
        spans.extend(code_spans);
        out.push(Line::from(spans));
    }
}

/// Tiny, deliberately-minimal syntax tinter. Token categories:
/// keyword (accent), string (a secondary accent), comment (dim
/// italic), everything else (ink-0). Recognises Rust, bash/shell,
/// Python, JSON/TOML basics, JS/TS. Falls through to plain for
/// unknown languages.
fn tint_code(line: &str, lang: &str, theme: &Theme) -> Vec<Span<'static>> {
    let lang_l = lang.to_lowercase();
    let keywords: &[&str] = match lang_l.as_str() {
        "rust" | "rs" => &[
            "fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "impl", "trait", "match",
            "if", "else", "for", "while", "loop", "return", "async", "await", "self", "Self",
            "const", "static", "as", "dyn", "where", "ref", "move", "unsafe", "crate", "super",
            "type", "in", "break", "continue",
        ],
        "python" | "py" => &[
            "def", "class", "return", "if", "elif", "else", "for", "while", "import", "from", "as",
            "with", "try", "except", "raise", "pass", "yield", "lambda", "async", "await", "True",
            "False", "None", "self",
        ],
        "bash" | "sh" | "shell" | "zsh" => &[
            "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "case", "esac",
            "function", "return", "export", "local", "echo", "read",
        ],
        "javascript" | "js" | "typescript" | "ts" | "tsx" | "jsx" => &[
            "function",
            "const",
            "let",
            "var",
            "return",
            "if",
            "else",
            "for",
            "while",
            "class",
            "extends",
            "new",
            "this",
            "async",
            "await",
            "import",
            "export",
            "from",
            "as",
            "default",
            "type",
            "interface",
            "enum",
        ],
        "json" | "toml" => &["true", "false", "null"],
        _ => &[],
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];

        // Comments — language-dependent sniff.
        let is_comment_start = match lang_l.as_str() {
            "rust" | "rs" | "javascript" | "js" | "typescript" | "ts" | "tsx" | "jsx" => {
                i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/'
            }
            "python" | "py" | "bash" | "sh" | "shell" | "zsh" | "toml" => b == b'#',
            _ => false,
        };
        if is_comment_start {
            let rest = &line[i..];
            spans.push(Span::styled(rest.to_string(), theme.italic_dim()));
            return spans;
        }

        // String literal — double-quoted.
        if b == b'"' || (b == b'\'' && (lang_l == "python" || lang_l == "py")) {
            let quote = b;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let literal: String = line[start..i].to_string();
            spans.push(Span::styled(
                literal,
                Style::default()
                    .fg(Color::Indexed(186)) // soft gold
                    .add_modifier(Modifier::ITALIC),
            ));
            continue;
        }

        // Word — tokenise by simple ASCII-word boundary.
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if keywords.contains(&word) {
                spans.push(Span::styled(
                    word.to_string(),
                    Style::default()
                        .fg(Palette::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(word.to_string(), theme.ink(Palette::INK_1)));
            }
            continue;
        }

        // Number.
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit() || bytes[i] == b'.' || bytes[i] == b'_')
            {
                i += 1;
            }
            let number = line[start..i].to_string();
            spans.push(Span::styled(
                number,
                Style::default().fg(Color::Indexed(179)),
            ));
            continue;
        }

        // Everything else — plain ink.
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_alphanumeric()
            && bytes[i] != b'_'
            && bytes[i] != b'"'
            && bytes[i] != b'\''
            && !(bytes[i] == b'#'
                && matches!(
                    lang_l.as_str(),
                    "python" | "bash" | "sh" | "shell" | "zsh" | "toml"
                ))
            && !(bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/')
        {
            i += 1;
        }
        let chunk = &line[start..i];
        spans.push(Span::styled(chunk.to_string(), theme.ink(Palette::INK_1)));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_prose_renders_as_single_line() {
        let theme = Theme { unicode: true };
        let lines = render("hello world", &theme);
        assert!(!lines.is_empty());
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn fenced_code_block_produces_bar_prefixed_lines() {
        let theme = Theme { unicode: true };
        let md = "prose\n\n```rust\nfn foo() {}\n```\nafter";
        let lines = render(md, &theme);
        // expect: prose, blank, lang header, code line, blank, after
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("rust"))));
        // The code line is tokenised by tint_code, so `fn`, `foo`, `()`
        // land in distinct spans. We verify by reconstructing the line
        // text and matching the full identifier.
        let has_code_line = lines.iter().any(|l| {
            let joined: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            joined.contains("fn foo()")
        });
        assert!(
            has_code_line,
            "code line with `fn foo()` should be rendered"
        );
    }

    #[test]
    fn bold_and_italic_apply_modifiers() {
        let theme = Theme { unicode: true };
        let lines = render("**bold** *italic*", &theme);
        let spans: Vec<&Span> = lines.iter().flat_map(|l| l.spans.iter()).collect();
        assert!(spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::ITALIC)));
    }

    #[test]
    fn inline_code_has_accent_background() {
        let theme = Theme { unicode: true };
        let lines = render("call `foo()` now", &theme);
        let has_code_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains("foo()") && s.style.bg.is_some());
        assert!(has_code_span);
    }

    #[test]
    fn heading_is_bolded() {
        let theme = Theme { unicode: true };
        let lines = render("# Title", &theme);
        let has_bold = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains("Title") && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold);
    }

    #[test]
    fn bulleted_list_emits_bullet_glyph() {
        let theme = Theme { unicode: true };
        let lines = render("- one\n- two", &theme);
        let has_bullet = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains("•"));
        assert!(has_bullet);
    }

    #[test]
    fn rust_keyword_gets_accent_color() {
        let spans = tint_code("fn foo() {}", "rust", &Theme { unicode: true });
        let has_accent_fn = spans
            .iter()
            .any(|s| s.content == "fn" && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_accent_fn);
    }

    #[test]
    fn string_literal_tinted_in_rust_code() {
        let spans = tint_code(r#"let s = "hello";"#, "rust", &Theme { unicode: true });
        let has_string = spans
            .iter()
            .any(|s| s.content.contains("\"hello\"") && s.style.fg.is_some());
        assert!(has_string);
    }

    #[test]
    fn gfm_table_renders_header_separator_and_rows() {
        let theme = Theme { unicode: true };
        let md = "\
| tool         | class       |
|--------------|-------------|
| repo_search  | observe     |
| fs_write     | apply_local |
";
        let lines = render(md, &theme);
        // Header cell text is present.
        let has_header = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("tool")));
        assert!(has_header, "expected header row with 'tool'");

        // Body cell text is present.
        let has_body = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("repo_search")));
        assert!(has_body, "expected body row with 'repo_search'");

        // Separator hairline uses '─' and spans multiple chars.
        let has_hairline = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("──")));
        assert!(has_hairline, "expected hairline separator under header");
    }

    #[test]
    fn table_cells_get_padded_to_column_width() {
        assert_eq!(pad_to("hi", 5), "hi   ");
        assert_eq!(pad_to("exactly", 7), "exactly");
        let truncated = pad_to("toolongforthecolumn", 8);
        assert_eq!(truncated.chars().count(), 8);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn strikethrough_renders_as_crossed_out_modifier() {
        let theme = Theme { unicode: true };
        let lines = render("~~gone~~", &theme);
        let has_strike = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.content.contains("gone") && s.style.add_modifier.contains(Modifier::CROSSED_OUT)
        });
        assert!(
            has_strike,
            "~~text~~ should render with CROSSED_OUT modifier"
        );
    }

    #[test]
    fn empty_table_does_not_panic() {
        let theme = Theme { unicode: true };
        // A table with only a header, no body.
        let lines = render("| a | b |\n|---|---|", &theme);
        // Should produce at least the header + hairline, no panic.
        assert!(lines.len() >= 2);
    }
}
