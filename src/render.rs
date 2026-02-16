use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::model::{ContentBlock, ConversationEntry, EntryKind};

pub fn render_entry(entry: &ConversationEntry) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    match entry.kind {
        EntryKind::User => {
            let has_tool_results = entry
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }));

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
                render_thinking(&mut lines, text);
            }
            ContentBlock::Text { text } => {
                render_text(&mut lines, text, &entry.kind);
            }
            ContentBlock::ToolUse {
                name,
                input_summary,
            } => {
                render_tool_use(&mut lines, name, input_summary);
            }
            ContentBlock::ToolResult { content, is_error } => {
                render_tool_result(&mut lines, content, *is_error);
            }
        }
    }

    lines.push(Line::from(""));
    lines
}

fn render_thinking(lines: &mut Vec<Line<'static>>, text: &str) {
    let style = Style::default().fg(Color::DarkGray);
    let marker_style = Style::default().fg(Color::Cyan);

    lines.push(Line::from(vec![
        Span::styled("  \u{2502} ", marker_style),
        Span::styled("thinking", style.add_modifier(Modifier::ITALIC)),
    ]));

    for line in text.lines() {
        lines.push(Line::from(vec![
            Span::styled("  \u{2502} ", marker_style),
            Span::styled(line.to_string(), style),
        ]));
    }

    lines.push(Line::from(Span::styled("  \u{2502}", marker_style)));
}

fn render_text(lines: &mut Vec<Line<'static>>, text: &str, kind: &EntryKind) {
    let style = match kind {
        EntryKind::User => Style::default().fg(Color::White),
        EntryKind::Assistant => Style::default(),
    };

    for line in text.lines() {
        lines.push(Line::from(Span::styled(line.to_string(), style)));
    }
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

fn render_tool_result(lines: &mut Vec<Line<'static>>, content: &str, is_error: bool) {
    let style = if is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let result_lines: Vec<&str> = content.lines().collect();
    let total = result_lines.len();

    let label = if is_error { "error" } else { "result" };
    lines.push(Line::from(Span::styled(
        format!("  \u{25c0} {} ({} lines)", label, total),
        style,
    )));

    for line in result_lines.iter().take(8) {
        lines.push(Line::from(Span::styled(
            format!("    {}", line),
            style,
        )));
    }

    if total > 8 {
        lines.push(Line::from(Span::styled(
            format!("    ... ({} more lines)", total - 8),
            style,
        )));
    }
}
