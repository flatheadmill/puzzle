use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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
use tokio::process::Command;
use tokio::time::Duration;
use tokio::time::sleep;

const CLIENT_NAME: &str = "puzzle";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const PARKING_SESSION: &str = "puzzle-parking";
const VIEWPORT_OPTION: &str = "@puzzle-viewport-pane";
const SLUG_OPTION: &str = "@puzzle-slug";

#[derive(Clone, Debug, PartialEq)]
struct Window {
    slug: String,
    preview: String,
    status: ThreadStatus,
    recency_at: i64,
}

struct Teleporter {
    puzzle_pane: String,
    window_id: String,
    viewport_pane: String,
}

impl Teleporter {
    async fn initialize() -> Result<Self> {
        let puzzle_pane = env::var("TMUX_PANE").context("Puzzle must run inside tmux")?;
        let window_id = tmux_output([
            "display-message",
            "-p",
            "-t",
            puzzle_pane.as_str(),
            "#{window_id}",
        ])
        .await?;

        let configured = tmux_output([
            "show-option",
            "-wqv",
            "-t",
            window_id.as_str(),
            VIEWPORT_OPTION,
        ])
        .await?;
        let viewport_pane = if pane_is_in_window(configured.as_str(), window_id.as_str()).await? {
            configured
        } else {
            create_viewport(&puzzle_pane, &window_id).await?
        };

        Ok(Self {
            puzzle_pane,
            window_id,
            viewport_pane,
        })
    }

    async fn show(&mut self, slug: &str) -> Result<()> {
        if !pane_is_in_window(&self.viewport_pane, &self.window_id).await? {
            self.viewport_pane = create_viewport(&self.puzzle_pane, &self.window_id).await?;
        }
        let pane = match tagged_pane(slug).await? {
            Some(pane) => pane,
            None => create_client_pane(slug).await?,
        };

        if pane != self.viewport_pane {
            tmux_status([
                "swap-pane",
                "-d",
                "-s",
                pane.as_str(),
                "-t",
                self.viewport_pane.as_str(),
            ])
            .await
            .context("swap Codex client into Puzzle viewport")?;
            self.viewport_pane = pane;
            tmux_status([
                "set-option",
                "-w",
                "-t",
                self.window_id.as_str(),
                VIEWPORT_OPTION,
                self.viewport_pane.as_str(),
            ])
            .await
            .context("record current Puzzle viewport pane")?;
        }
        Ok(())
    }

    async fn focus(&self) -> Result<()> {
        tmux_status(["select-pane", "-t", self.viewport_pane.as_str()])
            .await
            .context("focus Puzzle viewport pane")
    }
}

async fn create_viewport(puzzle_pane: &str, window_id: &str) -> Result<String> {
    let pane = tmux_output([
        "split-window",
        "-hdP",
        "-p",
        "65",
        "-F",
        "#{pane_id}",
        "-t",
        puzzle_pane,
    ])
    .await
    .context("create Puzzle viewport pane")?;
    tmux_status([
        "set-option",
        "-w",
        "-t",
        window_id,
        VIEWPORT_OPTION,
        pane.as_str(),
    ])
    .await
    .context("record Puzzle viewport pane")?;
    Ok(pane)
}

async fn pane_is_in_window(pane: &str, window_id: &str) -> Result<bool> {
    if pane.is_empty() {
        return Ok(false);
    }
    let panes = tmux_output(["list-panes", "-t", window_id, "-F", "#{pane_id}"]).await?;
    Ok(panes.lines().any(|candidate| candidate == pane))
}

async fn tagged_pane(slug: &str) -> Result<Option<String>> {
    let panes = tmux_output(["list-panes", "-a", "-F", "#{pane_id}\t#{@puzzle-slug}"]).await?;
    Ok(panes.lines().find_map(|line| {
        let (pane, candidate) = line.split_once('\t')?;
        (candidate == slug).then(|| pane.to_string())
    }))
}

async fn create_client_pane(slug: &str) -> Result<String> {
    if !valid_window_slug(slug) {
        bail!("invalid window slug: {slug}");
    }

    if !tmux_success(["has-session", "-t", PARKING_SESSION]).await? {
        tmux_status(["new-session", "-d", "-s", PARKING_SESSION, "-n", "reserve"]).await?;
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let window_name = format!("client-{}-{nonce}", std::process::id());
    let command = format!(
        "tmux new-window -ad -t {PARKING_SESSION}:reserve -n {window_name} \
         'exec muster codex tui --slug {slug}'"
    );
    tmux_status(["run-shell", "-b", command.as_str()]).await?;
    let target = format!("{PARKING_SESSION}:{window_name}");
    let mut pane = None;
    for _ in 0..40 {
        if let Ok(candidate) =
            tmux_output(["list-panes", "-t", target.as_str(), "-F", "#{pane_id}"]).await
            && !candidate.is_empty()
        {
            pane = Some(candidate);
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    let pane = pane.with_context(|| format!("tmux did not create a client pane for {slug}"))?;
    tmux_status(["set-option", "-p", "-t", pane.as_str(), SLUG_OPTION, slug]).await?;
    tmux_status(["rename-window", "-t", pane.as_str(), slug]).await?;
    Ok(pane)
}

fn valid_window_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.split('_').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        })
}

async fn tmux_success<const N: usize>(args: [&str; N]) -> Result<bool> {
    Ok(tmux_command()?.args(args).output().await?.status.success())
}

async fn tmux_status<const N: usize>(args: [&str; N]) -> Result<()> {
    let output = tmux_command()?.args(args).output().await?;
    if output.status.success() {
        return Ok(());
    }
    bail!(tmux_error(&output));
}

async fn tmux_output<const N: usize>(args: [&str; N]) -> Result<String> {
    let output = tmux_command()?.args(args).output().await?;
    if !output.status.success() {
        bail!(tmux_error(&output));
    }
    Ok(String::from_utf8(output.stdout)
        .context("tmux output is not valid UTF-8")?
        .trim()
        .to_string())
}

fn tmux_command() -> Result<Command> {
    let tmux = env::var("TMUX").context("Puzzle must run inside tmux")?;
    let socket = tmux
        .split(',')
        .next()
        .filter(|socket| !socket.is_empty())
        .context("TMUX does not name a server socket")?;
    let mut command = Command::new("tmux");
    command
        .arg("-S")
        .arg(socket)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null());
    Ok(command)
}

fn tmux_error(output: &Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr);
    let message = message.trim();
    if message.is_empty() {
        "tmux command failed".to_string()
    } else {
        format!("tmux: {message}")
    }
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
    let teleporter = Teleporter::initialize().await?;
    let mut terminal = ratatui::init();
    let result = run(
        &mut terminal,
        client,
        state_root,
        request_id,
        windows,
        teleporter,
    )
    .await;
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
    mut teleporter: Teleporter,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut offset = 0;
    let mut selected = 0;
    terminal.draw(|frame| draw(frame, &windows, offset, selected))?;

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
                        match key.code {
                            KeyCode::Down | KeyCode::Char('j') => {
                                let next = (selected + 1).min(windows.len().saturating_sub(1));
                                if next != selected {
                                    selected = next;
                                    teleporter.show(&windows[selected].slug).await?;
                                    offset = keep_visible(selected, offset, page);
                                    terminal.draw(|frame| draw(frame, &windows, offset, selected))?;
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                let next = selected.saturating_sub(1);
                                if next != selected {
                                    selected = next;
                                    teleporter.show(&windows[selected].slug).await?;
                                    offset = keep_visible(selected, offset, page);
                                    terminal.draw(|frame| draw(frame, &windows, offset, selected))?;
                                }
                            }
                            KeyCode::Home | KeyCode::Char('g') => {
                                selected = 0;
                                offset = 0;
                                terminal.draw(|frame| draw(frame, &windows, offset, selected))?;
                            }
                            KeyCode::End | KeyCode::Char('G') => {
                                selected = windows.len().saturating_sub(1);
                                offset = keep_visible(selected, offset, page);
                                terminal.draw(|frame| draw(frame, &windows, offset, selected))?;
                            }
                            KeyCode::Enter if !windows.is_empty() => {
                                teleporter.show(&windows[selected].slug).await?;
                                teleporter.focus().await?;
                            }
                            _ => {}
                        }
                    }
                    Event::Resize(_, _) => {
                        offset = keep_visible(
                            selected,
                            offset,
                            visible_rows(terminal.size()?.height),
                        );
                        terminal.draw(|frame| draw(frame, &windows, offset, selected))?;
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
                        let selected_slug = windows.get(selected).map(|window| window.slug.clone());
                        let next = load_windows(&client, &state_root, &mut request_id).await?;
                        if next != windows {
                            windows = next;
                            selected = selected_slug
                                .and_then(|slug| windows.iter().position(|window| window.slug == slug))
                                .unwrap_or_else(|| selected.min(windows.len().saturating_sub(1)));
                            offset = keep_visible(
                                selected,
                                offset,
                                visible_rows(terminal.size()?.height),
                            );
                            terminal.draw(|frame| draw(frame, &windows, offset, selected))?;
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

fn keep_visible(selected: usize, offset: usize, page: usize) -> usize {
    if selected < offset {
        selected
    } else if selected >= offset.saturating_add(page) {
        selected.saturating_add(1).saturating_sub(page)
    } else {
        offset
    }
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

fn draw(frame: &mut Frame<'_>, windows: &[Window], offset: usize, selected: usize) {
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
        .enumerate()
        .map(|(index, window)| {
            let (status, color) = status(&window.status);
            let line = Line::from(vec![
                Span::styled(format!("{status:<10}"), Style::default().fg(color)),
                Span::styled(
                    format!("{:<20}", window.slug),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(preview(&window.preview)),
            ]);
            if offset + index == selected {
                line.style(Style::default().bg(Color::DarkGray))
            } else {
                line
            }
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
        Paragraph::new(format!(
            "j/k select  g/G first/last  Enter open  q quit{position}"
        ))
        .style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}
