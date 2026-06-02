// Puzzle: protocol translator between the Codex TUI and Easement.
//
// Connects to Easement over WebSocket (bus protocol).
// Listens on a Unix socket for the Codex TUI to connect (JSON-RPC/WebSocket).
// Launches the Codex TUI binary pointing it at the Unix socket.
// Translates between the two protocols.

mod exchange;
mod translate;
mod wicket;

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;

use color_eyre::eyre::Result;
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio_tungstenite::accept_async;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::exchange::ExchangeLog;

fn init_tracing() -> WorkerGuard {
    let home = std::env::var("HOME").expect("HOME not set");
    let log_dir = std::path::Path::new(&home)
        .join(".local/state/puzzle");
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

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let args: Vec<String> = std::env::args().collect();

    let slug = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: puzzle <slug> [--codex <path>] [--full]");
        std::process::exit(1);
    });

    let is_full = args.iter().any(|a| a == "--full");
    let intent = if is_full { "full" } else { "latest" };

    let mut codex_path: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--codex" && i + 1 < args.len() {
            codex_path = Some(args[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }

    let _guard = init_tracing();
    tracing::info!(slug = %slug, intent = %intent, "puzzle starting");

    let home = std::env::var("HOME")?;
    let pane_dir = PathBuf::from(&home).join("pane").join(&slug);
    std::fs::create_dir_all(&pane_dir)?;

    let exchange = ExchangeLog::new(&slug);

    let codex_home = pane_dir.join(".codex");
    let socket_dir = codex_home.join("app-server-control");
    std::fs::create_dir_all(&socket_dir)?;
    let socket_path = socket_dir.join("app-server-control.sock");
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!(path = %socket_path.display(), "listening for codex TUI");

    // Connect to Easement. No handshake.
    let wicket_url = "ws://127.0.0.1:6502";
    let mut wicket = wicket::WicketClient::connect(wicket_url).await?;
    tracing::info!("connected to easement");

    // Request history — this resolves the timestamp and creates the coordinator.
    let replay_id = uuid::Uuid::new_v4().to_string();
    wicket.request_history(&slug, intent, &replay_id)?;
    tracing::info!(replay_id = %replay_id, intent = %intent, "history requested");

    // Collect history entries by replay_id. Buffer live entries.
    let mut history: Vec<serde_json::Value> = vec![];
    let mut live_buffer: Vec<wicket::WicketEvent> = vec![];
    let mut initial_usage: Option<serde_json::Value> = None;

    loop {
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wicket.event_rx.recv(),
        ).await {
            Ok(Some(wicket::WicketEvent::Entry { data, replay_id: Some(rid) })) if rid == replay_id => {
                history.push(data);
            }
            Ok(Some(wicket::WicketEvent::HistoryTerminate { replay_id: rid })) if rid == replay_id => {
                tracing::info!(entries = history.len(), "history replay complete");
                break;
            }
            Ok(Some(wicket::WicketEvent::Usage(usage))) => {
                initial_usage = Some(usage);
            }
            Ok(Some(wicket::WicketEvent::Entry { data, replay_id: None })) => {
                live_buffer.push(wicket::WicketEvent::Entry { data, replay_id: None });
            }
            Ok(Some(other)) => {
                live_buffer.push(other);
            }
            Ok(None) => break,
            Err(_) => {
                tracing::warn!("history request timed out after 10s");
                break;
            }
        }
    }

    // Dedup: remove live entries whose UUIDs are already in history.
    let history_uuids: HashSet<String> = history.iter()
        .filter_map(|e| e.get("uuid").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    for event in live_buffer {
        match event {
            wicket::WicketEvent::Entry { ref data, .. } => {
                let uuid = data.get("uuid").and_then(|v| v.as_str()).unwrap_or("");
                if !uuid.is_empty() && history_uuids.contains(uuid) {
                    continue;
                }
                history.push(data.clone());
            }
            wicket::WicketEvent::UserMessage { text, notification } => {
                if notification {
                    history.push(serde_json::json!({
                        "kind": "assistant",
                        "blocks": [{ "type": "text", "text": format!("**notification**: {}", text) }],
                    }));
                } else {
                    history.push(serde_json::json!({
                        "kind": "user",
                        "blocks": [{ "type": "text", "text": text }],
                    }));
                }
            }
            _ => {}
        }
    }

    tracing::info!(entries = history.len(), "history loaded (with dedup)");

    // Launch the Codex TUI.
    let codex_bin = codex_path.unwrap_or_else(|| "codex-tui".to_string());
    tracing::info!(bin = %codex_bin, codex_home = %codex_home.display(), "launching codex TUI");

    let mut tui_child = Command::new(&codex_bin)
        .env("CODEX_HOME", &codex_home)
        .env("PUZZLE_SLUG", &slug)
        .env("RUNNING_UNDER_PUZZLE", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    // Accept the TUI's connection.
    tracing::info!("waiting for TUI connection");
    let ws_stream = loop {
        let (stream, _addr) = listener.accept().await?;
        match accept_async(stream).await {
            Ok(ws) => {
                tracing::info!("TUI connected");
                break ws;
            }
            Err(e) => {
                tracing::debug!("probe or failed handshake, retrying: {}", e);
            }
        }
    };

    translate::run(ws_stream, wicket, exchange, &slug, history, initial_usage).await?;

    let status = tui_child.wait().await?;
    tracing::info!(code = ?status.code(), "codex TUI exited");

    let _ = std::fs::remove_file(&socket_path);

    Ok(())
}
