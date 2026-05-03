//! Renders markdown text into ratatui `Line`s.
//!
//! Supports inline formatting (bold, italic, inline code, links), headings,
//! bullet/numbered lists, fenced code blocks (with language-aware syntax
//! highlighting via `syntect`), and a simple aligned grid for tables.
//!
//! The output is plain `Line`s of styled `Span`s — no widget state — so it
//! plugs straight into the existing `Paragraph::new(lines)` flow.

use std::sync::OnceLock;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SynStyle, Theme, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};

const COLOR_HEADING: Color = Color::Rgb(166, 182, 255);
const COLOR_CODE_FG: Color = Color::Rgb(244, 228, 188);
const COLOR_CODE_BG: Color = Color::Rgb(40, 44, 56);
const COLOR_LINK: Color = Color::Rgb(132, 204, 232);
const COLOR_BLOCKQUOTE: Color = Color::Rgb(170, 178, 196);
const COLOR_TABLE_BORDER: Color = Color::Rgb(82, 90, 110);

/// Theme used for code-block syntax highlighting. `base16-ocean.dark` reads
/// well on the dark TUI background and ships with syntect by default.
const HIGHLIGHT_THEME: &str = "base16-ocean.dark";

struct HighlightAssets {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn highlight_assets() -> &'static HighlightAssets {
    static ASSETS: OnceLock<HighlightAssets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get(HIGHLIGHT_THEME)
            .cloned()
            .unwrap_or_else(|| themes.themes.values().next().cloned().unwrap_or_default());
        HighlightAssets { syntaxes, theme }
    })
}

fn syntax_for_lang<'a>(syntaxes: &'a SyntaxSet, lang: Option<&str>) -> Option<&'a SyntaxReference> {
    let token = lang?.trim();
    if token.is_empty() {
        return None;
    }
    // Try by token (e.g. "rust"), then file extension (".rs").
    syntaxes
        .find_syntax_by_token(token)
        .or_else(|| syntaxes.find_syntax_by_extension(token))
}

fn synstyle_to_ratatui(style: SynStyle) -> Style {
    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
    let mut out = Style::default().fg(fg).bg(COLOR_CODE_BG);
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Render a markdown source string into styled lines.
///
/// `base_style` is applied to inline text (so the assistant body keeps its
/// green tint while headings/code/links override their own foreground).
pub fn render(source: &str, base_style: Style) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(source, options);
    let mut renderer = Renderer::new(base_style);
    for event in parser {
        renderer.handle(event);
    }
    renderer.finish()
}

struct Renderer {
    base_style: Style,
    lines: Vec<Line<'static>>,
    /// Spans currently being collected for the in-progress line.
    current: Vec<Span<'static>>,
    /// Active inline modifiers (bold/italic/strike).
    modifier: Modifier,
    /// Are we inside an inline code span?
    in_code_span: bool,
    /// Are we inside a fenced code block?
    in_code_block: bool,
    /// Language hint for the active code block (from a fenced opener).
    code_lang: Option<String>,
    /// Buffered code-block source — flushed to syntect on close so the
    /// highlighter sees a complete (line-terminated) input.
    code_buffer: String,
    /// Are we inside a link? Active link target if so.
    link_target: Option<String>,
    /// List nesting state. `Some(Some(n))` = numbered list at index n;
    /// `Some(None)` = bullet list.
    list_stack: Vec<Option<u64>>,
    /// Are we inside a heading? Level if so.
    heading_level: Option<HeadingLevel>,
    /// Are we inside a blockquote?
    blockquote_depth: u32,
    /// In-progress table rows. Cells are flat span vectors.
    table: Option<TableState>,
}

struct TableState {
    rows: Vec<Vec<Vec<Span<'static>>>>,
    current_row: Vec<Vec<Span<'static>>>,
    current_cell: Vec<Span<'static>>,
    in_cell: bool,
}

impl Renderer {
    fn new(base_style: Style) -> Self {
        Self {
            base_style,
            lines: Vec::new(),
            current: Vec::new(),
            modifier: Modifier::empty(),
            in_code_span: false,
            in_code_block: false,
            code_lang: None,
            code_buffer: String::new(),
            link_target: None,
            list_stack: Vec::new(),
            heading_level: None,
            blockquote_depth: 0,
            table: None,
        }
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.push_text(text.as_ref()),
            Event::Code(code) => self.push_inline_code(code.as_ref()),
            Event::Html(_) | Event::InlineHtml(_) => {
                // Render raw HTML literally — terminals can't interpret it.
            }
            Event::FootnoteReference(_) => {}
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(COLOR_TABLE_BORDER),
                )));
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.current.push(Span::styled(marker, self.base_style));
            }
            Event::DisplayMath(_) | Event::InlineMath(_) => {
                // Pass through without typesetting — we have no math renderer.
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.blockquote_depth > 0 {
                    self.start_blockquote_line();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.heading_level = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                self.code_buffer.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(s) => {
                        let trimmed = s.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    }
                    CodeBlockKind::Indented => None,
                };
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.flush_line();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let s = format!("{n}. ");
                        *n = n.saturating_add(1);
                        s
                    }
                    Some(None) => "• ".to_string(),
                    None => "• ".to_string(),
                };
                self.current.push(Span::styled(
                    format!("{indent}{marker}"),
                    self.base_style,
                ));
            }
            Tag::Emphasis => self.modifier.insert(Modifier::ITALIC),
            Tag::Strong => self.modifier.insert(Modifier::BOLD),
            Tag::Strikethrough => self.modifier.insert(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } => {
                self.link_target = Some(dest_url.into_string());
            }
            Tag::Image { .. } => {
                // Fall back to alt-text rendering.
            }
            Tag::Table(_) => {
                self.flush_line();
                self.table = Some(TableState {
                    rows: Vec::new(),
                    current_row: Vec::new(),
                    current_cell: Vec::new(),
                    in_cell: false,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.current_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.current_cell.clear();
                    t.in_cell = true;
                }
            }
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::HtmlBlock
            | Tag::MetadataBlock(_)
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.lines.push(Line::from(""));
            }
            TagEnd::Heading(_) => {
                self.flush_line();
                self.heading_level = None;
                self.lines.push(Line::from(""));
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                let buffered = std::mem::take(&mut self.code_buffer);
                let lang = self.code_lang.take();
                self.in_code_block = false;
                self.emit_highlighted_code_block(&buffered, lang.as_deref());
                self.lines.push(Line::from(""));
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.lines.push(Line::from(""));
                }
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::Emphasis => self.modifier.remove(Modifier::ITALIC),
            TagEnd::Strong => self.modifier.remove(Modifier::BOLD),
            TagEnd::Strikethrough => self.modifier.remove(Modifier::CROSSED_OUT),
            TagEnd::Link => self.link_target = None,
            TagEnd::Image => {}
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.render_table(t);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.current_row);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    let cell = std::mem::take(&mut t.current_cell);
                    t.current_row.push(cell);
                    t.in_cell = false;
                }
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_)
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.in_code_block {
            // Buffer the entire block; we run syntect once on close so the
            // highlighter sees the full source and stays in sync across lines.
            self.code_buffer.push_str(text);
            return;
        }

        let style = self.inline_style();
        for (idx, segment) in text.split('\n').enumerate() {
            if idx > 0 {
                self.flush_line();
            }
            if !segment.is_empty() {
                self.push_to_current(segment.to_string(), style);
            }
        }
    }

    fn push_inline_code(&mut self, code: &str) {
        let style = Style::default().fg(COLOR_CODE_FG).bg(COLOR_CODE_BG);
        self.in_code_span = true;
        self.current
            .push(Span::styled(format!("`{code}`"), style));
        self.in_code_span = false;
    }

    fn push_to_current(&mut self, text: String, style: Style) {
        if let Some(target) = &self.link_target {
            // For terminals, append the URL inline so it's still useful.
            self.current.push(Span::styled(text, style));
            self.current.push(Span::styled(
                format!(" ({target})"),
                Style::default().fg(COLOR_LINK).add_modifier(Modifier::DIM),
            ));
        } else if self.table.as_ref().is_some_and(|t| t.in_cell) {
            // Cell text is collected into the current cell, not the line buffer.
            if let Some(t) = self.table.as_mut() {
                t.current_cell.push(Span::styled(text, style));
            }
        } else {
            self.current.push(Span::styled(text, style));
        }
    }

    fn inline_style(&self) -> Style {
        let mut style = self.base_style;
        if let Some(level) = self.heading_level {
            let extra = match level {
                HeadingLevel::H1 => Modifier::BOLD | Modifier::UNDERLINED,
                _ => Modifier::BOLD,
            };
            style = style.fg(COLOR_HEADING).add_modifier(extra);
        }
        if self.blockquote_depth > 0 {
            style = style
                .fg(COLOR_BLOCKQUOTE)
                .add_modifier(Modifier::ITALIC);
        }
        if self.link_target.is_some() {
            style = style
                .fg(COLOR_LINK)
                .add_modifier(Modifier::UNDERLINED);
        }
        if !self.modifier.is_empty() {
            style = style.add_modifier(self.modifier);
        }
        style
    }

    fn start_blockquote_line(&mut self) {
        self.current.push(Span::styled(
            "│ ",
            Style::default().fg(COLOR_BLOCKQUOTE),
        ));
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        // Drop trailing empty spacer if present.
        while self
            .lines
            .last()
            .is_some_and(|l| l.spans.is_empty() || l.spans.iter().all(|s| s.content.is_empty()))
        {
            self.lines.pop();
        }
        self.lines
    }

    fn emit_highlighted_code_block(&mut self, source: &str, lang: Option<&str>) {
        if source.is_empty() {
            return;
        }
        let assets = highlight_assets();
        let syntax = syntax_for_lang(&assets.syntaxes, lang);
        match syntax {
            Some(syn) => {
                let mut highlighter = HighlightLines::new(syn, &assets.theme);
                for raw_line in LinesWithEndings::from(source) {
                    let regions = highlighter
                        .highlight_line(raw_line, &assets.syntaxes)
                        .unwrap_or_default();
                    let trimmed_line = raw_line.trim_end_matches('\n');
                    let mut spans: Vec<Span<'static>> = Vec::with_capacity(regions.len() + 2);
                    // Leading gutter so the bg covers a small left margin.
                    spans.push(Span::styled(
                        " ",
                        Style::default().bg(COLOR_CODE_BG),
                    ));
                    if regions.is_empty() {
                        spans.push(Span::styled(
                            trimmed_line.to_string(),
                            Style::default().fg(COLOR_CODE_FG).bg(COLOR_CODE_BG),
                        ));
                    } else {
                        for (style, segment) in regions {
                            // Strip the trailing newline that LinesWithEndings keeps —
                            // ratatui adds its own line break between Line entries.
                            let text = segment.trim_end_matches('\n').to_string();
                            if text.is_empty() {
                                continue;
                            }
                            spans.push(Span::styled(text, synstyle_to_ratatui(style)));
                        }
                    }
                    spans.push(Span::styled(
                        " ",
                        Style::default().bg(COLOR_CODE_BG),
                    ));
                    self.lines.push(Line::from(spans));
                }
            }
            None => {
                // Unknown language — fall back to the muted plain code style.
                let style = Style::default().fg(COLOR_CODE_FG).bg(COLOR_CODE_BG);
                for raw_line in source.split('\n') {
                    self.lines.push(Line::from(Span::styled(
                        format!(" {raw_line} "),
                        style,
                    )));
                }
                // Drop the trailing empty line caused by the source ending in '\n'.
                if source.ends_with('\n')
                    && self
                        .lines
                        .last()
                        .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
                {
                    self.lines.pop();
                }
            }
        }
    }

    fn render_table(&mut self, t: TableState) {
        if t.rows.is_empty() {
            return;
        }
        let column_count = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if column_count == 0 {
            return;
        }
        let mut widths = vec![0usize; column_count];
        for row in &t.rows {
            for (idx, cell) in row.iter().enumerate() {
                let w: usize = cell.iter().map(|s| s.content.chars().count()).sum();
                if idx < widths.len() && w > widths[idx] {
                    widths[idx] = w;
                }
            }
        }

        let border = Style::default().fg(COLOR_TABLE_BORDER);
        for (row_idx, row) in t.rows.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (col_idx, cell) in row.iter().enumerate() {
                if col_idx > 0 {
                    spans.push(Span::styled(" │ ", border));
                }
                let used: usize = cell.iter().map(|s| s.content.chars().count()).sum();
                for span in cell {
                    spans.push(span.clone());
                }
                if col_idx < widths.len() && used < widths[col_idx] {
                    spans.push(Span::styled(
                        " ".repeat(widths[col_idx] - used),
                        self.base_style,
                    ));
                }
            }
            self.lines.push(Line::from(spans));
            // Header separator after first row.
            if row_idx == 0 {
                let total_width: usize =
                    widths.iter().sum::<usize>() + 3 * widths.len().saturating_sub(1);
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(total_width),
                    border,
                )));
            }
        }
        self.lines.push(Line::from(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_test(src: &str) -> Vec<Line<'static>> {
        render(src, Style::default())
    }

    fn flatten(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn plain_text_renders_unchanged_text() {
        let lines = render_test("hello world");
        let flat = flatten(&lines);
        assert!(flat.contains("hello world"));
    }

    #[test]
    fn bold_emits_bold_modifier() {
        let lines = render_test("this is **strong** text");
        let any_bold = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content == "strong" && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(any_bold, "expected a span with BOLD modifier on 'strong'");
    }

    #[test]
    fn italic_emits_italic_modifier() {
        let lines = render_test("this is *emphasized* text");
        let any_italic = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content == "emphasized" && s.style.add_modifier.contains(Modifier::ITALIC));
        assert!(any_italic);
    }

    #[test]
    fn inline_code_keeps_backticks() {
        let lines = render_test("a `foo()` call");
        let flat = flatten(&lines);
        assert!(flat.contains("`foo()`"));
    }

    #[test]
    fn fenced_code_block_renders_distinctly() {
        let src = "before\n\n```\nlet x = 1;\nlet y = 2;\n```\n\nafter";
        let lines = render_test(src);
        let flat = flatten(&lines);
        assert!(flat.contains("let x = 1;"));
        assert!(flat.contains("let y = 2;"));
        // Code-block spans should carry the code background color.
        let any_code_styled = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.style.bg == Some(COLOR_CODE_BG) && s.content.contains("let x = 1;")
        });
        assert!(any_code_styled, "expected a span with code bg on the code line");
    }

    #[test]
    fn bullet_list_emits_markers() {
        let lines = render_test("- alpha\n- beta\n- gamma");
        let flat = flatten(&lines);
        assert!(flat.contains("• alpha"));
        assert!(flat.contains("• beta"));
        assert!(flat.contains("• gamma"));
    }

    #[test]
    fn numbered_list_emits_indices() {
        let lines = render_test("1. first\n2. second");
        let flat = flatten(&lines);
        assert!(flat.contains("1. first"));
        assert!(flat.contains("2. second"));
    }

    #[test]
    fn heading_styles_with_bold() {
        let lines = render_test("# Heading text\n\nbody");
        let any_heading_bold = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.content == "Heading text" && s.style.add_modifier.contains(Modifier::BOLD)
        });
        assert!(any_heading_bold);
    }

    #[test]
    fn link_rendered_with_underline_and_url() {
        let lines = render_test("see [docs](https://example.com)");
        let flat = flatten(&lines);
        assert!(flat.contains("docs"));
        assert!(flat.contains("https://example.com"));
        let any_link = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.content == "docs" && s.style.add_modifier.contains(Modifier::UNDERLINED)
        });
        assert!(any_link);
    }

    #[test]
    fn table_renders_aligned_grid() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let lines = render_test(src);
        let flat = flatten(&lines);
        assert!(flat.contains("a"));
        assert!(flat.contains("│"));
        assert!(flat.contains("1"));
        assert!(flat.contains("2"));
    }

    #[test]
    fn empty_input_produces_no_lines() {
        let lines = render_test("");
        assert!(lines.is_empty());
    }

    // --- Syntax highlighting (#8) ---

    #[test]
    fn rust_code_block_tokens_get_distinct_colors() {
        let src = "```rust\nfn main() { let x = 1; }\n```\n";
        let lines = render_test(src);
        // Collect unique foreground colors on spans inside the code block.
        let fgs: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.bg == Some(COLOR_CODE_BG) && !s.content.trim().is_empty())
            .filter_map(|s| s.style.fg)
            .collect();
        // Highlighted rust should produce more than one distinct fg color
        // (keyword vs. identifier vs. literal vs. punctuation).
        assert!(
            fgs.len() > 1,
            "expected multiple foreground colors, got {fgs:?}"
        );
    }

    #[test]
    fn unknown_language_falls_back_to_plain_code_style() {
        let src = "```not-a-real-lang\nliteral content\n```\n";
        let lines = render_test(src);
        let plain_span = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.style.bg == Some(COLOR_CODE_BG) && s.content.contains("literal content")
        });
        assert!(plain_span);
    }

    #[test]
    fn unfenced_code_block_uses_plain_style() {
        // No language tag — fallback path.
        let src = "```\nplain block content\n```\n";
        let lines = render_test(src);
        let plain_span = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.style.bg == Some(COLOR_CODE_BG) && s.content.contains("plain block content")
        });
        assert!(plain_span);
    }

    #[test]
    fn syntax_for_lang_handles_aliases_and_extensions() {
        let assets = highlight_assets();
        // Common aliases that should resolve to a syntax.
        assert!(syntax_for_lang(&assets.syntaxes, Some("rust")).is_some());
        assert!(syntax_for_lang(&assets.syntaxes, Some("py")).is_some());
        // Empty / unknown returns None.
        assert!(syntax_for_lang(&assets.syntaxes, Some("")).is_none());
        assert!(syntax_for_lang(&assets.syntaxes, None).is_none());
    }
}
