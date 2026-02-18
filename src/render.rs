// Rendering layer. Each content block type has its own function that appends
// styled Lines to a Vec. The caller (main.rs) wraps these in ListItems for
// ratatui's List widget. The pattern is: render per block, collect, return.
//
// Thinking blocks get word-wrapped with textwrap because they arrive from the
// model as continuous text without newlines. Tool results collapse by default
// to a header line with the line count, expandable via Enter on the selected
// entry. Assistant text is parsed as markdown via pulldown-cmark and rendered
// with styled headings, emphasis, strong, inline code, code blocks, and lists
// with depth-based indentation and bullet/numbered markers. Long prose lines
// wrap at terminal width on word boundaries via style_wrap_with_indent, which
// preserves span styles across breaks and indents continuation lines for list
// items. Code block lines pass through unwrapped. The markdown walker
// (MdWriter) produces MarkedLine values — each a styled Line annotated with
// no_wrap and indent_level — that the wrapping layer consumes. User text
// stays as-is (white, no markdown parsing). The walker maintains an inline
// style stack so nested formatting composes correctly — bold inside italic
// gets both modifiers via ratatui's Style::patch. The pattern is modeled on
// steer's TextWriter but stripped down: no theme system, no syntax
// highlighting, no tables.

use ansi_to_tui::IntoText;
use pulldown_cmark::{Event, Parser, Tag};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::model::{ContentBlock, ConversationEntry, EntryKind};

// Markdown styles for assistant text. Simple first-pass palette — headings get
// color and bold, emphasis and strong use standard terminal modifiers, code uses
// a distinct color to stand out from surrounding prose.
const MD_HEADING: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const MD_EMPHASIS: Style = Style::new().add_modifier(Modifier::ITALIC);
const MD_STRONG: Style = Style::new().add_modifier(Modifier::BOLD);
const MD_CODE_INLINE: Style = Style::new().fg(Color::Green);
const MD_CODE_BLOCK: Style = Style::new().fg(Color::White);

// A styled Line annotated with wrapping metadata. The markdown walker produces
// these so the wrapping layer knows which lines to wrap and how to indent
// continuation lines. Code block lines carry no_wrap so they pass through at
// full width. List item lines carry indent_level equal to the marker width so
// wrapped continuations align with the text after the marker.
struct MarkedLine {
    line: Line<'static>,
    no_wrap: bool,
    indent_level: usize,
}

pub fn render_entry(entry: &ConversationEntry, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    match entry.kind {
        EntryKind::User => {
            let has_tool_results = entry
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }));

            // User entries that consist only of tool results suppress the
            // "Human" header. These entries follow a tool_use visually, and
            // adding a header between the tool call and its result breaks
            // the reading flow.
            if !has_tool_results {
                lines.push(Line::from(Span::styled(
                    "Human",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
            }
        }
        EntryKind::Assistant => {
            lines.push(Line::from(Span::styled(
                "Assistant",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
        }
    }

    for block in &entry.blocks {
        match block {
            ContentBlock::Thinking { text } => {
                render_thinking(&mut lines, text, width);
            }
            ContentBlock::Text { text } => {
                render_text(&mut lines, text, &entry.kind, width);
            }
            ContentBlock::ToolUse {
                name,
                input_summary,
            } => {
                render_tool_use(&mut lines, name, input_summary);
            }
            ContentBlock::ToolResult { content, is_error, collapsed } => {
                render_tool_result(&mut lines, content, *is_error, *collapsed);
            }
        }
    }

    lines.push(Line::from(""));
    lines
}

fn render_thinking(lines: &mut Vec<Line<'static>>, text: &str, width: u16) {
    let style = Style::default().fg(Color::DarkGray);
    let marker_style = Style::default().fg(Color::Cyan);

    lines.push(Line::from(vec![
        Span::styled("  \u{2502} ", marker_style),
        Span::styled("thinking", style.add_modifier(Modifier::ITALIC)),
    ]));

    // prefix "  │ " is 4 columns, wrap thinking text to fit
    let available = (width as usize).saturating_sub(4).max(1);

    for line in text.lines() {
        if line.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  \u{2502} ", marker_style),
            ]));
            continue;
        }
        for wrapped in textwrap::wrap(line, available) {
            lines.push(Line::from(vec![
                Span::styled("  \u{2502} ", marker_style),
                Span::styled(wrapped.into_owned(), style),
            ]));
        }
    }

    lines.push(Line::from(Span::styled("  \u{2502}", marker_style)));
}

fn render_text(lines: &mut Vec<Line<'static>>, text: &str, kind: &EntryKind, width: u16) {
    match kind {
        EntryKind::User => {
            let style = Style::default().fg(Color::White);
            for line in text.lines() {
                lines.push(Line::from(Span::styled(line.to_string(), style)));
            }
        }
        EntryKind::Assistant => {
            for ml in render_markdown(text) {
                if ml.no_wrap {
                    lines.push(ml.line);
                } else {
                    lines.extend(style_wrap_with_indent(ml.line, width, ml.indent_level));
                }
            }
        }
    }
}

// Walks pulldown-cmark events and produces MarkedLine values — styled ratatui
// Lines annotated with wrapping metadata. Maintains an inline style stack so
// nested styles compose (bold inside italic gets both via patch), a list index
// stack that drives marker rendering (None for bullet lists, Some(n) for
// numbered), and an item indent stack that tracks the marker width at each
// nesting level for continuation line indentation. Code blocks set a flag that
// changes text handling to preserve whitespace and marks output lines no_wrap.
// The needs_newline flag tracks paragraph separation. List markers are deferred
// until the first content event in each item so they land on the same line as
// the text.
struct MdWriter {
    lines: Vec<MarkedLine>,
    inline_styles: Vec<Style>,
    list_indices: Vec<Option<u64>>,
    item_indents: Vec<usize>,
    in_code_block: bool,
    in_list_item_start: bool,
    needs_newline: bool,
}

impl MdWriter {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            inline_styles: Vec::new(),
            list_indices: Vec::new(),
            item_indents: Vec::new(),
            in_code_block: false,
            in_list_item_start: false,
            needs_newline: false,
        }
    }

    fn push_line(&mut self, line: Line<'static>) {
        self.lines.push(MarkedLine {
            line,
            no_wrap: self.in_code_block,
            indent_level: self.item_indents.last().copied().unwrap_or(0),
        });
    }

    fn push_span(&mut self, span: Span<'static>) {
        if let Some(last) = self.lines.last_mut() {
            last.line.spans.push(span);
        } else {
            self.lines.push(MarkedLine {
                line: Line::from(vec![span]),
                no_wrap: self.in_code_block,
                indent_level: self.item_indents.last().copied().unwrap_or(0),
            });
        }
    }

    fn current_style(&self) -> Style {
        self.inline_styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, style: Style) {
        let composed = self.current_style().patch(style);
        self.inline_styles.push(composed);
    }

    fn pop_style(&mut self) {
        self.inline_styles.pop();
    }

    fn push_list_marker(&mut self) {
        if self.list_indices.is_empty() {
            return;
        }
        let depth = self.list_indices.len();
        let indent = depth.saturating_sub(1) * 4;
        let indent_str = " ".repeat(indent);

        if let Some(idx) = self.list_indices.last_mut() {
            let marker = match idx {
                None => format!("{}- ", indent_str),
                Some(n) => {
                    *n += 1;
                    format!("{}{}. ", indent_str, *n - 1)
                }
            };
            // Set the indent level for this item's continuation lines so
            // wrapped text aligns with the content after the marker.
            if let Some(item_indent) = self.item_indents.last_mut() {
                *item_indent = marker.len();
            }
            self.push_span(Span::raw(marker));
            // The line was created at Start(Item) before the marker width was
            // known. Update it now that the marker has been computed.
            if let Some(last) = self.lines.last_mut() {
                last.indent_level = self.item_indents.last().copied().unwrap_or(0);
            }
        }
    }

    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(text.as_ref()),
            Event::Code(code) => self.code(code.as_ref()),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => self.rule(),
            Event::Html(html) => self.text(html.as_ref()),
            Event::FootnoteReference(r) => self.text(r.as_ref()),
            Event::TaskListMarker(_) => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.needs_newline {
                    self.push_line(Line::default());
                }
                // Inside list items, Start(Item) already pushed the content
                // line. Adding another would leave an orphan blank line.
                if self.list_indices.is_empty() {
                    self.push_line(Line::default());
                }
                self.needs_newline = false;
            }
            Tag::Heading(level, _, _) => {
                if self.needs_newline {
                    self.push_line(Line::default());
                }
                self.push_style(MD_HEADING);
                let prefix = format!("{} ", "#".repeat(level as usize));
                self.push_line(Line::from(Span::styled(prefix, MD_HEADING)));
                self.needs_newline = false;
            }
            Tag::CodeBlock(_) => {
                if self.needs_newline {
                    self.push_line(Line::default());
                }
                self.in_code_block = true;
                self.needs_newline = false;
            }
            Tag::Emphasis => {
                if self.in_list_item_start {
                    self.push_list_marker();
                    self.in_list_item_start = false;
                }
                self.push_style(MD_EMPHASIS);
            }
            Tag::Strong => {
                if self.in_list_item_start {
                    self.push_list_marker();
                    self.in_list_item_start = false;
                }
                self.push_style(MD_STRONG);
            }
            Tag::BlockQuote => {
                if self.needs_newline {
                    self.push_line(Line::default());
                }
                self.needs_newline = false;
            }
            Tag::List(start_index) => {
                if self.list_indices.is_empty() && self.needs_newline {
                    self.push_line(Line::default());
                }
                self.list_indices.push(start_index);
                self.needs_newline = false;
            }
            Tag::Item => {
                self.item_indents.push(0);
                self.push_line(Line::default());
                self.in_list_item_start = true;
                self.needs_newline = false;
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::CodeBlock(_) | Tag::BlockQuote => {
                if matches!(tag, Tag::CodeBlock(_)) {
                    self.in_code_block = false;
                }
                self.needs_newline = true;
            }
            Tag::Heading(..) => {
                self.pop_style();
                self.needs_newline = true;
            }
            Tag::Emphasis | Tag::Strong => self.pop_style(),
            Tag::List(_) => {
                self.list_indices.pop();
                self.needs_newline = true;
            }
            Tag::Item => {
                if self.in_list_item_start {
                    self.push_list_marker();
                    self.in_list_item_start = false;
                }
                self.item_indents.pop();
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.in_list_item_start {
            self.push_list_marker();
            self.in_list_item_start = false;
        }
        if self.in_code_block {
            let text_lines: Vec<&str> = text.lines().collect();
            for (idx, line) in text_lines.iter().enumerate() {
                if idx > 0 || self.needs_newline {
                    self.push_line(Line::default());
                }
                self.push_span(Span::styled(line.to_string(), MD_CODE_BLOCK));
            }
            self.needs_newline = text.ends_with('\n') && !text_lines.is_empty();
        } else {
            if self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            let style = self.current_style();
            self.push_span(Span::styled(text.to_string(), style));
        }
    }

    fn code(&mut self, code: &str) {
        if self.in_list_item_start {
            self.push_list_marker();
            self.in_list_item_start = false;
        }
        self.push_span(Span::styled(code.to_string(), MD_CODE_INLINE));
    }

    fn soft_break(&mut self) {
        self.push_line(Line::default());
    }

    fn hard_break(&mut self) {
        self.push_line(Line::default());
    }

    fn rule(&mut self) {
        if self.needs_newline {
            self.push_line(Line::default());
        }
        self.push_line(Line::from(Span::styled(
            "───",
            Style::default().fg(Color::DarkGray),
        )));
        self.needs_newline = true;
    }
}

fn render_markdown(text: &str) -> Vec<MarkedLine> {
    let parser = Parser::new(text);
    let mut writer = MdWriter::new();
    for event in parser {
        writer.handle_event(event);
    }
    writer.lines
}

// Wraps a styled Line at word boundaries while preserving span styles across
// breaks. Continuation lines are indented by `indent` spaces so list item text
// aligns after the marker. The approach is from steer's style_wrap_with_indent:
// walk spans, split on whitespace via split_inclusive, track width with
// unicode-width. When a word would exceed the effective width, start a new line
// with the indent prepended.
fn style_wrap_with_indent(line: Line<'_>, max_width: u16, indent: usize) -> Vec<Line<'static>> {
    let max_width = max_width as usize;
    let mut output_lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;
    let mut is_first_line = true;

    for span in line.spans {
        let style = span.style;
        let content = span.content.as_ref();

        for word in content.split_inclusive(' ') {
            let word_width = word.width();

            let effective_max = if is_first_line {
                max_width
            } else {
                max_width.saturating_sub(indent)
            };

            if current_width > 0 && current_width + word_width > effective_max {
                if !current_spans.is_empty() {
                    output_lines.push(Line::from(current_spans));
                    current_spans = Vec::new();
                    current_width = 0;
                    is_first_line = false;

                    if indent > 0 {
                        current_spans.push(Span::raw(" ".repeat(indent)));
                        current_width = indent;
                    }
                }
            }

            current_spans.push(Span::styled(word.to_string(), style));
            current_width += word_width;
        }
    }

    if !current_spans.is_empty() {
        output_lines.push(Line::from(current_spans));
    }

    if output_lines.is_empty() {
        output_lines.push(Line::from(""));
    }

    output_lines
}

fn render_tool_use(lines: &mut Vec<Line<'static>>, name: &str, summary: &str) {
    let tool_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let summary_style = Style::default().fg(Color::Yellow);

    lines.push(Line::from(vec![
        Span::styled(format!("  \u{25b6} {}", name), tool_style),
    ]));

    if !summary.is_empty() {
        for line in summary.lines().take(5) {
            lines.push(Line::from(Span::styled(
                format!("    {}", line),
                summary_style,
            )));
        }
        let line_count = summary.lines().count();
        if line_count > 5 {
            lines.push(Line::from(Span::styled(
                format!("    ... ({} more lines)", line_count - 5),
                summary_style,
            )));
        }
    }
}

// Tool results collapse by default to a single header line showing the line
// count. Pressing Enter on the selected entry expands all tool results in it
// to show full content. The header uses the line count as context — the reader
// knows what they would get by expanding.
//
// When expanded, ANSI escape codes in tool output (colored grep results, styled
// command output) are parsed via ansi-to-tui into styled ratatui lines. If the
// content has no ANSI codes, the crate produces unstyled text, same as before.
// Error results keep the red style regardless of ANSI content.
fn render_tool_result(lines: &mut Vec<Line<'static>>, content: &str, is_error: bool, collapsed: bool) {
    let error_style = Style::default().fg(Color::Red);
    let header_style = if is_error {
        error_style
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let total = content.lines().count();

    let label = if is_error { "error" } else { "result" };
    lines.push(Line::from(Span::styled(
        format!("  \u{25c0} {} ({} lines)", label, total),
        header_style,
    )));

    if !collapsed {
        if is_error {
            for line in content.lines() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", line),
                    error_style,
                )));
            }
        } else if let Ok(text) = content.as_bytes().into_text() {
            for line in text.lines {
                let mut prefixed: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
                prefixed.push(Span::raw("    "));
                prefixed.extend(line.spans.into_iter().map(|s| {
                    Span::styled(s.content.into_owned(), s.style)
                }));
                lines.push(Line::from(prefixed));
            }
        } else {
            for line in content.lines() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", line),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }
}
