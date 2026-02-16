mod app;
mod model;
mod parser;
mod render;
mod tailer;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use color_eyre::eyre::{bail, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::app::{App, Mode, RunState};
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

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{}/code/tmp/puzzle.log", std::env::var("HOME").unwrap()))?;
    let log_file = Mutex::new(log_file);
    macro_rules! log {
        ($($arg:tt)*) => {
            if let Ok(mut f) = log_file.lock() {
                let _ = writeln!(f, "{}", format!($($arg)*));
            }
        };
    }

    let args: Vec<String> = std::env::args().collect();

    let check_mode = args.iter().any(|a| a == "--check");
    let repl_mode = args.iter().any(|a| a == "--repl");

    // --resume <session-id> for REPL mode
    let session_id = args.iter().position(|a| a == "--resume")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let path_arg = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--") && session_id.as_ref() != Some(a));

    if repl_mode && session_id.is_none() {
        bail!("usage: puzzle --repl --resume <session-id>");
    }

    if path_arg.is_none() && !repl_mode {
        bail!("usage: puzzle <path-to-session.jsonl>\n       puzzle --repl --resume <session-id>");
    }

    // in REPL mode, derive the JSONL path from the session ID and cwd
    let path = if repl_mode {
        let sid = session_id.as_ref().unwrap();
        let p = session_jsonl_path(sid)?;
        log!("repl mode: session={}, jsonl={}", sid, p.display());
        Some(p)
    } else {
        path_arg.map(|p| PathBuf::from(p))
    };

    if let Some(ref p) = path {
        if !p.exists() && !repl_mode {
            bail!("file not found: {}", p.display());
        }
    }

    if check_mode {
        return check_parse(path.as_ref().unwrap());
    }

    // start the tailer — in REPL mode the file may not exist yet,
    // but the tailer polls until it appears
    let tailer_path = path.clone().unwrap();
    log!("starting tailer on {}", tailer_path.display());
    let (tx, mut rx) = mpsc::channel(256);
    let tailer_tx = tx.clone();
    tokio::spawn(async move {
        if let Err(e) = run_tailer(tailer_path, tailer_tx).await {
            eprintln!("tailer error: {}", e);
        }
    });

    let mut terminal = ratatui::init();
    let mut app = App::new(repl_mode);
    let mut events = EventStream::new();
    let mut frame_interval = tokio::time::interval(Duration::from_millis(33));

    // channel for claude --print process completion
    let (print_tx, mut print_rx) = mpsc::channel::<Result<(), String>>(1);

    loop {
        tokio::select! {
            _ = frame_interval.tick() => {
                terminal.draw(|frame| {
                    let area = frame.area();

                    let main_area;
                    let input_area;

                    if app.repl_mode {
                        let layout = Layout::vertical([
                            Constraint::Min(0),
                            Constraint::Length(3),
                        ]).split(area);
                        main_area = layout[0];
                        input_area = Some(layout[1]);
                    } else {
                        main_area = area;
                        input_area = None;
                    }

                    // conversation area with scrollbar
                    let conv_chunks = Layout::horizontal([
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ]).split(main_area);

                    let conv_width = conv_chunks[0].width;
                    let items: Vec<ListItem> = app
                        .entries
                        .iter()
                        .map(|entry| {
                            let lines = render_entry(entry, conv_width);
                            ListItem::new(Text::from(lines))
                        })
                        .collect();

                    let list = List::new(items)
                        .block(Block::default().borders(Borders::NONE));

                    frame.render_stateful_widget(list, conv_chunks[0], &mut app.list_state);

                    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
                    let mut scrollbar_state = ScrollbarState::new(app.entries.len())
                        .position(app.list_state.selected().unwrap_or(0));
                    frame.render_stateful_widget(scrollbar, conv_chunks[1], &mut scrollbar_state);

                    // prompt input area
                    if let Some(input_rect) = input_area {
                        let (title, style) = match (&app.mode, &app.run_state) {
                            (_, RunState::Running) => (
                                " running... ",
                                Style::default().fg(Color::DarkGray),
                            ),
                            (Mode::Input, RunState::Idle) => (
                                " prompt (esc: scroll) ",
                                Style::default().fg(Color::White),
                            ),
                            (Mode::Scroll, RunState::Idle) => (
                                " scroll (i: input) ",
                                Style::default().fg(Color::DarkGray),
                            ),
                        };

                        let input_block = Block::default()
                            .borders(Borders::ALL)
                            .title(title)
                            .border_style(if app.mode == Mode::Input && app.run_state == RunState::Idle {
                                Style::default().fg(Color::Cyan)
                            } else {
                                Style::default().fg(Color::DarkGray)
                            });

                        let input_text = Paragraph::new(Line::from(vec![
                            Span::styled(app.input.clone(), style),
                        ]))
                        .block(input_block);

                        frame.render_widget(input_text, input_rect);

                        // show cursor in input mode
                        if app.mode == Mode::Input && app.run_state == RunState::Idle {
                            frame.set_cursor_position((
                                input_rect.x + app.cursor_pos as u16 + 1,
                                input_rect.y + 1,
                            ));
                        }
                    }
                })?;
            }
            Some(entry) = rx.recv() => {
                app.push_entry(entry);
            }
            Some(result) = print_rx.recv() => {
                app.run_state = RunState::Idle;
                app.mode = Mode::Input;
                app.follow = true;
                match &result {
                    Ok(()) => log!("claude --print completed ok"),
                    Err(e) => log!("claude --print error: {}", e),
                }
            }
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        // check for Enter in input mode to submit
                        if app.mode == Mode::Input
                            && app.run_state == RunState::Idle
                            && key.code == KeyCode::Enter
                        {
                            if let Some(prompt) = app.submit_input() {
                                log!("submitting prompt: {}", &prompt);
                                app.run_state = RunState::Running;
                                app.follow = true;

                                let tx = print_tx.clone();
                                let sid = session_id.clone().unwrap();

                                tokio::spawn(async move {
                                    let result = run_claude_print(&prompt, &sid).await;
                                    let _ = tx.send(result).await;
                                });
                            }
                        } else {
                            app.handle_key(key);
                        }
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

async fn run_claude_print(prompt: &str, session_id: &str) -> Result<(), String> {
    let output = Command::new("claude")
        .arg("--print")
        .arg("--verbose")
        .arg("--max-thinking-tokens")
        .arg("31999")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--resume")
        .arg(session_id)
        .arg(prompt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("failed to spawn claude: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("claude --print exited with {}: {}", output.status, stderr));
    }

    Ok(())
}

/// Build the JSONL path for a session ID from the cwd.
/// ~/.claude/projects/<cwd-slug>/<session-id>.jsonl
fn session_jsonl_path(session_id: &str) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let home = std::env::var("HOME")
        .map_err(|e| color_eyre::eyre::eyre!("HOME not set: {}", e))?;

    let cwd_str = cwd.to_str()
        .ok_or_else(|| color_eyre::eyre::eyre!("cwd is not valid UTF-8"))?;
    let slug = cwd_str.replace('/', "-");

    Ok(PathBuf::from(&home)
        .join(".claude")
        .join("projects")
        .join(&slug)
        .join(format!("{}.jsonl", session_id)))
}
