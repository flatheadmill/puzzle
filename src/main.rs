// Puzzle: protocol translator between the Codex TUI and Easement.
//
// Connects to Easement over WebSocket (bus protocol).
// Runs the Codex TUI in-process, connected via Unix socket.
// Translates between the two protocols.

mod exchange;
mod translate;
mod wicket;

use std::collections::HashSet;
use std::path::PathBuf;

use color_eyre::eyre::Result;
use tokio::net::UnixListener;
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
    let args: Vec<String> = std::env::args().collect();

    let slug = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: puzzle <slug> [--full]");
        std::process::exit(1);
    });

    let is_full = args.iter().any(|a| a == "--full");
    let intent = if is_full { "full" } else { "latest" };

    let _guard = init_tracing();
    tracing::info!(slug = %slug, intent = %intent, "puzzle starting");

    let home = std::env::var("HOME")?;
    let pane_dir = PathBuf::from(&home).join("pane").join(&slug);
    std::fs::create_dir_all(&pane_dir)?;

    let exchange = ExchangeLog::new(&slug);

    // CODEX_HOME shared for config. Socket per-instance.
    let codex_home = PathBuf::from(&home).join(".config/puzzle");
    std::fs::create_dir_all(&codex_home)?;

    let state_dir = PathBuf::from(&home).join(".local/state/puzzle").join(&slug);
    let socket_dir = state_dir.join("socket");
    std::fs::create_dir_all(&socket_dir)?;
    let socket_path = socket_dir.join("puzzle.sock");
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!(path = %socket_path.display(), "listening for codex TUI");

    // Connect to Easement.
    let port = std::env::var("EASEMENT_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(6502);
    let wicket_url = format!("ws://127.0.0.1:{}", port);
    let mut wicket = wicket::WicketClient::connect(&wicket_url, &slug).await?;
    tracing::info!("connected to easement");

    // Request history.
    let replay_id = uuid::Uuid::new_v4().to_string();
    wicket.request_history(&slug, intent, &replay_id)?;
    tracing::info!(replay_id = %replay_id, intent = %intent, "history requested");

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
            Ok(Some(wicket::WicketEvent::HistoryTerminate { replay_id: rid, timestamp: ts })) if rid == replay_id => {
                tracing::info!(entries = history.len(), timestamp = %ts, "history replay complete");
                if !ts.is_empty() {
                    *wicket.pinned_timestamp.write().await = ts;
                }
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

    // Set up the TUI endpoint.
    let socket_uri = format!("unix://{}", socket_path.display());
    let remote_endpoint = codex_tui::resolve_remote_addr(&socket_uri)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    std::env::set_var("CODEX_HOME", &codex_home);

    // Accept the TUI's connection in the background.
    let translate_handle = tokio::spawn(async move {
        tracing::info!("waiting for TUI connection");
        let ws_stream = loop {
            let (stream, _addr) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("listener accept error: {}", e);
                    return;
                }
            };
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

        if let Err(e) = translate::run(ws_stream, wicket, exchange, &slug, history, initial_usage).await {
            tracing::error!(error = %e, "translate loop error");
        }
    });

    // Run the TUI on the main thread.
    let cli = <codex_tui::Cli as clap::Parser>::parse_from::<Vec<String>, String>(vec![]);
    let paths = codex_arg0::Arg0DispatchPaths {
        codex_self_exe: std::env::current_exe().ok(),
        codex_linux_sandbox_exe: None,
        main_execve_wrapper_exe: None,
    };
    let exit_info = codex_tui::run_main(
        cli,
        paths,
        codex_config::LoaderOverrides::default(),
        Some(remote_endpoint),
    ).await?;
    tracing::info!("codex TUI exited");

    translate_handle.abort();
    let _ = std::fs::remove_file(&socket_path);

    Ok(())
}
