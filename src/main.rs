// Puzzle: a TUI for reading Claude Code session transcripts with thinking
// blocks visible. The core conviction: claude --print does the heavy lifting,
// Puzzle is just a different renderer for what it produces.
//
// Two modes of operation: viewer (tails an existing JSONL transcript) and REPL
// (orchestrates claude --print and tails the session file it writes to). The
// event loop is tokio::select! across four sources: a frame timer at 33ms for
// screen refresh, the tailer channel for new conversation entries, a completion
// channel for claude --print process exit, and crossterm's EventStream for
// keyboard input.
//
// The rendering pipeline: File Tailer (100ms polling) → Parser (serde, kebab-
// case tag enum) → Model (filter user/assistant on main chain, summarize tool
// input) → Renderer (styled Lines per content block) → App (scroll state,
// follow mode, input) → Terminal (ratatui List widget with Scrollbar). The
// input area is separate from this pipeline — it is a tui-textarea widget that
// renders itself into its own Rect and manages its own cursor.
//
// Logging goes to /tmp/puzzle.log via tracing with a non-blocking file writer.
// RUST_LOG controls the filter; defaults to puzzle=debug. The subscriber is
// initialized before the TUI so early errors are captured.

mod app;
mod model;
mod parser;
mod render;
mod tailer;

use std::path::PathBuf;
use std::time::Duration;

use color_eyre::eyre::{bail, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::{
    Block, Borders, List, ListItem, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

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

// Initialize tracing with a non-blocking file writer so log output goes to
// /tmp/puzzle.log instead of fighting the TUI on stdout/stderr. The guard
// must be held for the program's lifetime to ensure the writer flushes.
fn init_tracing() -> WorkerGuard {
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/puzzle.log")
        .expect("failed to open /tmp/puzzle.log");

    let (non_blocking, guard) = tracing_appender::non_blocking(log_file);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("puzzle=debug"));

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .init();

    guard
}

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_tracing();
    color_eyre::install()?;

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
        Some(session_jsonl_path(sid)?)
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
    let (tx, mut rx) = mpsc::channel(256);
    let tailer_tx = tx.clone();
    tokio::spawn(async move {
        if let Err(e) = run_tailer(tailer_path, tailer_tx).await {
            tracing::error!("tailer error: {}", e);
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

                    // prompt input area — tui-textarea renders itself as a
                    // widget and manages its own cursor. The block and style
                    // are set dynamically each frame based on mode and run state.
                    if let Some(input_rect) = input_area {
                        let title = match (&app.mode, &app.run_state) {
                            (_, RunState::Running) => " running... ",
                            (Mode::Input, RunState::Idle) => " prompt (esc: scroll) ",
                            (Mode::Scroll, RunState::Idle) => " scroll (i: input) ",
                        };

                        let border_style = if app.mode == Mode::Input && app.run_state == RunState::Idle {
                            Style::default().fg(Color::Cyan)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        };

                        let text_style = match (&app.mode, &app.run_state) {
                            (_, RunState::Running) => Style::default().fg(Color::DarkGray),
                            (Mode::Input, RunState::Idle) => Style::default().fg(Color::White),
                            (Mode::Scroll, RunState::Idle) => Style::default().fg(Color::DarkGray),
                        };

                        let input_block = Block::default()
                            .borders(Borders::ALL)
                            .title(title)
                            .border_style(border_style);

                        app.textarea.set_block(input_block);
                        app.textarea.set_style(text_style);

                        frame.render_widget(&app.textarea, input_rect);
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
                if let Err(e) = result {
                    tracing::error!("claude --print error: {}", e);
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

// Spawns claude --print as a child process. The prompt is a positional argument.
// stdout and stdin are nulled — we don't read the stream output, we tail the
// transcript file that claude --print writes to. stderr is piped for error
// reporting. The --verbose flag is required alongside --output-format stream-json
// when using --print (discovered during REPL first contact — without it, the
// stream-json flag is silently ignored).
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
