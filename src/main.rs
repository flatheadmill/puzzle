use std::collections::HashMap;
use std::collections::HashSet;
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
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_protocol::openai_models::ReasoningEffort;
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
use serde::Deserialize;
use serde::Serialize;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio::time::sleep;

const CLIENT_NAME: &str = "puzzle";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const PARKING_SESSION: &str = "puzzle-parking";
const VIEWPORT_OPTION: &str = "@puzzle-viewport-pane";
const SLUG_OPTION: &str = "@puzzle-slug";
const SUMMARY_MODEL: &str = "gpt-5.6-terra";
const SUMMARY_PROMPT_VERSION: u32 = 1;
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(30);
const SUMMARY_PROMPT: &str = "Write a plain summary of the result of the previous turn in this conversation. It must be useful for recognizing what work was taking place. If the turn was insignificant or the conversation was mostly discussion, summarize the useful subject or state instead. Use no more than 40 characters.";

#[derive(Clone, Debug, PartialEq)]
struct Window {
    slug: String,
    thread_id: String,
    completed_turn_id: Option<String>,
    preview: String,
    summary: Option<String>,
    summary_turn_id: Option<String>,
    status: ThreadStatus,
    recency_at: i64,
}

#[derive(Clone, Debug)]
struct SummaryJob {
    thread_id: String,
    turn_id: String,
}

#[derive(Debug)]
struct SummaryResult {
    job: SummaryJob,
    summary: Result<String, String>,
}

#[derive(Debug)]
struct CompletedTurnResult {
    thread_id: String,
    generation: u64,
    completed_turn_id: Result<Option<String>>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct SummaryCache {
    version: u32,
    entries: HashMap<String, SummaryCacheEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SummaryCacheEntry {
    turn_id: String,
    summary: String,
}

#[derive(Deserialize)]
struct SummaryPayload {
    summary: String,
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
    let client = connect_app_server(&socket_path, CLIENT_NAME, false).await?;
    let summary_client = connect_app_server(&socket_path, "puzzle-summary", false).await?;

    let cache_path = summary_cache_path()?;
    let mut cache = load_summary_cache(&cache_path)?;
    let (summary_jobs, summary_job_rx) = mpsc::unbounded_channel();
    let (summary_result_tx, summary_results) = mpsc::unbounded_channel();
    tokio::spawn(summary_worker(
        summary_client,
        summary_job_rx,
        summary_result_tx,
    ));

    let mut request_id = 1;
    let windows = load_windows(&client, &state_root, &cache, &mut request_id).await?;
    let mut attempted_summaries = HashSet::new();
    schedule_summaries(&windows, &mut attempted_summaries, &summary_jobs);
    let teleporter = Teleporter::initialize().await?;
    let mut terminal = ratatui::init();
    let result = run(
        &mut terminal,
        client,
        state_root,
        request_id,
        windows,
        teleporter,
        &mut cache,
        cache_path,
        summary_jobs,
        summary_results,
        attempted_summaries,
    )
    .await;
    ratatui::restore();
    result
}

async fn connect_app_server(
    socket_path: &AbsolutePathBuf,
    client_name: &str,
    experimental_api: bool,
) -> Result<RemoteAppServerClient> {
    RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        },
        client_name: client_name.to_string(),
        client_version: CLIENT_VERSION.to_string(),
        experimental_api,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 32,
    })
    .await
    .with_context(|| format!("connect {client_name} to shared Codex app-server"))
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

fn summary_cache_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("puzzle/thread-summaries.json"));
    }

    let Some(home) = env::var_os("HOME") else {
        bail!("set XDG_STATE_HOME or HOME");
    };
    Ok(PathBuf::from(home).join(".local/state/puzzle/thread-summaries.json"))
}

fn load_summary_cache(path: &Path) -> Result<SummaryCache> {
    if !path.exists() {
        return Ok(SummaryCache {
            version: SUMMARY_PROMPT_VERSION,
            ..SummaryCache::default()
        });
    }
    let encoded = fs::read_to_string(path)
        .with_context(|| format!("read summary cache at {}", path.display()))?;
    let cache: SummaryCache = serde_json::from_str(&encoded)
        .with_context(|| format!("decode summary cache at {}", path.display()))?;
    if cache.version == SUMMARY_PROMPT_VERSION {
        Ok(cache)
    } else {
        Ok(SummaryCache {
            version: SUMMARY_PROMPT_VERSION,
            ..SummaryCache::default()
        })
    }
}

fn save_summary_cache(path: &Path, cache: &SummaryCache) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("summary cache path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create Puzzle state directory at {}", parent.display()))?;
    let staged = path.with_extension("json.new");
    let encoded = serde_json::to_vec_pretty(cache).context("encode summary cache")?;
    fs::write(&staged, encoded)
        .with_context(|| format!("write staged summary cache at {}", staged.display()))?;
    fs::rename(&staged, path)
        .with_context(|| format!("install summary cache at {}", path.display()))
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
    cache: &SummaryCache,
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
                    thread_id: thread_id.clone(),
                    include_turns: false,
                },
            })
            .await
            .with_context(|| format!("read Codex thread for window {slug}"))?;
        let completed_turn_id = latest_completed_turn(client, &thread_id, request_id)
            .await
            .with_context(|| format!("read latest completed turn for window {slug}"))?;
        let cached = cache.entries.get(&thread_id);
        let summary = cached.map(|entry| entry.summary.clone());
        let summary_turn_id = cached.map(|entry| entry.turn_id.clone());

        windows.push(Window {
            slug,
            thread_id,
            completed_turn_id,
            preview: response.thread.preview,
            summary,
            summary_turn_id,
            status: response.thread.status,
            recency_at: response
                .thread
                .recency_at
                .unwrap_or(response.thread.updated_at),
        });
    }

    sort_windows(&mut windows);
    Ok(windows)
}

fn sort_windows(windows: &mut [Window]) {
    windows.sort_by(|left, right| {
        right
            .recency_at
            .cmp(&left.recency_at)
            .then_with(|| left.slug.cmp(&right.slug))
    });
}

fn recent_turns_request(thread_id: String, request_id: RequestId) -> ClientRequest {
    ClientRequest::ThreadTurnsList {
        request_id,
        params: ThreadTurnsListParams {
            thread_id,
            cursor: None,
            limit: Some(4),
            sort_direction: Some(SortDirection::Desc),
            items_view: Some(TurnItemsView::NotLoaded),
        },
    }
}

fn completed_turn_id(response: ThreadTurnsListResponse) -> Option<String> {
    response
        .data
        .into_iter()
        .find(|turn| turn.status == TurnStatus::Completed)
        .map(|turn| turn.id)
}

async fn latest_completed_turn(
    client: &RemoteAppServerClient,
    thread_id: &str,
    request_id: &mut i64,
) -> Result<Option<String>> {
    let response: ThreadTurnsListResponse = client
        .request_typed(recent_turns_request(
            thread_id.to_string(),
            next_request_id(request_id),
        ))
        .await
        .context("list recent Codex turns")?;
    Ok(completed_turn_id(response))
}

fn schedule_summary(
    window: &Window,
    attempted: &mut HashSet<(String, String)>,
    jobs: &mpsc::UnboundedSender<SummaryJob>,
) {
    if window.status != ThreadStatus::Idle {
        return;
    }
    let Some(turn_id) = &window.completed_turn_id else {
        return;
    };
    if window.summary_turn_id.as_deref() == Some(turn_id) {
        return;
    }
    let key = (window.thread_id.clone(), turn_id.clone());
    if attempted.insert(key) {
        let _ = jobs.send(SummaryJob {
            thread_id: window.thread_id.clone(),
            turn_id: turn_id.clone(),
        });
    }
}

fn schedule_summaries(
    windows: &[Window],
    attempted: &mut HashSet<(String, String)>,
    jobs: &mpsc::UnboundedSender<SummaryJob>,
) {
    for window in windows {
        schedule_summary(window, attempted, jobs);
    }
}

async fn summary_worker(
    mut client: RemoteAppServerClient,
    mut jobs: mpsc::UnboundedReceiver<SummaryJob>,
    results: mpsc::UnboundedSender<SummaryResult>,
) {
    let mut request_id = 1;
    while let Some(job) = jobs.recv().await {
        let summary = generate_summary(&mut client, &job, &mut request_id)
            .await
            .map_err(|error| format!("{error:#}"));
        if results.send(SummaryResult { job, summary }).is_err() {
            break;
        }
    }
    let _ = client.shutdown().await;
}

async fn generate_summary(
    client: &mut RemoteAppServerClient,
    job: &SummaryJob,
    request_id: &mut i64,
) -> Result<String> {
    let fork_request_id = next_request_id(request_id);
    let fork: ThreadForkResponse = tokio::time::timeout(
        SUMMARY_TIMEOUT,
        client.request_typed(ClientRequest::ThreadFork {
            request_id: fork_request_id,
            params: ThreadForkParams {
                thread_id: job.thread_id.clone(),
                last_turn_id: Some(job.turn_id.clone()),
                model: Some(SUMMARY_MODEL.to_string()),
                ephemeral: true,
                exclude_turns: true,
                ..ThreadForkParams::default()
            },
        }),
    )
    .await
    .context("summary fork timed out")?
    .context("fork source thread for summary")?;
    let fork_id = fork.thread.id;

    let summary = tokio::time::timeout(
        SUMMARY_TIMEOUT,
        generate_summary_in_fork(client, &fork_id, request_id),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("summary turn timed out")));
    let unsubscribe_request_id = next_request_id(request_id);
    let cleanup = tokio::time::timeout(
        SUMMARY_TIMEOUT,
        client.request_typed::<ThreadUnsubscribeResponse>(ClientRequest::ThreadUnsubscribe {
            request_id: unsubscribe_request_id,
            params: ThreadUnsubscribeParams {
                thread_id: fork_id.clone(),
            },
        }),
    )
    .await;

    let _ = cleanup;
    summary
}

async fn generate_summary_in_fork(
    client: &mut RemoteAppServerClient,
    fork_id: &str,
    request_id: &mut i64,
) -> Result<String> {
    let response: TurnStartResponse = client
        .request_typed(ClientRequest::TurnStart {
            request_id: next_request_id(request_id),
            params: TurnStartParams {
                thread_id: fork_id.to_string(),
                input: vec![UserInput::Text {
                    text: SUMMARY_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                model: Some(SUMMARY_MODEL.to_string()),
                effort: Some(ReasoningEffort::Low),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "summary": {
                            "type": "string",
                            "maxLength": 40
                        }
                    },
                    "required": ["summary"],
                    "additionalProperties": false
                })),
                ..TurnStartParams::default()
            },
        })
        .await
        .context("start summary turn")?;
    let turn_id = response.turn.id;

    loop {
        match client.next_event().await {
            Some(AppServerEvent::ServerNotification(notification)) => {
                if let ServerNotification::TurnCompleted(completed) = *notification
                    && completed.thread_id == fork_id
                    && completed.turn.id == turn_id
                {
                    if completed.turn.status != TurnStatus::Completed {
                        bail!("summary turn ended with {:?}", completed.turn.status);
                    }
                    return summary_from_items(&completed.turn.items);
                }
            }
            Some(AppServerEvent::Disconnected { message }) => bail!(message),
            None => bail!("summary app-server connection closed"),
            _ => {}
        }
    }
}

fn summary_from_items(items: &[ThreadItem]) -> Result<String> {
    let text = items
        .iter()
        .rev()
        .find_map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .context("summary turn returned no assistant message")?;
    let payload: SummaryPayload =
        serde_json::from_str(text).context("decode structured summary response")?;
    let summary = normalize_summary(&payload.summary);
    if summary.is_empty() {
        bail!("summary response was empty");
    }
    Ok(summary)
}

fn normalize_summary(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(40)
        .collect()
}

fn next_request_id(request_id: &mut i64) -> RequestId {
    let id = *request_id;
    *request_id += 1;
    RequestId::Integer(id)
}

fn is_active(status: &ThreadStatus) -> bool {
    matches!(status, ThreadStatus::Active { .. })
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut client: RemoteAppServerClient,
    state_root: PathBuf,
    mut request_id: i64,
    mut windows: Vec<Window>,
    mut teleporter: Teleporter,
    cache: &mut SummaryCache,
    cache_path: PathBuf,
    summary_jobs: mpsc::UnboundedSender<SummaryJob>,
    mut summary_results: mpsc::UnboundedReceiver<SummaryResult>,
    mut attempted_summaries: HashSet<(String, String)>,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let request_handle = client.request_handle();
    let (completed_turn_tx, mut completed_turn_results) = mpsc::channel(8);
    let mut thread_generations = HashMap::<String, u64>::new();
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
                                let next = if windows.is_empty() || selected + 1 >= windows.len() {
                                    0
                                } else {
                                    selected + 1
                                };
                                if next != selected {
                                    selected = next;
                                    teleporter.show(&windows[selected].slug).await?;
                                    offset = keep_visible(selected, offset, page);
                                    terminal.draw(|frame| draw(frame, &windows, offset, selected))?;
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                let next = if selected == 0 {
                                    windows.len().saturating_sub(1)
                                } else {
                                    selected - 1
                                };
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
                    Some(AppServerEvent::ServerNotification(notification)) => {
                        match *notification {
                            ServerNotification::ThreadStatusChanged(changed) => {
                                let Some(position) = windows
                                    .iter()
                                    .position(|window| window.thread_id == changed.thread_id)
                                else {
                                    continue;
                                };
                                let previous_status = windows[position].status.clone();
                                let became_active =
                                    !is_active(&previous_status) && is_active(&changed.status);
                                let became_idle = previous_status != ThreadStatus::Idle
                                    && changed.status == ThreadStatus::Idle;
                                let generation = thread_generations
                                    .entry(changed.thread_id.clone())
                                    .or_default();
                                *generation += 1;
                                let generation = *generation;

                                windows[position].status = changed.status;
                                if became_active {
                                    let recency_at = windows
                                        .iter()
                                        .map(|window| window.recency_at)
                                        .max()
                                        .unwrap_or_default()
                                        + 1;
                                    windows[position].recency_at = recency_at;
                                    let selected_slug = windows
                                        .get(selected)
                                        .map(|window| window.slug.clone());
                                    sort_windows(&mut windows);
                                    selected = selected_slug
                                        .and_then(|slug| {
                                            windows
                                                .iter()
                                                .position(|window| window.slug == slug)
                                        })
                                        .unwrap_or_else(|| {
                                            selected.min(windows.len().saturating_sub(1))
                                        });
                                    offset = keep_visible(
                                        selected,
                                        offset,
                                        visible_rows(terminal.size()?.height),
                                    );
                                }
                                terminal.draw(|frame| draw(frame, &windows, offset, selected))?;

                                if became_idle {
                                    let client = request_handle.clone();
                                    let results = completed_turn_tx.clone();
                                    let thread_id = changed.thread_id;
                                    let lookup_request_id = next_request_id(&mut request_id);
                                    tokio::spawn(async move {
                                        let completed_turn_id = async {
                                            let response: ThreadTurnsListResponse = client
                                                .request_typed(recent_turns_request(
                                                    thread_id.clone(),
                                                    lookup_request_id,
                                                ))
                                                .await
                                                .context("list recent Codex turns")?;
                                            Ok::<_, anyhow::Error>(completed_turn_id(response))
                                        }
                                        .await
                                        .with_context(|| {
                                            format!(
                                                "read latest completed turn for thread {thread_id}"
                                            )
                                        });
                                        let _ = results
                                            .send(CompletedTurnResult {
                                                thread_id,
                                                generation,
                                                completed_turn_id,
                                            })
                                            .await;
                                    });
                                }
                            }
                            ServerNotification::ThreadStarted(started)
                                if !started.thread.ephemeral
                                    && !windows.iter().any(|window| {
                                        window.thread_id == started.thread.id
                                    })
                                    && primary_sessions(&state_root).is_ok_and(|sessions| {
                                        sessions.iter().any(|(_, thread_id)| {
                                            thread_id == &started.thread.id
                                        })
                                    }) =>
                            {
                                for generation in thread_generations.values_mut() {
                                    *generation += 1;
                                }
                                let selected_slug = windows
                                    .get(selected)
                                    .map(|window| window.slug.clone());
                                let next =
                                    load_windows(&client, &state_root, cache, &mut request_id).await?;
                                if next != windows {
                                    windows = next;
                                    selected = selected_slug
                                        .and_then(|slug| {
                                            windows
                                                .iter()
                                                .position(|window| window.slug == slug)
                                        })
                                        .unwrap_or_else(|| {
                                            selected.min(windows.len().saturating_sub(1))
                                        });
                                    offset = keep_visible(
                                        selected,
                                        offset,
                                        visible_rows(terminal.size()?.height),
                                    );
                                    terminal.draw(|frame| {
                                        draw(frame, &windows, offset, selected)
                                    })?;
                                }
                                schedule_summaries(
                                    &windows,
                                    &mut attempted_summaries,
                                    &summary_jobs,
                                );
                            }
                            _ => {}
                        }
                    }
                    Some(AppServerEvent::Disconnected { message }) => bail!(message),
                    None => bail!("shared Codex app-server connection closed"),
                    _ => {}
                }
            }
            result = completed_turn_results.recv() => {
                let Some(result) = result else {
                    continue;
                };
                if thread_generations.get(&result.thread_id).copied()
                    != Some(result.generation)
                {
                    continue;
                }
                let Some(position) = windows
                    .iter()
                    .position(|window| window.thread_id == result.thread_id)
                else {
                    continue;
                };
                if windows[position].status != ThreadStatus::Idle {
                    continue;
                }
                let Ok(completed_turn_id) = result.completed_turn_id else {
                    continue;
                };
                windows[position].completed_turn_id = completed_turn_id;
                schedule_summary(
                    &windows[position],
                    &mut attempted_summaries,
                    &summary_jobs,
                );
            }
            result = summary_results.recv() => {
                let Some(result) = result else {
                    continue;
                };
                if let Ok(summary) = result.summary {
                    cache.entries.insert(
                        result.job.thread_id.clone(),
                        SummaryCacheEntry {
                            turn_id: result.job.turn_id.clone(),
                            summary: summary.clone(),
                        },
                    );
                    save_summary_cache(&cache_path, cache)?;
                    if let Some(window) = windows.iter_mut().find(|window| {
                        window.thread_id == result.job.thread_id
                            && window.completed_turn_id.as_deref()
                                == Some(result.job.turn_id.as_str())
                    }) {
                        window.summary = Some(summary);
                        window.summary_turn_id = Some(result.job.turn_id);
                        terminal.draw(|frame| draw(frame, &windows, offset, selected))?;
                    }
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
                Span::raw(
                    window
                        .summary
                        .clone()
                        .unwrap_or_else(|| preview(&window.preview)),
                ),
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
