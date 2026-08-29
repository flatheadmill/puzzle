use std::env;
use std::fs;
use std::path::Path;
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
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

const CLIENT_NAME: &str = "puzzle";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq)]
struct Window {
    slug: String,
    preview: String,
    status: ThreadStatus,
    recency_at: i64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let state_root = muster_state_root()?;
    let socket_path = socket_path(&state_root);
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

    let mut request_id = 1;
    let windows = load_windows(&client, &state_root, &mut request_id).await?;
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, client, state_root, request_id, windows).await;
    ratatui::restore();
    result
}

fn muster_state_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("MUSTER_STATE_HOME") {
        return Ok(path.into());
    }

    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("muster"));
    }

    let Some(home) = env::var_os("HOME") else {
        bail!("set MUSTER_STATE_HOME, XDG_STATE_HOME, or HOME");
    };
    Ok(PathBuf::from(home).join(".local/state/muster"))
}

fn socket_path(state_root: &Path) -> PathBuf {
    if let Some(path) = env::args_os().nth(1) {
        return path.into();
    }

    state_root.join("codex/app-server.sock")
}

fn primary_sessions(state_root: &Path) -> Result<Vec<(String, String)>> {
    let codex_state = state_root.join("codex");
    let entries = fs::read_dir(&codex_state)
        .with_context(|| format!("read Muster Codex state at {}", codex_state.display()))?;
    let mut sessions = Vec::new();

    for entry in entries {
        let entry = entry.context("read Muster Codex state entry")?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let sid_path = entry.path().join("sid.json");
        if !sid_path.is_file() {
            continue;
        }

        let slug = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("window slug is not valid UTF-8"))?;
        let encoded = fs::read_to_string(&sid_path)
            .with_context(|| format!("read session ID at {}", sid_path.display()))?;
        let thread_id: String = serde_json::from_str(&encoded)
            .with_context(|| format!("decode session ID at {}", sid_path.display()))?;
        sessions.push((slug, thread_id));
    }

    Ok(sessions)
}

async fn load_windows(
    client: &RemoteAppServerClient,
    state_root: &Path,
    request_id: &mut i64,
) -> Result<Vec<Window>> {
    let mut windows = Vec::new();
    for (slug, thread_id) in primary_sessions(state_root)? {
        let id = *request_id;
        *request_id += 1;
        let response: ThreadReadResponse = client
            .request_typed(ClientRequest::ThreadRead {
                request_id: RequestId::Integer(id),
                params: ThreadReadParams {
                    thread_id,
                    include_turns: false,
                },
            })
            .await
            .with_context(|| format!("read Codex thread for window {slug}"))?;

        windows.push(Window {
            slug,
            preview: response.thread.preview,
            status: response.thread.status,
            recency_at: response
                .thread
                .recency_at
                .unwrap_or(response.thread.updated_at),
        });
    }

    windows.sort_by(|left, right| {
        right
            .recency_at
            .cmp(&left.recency_at)
            .then_with(|| left.slug.cmp(&right.slug))
    });
    Ok(windows)
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut client: RemoteAppServerClient,
    state_root: PathBuf,
    mut request_id: i64,
    mut windows: Vec<Window>,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut offset = 0;
    terminal.draw(|frame| draw(frame, &windows, offset))?;

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
                        let page = visible_rows(terminal.size()?.height);
                        let maximum = windows.len().saturating_sub(page);
                        match key.code {
                            KeyCode::Down | KeyCode::Char('j') => {
                                offset = (offset + 1).min(maximum);
                                terminal.draw(|frame| draw(frame, &windows, offset))?;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                offset = offset.saturating_sub(1);
                                terminal.draw(|frame| draw(frame, &windows, offset))?;
                            }
                            KeyCode::Home | KeyCode::Char('g') => {
                                offset = 0;
                                terminal.draw(|frame| draw(frame, &windows, offset))?;
                            }
                            KeyCode::End | KeyCode::Char('G') => {
                                offset = maximum;
                                terminal.draw(|frame| draw(frame, &windows, offset))?;
                            }
                            _ => {}
                        }
                    }
                    Event::Resize(_, _) => {
                        let maximum = windows
                            .len()
                            .saturating_sub(visible_rows(terminal.size()?.height));
                        offset = offset.min(maximum);
                        terminal.draw(|frame| draw(frame, &windows, offset))?;
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
                        let next = load_windows(&client, &state_root, &mut request_id).await?;
                        if next != windows {
                            windows = next;
                            let maximum = windows
                                .len()
                                .saturating_sub(visible_rows(terminal.size()?.height));
                            offset = offset.min(maximum);
                            terminal.draw(|frame| draw(frame, &windows, offset))?;
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

fn visible_rows(height: u16) -> usize {
    usize::from(height.saturating_sub(2))
}

fn status(thread_status: &ThreadStatus) -> (&'static str, Color) {
    match thread_status {
        ThreadStatus::Active { .. } => ("active", Color::Green),
        ThreadStatus::Idle => ("idle", Color::DarkGray),
        ThreadStatus::NotLoaded => ("unloaded", Color::DarkGray),
        ThreadStatus::SystemError => ("error", Color::Red),
    }
}

fn preview(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn draw(frame: &mut Frame<'_>, windows: &[Window], offset: usize) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let title = format!("{} primary Codex windows", windows.len());
    frame.render_widget(Paragraph::new(title), header);

    let rows = windows
        .iter()
        .skip(offset)
        .take(visible_rows(area.height))
        .map(|window| {
            let (status, color) = status(&window.status);
            Line::from(vec![
                Span::styled(format!("{status:<10}"), Style::default().fg(color)),
                Span::styled(
                    format!("{:<20}", window.slug),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(preview(&window.preview)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rows), body);

    let position = if windows.len() <= visible_rows(area.height) {
        String::new()
    } else {
        format!(
            "  {}-{} of {}",
            offset + 1,
            (offset + visible_rows(area.height)).min(windows.len()),
            windows.len()
        )
    };
    frame.render_widget(
        Paragraph::new(format!("j/k scroll  g/G first/last  q quit{position}"))
            .style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}
