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
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use tokio::sync::mpsc;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::app::{App, ApprovalDecision, Mode, PendingApproval, RunState};
use crate::claude::{EventReceiver, SpawnTarget, WicketConnection, WicketEvent};
use crate::render::{render_entry, ACCENT, BASE, CODE, ERROR, FAINT, MUTED, WARNING};
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
                        ])
                        .split(area);
                        main_area = layout[0];
                        input_area = Some(layout[1]);
                    } else {
                        main_area = area;
                        input_area = None;
                    }

                    let conv_chunks = Layout::horizontal([
                        Constraint::Length(1),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(main_area);

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
                    frame.render_stateful_widget(
                        scrollbar,
                        conv_chunks[2],
                        &mut scrollbar_state,
                    );

                    if let Some(input_rect) = input_area {
                        let target_suffix = match &app.target_label {
                            Some(label) => format!(" [{}] ", label),
                            None => String::new(),
                        };
                        let title = match (&app.mode, &app.run_state) {
                            (_, RunState::Running) => {
                                format!(" running...{}", target_suffix)
                            }
                            (Mode::Input, RunState::Idle) => {
                                format!(" prompt (esc: scroll){}", target_suffix)
                            }
                            (Mode::Scroll, RunState::Idle) => {
                                format!(" scroll (i: input){}", target_suffix)
                            }
                        };

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

            // Viewer mode: tailer entries.
            Some(entry) = rx.recv() => {
                app.push_entry(entry);
            }

            // REPL mode: Wicket events.
            event = recv_wicket(&mut wicket_rx) => {
                match event {
                    Some(WicketEvent::Entry(entry)) => {
                        app.push_entry(entry);
                    }
                    Some(WicketEvent::Lifecycle(event_name)) => {
                        match event_name.as_str() {
                            "round_started" => {
                                tracing::info!("round started");
                                // Run state was already set when we submitted.
                            }
                            "round_completed" => {
                                tracing::info!("round completed");
                                app.run_state = RunState::Idle;
                                app.mode = Mode::Input;
                                app.follow = true;
                            }
                            other => {
                                // round_failed or unknown
                                if other.starts_with("round_failed") || other.contains("round_failed") {
                                    tracing::warn!("round failed: {}", other);
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
                            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                            .unwrap_or_default();
                        tracing::info!(tool = %tool_name, "approval request");
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
                    }
                    None => {
                        // Wicket closed — connection lost.
                        tracing::warn!("wicket connection closed");
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
                                    match conn.send(&prompt, &target).await {
                                        Ok(()) => {
                                            app.run_state = RunState::Running;
                                            app.follow = true;
                                        }
                                        Err(e) => {
                                            tracing::error!("failed to send to wicket: {}", e);
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
                        let _ = conn.approve(true, None).await;
                    }
                    ApprovalDecision::Deny => {
                        tracing::info!("approval: deny");
                        let _ = conn.approve(false, Some("User denied permission")).await;
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    ratatui::restore();

    // Disconnect from Wicket.
    if let Some(conn) = wicket {
        let _ = conn.shutdown().await;
    }

    Ok(())
}
