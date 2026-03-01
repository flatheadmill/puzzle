// Puzzle: a TUI for Claude Code conversations. Two modes: viewer (tails an
// existing JSONL transcript) and REPL (orchestrates Claude through Easement).
//
// In REPL mode, Puzzle spawns Easement per spurt. Easement wraps the Claude
// CLI and multiplexes output into typed NDJSON envelopes. Puzzle reads one
// stream and routes by the stream field: stdout events drive the drain gate,
// transcript entries feed the official transcript for deduplication and
// persistence, then new entries push to the UI.
//
// The event loop is tokio::select! across five sources: a frame timer at 33ms,
// the tailer channel (viewer mode only), the Easement event channel for stdout
// and transcript envelopes, the Wicket Unix socket for approval requests, and
// crossterm's EventStream for keyboard input.
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
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::app::{App, ApprovalDecision, Mode, PendingApproval, RunState};
use crate::claude::{EasementEvent, EventReceiver, Invocation, Payload, SpawnTarget, StdoutEvent};
use crate::model::{try_convert, ContentBlock, EntryKind};
use crate::parser::parse_line;
use crate::render::{render_entry, ACCENT, BASE, CODE, ERROR, FAINT, MUTED, WARNING};
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

/// Accepts the next connection on the Wicket socket, or pends forever if
/// no listener is bound. Used as a select! arm that disables itself in
/// viewer mode or if the bind failed.
async fn accept_wicket(
    listener: &Option<UnixListener>,
) -> std::io::Result<tokio::net::UnixStream> {
    match listener {
        Some(l) => l.accept().await.map(|(stream, _)| stream),
        None => std::future::pending().await,
    }
}

/// Computes a centered rectangle within `area` with the given width and
/// height constraints, clamped to the available space.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Find the most recent previous window's transcript.jsonl for a slug.
/// Scans the windows directory in reverse chronological order, skipping
/// the current window, and returns the first non-empty transcript found.
fn find_previous_transcript(
    state_dir: &std::path::Path,
    slug: &str,
    current_window_ts: &str,
) -> Option<PathBuf> {
    let windows_dir = state_dir.join(slug).join("windows");
    let mut dirs: Vec<_> = std::fs::read_dir(&windows_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| e.file_name().to_str() != Some(current_window_ts))
        .collect();
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    for dir in dirs {
        let transcript = dir.path().join("transcript.jsonl");
        if let Ok(meta) = std::fs::metadata(&transcript) {
            if meta.len() > 0 {
                return Some(transcript);
            }
        }
    }
    None
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
    // Local and remote targets maintain separate sessions. The local session
    // persists to sessions.jsonl and survives window restarts. The remote
    // session lives only in memory — when the window dies, the remote session
    // is abandoned and the next window will do a fresh transcript transfer.
    // This is fine because the official transcript is the portable artifact,
    // not the remote session.
    let mut target = SpawnTarget::Local;
    let mut remote_session_id: Option<String> = None;
    let mut loaded_entries = vec![];

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
        let mut t = Transcript::new(transcript_path);

        // Load the previous window's transcript so the conversation
        // history is visible immediately, before the first prompt.
        if let Some(prev) = find_previous_transcript(&puzzle_state_dir, &s, &window_ts) {
            let loaded = t.load(&prev);
            // Stash for pushing to the app after it's created.
            // The entries are in self.entries for dedup; the UI
            // entries go to the app below.
            loaded_entries = loaded;
        }
        transcript = Some(t);

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

    // Wicket socket listener. Binds in the pane directory so the path is
    // naturally scoped by slug — no collisions between windows. Remove a
    // stale socket file if one exists from a previous run. The listener
    // persists across spurts — Puzzle binds once at startup and accepts
    // connections as they arrive.
    let wicket_socket_path = PathBuf::from(&home)
        .join("pane")
        .join(slug.as_ref().unwrap_or(&positional))
        .join("wicket.sock");
    let wicket_listener = if repl_mode {
        let _ = std::fs::remove_file(&wicket_socket_path);
        match UnixListener::bind(&wicket_socket_path) {
            Ok(l) => {
                tracing::info!("wicket socket listener bound at {}", wicket_socket_path.display());
                Some(l)
            }
            Err(e) => {
                tracing::error!("failed to bind wicket socket: {}", e);
                None
            }
        }
    } else {
        None
    };
    // The Wicket connection is split: the read half is consumed in the
    // accept arm to parse the request, and the write half is stashed here
    // until the operator presses y/n. The connection stays open across loop
    // iterations — Wicket is blocking on the other end waiting for our
    // response, which keeps Claude blocked too.
    let mut wicket_writer: Option<tokio::net::unix::OwnedWriteHalf> = None;

    let mut terminal = ratatui::init();
    let mut app = App::new(repl_mode);

    // Push any entries loaded from a previous window's transcript.
    for entry in loaded_entries {
        app.push_entry(entry);
    }

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

                    // conversation area with left margin and scrollbar
                    let conv_chunks = Layout::horizontal([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ]).split(main_area);

                    let conv_width = conv_chunks[1].width;
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

                    frame.render_stateful_widget(list, conv_chunks[1], &mut app.list_state);

                    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
                    let mut scrollbar_state = ScrollbarState::new(app.entries.len())
                        .position(app.list_state.selected().unwrap_or(0));
                    frame.render_stateful_widget(scrollbar, conv_chunks[2], &mut scrollbar_state);

                    // prompt input area — tui-textarea renders itself as a
                    // widget and manages its own cursor. The block and style
                    // are set dynamically each frame based on mode and run state.
                    if let Some(input_rect) = input_area {
                        let target_suffix = match &app.target_label {
                            Some(label) => format!(" [{}] ", label),
                            None => String::new(),
                        };
                        let title = match (&app.mode, &app.run_state) {
                            (_, RunState::Running) => format!(" running...{}", target_suffix),
                            (Mode::Input, RunState::Idle) => format!(" prompt (esc: scroll){}", target_suffix),
                            (Mode::Scroll, RunState::Idle) => format!(" scroll (i: input){}", target_suffix),
                        };

                        // Warning tone when targeting a remote machine so it is
                        // impossible to miss that you are running on someone else's
                        // filesystem. Accent for active local input, muted otherwise.
                        let border_style = if app.target_label.is_some() {
                            Style::default().fg(WARNING)
                        } else if app.mode == Mode::Input && app.run_state == RunState::Idle {
                            Style::default().fg(ACCENT)
                        } else {
                            Style::default().fg(FAINT)
                        };

                        let text_style = match (&app.mode, &app.run_state) {
                            (_, RunState::Running) => Style::default().fg(MUTED),
                            (Mode::Input, RunState::Idle) => Style::default().fg(BASE),
                            (Mode::Scroll, RunState::Idle) => Style::default().fg(MUTED),
                        };

                        let input_block = Block::default()
                            .borders(Borders::ALL)
                            .title(title)
                            .border_style(border_style);

                        app.textarea.set_block(input_block);
                        app.textarea.set_style(text_style);

                        frame.render_widget(&app.textarea, input_rect);
                    }

                    // Approval dialog overlay. Rendered last so it appears
                    // on top of everything else.
                    if let Some(ref approval) = app.pending_approval {
                        let popup_area = centered_rect(area, 60, 16);
                        frame.render_widget(Clear, popup_area);

                        let mut lines: Vec<Line> = vec![
                            Line::from(""),
                            Line::from(vec![
                                Span::styled("  Tool: ", Style::default().fg(MUTED)),
                                Span::styled(
                                    approval.tool_name.clone(),
                                    Style::default().fg(WARNING),
                                ),
                            ]),
                            Line::from(""),
                        ];

                        // Show a few lines of the input summary.
                        for line in approval.input_summary.lines().take(8) {
                            lines.push(Line::from(Span::styled(
                                format!("  {}", line),
                                Style::default().fg(BASE),
                            )));
                        }
                        let total_lines = approval.input_summary.lines().count();
                        if total_lines > 8 {
                            lines.push(Line::from(Span::styled(
                                format!("  ... ({} more lines)", total_lines - 8),
                                Style::default().fg(MUTED),
                            )));
                        }

                        lines.push(Line::from(""));
                        lines.push(Line::from(vec![
                            Span::styled("  [y] ", Style::default().fg(CODE)),
                            Span::styled("Allow  ", Style::default().fg(BASE)),
                            Span::styled("[n] ", Style::default().fg(ERROR)),
                            Span::styled("Deny", Style::default().fg(BASE)),
                        ]));

                        let dialog = Paragraph::new(lines)
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .title(" Approve? ")
                                    .border_style(Style::default().fg(WARNING)),
                            )
                            .wrap(Wrap { trim: false });

                        frame.render_widget(dialog, popup_area);
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
                            // boundary for transcript deduplication. The uuid
                            // lives at the top level of the event, not inside
                            // the message object.
                            if let StdoutEvent::Assistant { ref uuid, .. } = stdout_event {
                                if let Some(ref mut t) = transcript {
                                    if let Some(uuid) = uuid {
                                        let new_entries = t.set_boundary(uuid.clone());
                                        for entry in new_entries {
                                            app.push_entry(entry);
                                        }
                                    }
                                }
                            }

                            let drained = inv.handle_event(&stdout_event);

                            // Capture the session ID for the active target.
                            // Local sessions get recorded to disk for persistence
                            // across window restarts. Remote sessions are held in
                            // memory for the window's lifetime.
                            if let Some(sid) = inv.session_id() {
                                match &target {
                                    SpawnTarget::Local => {
                                        if session_id.is_none() {
                                            if let Some(ref s) = slug {
                                                let _ = sessions::record_session(&puzzle_state_dir, s, sid);
                                            }
                                        }
                                        session_id = Some(sid.to_string());
                                    }
                                    SpawnTarget::Remote { .. } => {
                                        remote_session_id = Some(sid.to_string());
                                    }
                                }
                            }

                            if drained {
                                if let Some(sid) = inv.session_id() {
                                    tracing::info!(session_id = %sid, target = ?target, "invocation drained");
                                }
                                // Shut down Easement (close stdin, wait for
                                // exit) but keep the channel alive. Transcript
                                // envelopes arrive after stdout events — the
                                // reader task will deliver them and then close
                                // the channel when Easement's stdout hits EOF.
                                let inv = invocation.take().unwrap();
                                let _ = inv.shutdown().await;
                                app.run_state = RunState::Idle;
                                app.mode = Mode::Input;
                                app.follow = true;
                                tracing::info!("invocation drained, draining channel");
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
                        // Channel closed — reader task finished. Normal
                        // after drain. If the invocation is still alive
                        // it means Easement died unexpectedly.
                        if let Some(inv) = invocation.take() {
                            tracing::warn!("easement channel closed with invocation still active");
                            if let Some(sid) = inv.session_id() {
                                session_id = Some(sid.to_string());
                            }
                            let _ = inv.shutdown().await;
                            app.run_state = RunState::Idle;
                            app.mode = Mode::Input;
                            app.follow = true;
                        } else {
                            tracing::info!("channel closed, invocation complete");
                        }
                        claude_rx = None;
                    }
                }
            }
            // The select guard is the concurrency model. While an approval
            // is pending, we stop accepting new connections entirely. This is
            // why pending_approval and wicket_writer can both be simple Options
            // rather than queues — there is never more than one in flight.
            // Wicket holds the MCP connection open on its side, so Claude blocks
            // until we respond. No races, no ordering concerns, no dropped requests.
            Ok(stream) = accept_wicket(&wicket_listener), if app.pending_approval.is_none() => {
                let (reader, writer) = stream.into_split();
                let mut buf_reader = BufReader::new(reader);
                let mut line = String::new();
                match buf_reader.read_line(&mut line).await {
                    Ok(0) => {
                        tracing::warn!("wicket connection closed before sending request");
                    }
                    Ok(_) => {
                        if let Ok(request) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                            let tool_name = request.get("tool_name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let input = request.get("input")
                                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                                .unwrap_or_default();
                            tracing::info!(tool = %tool_name, "approval request received");
                            app.pending_approval = Some(PendingApproval {
                                tool_name,
                                input_summary: input,
                            });
                            wicket_writer = Some(writer);
                        } else {
                            tracing::warn!("failed to parse wicket request: {}", line.trim());
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to read wicket request: {}", e);
                    }
                }
            }
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        // check for Enter in input mode to submit.
                        // Skip when an approval dialog is visible — the
                        // dialog handler in app.handle_key takes priority.
                        if app.pending_approval.is_none()
                            && app.mode == Mode::Input
                            && app.run_state == RunState::Idle
                            && key.code == KeyCode::Enter
                        {
                            if let Some(prompt) = app.submit_input() {
                                // Slash commands switch the spawn target, not the
                                // session. Nothing happens until the next prompt —
                                // the target just determines where that prompt runs.
                                // /yolo switches to ssh yolo@orb with dangerously-
                                // skip-permissions. /mac switches back to local.
                                // Repeating the current target is a no-op.
                                let trimmed = prompt.trim();
                                if trimmed == "/yolo" {
                                    let new_target = SpawnTarget::Remote {
                                        host: "yolo@orb".to_string(),
                                        yolo: true,
                                    };
                                    if target != new_target {
                                        target = new_target;
                                        app.target_label = Some("yolo".to_string());
                                        tracing::info!("target switched to yolo@orb");
                                    }
                                    continue;
                                } else if let Some(host) = trimmed.strip_prefix("/remote ") {
                                    let host = host.trim().to_string();
                                    let new_target = SpawnTarget::Remote {
                                        host: host.clone(),
                                        yolo: false,
                                    };
                                    if target != new_target {
                                        app.target_label = Some(host.clone());
                                        tracing::info!("target switched to {}", host);
                                        target = new_target;
                                    }
                                    continue;
                                } else if trimmed == "/mac" {
                                    if target != SpawnTarget::Local {
                                        target = SpawnTarget::Local;
                                        app.target_label = None;
                                        tracing::info!("target switched to local");
                                    }
                                    continue;
                                }

                                tracing::info!(
                                    prompt_len = prompt.len(),
                                    has_session = session_id.is_some(),
                                    target = ?target,
                                    "prompt submitted"
                                );
                                if let Some(ref s) = slug {
                                    if let Some(ref mut t) = transcript {
                                        t.begin_spurt();
                                    }

                                    // Pick the session ID for the current target.
                                    // Remote targets track their own session because
                                    // --print mode always forks into a new session ID.
                                    // The yolo flag maps to --dangerously-skip-permissions
                                    // in Easement — the VM sandbox is the permission.
                                    let (active_session, is_yolo) = match &target {
                                        SpawnTarget::Local => (session_id.clone(), false),
                                        SpawnTarget::Remote { yolo, .. } => (remote_session_id.clone(), *yolo),
                                    };

                                    let payload = Payload {
                                        slug: s.clone(),
                                        yolo: is_yolo,
                                        message: prompt,
                                        session_id: active_session.clone(),
                                        transcript: if active_session.is_none() {
                                            // First spurt on this target. Send the official
                                            // transcript so Claude forks from it.
                                            Some(
                                                transcript.as_ref()
                                                    .map(|t| t.entries().to_vec())
                                                    .unwrap_or_default()
                                            )
                                        } else {
                                            None
                                        },
                                        wicket_socket: None,
                                    };

                                    match Invocation::spawn(&target, payload).await {
                                        Ok((inv, erx)) => {
                                            tracing::info!("easement spawned");
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

        // Handle approval decisions outside the select! so the write
        // happens promptly after the key press, not on the next tick.
        //
        // The response goes to Wicket over the Unix socket, not to Claude
        // directly. Wicket constructs the full MCP tool response including
        // updatedInput (which the CLI's Zod schema requires on allow) before
        // relaying to Claude. Puzzle only needs behavior and an optional
        // denial message.
        if let Some(decision) = app.approval_decision.take() {
            if let Some(mut writer) = wicket_writer.take() {
                let response = match decision {
                    ApprovalDecision::Allow => {
                        tracing::info!("approval: allow");
                        "{\"behavior\":\"allow\"}\n"
                    }
                    ApprovalDecision::Deny => {
                        tracing::info!("approval: deny");
                        "{\"behavior\":\"deny\",\"message\":\"User denied permission\"}\n"
                    }
                };
                let _ = writer.write_all(response.as_bytes()).await;
                let _ = writer.flush().await;
                let _ = writer.shutdown().await;
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Clean up the wicket socket on exit.
    if repl_mode {
        let _ = std::fs::remove_file(wicket_socket_path);
    }

    ratatui::restore();
    Ok(())
}
