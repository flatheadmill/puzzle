// Puzzle: protocol translator between the Codex TUI and Wicket.
//
// Listens on a Unix socket for the Codex TUI to connect (JSON-RPC/WebSocket).
// Connects to Wicket as a WebSocket client (envelope protocol).
// Launches the Codex TUI binary pointing it at the Unix socket.
// Translates between the two protocols.

mod exchange;
mod translate;
mod wicket;

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
        eprintln!("usage: puzzle <slug> [--codex <path>]");
        std::process::exit(1);
    });

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
    tracing::info!(slug = %slug, "puzzle starting");

    let home = std::env::var("HOME")?;
    let pane_dir = PathBuf::from(&home).join("pane").join(&slug);
    std::fs::create_dir_all(&pane_dir)?;

    let exchange = ExchangeLog::new(&slug);

    // Set up CODEX_HOME so the TUI discovers our socket.
    // The socket goes at <codex_home>/app-server-control/app-server-control.sock
    let codex_home = pane_dir.join(".codex");
    let socket_dir = codex_home.join("app-server-control");
    std::fs::create_dir_all(&socket_dir)?;
    let socket_path = socket_dir.join("app-server-control.sock");
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!(path = %socket_path.display(), "listening for codex TUI");

    // Connect to Wicket.
    let wicket_url = "ws://127.0.0.1:6502";
    let wicket = wicket::WicketClient::connect(wicket_url, &slug).await?;
    tracing::info!("connected to wicket");

    // Launch the Codex TUI with CODEX_HOME pointing to our directory.
    let codex_bin = codex_path.unwrap_or_else(|| "codex-tui".to_string());
    tracing::info!(bin = %codex_bin, codex_home = %codex_home.display(), "launching codex TUI");

    let mut tui_child = Command::new(&codex_bin)
        .env("CODEX_HOME", &codex_home)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    // Accept the TUI's connection and upgrade to WebSocket.
    // The TUI probes the socket first (a bare TCP connect to check liveness),
    // then does the real WebSocket connection. Loop until handshake succeeds.
    tracing::info!("waiting for TUI connection");
    let ws_stream = loop {
        let (stream, _addr) = listener.accept().await?;
        match accept_async(stream).await {
            Ok(ws) => {
                tracing::info!("TUI connected, websocket established");
                break ws;
            }
            Err(e) => {
                tracing::debug!("probe or failed handshake, retrying: {}", e);
            }
        }
    };

    // Run the translation loop.
    translate::run(ws_stream, wicket, exchange, &slug).await?;

    // Wait for the TUI to exit.
    let status = tui_child.wait().await?;
    tracing::info!(code = ?status.code(), "codex TUI exited");

    // Clean up.
    let _ = std::fs::remove_file(&socket_path);

    Ok(())
}
