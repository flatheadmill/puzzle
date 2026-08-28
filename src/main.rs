use std::env;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use crossterm::event::Event;
use crossterm::event::EventStream;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::prelude::Alignment;
use ratatui::widgets::Paragraph;

const CLIENT_NAME: &str = "puzzle";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    let socket_path = socket_path()?;
    let socket_path = AbsolutePathBuf::from_absolute_path(socket_path)
        .context("app-server socket path must be absolute")?;
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
        client_name: CLIENT_NAME.to_string(),
        client_version: CLIENT_VERSION.to_string(),
        experimental_api: false,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 32,
    })
    .await
    .context("connect to shared Codex app-server")?;

    let count = active_root_count(&client).await?;
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, client, count).await;
    ratatui::restore();
    result
}

fn socket_path() -> Result<PathBuf> {
    if let Some(path) = env::args_os().nth(1) {
        return Ok(path.into());
    }

    if let Some(path) = env::var_os("MUSTER_STATE_HOME") {
        return Ok(PathBuf::from(path).join("codex/app-server.sock"));
    }

    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("muster/codex/app-server.sock"));
    }

    let Some(home) = env::var_os("HOME") else {
        bail!("pass the app-server socket path or set HOME");
    };
    Ok(PathBuf::from(home).join(".local/state/muster/codex/app-server.sock"))
}

async fn active_root_count(client: &RemoteAppServerClient) -> Result<usize> {
    let loaded: ThreadLoadedListResponse = client
        .request_typed(ClientRequest::ThreadLoadedList {
            request_id: RequestId::Integer(1),
            params: ThreadLoadedListParams {
                cursor: None,
                limit: None,
            },
        })
        .await
        .context("list loaded Codex threads")?;

    let mut count = 0;
    for (index, thread_id) in loaded.data.into_iter().enumerate() {
        let response: ThreadReadResponse = client
            .request_typed(ClientRequest::ThreadRead {
                request_id: RequestId::Integer(index as i64 + 2),
                params: ThreadReadParams {
                    thread_id,
                    include_turns: false,
                },
            })
            .await
            .context("read loaded Codex thread")?;

        if !response.thread.ephemeral
            && response.thread.parent_thread_id.is_none()
            && matches!(response.thread.status, ThreadStatus::Active { .. })
        {
            count += 1;
        }
    }

    Ok(count)
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut client: RemoteAppServerClient,
    mut count: usize,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    terminal.draw(|frame| draw(frame, count))?;

    loop {
        tokio::select! {
            event = terminal_events.next() => {
                let Some(event) = event else {
                    break;
                };
                match event? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let quit = matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                            || key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL);
                        if quit {
                            break;
                        }
                    }
                    Event::Resize(_, _) => {
                        terminal.draw(|frame| draw(frame, count))?;
                    }
                    _ => {}
                }
            }
            event = client.next_event() => {
                match event {
                    Some(AppServerEvent::ServerNotification(notification))
                        if matches!(
                            *notification,
                            ServerNotification::ThreadStarted(_)
                                | ServerNotification::ThreadStatusChanged(_)
                                | ServerNotification::ThreadClosed(_)
                        ) =>
                    {
                        let next = active_root_count(&client).await?;
                        if next != count {
                            count = next;
                            terminal.draw(|frame| draw(frame, count))?;
                        }
                    }
                    Some(AppServerEvent::Disconnected { message }) => bail!(message),
                    None => bail!("shared Codex app-server connection closed"),
                    _ => {}
                }
            }
        }
    }

    client.shutdown().await?;
    Ok(())
}

fn draw(frame: &mut Frame<'_>, count: usize) {
    let area = frame.area();
    let line = Rect::new(area.x, area.y + area.height / 2, area.width, 1);
    frame.render_widget(
        Paragraph::new(count.to_string()).alignment(Alignment::Center),
        line,
    );
}
