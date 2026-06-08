// puzzle-tui: thin wrapper that launches the Codex TUI connected to a Puzzle Unix socket.
// Modeled after codex-rs/tui/src/main.rs. Uses arg0_dispatch_or_else for runtime setup.
// CODEX_HOME and the socket path are passed by Puzzle at spawn time.

use codex_arg0::{arg0_dispatch_or_else, Arg0DispatchPaths};
use codex_config::LoaderOverrides;
use codex_tui::{run_main, Cli, ExitReason};

fn main() -> anyhow::Result<()> {
    let socket_path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: puzzle-tui <socket-path>");
        std::process::exit(1);
    });

    arg0_dispatch_or_else(|arg0_paths: Arg0DispatchPaths| async move {
        let socket_uri = format!("unix://{}", socket_path);
        let remote_endpoint =
            codex_tui::resolve_remote_addr(&socket_uri).map_err(|e| anyhow::anyhow!("{}", e))?;

        let cli = <Cli as clap::Parser>::parse_from::<Vec<String>, String>(vec![]);

        let exit_info = run_main(
            cli,
            arg0_paths,
            LoaderOverrides::default(),
            Some(remote_endpoint),
        )
        .await?;

        match exit_info.exit_reason {
            ExitReason::Fatal(message) => {
                eprintln!("ERROR: {message}");
                std::process::exit(1);
            }
            ExitReason::UserRequested => {}
        }

        Ok(())
    })
}
