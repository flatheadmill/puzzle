mod app;
mod model;
mod parser;
mod render;
mod tailer;

use std::path::PathBuf;
use std::time::Duration;

use color_eyre::eyre::{bail, Result};
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Text;
use ratatui::widgets::{
    Block, Borders, List, ListItem, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use tokio::sync::mpsc;

use crate::app::App;
use crate::model::{try_convert, ContentBlock, EntryKind};
use crate::parser::parse_line;
use crate::render::render_entry;
use crate::tailer::run_tailer;

fn check_parse(path: &PathBuf) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let mut total = 0;
    let mut parsed = 0;
    let mut accepted = 0;
    let mut thinking = 0;
    let mut text = 0;
    let mut tool_use = 0;
    let mut tool_result = 0;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        total += 1;
        if let Some(entry) = parse_line(line) {
            parsed += 1;
            if let Some(ce) = try_convert(entry) {
                accepted += 1;
                for block in &ce.blocks {
                    match block {
                        ContentBlock::Thinking { .. } => thinking += 1,
                        ContentBlock::Text { .. } => text += 1,
                        ContentBlock::ToolUse { .. } => tool_use += 1,
                        ContentBlock::ToolResult { .. } => tool_result += 1,
                    }
                }
                let kind = match ce.kind {
                    EntryKind::User => "user",
                    EntryKind::Assistant => "assistant",
                };
                println!("  {} ({} blocks)", kind, ce.blocks.len());
            }
        }
    }

    println!();
    println!("total lines:   {}", total);
    println!("parsed:        {}", parsed);
    println!("accepted:      {}", accepted);
    println!("  thinking:    {}", thinking);
    println!("  text:        {}", text);
    println!("  tool_use:    {}", tool_use);
    println!("  tool_result: {}", tool_result);
    println!("dropped:       {}", parsed - accepted);
    println!("parse errors:  {}", total - parsed);

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        bail!("usage: puzzle <path-to-session.jsonl>");
    }

    let check_mode = args.iter().any(|a| a == "--check");
    let path = PathBuf::from(args.iter().find(|a| *a != "--check" && *a != &args[0]).unwrap());
    if !path.exists() {
        bail!("file not found: {}", path.display());
    }

    if check_mode {
        return check_parse(&path);
    }

    let (tx, mut rx) = mpsc::channel(256);

    tokio::spawn(async move {
        if let Err(e) = run_tailer(path, tx).await {
            eprintln!("tailer error: {}", e);
        }
    });

    let mut terminal = ratatui::init();
    let mut app = App::new();
    let mut events = EventStream::new();
    let mut frame_interval = tokio::time::interval(Duration::from_millis(33));

    loop {
        tokio::select! {
            _ = frame_interval.tick() => {
                terminal.draw(|frame| {
                    let area = frame.area();

                    let chunks = Layout::horizontal([
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ]).split(area);

                    let items: Vec<ListItem> = app
                        .entries
                        .iter()
                        .map(|entry| {
                            let lines = render_entry(entry);
                            ListItem::new(Text::from(lines))
                        })
                        .collect();

                    let list = List::new(items)
                        .block(Block::default().borders(Borders::NONE));

                    frame.render_stateful_widget(list, chunks[0], &mut app.list_state);

                    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
                    let mut scrollbar_state = ScrollbarState::new(app.entries.len())
                        .position(app.list_state.selected().unwrap_or(0));
                    frame.render_stateful_widget(scrollbar, chunks[1], &mut scrollbar_state);
                })?;
            }
            Some(entry) = rx.recv() => {
                app.push_entry(entry);
            }
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        app.handle_key(key);
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    ratatui::restore();
    Ok(())
}
