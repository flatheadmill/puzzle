// Puzzle: a TUI for Claude Code conversations. Two modes: viewer (tails an
// existing JSONL transcript) and REPL (orchestrates Claude through Easement).
//
// In REPL mode, Puzzle spawns Easement per spurt. Easement wraps the Claude
// CLI and multiplexes output into typed NDJSON envelopes. Puzzle reads one
// stream and routes by the stream field: stdout events drive the drain gate,
// transcript entries feed the official transcript for deduplication and
// persistence, then new entries push to the UI.
//
// The event loop is tokio::select! across four sources: a frame timer at 33ms,
// the tailer channel (viewer mode only), the Easement event channel for stdout
// and transcript envelopes, and crossterm's EventStream for keyboard input.
//
// State lives under ~/.local/state/puzzle/<slug>/ with sessions.jsonl for
// session tracking and windows/<timestamp>/ directories for each window
// lifetime. The official transcript at transcript.jsonl is the deduplicated
// record of the conversation. It feeds the UI and serves as the portable
// artifact for machine migration.
//
// Logging goes to /tmp/puzzle.log via tracing with a non-blocking file writer.
// RUST_LOG controls the filter; defaults to puzzle=debug.

mod app;
mod claude;
mod config;
mod model;
mod sessions;
mod parser;
mod render;
mod tailer;
mod transcript;

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
use crate::claude::{EasementEvent, EventReceiver, Invocation, Payload, StdoutEvent};
use crate::model::{try_convert, ContentBlock, EntryKind};
use crate::parser::parse_line;
use crate::render::render_entry;
use crate::tailer::run_tailer;
use crate::transcript::Transcript;

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

// Initialize tracing with a non-blocking file writer at
// ~/.local/state/puzzle/puzzle.log. The guard must be held for the
// program's lifetime to ensure the writer flushes.
fn init_tracing() -> WorkerGuard {
    let home = std::env::var("HOME").expect("HOME not set");
    let log_dir = std::path::Path::new(&home)
        .join(".local").join("state").join("puzzle");
    let _ = std::fs::create_dir_all(&log_dir);

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("puzzle.log"))
        .expect("failed to open puzzle.log");

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

/// Format a timestamp for the window directory name. Uses the same style
/// as the phase doc examples: 2026-02-22T04-30-00.123.
fn window_timestamp() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();

    // Convert unix timestamp to UTC components. No chrono dependency.
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days since epoch to Y-M-D. Good until 2100.
    let mut y = 1970;
    let mut remaining = days;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    let mut m = 0;
    for days_in_month in &month_days {
        if remaining < *days_in_month {
            break;
        }
        remaining -= days_in_month;
        m += 1;
    }
    let d = remaining + 1;
    m += 1;

    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}.{:03}",
        y, m, d, hours, minutes, seconds, millis,
    )
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
    let puzzle_state_dir: PathBuf;
    let viewer_path: Option<PathBuf>;
    let mut transcript: Option<Transcript> = None;

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

        puzzle_state_dir = PathBuf::from(&home)
            .join(".local").join("state").join("puzzle");

        // Set working directory to the pane dir.
        std::env::set_current_dir(&pane_dir)?;

        // Create this window's state directory with a timestamped name.
        let window_ts = window_timestamp();
        let window_dir = puzzle_state_dir
            .join(&s).join("windows").join(&window_ts);
        std::fs::create_dir_all(&window_dir)?;

        let transcript_path = window_dir.join("transcript.jsonl");
        transcript = Some(Transcript::new(transcript_path));

        // Look up the latest session. If none exists, bootstrap is
        // deferred to the first prompt because Easement requires a
        // kickoff message in the payload.
        if let Some(sid) = sessions::latest_session(&puzzle_state_dir, &s) {
            sessions::record_session(&puzzle_state_dir, &s, &sid)?;
            session_id = Some(sid);
        }
    } else {
        slug = None;
        puzzle_state_dir = PathBuf::from(&home)
            .join(".local").join("state").join("puzzle");

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
                            // Capture the first assistant message UUID as the
                            // boundary for transcript deduplication.
                            if let StdoutEvent::Assistant { ref message, .. } = stdout_event {
                                if let Some(ref mut t) = transcript {
                                    if let Some(uuid) = message.get("uuid").and_then(|v| v.as_str()) {
                                        let new_entries = t.set_boundary(uuid.to_string());
                                        for entry in new_entries {
                                            app.push_entry(entry);
                                        }
                                    }
                                }
                            }

                            let drained = inv.handle_event(&stdout_event);

                            // On first session ID capture from a bootstrap,
                            // record the session.
                            if session_id.is_none() {
                                if let Some(sid) = inv.session_id() {
                                    if let Some(ref s) = slug {
                                        let _ = sessions::record_session(&puzzle_state_dir, s, sid);
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
                        // Transcript entries pass through the transcript
                        // layer for deduplication and persistence. On the
                        // first spurt, everything is new. On subsequent
                        // spurts, history replay is skipped.
                        if let Some(ref mut t) = transcript {
                            let new_entries = t.handle_envelope(data);
                            for entry in new_entries {
                                app.push_entry(entry);
                            }
                        } else {
                            // Viewer mode fallback — no transcript layer.
                            let json = serde_json::to_string(&data).unwrap_or_default();
                            if let Some(entry) = parse_line(&json) {
                                if let Some(ce) = try_convert(entry) {
                                    app.push_entry(ce);
                                }
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
                                    if let Some(ref mut t) = transcript {
                                        t.begin_spurt();
                                    }

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
