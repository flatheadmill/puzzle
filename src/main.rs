// Puzzle: a TUI for reading Claude Code session transcripts with thinking
// blocks visible. The core conviction: claude --print does the heavy lifting,
// Puzzle is just a different renderer for what it produces.
//
// Two modes of operation: viewer (tails an existing JSONL transcript) and REPL
// (orchestrates claude --print via the Invocation abstraction in claude.rs and
// tails the session file it writes to). The event loop is tokio::select! across
// four sources: a frame timer at 33ms for screen refresh, the tailer channel
// for new conversation entries, the claude stdout event channel for process
// lifecycle and drain tracking, and crossterm's EventStream for keyboard input.
//
// The REPL spawns one Invocation per prompt cycle. The Invocation holds a child
// process with --input-format stream-json, writes NDJSON to stdin, reads events
// from stdout. When the round completes (result event, all messages drained),
// the Invocation shuts down and a new one is spawned for the next prompt. The
// tailer is independent — it watches the JSONL file on disk, not the process.
// This means the tailer survives across child process boundaries.
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
mod claude;
mod config;
mod model;
mod sessions;
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
use tokio::sync::mpsc;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::app::{App, Mode, RunState};
use crate::claude::{EasementEvent, EventReceiver, Invocation, Payload};
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

/// Receives the next event from Easement, or pends forever if no invocation
/// is active. Used as a select! arm that effectively disables itself when
/// there is no child process running.
async fn recv_easement(rx: &mut Option<EventReceiver>) -> Option<EasementEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_tracing();
    color_eyre::install()?;

    let args: Vec<String> = std::env::args().collect();
    let check_mode = args.iter().any(|a| a == "--check");

    // First positional arg is either a .jsonl path (viewer) or a slug (REPL).
    let positional = args.iter().skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned();

    let positional = match positional {
        Some(p) => p,
        None => bail!("usage: puzzle <slug>\n       puzzle <path-to-session.jsonl>"),
    };

    if check_mode {
        let path = PathBuf::from(&positional);
        if !path.exists() {
            bail!("file not found: {}", path.display());
        }
        return check_parse(&path);
    }

    let home = std::env::var("HOME")
        .map_err(|e| color_eyre::eyre::eyre!("HOME not set: {}", e))?;

    let repl_mode = !positional.ends_with(".jsonl");
    let mut session_id: Option<String> = None;
    let mut invocation: Option<Invocation> = None;
    let mut claude_rx: Option<EventReceiver> = None;
    let slug: Option<String>;
    let puzzle_config_dir: PathBuf;
    let viewer_path: Option<PathBuf>;

    if repl_mode {
        let s = positional.clone();
        slug = Some(s.clone());
        viewer_path = None;

        // Ensure the pane directory exists.
        let pane_dir = PathBuf::from(&home).join("pane").join(&s);
        std::fs::create_dir_all(&pane_dir)?;

        // Ensure trust so the CLI skips the approval dialog.
        let config_path = PathBuf::from(&home).join(".claude.json");
        let pane_dir_str = pane_dir.to_str()
            .ok_or_else(|| color_eyre::eyre::eyre!("pane dir is not valid UTF-8"))?;
        config::modify_config(&config_path, |c| config::ensure_trust(c, pane_dir_str))
            .map_err(|e| color_eyre::eyre::eyre!("{}", e))?;

        puzzle_config_dir = PathBuf::from(&home).join(".config").join("puzzle");

        // Set working directory to the pane dir.
        std::env::set_current_dir(&pane_dir)?;

        // Look up the latest session. If none exists, bootstrap is
        // deferred to the first prompt because Easement requires a
        // kickoff message in the payload.
        if let Some(sid) = sessions::latest_session(&puzzle_config_dir, &s) {
            sessions::record_session(&puzzle_config_dir, &s, &sid)?;
            session_id = Some(sid);
        }
    } else {
        slug = None;
        puzzle_config_dir = PathBuf::from(&home).join(".config").join("puzzle");

        // Viewer mode — tail an existing .jsonl file.
        let path = PathBuf::from(&positional);
        if !path.exists() {
            bail!("file not found: {}", path.display());
        }
        viewer_path = Some(path);
    }

    // In REPL mode, transcript entries arrive through Easement's envelope
    // stream — Puzzle does not touch Claude's filesystem. The tailer is
    // only used in viewer mode to tail an existing JSONL file.
    let (tx, mut rx) = mpsc::channel(256);

    if let Some(path) = viewer_path {
        let tailer_tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = run_tailer(path, tailer_tx).await {
                tracing::error!("tailer error: {}", e);
            }
        });
    }

    let mut terminal = ratatui::init();
    let mut app = App::new(repl_mode);
    let mut events = EventStream::new();
    let mut frame_interval = tokio::time::interval(Duration::from_millis(33));

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
            event = recv_easement(&mut claude_rx) => {
                match event {
                    Some(EasementEvent::Stdout(stdout_event)) => {
                        if let Some(ref mut inv) = invocation {
                            let drained = inv.handle_event(&stdout_event);

                            // On first session ID capture from a bootstrap,
                            // record the session.
                            if session_id.is_none() {
                                if let Some(sid) = inv.session_id() {
                                    if let Some(ref s) = slug {
                                        let _ = sessions::record_session(&puzzle_config_dir, s, sid);
                                    }
                                    session_id = Some(sid.to_string());
                                }
                            }

                            if drained {
                                if let Some(sid) = inv.session_id() {
                                    session_id = Some(sid.to_string());
                                }
                                let inv = invocation.take().unwrap();
                                let _ = inv.shutdown().await;
                                claude_rx = None;
                                app.run_state = RunState::Idle;
                                app.mode = Mode::Input;
                                app.follow = true;
                            }
                        }
                    }
                    Some(EasementEvent::Transcript(data)) => {
                        // Transcript entries from Easement feed the UI
                        // through the same parse pipeline as the tailer.
                        let json = serde_json::to_string(&data).unwrap_or_default();
                        if let Some(entry) = parse_line(&json) {
                            if let Some(ce) = try_convert(entry) {
                                app.push_entry(ce);
                            }
                        }
                    }
                    None => {
                        if let Some(inv) = invocation.take() {
                            if let Some(sid) = inv.session_id() {
                                session_id = Some(sid.to_string());
                            }
                            let _ = inv.shutdown().await;
                        }
                        claude_rx = None;
                        app.run_state = RunState::Idle;
                        app.mode = Mode::Input;
                        app.follow = true;
                    }
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
                                if let Some(ref s) = slug {
                                    // Build the Easement payload. If we have
                                    // a session ID, resume it. Otherwise,
                                    // bootstrap with an empty transcript.
                                    let payload = Payload {
                                        slug: s.clone(),
                                        yolo: false,
                                        message: prompt,
                                        session_id: session_id.clone(),
                                        transcript: if session_id.is_none() {
                                            Some(vec![])
                                        } else {
                                            None
                                        },
                                    };

                                    match Invocation::spawn(payload).await {
                                        Ok((inv, erx)) => {
                                            invocation = Some(inv);
                                            claude_rx = Some(erx);
                                        }
                                        Err(e) => {
                                            tracing::error!("failed to spawn easement: {}", e);
                                            continue;
                                        }
                                    }
                                }

                                app.run_state = RunState::Running;
                                app.follow = true;
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
