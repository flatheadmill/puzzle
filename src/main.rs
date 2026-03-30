// Puzzle: a TUI for Claude Code conversations. Two modes: viewer (tails an
// existing JSONL transcript) and REPL (talks to Claude through Wicket).
//
// In REPL mode, Puzzle spawns Wicket once and holds the connection for the
// life of the window. Wicket is the coordinator — it owns the transcript,
// parses Claude's JSONL, deduplicates, normalizes entries, and manages the
// drain gate. Puzzle receives clean normalized entries and lifecycle events.
// No JSONL parsing, no dedup, no drain gate in the client.
//
// The event loop is tokio::select! across four sources: a frame timer at 33ms,
// the tailer channel (viewer mode only), the Wicket event channel for entries,
// lifecycle, and approval envelopes, and crossterm's EventStream for keyboard
// input.
//
// Logging goes to ~/.local/state/puzzle/puzzle.log via tracing with a
// non-blocking file writer. RUST_LOG controls the filter; defaults to
// puzzle=debug.

mod app;
mod claude;
mod config;
mod model;
mod render;
mod tailer;

use std::path::PathBuf;
use std::time::Duration;

use color_eyre::eyre::{bail, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::app::{App, ApprovalDecision, Mode, PendingApproval, RunState};
use crate::claude::{EventReceiver, SpawnTarget, WicketConnection, WicketEvent};
use crate::render::{render_entry, ACCENT, BASE, BG_USER, FAINT, MUTED, WARNING};
use crate::tailer::run_tailer;

fn init_tracing() -> WorkerGuard {
    let home = std::env::var("HOME").expect("HOME not set");
    let log_dir = std::path::Path::new(&home)
        .join(".local")
        .join("state")
        .join("puzzle");
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

/// Receives the next event from Wicket, or pends forever if no connection
/// is active. Used as a select! arm that effectively disables itself when
/// there is no Wicket process running.
async fn recv_wicket(rx: &mut Option<EventReceiver>) -> Option<WicketEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Render a conversation entry and insert it above the inline viewport,
/// scrolling into the terminal's native scrollback.
fn insert_entry(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    entry: &crate::model::ConversationEntry,
) -> Result<()> {
    let width = terminal.size()?.width;
    let (lines, bg) = render_entry(entry, width);
    let height = lines.len() as u16;

    terminal.insert_before(height, |buf| {
        // Fill the buffer with the entry background.
        let area = buf.area;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].set_style(Style::default().bg(bg));
            }
        }
        // Render each line.
        for (i, line) in lines.into_iter().enumerate() {
            if i as u16 >= area.height {
                break;
            }
            let line_area = Rect::new(area.x, area.y + i as u16, area.width, 1);
            line.render(line_area, buf);
        }
    })?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_tracing();
    color_eyre::install()?;

    let args: Vec<String> = std::env::args().collect();

    // First positional arg is either a .jsonl path (viewer) or a slug (REPL).
    let positional = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned();

    let positional = match positional {
        Some(p) => p,
        None => bail!("usage: puzzle <slug>\n       puzzle <path-to-session.jsonl>"),
    };

    let home = std::env::var("HOME")
        .map_err(|e| color_eyre::eyre::eyre!("HOME not set: {}", e))?;

    let repl_mode = !positional.ends_with(".jsonl");
    let mut wicket: Option<WicketConnection> = None;
    let mut wicket_rx: Option<EventReceiver> = None;
    let viewer_path: Option<PathBuf>;
    let mut target = SpawnTarget::Local;

    if repl_mode {
        let s = positional.clone();
        viewer_path = None;

        // Ensure the pane directory exists.
        let pane_dir = PathBuf::from(&home).join("pane").join(&s);
        std::fs::create_dir_all(&pane_dir)?;

        // Ensure trust so the CLI skips the approval dialog.
        let config_path = PathBuf::from(&home).join(".claude.json");
        let pane_dir_str = pane_dir
            .to_str()
            .ok_or_else(|| color_eyre::eyre::eyre!("pane dir is not valid UTF-8"))?;
        config::modify_config(&config_path, |c| config::ensure_trust(c, pane_dir_str))
            .map_err(|e| color_eyre::eyre::eyre!("{}", e))?;

        // Set working directory to the pane dir.
        std::env::set_current_dir(&pane_dir)?;

        // Connect to Wicket. History streams back as entry events.
        match WicketConnection::connect(&s).await {
            Ok((conn, rx)) => {
                tracing::info!("wicket connected");
                wicket = Some(conn);
                wicket_rx = Some(rx);
            }
            Err(e) => {
                tracing::error!("failed to connect to wicket: {}", e);
                bail!("failed to connect to wicket: {}", e);
            }
        }
    } else {
        viewer_path = Some(PathBuf::from(&positional));
        if !PathBuf::from(&positional).exists() {
            bail!("file not found: {}", positional);
        }
    }

    // Viewer mode tailer.
    let (tx, mut rx) = mpsc::channel(256);
    if let Some(path) = viewer_path {
        let tailer_tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = run_tailer(path, tailer_tx).await {
                tracing::error!("tailer error: {}", e);
            }
        });
    }

    // Inline viewport: the input area lives at the bottom of the terminal.
    // Conversation content is inserted above via insert_before, scrolling
    // into tmux's native scrollback. No alternate screen.
    crossterm::terminal::enable_raw_mode()?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let viewport_height = if repl_mode { 3 } else { 0 };
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )?;
    let mut app = App::new(repl_mode);
    if repl_mode {
        app.slug = Some(positional.clone());
    }
    let mut events = EventStream::new();
    let mut frame_interval = tokio::time::interval(Duration::from_millis(33));

    loop {
        tokio::select! {
            _ = frame_interval.tick() => {
                if app.repl_mode {
                    terminal.draw(|frame| {
                        let area = frame.area();

                        if app.pending_approval.is_some() {
                            // Approval mode: the viewport shows the dialog.
                            // Same viewport, different face.
                            let approval = app.pending_approval.as_ref().unwrap();

                            // Line 1: title question.
                            let title = format!(
                                "  Would you like to run {}?",
                                approval.tool_name
                            );
                            let r0 = Rect::new(area.x, area.y, area.width, 1);
                            frame.render_widget(
                                Paragraph::new(Line::from(Span::styled(
                                    title, Style::default().fg(WARNING),
                                ))),
                                r0,
                            );

                            // Line 2: the command/summary with $ prefix.
                            let summary = if approval.input_summary.is_empty() {
                                approval.tool_name.clone()
                            } else {
                                approval.input_summary.lines().next()
                                    .unwrap_or(&approval.tool_name).to_string()
                            };
                            if area.height > 1 {
                                let r1 = Rect::new(area.x, area.y + 1, area.width, 1);
                                frame.render_widget(
                                    Paragraph::new(Line::from(vec![
                                        Span::styled("  $ ", Style::default().fg(MUTED)),
                                        Span::styled(summary, Style::default().fg(BASE)),
                                    ])),
                                    r1,
                                );
                            }

                            // Line 3: options.
                            if area.height > 2 {
                                let r2 = Rect::new(area.x, area.y + 2, area.width, 1);
                                frame.render_widget(
                                    Paragraph::new(Line::from(vec![
                                        Span::styled("  y ", Style::default().fg(ACCENT)),
                                        Span::styled("allow  ", Style::default().fg(BASE)),
                                        Span::styled("n ", Style::default().fg(WARNING)),
                                        Span::styled("deny", Style::default().fg(BASE)),
                                    ])),
                                    r2,
                                );
                            }
                        } else {
                            // Normal mode: input area + status bar.
                            let layout = Layout::vertical([
                                Constraint::Length(1),
                                Constraint::Length(1),
                                Constraint::Length(1),
                            ])
                            .split(area);

                            let input_rect = layout[1];
                            let status_rect = layout[2];

                            // Separator line.
                            let sep = Paragraph::new(Line::from(Span::styled(
                                "\u{2500}".repeat(area.width as usize),
                                Style::default().fg(FAINT),
                            )));
                            frame.render_widget(sep, layout[0]);

                            let text_style = match (&app.mode, &app.run_state) {
                                (_, RunState::Running) => Style::default().fg(MUTED),
                                (Mode::Input, RunState::Idle) => Style::default().fg(BASE),
                                (Mode::Scroll, RunState::Idle) => Style::default().fg(MUTED),
                            };

                            let caret_style = if app.target_label.is_some() {
                                Style::default().fg(WARNING)
                            } else if app.mode == Mode::Input && app.run_state == RunState::Idle {
                                Style::default().fg(ACCENT)
                            } else {
                                Style::default().fg(FAINT)
                            };

                            // Render the input as: caret + textarea content on one line.
                            // No Block wrapper — tui-textarea renders directly.
                            app.textarea.set_style(text_style.bg(BG_USER));
                            app.textarea.set_cursor_line_style(Style::default().bg(BG_USER));

                            // Caret in the gutter (columns 0-1).
                            let caret_label = if app.run_state == RunState::Running {
                                "\u{2026}"
                            } else {
                                "\u{203a}"
                            };
                            let caret_line = Line::from(vec![
                                Span::styled(
                                    format!("{} ", caret_label),
                                    caret_style.bg(BG_USER),
                                ),
                            ]).style(Style::default().bg(BG_USER));
                            let caret_area = Rect::new(input_rect.x, input_rect.y, 2, 1);
                            frame.render_widget(Paragraph::new(caret_line), caret_area);

                            // Textarea starts at column 2.
                            let ta_rect = Rect::new(
                                input_rect.x + 2,
                                input_rect.y,
                                input_rect.width.saturating_sub(2),
                                input_rect.height,
                            );
                            frame.render_widget(&app.textarea, ta_rect);

                            // Status bar.
                            let target_info = match &app.target_label {
                                Some(label) => format!(" \u{00b7} {}", label),
                                None => " \u{00b7} local".to_string(),
                            };
                            let status_text = format!("  {}{}",
                                app.slug.as_deref().unwrap_or("puzzle"),
                                target_info,
                            );
                            let status = Paragraph::new(Line::from(Span::styled(
                                status_text,
                                Style::default().fg(FAINT),
                            )));
                            frame.render_widget(status, status_rect);
                        }
                    })?;
                }
            }

            // Viewer mode: tailer entries.
            Some(entry) = rx.recv() => {
                insert_entry(&mut terminal, &entry)?;
                app.push_entry(entry);
            }

            // REPL mode: Wicket events.
            event = recv_wicket(&mut wicket_rx) => {
                match event {
                    Some(WicketEvent::Entry(entry)) => {
                        if let Some(ref conn) = wicket {
                            conn.log("info", "entry received", serde_json::json!({
                                "kind": format!("{:?}", entry.kind),
                                "blocks": entry.blocks.len(),
                                "seq": entry.seq,
                                "total": app.entries.len() + 1,
                            }));
                        }
                        insert_entry(&mut terminal, &entry)?;
                        app.push_entry(entry);
                    }
                    Some(WicketEvent::Lifecycle(event_name)) => {
                        match event_name.as_str() {
                            "round_started" => {
                                tracing::info!("round started");
                            }
                            "round_completed" => {
                                tracing::info!("round completed");
                                if let Some(ref conn) = wicket {
                                    conn.log("info", "round completed", serde_json::json!({
                                        "entries": app.entries.len(),
                                    }));
                                }
                                app.run_state = RunState::Idle;
                                app.mode = Mode::Input;
                                app.follow = true;
                            }
                            other => {
                                if other.starts_with("round_failed") || other.contains("round_failed") {
                                    tracing::warn!("round failed: {}", other);
                                    if let Some(ref conn) = wicket {
                                        conn.log("error", "round failed", serde_json::json!({
                                            "detail": other,
                                        }));
                                    }
                                    app.run_state = RunState::Idle;
                                    app.mode = Mode::Input;
                                    app.follow = true;
                                } else {
                                    tracing::info!("lifecycle: {}", other);
                                }
                            }
                        }
                    }
                    Some(WicketEvent::Approval(data)) => {
                        let tool_name = data
                            .get("tool_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let input = data
                            .get("input")
                            .map(|v| {
                                // For Bash, show the command directly.
                                if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                                    cmd.to_string()
                                } else {
                                    serde_json::to_string_pretty(v).unwrap_or_default()
                                }
                            })
                            .unwrap_or_default();
                        tracing::info!(tool = %tool_name, "approval request");
                        if let Some(ref conn) = wicket {
                            conn.log("info", "approval request", serde_json::json!({
                                "tool": &tool_name,
                            }));
                        }
                        app.pending_approval = Some(PendingApproval {
                            tool_name,
                            input_summary: input,
                        });
                    }
                    Some(WicketEvent::Meta(data)) => {
                        tracing::info!("meta: {}", data);
                    }
                    Some(WicketEvent::Error(msg)) => {
                        tracing::error!("wicket error: {}", msg);
                        if let Some(ref conn) = wicket {
                            conn.log("error", "wicket error", serde_json::json!({
                                "message": &msg,
                            }));
                        }
                    }
                    None => {
                        tracing::warn!("wicket connection closed");
                        if let Some(ref conn) = wicket {
                            conn.log("warn", "connection closed", serde_json::json!({}));
                        }
                        wicket_rx = None;
                        app.run_state = RunState::Idle;
                        app.mode = Mode::Input;
                    }
                }
            }

            // Keyboard input.
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        if app.pending_approval.is_none()
                            && app.mode == Mode::Input
                            && app.run_state == RunState::Idle
                            && key.code == KeyCode::Enter
                        {
                            if let Some(prompt) = app.submit_input() {
                                let trimmed = prompt.trim();

                                // Slash commands switch the spawn target.
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

                                // Send the message to Wicket.
                                if let Some(ref mut conn) = wicket {
                                    tracing::info!(
                                        prompt_len = prompt.len(),
                                        target = ?target,
                                        "prompt submitted"
                                    );
                                    conn.log("info", "prompt submitted", serde_json::json!({
                                        "prompt_len": prompt.len(),
                                        "target": format!("{:?}", target),
                                    }));
                                    match conn.send(&prompt, &target).await {
                                        Ok(()) => {
                                            app.run_state = RunState::Running;
                                            app.follow = true;
                                        }
                                        Err(e) => {
                                            tracing::error!("failed to send to wicket: {}", e);
                                            conn.log("error", "send failed", serde_json::json!({
                                                "error": e.to_string(),
                                            }));
                                        }
                                    }
                                }
                            }
                        } else {
                            app.handle_key(key);
                        }
                    }
                }
            }
        }

        // Handle approval decisions.
        if let Some(decision) = app.approval_decision.take() {
            if let Some(ref mut conn) = wicket {
                match decision {
                    ApprovalDecision::Allow => {
                        tracing::info!("approval: allow");
                        conn.log("info", "approval: allow", serde_json::json!({}));
                        let _ = conn.approve(true, None).await;
                    }
                    ApprovalDecision::Deny => {
                        tracing::info!("approval: deny");
                        conn.log("info", "approval: deny", serde_json::json!({}));
                        let _ = conn.approve(false, Some("User denied permission")).await;
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    crossterm::terminal::disable_raw_mode()?;

    // Disconnect from Wicket.
    if let Some(conn) = wicket {
        let _ = conn.shutdown().await;
    }

    Ok(())
}
