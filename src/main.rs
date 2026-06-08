// Puzzle: protocol translator between the Codex TUI and Easement.
//
// Connects to Easement over WebSocket (bus protocol).
// Runs the Codex TUI in-process, connected via Unix socket.
// Translates between the two protocols.

// mod translate;
// mod easement;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::OnceLock;

use color_eyre::eyre::Result;
use serde::Serialize;
use serde_json::Value;

// Codex TUI protocol types. Our subset, shaped from the reference at
// ~/code/reference/codex/codex-rs/app-server-protocol/. Only the fields the
// TUI needs to render. Compiles in milliseconds because there are no macro
// dependencies, no schemars, no ts-rs.

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Turn {
    id: String,
    items: Vec<ThreadItem>,
    #[serde(default)]
    items_view: String,
    status: TurnStatus,
    error: Option<Value>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ThreadItem {
    #[serde(rename_all = "camelCase")]
    UserMessage { id: String, content: Vec<UserInput> },
    #[serde(rename_all = "camelCase")]
    AgentMessage { id: String, text: String },
    #[serde(rename_all = "camelCase")]
    Reasoning {
        id: String,
        summary: Vec<String>,
        content: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    CommandExecution {
        id: String,
        command: String,
        cwd: String,
        source: String,
        status: String,
        command_actions: Vec<Value>,
        aggregated_output: Option<String>,
        exit_code: Option<i32>,
        duration_ms: Option<i64>,
    },
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
enum UserInput {
    Text { text: String },
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    result: Value,
}

// Easement broadcast messages. What Easement sends to Puzzle over the WebSocket.
#[derive(serde::Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
enum Inbound {
    History {
        slug: String,
        transcript: String,
        #[serde(flatten)]
        event: HistoryInbound,
    },
    Delta {
        slug: String,
        transcript: String,
        event: Value,
    },
    Usage {
        slug: String,
        transcript: String,
        #[serde(flatten)]
        usage: Value,
    },
    Turn {
        slug: String,
        transcript: String,
        #[serde(flatten)]
        event: TurnInbound,
    },
    UserMessage {
        slug: String,
        transcript: String,
        text: String,
    },
    ToolResult {
        slug: String,
        transcript: String,
        tool_use_id: String,
        output: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(serde::Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum HistoryInbound {
    Begin {
        replay_id: String,
        last_uuid: Option<String>,
    },
    Entry {
        replay_id: String,
        entry: Value,
    },
}

#[derive(serde::Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum TurnInbound {
    Started { turn_id: String },
    Completed { turn_id: String, status: String },
}

// What Puzzle sends to Easement.
#[derive(serde::Serialize)]
#[serde(tag = "what", rename_all = "snake_case")]
enum Outbound {
    Socket(SocketOutbound),
    History(HistoryOutbound),
    Turn(TurnOutbound),
    Shell(ShellOutbound),
}

#[derive(serde::Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum SocketOutbound {
    Connect {
        who: String,
        r#where: String,
        tools: Vec<Value>,
    },
}

#[derive(serde::Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum HistoryOutbound {
    Replay {
        slug: String,
        transcript: String,
        replay_id: String,
    },
}

#[derive(serde::Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum TurnOutbound {
    Start {
        slug: String,
        transcript: String,
        turn_id: String,
        message: String,
    },
    Steer {
        slug: String,
        transcript: String,
        message: String,
        expected_turn_id: String,
    },
}

#[derive(serde::Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum ShellOutbound {
    Run {
        slug: String,
        transcript: String,
        command: String,
    },
}

// Parsed events from the Easement broadcast, ready for the select loop.
#[derive(Debug)]
enum EasementEvent {
    HistoryBegin {
        replay_id: String,
        last_uuid: Option<String>,
        transcript: String,
    },
    HistoryEntry {
        replay_id: String,
        entry: Value,
    },
    Delta(Value),
    Usage(Value),
    TurnStarted {
        turn_id: String,
    },
    TurnCompleted {
        turn_id: String,
        status: String,
    },
    UserMessage {
        text: String,
    },
    ToolResult {
        tool_use_id: String,
        output: String,
        is_error: bool,
    },
}

fn send_outbound(tx: &tokio::sync::mpsc::UnboundedSender<String>, msg: Outbound) {
    if let Ok(json) = serde_json::to_string(&msg) {
        let _ = tx.send(json);
    }
}

use tokio::net::UnixListener;
use tokio::sync::broadcast;

#[derive(Clone, Serialize)]
struct LogMessage {
    when: String,
    level: u8,
    who: &'static str,
    what: &'static str,
    why: &'static str,
    #[serde(flatten)]
    payload: Value,
}

#[derive(Serialize)]
struct LogEntry {
    when: String,
    what: LogMessage,
}

static LOG: OnceLock<broadcast::Sender<LogMessage>> = OnceLock::new();

fn log(level: u8, msg: LogMessage) {
    if let Some(tx) = LOG.get() {
        let _ = tx.send(LogMessage { level, ..msg });
    }
}

macro_rules! trace {
    ($who:expr, $what:expr, $why:expr $(, $key:tt: $val:expr)* $(,)?) => {
        crate::log(0, LogMessage {
            when: now(), level: 0, who: $who, what: $what, why: $why,
            payload: serde_json::json!({ $($key: $val),* }),
        })
    };
}

macro_rules! wire {
    ($who:expr, $what:expr, $why:expr $(, $key:tt: $val:expr)* $(,)?) => {
        crate::log(1, LogMessage {
            when: now(), level: 1, who: $who, what: $what, why: $why,
            payload: serde_json::json!({ $($key: $val),* }),
        })
    };
}

macro_rules! dump {
    ($who:expr, $what:expr, $why:expr $(, $key:tt: $val:expr)* $(,)?) => {
        crate::log(2, LogMessage {
            when: now(), level: 2, who: $who, what: $what, why: $why,
            payload: serde_json::json!({ $($key: $val),* }),
        })
    };
}

macro_rules! error {
    ($who:expr, $what:expr, $how:expr, $error:expr $(, $key:tt: $val:expr)* $(,)?) => {
        crate::log(0, LogMessage {
            when: now(), level: 0, who: $who, what: $what, why: "error",
            payload: serde_json::json!({ "how": $how, "error": $error.to_string() $(, $key: $val)* }),
        })
    };
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

async fn init_log(slug: &str) {
    let home = std::env::var("HOME").expect("HOME not set");
    let log_dir = PathBuf::from(&home).join(".local/state/puzzle");
    let _ = tokio::fs::create_dir_all(&log_dir).await;

    let log_path = log_dir.join(format!("{}.jsonl", slug));

    let (tx, _) = broadcast::channel::<LogMessage>(4096);
    let mut rx = tx.subscribe();
    LOG.set(tx).expect("log already initialized");

    tokio::spawn(async move {
        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("cannot open log file {}: {}", log_path.display(), e);
                return;
            }
        };

        use tokio::io::AsyncWriteExt;
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let entry = LogEntry {
                        when: now(),
                        what: msg,
                    };
                    if let Ok(mut line) = serde_json::to_string(&entry) {
                        line.push('\n');
                        let _ = file.write_all(line.as_bytes()).await;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let shed = LogEntry {
                        when: now(),
                        what: LogMessage {
                            when: now(),
                            level: 0,
                            who: "log",
                            what: "lifecycle",
                            why: "shed",
                            payload: serde_json::json!({ "count": n }),
                        },
                    };
                    if let Ok(mut line) = serde_json::to_string(&shed) {
                        line.push('\n');
                        let _ = file.write_all(line.as_bytes()).await;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn is_our_tool(name: &str) -> bool {
    name.contains("wicket") || name.starts_with("mcp__o__")
}

fn tool_function(name: &str, input: &Value) -> Option<String> {
    if name == "mcp__o__call" {
        input
            .get("f")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else if name.contains("wicket") {
        name.rsplit("__").next().map(|s| s.to_string())
    } else {
        None
    }
}

fn tool_args<'a>(name: &str, input: &'a Value) -> &'a Value {
    if name == "mcp__o__call" {
        input.get("args").unwrap_or(input)
    } else {
        input
    }
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct ToolKey {
    index: i64,
    tool_use_id: String,
}

struct ToolRun {
    index: i64,
    tool_use_id: String,
    name: String,
    input_json: String,
    command: Option<String>,
    started: bool,
}

struct ToolRunResult {
    output: String,
    is_error: bool,
    complete: bool,
}

const MODEL_CONTEXT_WINDOW: i64 = 1_000_000;
const USAGE_SMOOTHING_SAMPLES: usize = 3;

#[derive(Clone, Debug)]
struct UsageSample {
    uncached_input_tokens: i64,
    cached_input_tokens: i64,
    cache_creation_input_tokens: i64,
    output_tokens: i64,
    context_size: i64,
}

fn usage_i64(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(|v| v.as_i64()).unwrap_or(0).max(0)
}

fn usage_sample_from_value(value: &Value) -> UsageSample {
    let uncached_input_tokens = usage_i64(value, "input_tokens");
    let cached_input_tokens = usage_i64(value, "cache_read_input_tokens");
    let cache_creation_input_tokens = usage_i64(value, "cache_creation_input_tokens");
    let output_tokens = usage_i64(value, "output_tokens");
    UsageSample {
        uncached_input_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        output_tokens,
        context_size: uncached_input_tokens
            + cached_input_tokens
            + cache_creation_input_tokens
            + output_tokens,
    }
}

fn parse_usage_sample(usage: &Value) -> Option<UsageSample> {
    if let Some(iterations) = usage.get("iterations").and_then(|v| v.as_array()) {
        let samples = iterations.iter().map(usage_sample_from_value);
        return samples.max_by_key(|sample| sample.context_size);
    }
    Some(usage_sample_from_value(usage))
}

fn push_usage_sample(samples: &mut VecDeque<UsageSample>, sample: UsageSample) {
    samples.push_back(sample);
    while samples.len() > USAGE_SMOOTHING_SAMPLES {
        samples.pop_front();
    }
}

fn displayed_usage_sample(samples: &VecDeque<UsageSample>) -> Option<UsageSample> {
    samples
        .iter()
        .cloned()
        .max_by_key(|sample| sample.context_size)
}

fn token_usage_notification(thread_id: &str, turn_id: &str, sample: &UsageSample) -> Value {
    let input_tokens = sample.uncached_input_tokens
        + sample.cached_input_tokens
        + sample.cache_creation_input_tokens;
    serde_json::json!({
        "method": "thread/tokenUsage/updated",
        "params": {
            "threadId": thread_id,
            "turnId": turn_id,
            "tokenUsage": {
                "total": {
                    "totalTokens": sample.context_size,
                    "inputTokens": input_tokens,
                    "cachedInputTokens": sample.cached_input_tokens,
                    "outputTokens": sample.output_tokens,
                    "reasoningOutputTokens": 0
                },
                "last": {
                    "totalTokens": sample.context_size,
                    "inputTokens": input_tokens,
                    "cachedInputTokens": sample.cached_input_tokens,
                    "outputTokens": sample.output_tokens,
                    "reasoningOutputTokens": 0
                },
                "modelContextWindow": MODEL_CONTEXT_WINDOW
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_prefers_iteration_context_over_cumulative_top_level_cache() {
        let usage = json!({
            "input_tokens": 3,
            "cache_creation_input_tokens": 403,
            "cache_read_input_tokens": 3_378_745,
            "output_tokens": 920,
            "iterations": [{
                "input_tokens": 3,
                "cache_creation_input_tokens": 403,
                "cache_read_input_tokens": 676_934,
                "output_tokens": 7
            }]
        });

        let sample = parse_usage_sample(&usage).expect("usage sample");
        assert_eq!(sample.context_size, 677_347);
        assert_eq!(sample.cached_input_tokens, 676_934);
        assert_eq!(sample.cache_creation_input_tokens, 403);
        assert_eq!(sample.output_tokens, 7);
    }

    #[test]
    fn token_usage_notification_keeps_context_and_input_fields_consistent() {
        let sample = UsageSample {
            uncached_input_tokens: 3,
            cached_input_tokens: 676_934,
            cache_creation_input_tokens: 403,
            output_tokens: 7,
            context_size: 677_347,
        };

        let notification = token_usage_notification("thread", "turn", &sample);
        let total = &notification["params"]["tokenUsage"]["total"];
        assert_eq!(total["totalTokens"], 677_347);
        assert_eq!(total["inputTokens"], 677_340);
        assert_eq!(total["cachedInputTokens"], 676_934);
        assert_eq!(total["outputTokens"], 7);
    }
}

async fn send_tui(
    tui_sink: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
        tokio_tungstenite::tungstenite::Message,
    >,
    notif: Value,
) {
    let _ = futures_util::SinkExt::send(
        tui_sink,
        tokio_tungstenite::tungstenite::Message::text(notif.to_string()),
    )
    .await;
}

fn pump_tool_runs(
    thread_id: &str,
    turn_id: &str,
    cwd: &str,
    tool_heap: &mut BinaryHeap<Reverse<ToolKey>>,
    tool_runs: &mut HashMap<String, ToolRun>,
    tool_results: &mut HashMap<String, ToolRunResult>,
) -> Vec<Value> {
    let mut notifications = Vec::new();

    loop {
        let Some(Reverse(key)) = tool_heap.peek().cloned() else {
            break;
        };

        let Some(tool) = tool_runs.get_mut(&key.tool_use_id) else {
            tool_heap.pop();
            continue;
        };

        if tool.index != key.index {
            tool_heap.pop();
            continue;
        }

        if !tool.started {
            let Some(command) = tool.command.as_deref() else {
                break;
            };
            let notif = serde_json::json!({
                "method": "item/started",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "startedAtMs": chrono::Utc::now().timestamp_millis(),
                    "item": {
                        "type": "commandExecution",
                        "id": tool.tool_use_id,
                        "command": command,
                        "cwd": cwd,
                        "source": "agent",
                        "status": "inProgress",
                        "commandActions": [],
                        "aggregatedOutput": null,
                        "exitCode": null,
                        "durationMs": null
                    }
                }
            });
            notifications.push(notif);
            tool.started = true;
        }

        let Some(result) = tool_results.get(&key.tool_use_id) else {
            break;
        };
        if !result.complete {
            break;
        }

        tool_heap.pop();
        let tool = tool_runs
            .remove(&key.tool_use_id)
            .expect("peeked tool missing");
        let result = tool_results
            .remove(&key.tool_use_id)
            .expect("peeked result missing");
        let command = tool.command.unwrap_or_default();

        if !result.output.is_empty() {
            let notif = serde_json::json!({
                "method": "item/commandExecution/outputDelta",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": tool.tool_use_id,
                    "delta": result.output
                }
            });
            notifications.push(notif);
        }

        let status = if result.is_error {
            "failed"
        } else {
            "completed"
        };
        let notif = serde_json::json!({
            "method": "item/completed",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "completedAtMs": chrono::Utc::now().timestamp_millis(),
                "item": {
                    "type": "commandExecution",
                    "id": tool.tool_use_id,
                    "command": command,
                    "cwd": cwd,
                    "source": "agent",
                    "status": status,
                    "commandActions": [],
                    "aggregatedOutput": result.output,
                    "exitCode": if result.is_error { 1 } else { 0 },
                    "durationMs": null
                }
            }
        });
        notifications.push(notif);
    }

    notifications
}

fn is_interrupt_marker(entry: &Value) -> bool {
    let kind = entry.get("who").and_then(|v| v.as_str()).unwrap_or("");
    if kind != "user" {
        return false;
    }
    entry
        .get("blocks")
        .and_then(|v| v.as_array())
        .and_then(|blocks| blocks.first())
        .and_then(|b| b.get("text"))
        .and_then(|v| v.as_str())
        .map(|t| t.starts_with("[Request interrupted by user"))
        .unwrap_or(false)
}

fn is_user_text_entry(entry: &Value) -> bool {
    let kind = entry.get("who").and_then(|v| v.as_str()).unwrap_or("");
    if kind != "user" {
        return false;
    }
    entry
        .get("blocks")
        .and_then(|v| v.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
        })
        .unwrap_or(false)
}

fn flush_turn(turns: &mut Vec<Turn>, items: &mut Vec<ThreadItem>, status: TurnStatus) {
    if items.is_empty() {
        return;
    }
    turns.push(Turn {
        id: uuid::Uuid::new_v4().to_string(),
        items: items.drain(..).collect(),
        items_view: "full".to_string(),
        status,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    });
}

fn build_turns_from_entries(entries: &[Value], cwd: &str) -> Vec<Turn> {
    let mut turns: Vec<Turn> = vec![];
    let mut current_items: Vec<ThreadItem> = vec![];

    let mut i = 0;
    while i < entries.len() {
        let entry = &entries[i];

        if is_interrupt_marker(entry) {
            flush_turn(&mut turns, &mut current_items, TurnStatus::Interrupted);
            i += 1;
            continue;
        }

        let kind = entry
            .get("who")
            .and_then(|v| v.as_str())
            .expect("missing who");
        let blocks = entry.get("blocks").and_then(|v| v.as_array());
        let entry_id = entry
            .get("uuid")
            .and_then(|v| v.as_str())
            .expect("missing uuid");

        if is_user_text_entry(entry) && !current_items.is_empty() {
            flush_turn(&mut turns, &mut current_items, TurnStatus::Completed);
        }

        if let Some(blocks) = blocks {
            for block in blocks {
                let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match (kind, btype) {
                    ("user", "text") => {
                        let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        current_items.push(ThreadItem::UserMessage {
                            id: entry_id.to_string(),
                            content: vec![UserInput::Text {
                                text: text.to_string(),
                            }],
                        });
                    }
                    ("assistant", "text") => {
                        let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        current_items.push(ThreadItem::AgentMessage {
                            id: entry_id.to_string(),
                            text: text.to_string(),
                        });
                    }
                    ("assistant", "thinking") => {
                        let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        current_items.push(ThreadItem::Reasoning {
                            id: entry_id.to_string(),
                            summary: vec![format!("**Thinking**\n\n{}", text)],
                            content: vec![],
                        });
                    }
                    ("assistant", "tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .expect("missing tool name");
                        if !is_our_tool(name) {
                            continue;
                        }

                        let input = block.get("input").unwrap_or(&Value::Null);
                        let f = tool_function(name, input).unwrap_or_default();
                        let args = tool_args(name, input);

                        let mut tool_output = String::new();
                        if i + 1 < entries.len() {
                            let next = &entries[i + 1];
                            if next.get("who").and_then(|v| v.as_str()) == Some("user") {
                                if let Some(next_blocks) =
                                    next.get("blocks").and_then(|v| v.as_array())
                                {
                                    for nb in next_blocks {
                                        if nb.get("type").and_then(|v| v.as_str())
                                            == Some("tool_result")
                                        {
                                            tool_output = nb
                                                .get("content")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                        }
                                    }
                                }
                            }
                        }

                        let command = args
                            .get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&f)
                            .to_string();
                        current_items.push(ThreadItem::CommandExecution {
                            id: entry_id.to_string(),
                            command,
                            cwd: cwd.to_string(),
                            source: "agent".to_string(),
                            status: "completed".to_string(),
                            command_actions: vec![],
                            aggregated_output: if tool_output.is_empty() {
                                None
                            } else {
                                Some(tool_output)
                            },
                            exit_code: Some(0),
                            duration_ms: None,
                        });
                    }
                    ("user", "tool_result") => {}
                    _ => {}
                }
            }
        }
        i += 1;
    }

    flush_turn(&mut turns, &mut current_items, TurnStatus::Completed);
    turns
}

#[derive(clap::Parser)]
#[command(
    name = "puzzle",
    about = "Protocol translator between the Codex TUI and Easement"
)]
struct Args {
    slug: String,
    #[arg(long)]
    full: bool,
    #[arg(long)]
    codex_tui: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = <Args as clap::Parser>::parse();
    let home = std::env::var("HOME").expect("HOME not set");
    let codex_home = PathBuf::from(&home).join(".config/puzzle");

    let slug = args.slug;
    let use_codex_tui = args.codex_tui;
    let intent = if args.full { "full" } else { "latest" };

    init_log(&slug).await;
    trace!("puzzle", "lifecycle", "started", "slug": slug, "intent": intent);

    let pane_dir = PathBuf::from(&home).join("pane").join(&slug);
    tokio::fs::create_dir_all(&pane_dir).await?;

    let (socket_dir, socket_path) = if use_codex_tui {
        let dir = codex_home.join("app-server-control");
        let path = dir.join("app-server-control.sock");
        (dir, path)
    } else {
        let dir = PathBuf::from(&home)
            .join(".local/state/puzzle")
            .join(&slug)
            .join("socket");
        let path = dir.join("puzzle.sock");
        (dir, path)
    };
    tokio::fs::create_dir_all(&socket_dir).await?;
    if socket_path.exists() {
        tokio::fs::remove_file(&socket_path).await?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    trace!("puzzle", "lifecycle", "listening", "path": socket_path.display().to_string());

    // Connect to Easement.
    let port = std::env::var("EASEMENT_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(6502);
    let easement_url = format!("ws://127.0.0.1:{}", port);
    let (ws_stream, _) = tokio_tungstenite::connect_async(&easement_url)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;
    let (mut ws_sink, ws_reader) = futures_util::StreamExt::split(ws_stream);

    let (easement_tx, mut easement_outbound_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(msg) = easement_outbound_rx.recv().await {
            if futures_util::SinkExt::send(
                &mut ws_sink,
                tokio_tungstenite::tungstenite::Message::text(msg),
            )
            .await
            .is_err()
            {
                break;
            }
        }
    });

    // Identify ourselves.
    send_outbound(
        &easement_tx,
        Outbound::Socket(SocketOutbound::Connect {
            who: "puzzle".to_string(),
            r#where: "localhost".to_string(),
            tools: vec![],
        }),
    );

    // Reader task: parse Easement broadcasts into EasementEvents.
    let my_slug = slug.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<EasementEvent>(256);
    let mut pinned_transcript = String::new();
    tokio::spawn(async move {
        let mut stream = ws_reader;
        while let Some(result) = futures_util::StreamExt::next(&mut stream).await {
            match result {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                    let msg: Inbound = match serde_json::from_str(&text) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let (msg_slug, msg_transcript) = match &msg {
                        Inbound::History {
                            slug, transcript, ..
                        } => (slug.as_str(), transcript.as_str()),
                        Inbound::Delta {
                            slug, transcript, ..
                        } => (slug.as_str(), transcript.as_str()),
                        Inbound::Usage {
                            slug, transcript, ..
                        } => (slug.as_str(), transcript.as_str()),
                        Inbound::Turn {
                            slug, transcript, ..
                        } => (slug.as_str(), transcript.as_str()),
                        Inbound::UserMessage {
                            slug, transcript, ..
                        } => (slug.as_str(), transcript.as_str()),
                        Inbound::ToolResult {
                            slug, transcript, ..
                        } => (slug.as_str(), transcript.as_str()),
                    };
                    if msg_slug != my_slug {
                        continue;
                    }
                    let event = match msg {
                        Inbound::History {
                            transcript,
                            event:
                                HistoryInbound::Begin {
                                    replay_id,
                                    last_uuid,
                                },
                            ..
                        } => EasementEvent::HistoryBegin {
                            replay_id,
                            last_uuid,
                            transcript,
                        },
                        Inbound::History {
                            event: HistoryInbound::Entry { replay_id, entry },
                            ..
                        } => EasementEvent::HistoryEntry { replay_id, entry },
                        Inbound::Delta { event, .. } => EasementEvent::Delta(event),
                        Inbound::Usage { usage, .. } => EasementEvent::Usage(usage),
                        Inbound::Turn {
                            event: TurnInbound::Started { turn_id },
                            ..
                        } => EasementEvent::TurnStarted { turn_id },
                        Inbound::Turn {
                            event: TurnInbound::Completed { turn_id, status },
                            ..
                        } => EasementEvent::TurnCompleted { turn_id, status },
                        Inbound::UserMessage { text, .. } => EasementEvent::UserMessage { text },
                        Inbound::ToolResult {
                            tool_use_id,
                            output,
                            is_error,
                            ..
                        } => EasementEvent::ToolResult {
                            tool_use_id,
                            output,
                            is_error,
                        },
                    };
                    if event_tx.send(event).await.is_err() {
                        break;
                    }
                }
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                Err(_) => break,
                _ => {}
            }
        }
    });

    trace!("puzzle", "easement", "connected", "port": port);

    // Request history.
    let replay_id = uuid::Uuid::new_v4().to_string();
    send_outbound(
        &easement_tx,
        Outbound::History(HistoryOutbound::Replay {
            slug: slug.clone(),
            transcript: intent.to_string(),
            replay_id: replay_id.clone(),
        }),
    );

    let mut history: Vec<serde_json::Value> = vec![];
    let mut transcript_id = String::new();
    let mut last_uuid: Option<String> = None;
    let mut initial_usage_samples: VecDeque<UsageSample> = VecDeque::new();

    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(10), event_rx.recv()).await {
            Ok(Some(EasementEvent::HistoryBegin {
                replay_id: rid,
                last_uuid: lu,
                transcript,
            })) if rid == replay_id => {
                trace!("puzzle", "history", "begin", "last_uuid": lu, "transcript": transcript, "replay_id": replay_id);
                transcript_id = transcript;
                // transcript pinned for this session
                if lu.is_none() {
                    break;
                }
                last_uuid = lu;
            }
            Ok(Some(EasementEvent::HistoryEntry {
                replay_id: rid,
                entry,
            })) if rid == replay_id => {
                let done = last_uuid.as_deref() == entry.get("uuid").and_then(|v| v.as_str());
                history.push(entry);
                if done {
                    break;
                }
            }
            Ok(Some(EasementEvent::Usage(usage))) => {
                if let Some(sample) = parse_usage_sample(&usage) {
                    trace!(
                        "puzzle", "history", "usage",
                        "context_size": sample.context_size,
                        "uncached_input_tokens": sample.uncached_input_tokens,
                        "cached_input_tokens": sample.cached_input_tokens,
                        "cache_creation_input_tokens": sample.cache_creation_input_tokens,
                        "output_tokens": sample.output_tokens,
                    );
                    push_usage_sample(&mut initial_usage_samples, sample);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                trace!("puzzle", "history", "timeout", "replay_id": replay_id);
                break;
            }
        }
    }

    trace!("puzzle", "history", "loaded", "entries": history.len(), "transcript": transcript_id);

    // Spawn puzzle-tui as a child process. It gets CODEX_HOME in its environment
    // and the socket path as its argument. No unsafe set_var needed.
    let mut tui_child = if use_codex_tui {
        tokio::process::Command::new("codex-tui")
            .env("CODEX_HOME", &codex_home)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("cannot spawn codex-tui")
    } else {
        tokio::process::Command::new("puzzle-tui")
            .arg(&socket_path)
            .env("CODEX_HOME", &codex_home)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("cannot spawn puzzle-tui")
    };

    let slug_owned = slug.clone();
    tokio::spawn(async move {
        // Accept the TUI's WebSocket connection.
        let tui_ws = loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    error!("puzzle", "tui", "accept_error", e);
                    return;
                }
            };
            match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => {
                    trace!("puzzle", "tui", "connected");
                    break ws;
                }
                Err(e) => {
                    error!("puzzle", "tui", "handshake_failed", e);
                }
            }
        };

        let (mut tui_sink, mut tui_stream) = futures_util::StreamExt::split(tui_ws);
        let thread_id = uuid::Uuid::new_v4().to_string();
        let home = std::env::var("HOME").unwrap_or_default();
        let cwd = format!("{}/pane/{}", home, slug_owned);

        let mut active_turn_id: Option<String> = None;
        let mut active_item_id: Option<String> = None;
        let mut active_reasoning_item_id: Option<String> = None;
        let mut in_thinking = false;
        let mut tool_heap: BinaryHeap<Reverse<ToolKey>> = BinaryHeap::new();
        let mut tool_runs: HashMap<String, ToolRun> = HashMap::new();
        let mut tool_results: HashMap<String, ToolRunResult> = HashMap::new();
        let mut tool_id_by_index: HashMap<i64, String> = HashMap::new();
        let mut last_stop_reason: Option<String> = None;
        let mut pending_turn_start_response: Option<Value> = None;
        let mut pending_turn_start_message: Option<String> = None;
        let mut usage_samples = initial_usage_samples;

        loop {
            tokio::select! {
                // TUI -> Puzzle: JSON-RPC requests from the Codex TUI.
                msg = futures_util::StreamExt::next(&mut tui_stream) => {
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            let raw: Value = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };

                            if raw.get("id").is_none() {
                                dump!("puzzle", "tui", "notification", "payload": raw);
                                continue;
                            }

                            let id = raw.get("id").cloned().unwrap_or(Value::Null);
                            let method = raw.get("method").and_then(|v| v.as_str()).unwrap_or("");
                            wire!("puzzle", "tui", "request", "method": method, "id": id);

                            let mut send_initial_usage = false;
                            let result: Value = match method {
                                "initialize" => {
                                    serde_json::json!({
                                        "protocolVersion": "2.0",
                                        "serverInfo": { "name": "puzzle", "version": "0.1.0" },
                                        "capabilities": {}
                                    })
                                }
                                "account/read" => {
                                    serde_json::json!({
                                        "account": { "type": "apiKey" },
                                        "requiresOpenaiAuth": false
                                    })
                                }
                                "model/list" => {
                                    serde_json::json!({
                                        "data": [{
                                            "id": "claude-opus-4-6",
                                            "model": "claude-opus-4-6",
                                            "displayName": "Claude Opus 4.6",
                                            "description": "Anthropic Claude Opus 4.6",
                                            "hidden": false,
                                            "supportedReasoningEfforts": [
                                                { "reasoningEffort": "high", "description": "High" }
                                            ],
                                            "defaultReasoningEffort": "high",
                                            "inputModalities": ["text", "image"],
                                            "supportsPersonality": false,
                                            "serviceTiers": [],
                                            "isDefault": true
                                        }],
                                        "nextCursor": null
                                    })
                                }
                                "thread/start" | "thread/resume" => {
                                    let turns = build_turns_from_entries(&history, &cwd);
                                    trace!("puzzle", "tui", "thread_start", "turns": turns.len());
                                    send_initial_usage = true;
                                    serde_json::json!({
                                        "thread": {
                                            "id": thread_id,
                                            "sessionId": uuid::Uuid::new_v4().to_string(),
                                            "preview": "",
                                            "ephemeral": false,
                                            "modelProvider": "anthropic",
                                            "createdAt": 0,
                                            "updatedAt": 0,
                                            "status": { "type": "idle" },
                                            "cwd": cwd,
                                            "cliVersion": "0.1.0",
                                            "source": "cli",
                                            "turns": turns
                                        },
                                        "model": "claude-opus-4-6",
                                        "modelProvider": "anthropic",
                                        "cwd": cwd,
                                        "approvalPolicy": "on-failure",
                                        "approvalsReviewer": "user",
                                        "sandbox": { "type": "dangerFullAccess" },
                                        "instructionSources": []
                                    })
                                }
                                "skills/list" | "hooks/list" | "plugin/list" => {
                                    serde_json::json!({ "data": [] })
                                }
                                "config/read" => {
                                    let codex_home = std::env::var("CODEX_HOME").unwrap_or_default();
                                    let config_path = std::path::Path::new(&codex_home).join("config.toml");
                                    let config: Value = if config_path.exists() {
                                        match std::fs::read_to_string(&config_path) {
                                            Ok(content) => {
                                                match toml::from_str::<toml::Value>(&content) {
                                                    Ok(v) => serde_json::to_value(v).unwrap_or_default(),
                                                    Err(_) => serde_json::json!({}),
                                                }
                                            }
                                            Err(_) => serde_json::json!({}),
                                        }
                                    } else {
                                        serde_json::json!({})
                                    };
                                    serde_json::json!({
                                        "config": config,
                                        "origins": {},
                                        "layers": null
                                    })
                                }
                                "turn/start" => {
                                    let params = raw.get("params").cloned().unwrap_or_default();
                                    let message = params.get("input")
                                        .and_then(|input| input.as_array())
                                        .and_then(|items| items.iter().find_map(|item| {
                                            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                                item.get("text").and_then(|t| t.as_str())
                                            } else {
                                                None
                                            }
                                        }))
                                        .unwrap_or("")
                                        .to_string();

                                    let turn_id = uuid::Uuid::new_v4().to_string();
                                    trace!("puzzle", "tui", "turn_start", "turn_id": turn_id, "message": message);

                                    send_outbound(&easement_tx, Outbound::Turn(TurnOutbound::Start {
                                        slug: slug_owned.clone(),
                                        transcript: transcript_id.clone(),
                                        turn_id: turn_id.clone(),
                                        message: message.clone(),
                                    }));

                                    pending_turn_start_response = Some(id.clone());
                                    pending_turn_start_message = Some(message);

                                    // Response deferred until TurnStarted arrives from Easement.
                                    continue;
                                }
                                "turn/steer" => {
                                    let params = raw.get("params").cloned().unwrap_or_default();
                                    let expected_turn_id = params.get("expectedTurnId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let message = params.get("input")
                                        .and_then(|input| input.as_array())
                                        .and_then(|items| items.iter().find_map(|item| {
                                            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                                item.get("text").and_then(|t| t.as_str())
                                            } else {
                                                None
                                            }
                                        }))
                                        .unwrap_or("")
                                        .to_string();

                                    trace!("puzzle", "tui", "turn_steer", "message": message, "expected_turn_id": expected_turn_id);

                                    send_outbound(&easement_tx, Outbound::Turn(TurnOutbound::Steer {
                                        slug: slug_owned.clone(),
                                        transcript: transcript_id.clone(),
                                        message,
                                        expected_turn_id: expected_turn_id.clone(),
                                    }));

                                    serde_json::json!({ "turnId": expected_turn_id })
                                }
                                "turn/interrupt" => {
                                    trace!("puzzle", "tui", "turn_interrupt");
                                    // TODO: send interrupt to Easement
                                    serde_json::json!({})
                                }
                                "thread/shellCommand" => {
                                    let params = raw.get("params").cloned().unwrap_or_default();
                                    let command = params.get("command")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    trace!("puzzle", "tui", "shell", "command": command);

                                    send_outbound(&easement_tx, Outbound::Shell(ShellOutbound::Run {
                                        slug: slug_owned.clone(),
                                        transcript: transcript_id.clone(),
                                        command,
                                    }));

                                    serde_json::json!({})
                                }
                                "thread/unsubscribe" => {
                                    serde_json::json!({})
                                }
                                _ => {
                                    trace!("puzzle", "tui", "unhandled", "method": method);
                                    serde_json::json!({})
                                }
                            };

                            let resp = JsonRpcResponse { jsonrpc: "2.0", id, result };
                            if let Ok(json) = serde_json::to_string(&resp) {
                                let _ = futures_util::SinkExt::send(&mut tui_sink,
                                    tokio_tungstenite::tungstenite::Message::text(json)).await;
                            }
                            if send_initial_usage {
                                if let Some(sample) = displayed_usage_sample(&usage_samples) {
                                    let turn_id = active_turn_id.as_deref().unwrap_or("");
                                    trace!(
                                        "puzzle", "tui", "initial_usage",
                                        "context_size": sample.context_size,
                                        "turn_id": turn_id,
                                    );
                                    send_tui(
                                        &mut tui_sink,
                                        token_usage_notification(&thread_id, turn_id, &sample),
                                    ).await;
                                }
                            }
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                            trace!("puzzle", "tui", "disconnected");
                            break;
                        }
                        _ => {}
                    }
                }
                // Easement -> Puzzle: broadcast events from Easement.
                event = event_rx.recv() => {
                    match event {
                        Some(EasementEvent::TurnStarted { turn_id }) => {
                            trace!("puzzle", "easement", "turn_started", "turn_id": turn_id);
                            active_turn_id = Some(turn_id.clone());
                            let item_id = uuid::Uuid::new_v4().to_string();
                            active_item_id = Some(item_id.clone());

                            // If we were waiting to respond to the TUI's turn/start request, do it now.
                            if let Some(response_id) = pending_turn_start_response.take() {
                                let message = pending_turn_start_message.take().unwrap_or_default();
                                let resp = JsonRpcResponse { jsonrpc: "2.0", id: response_id, result: serde_json::json!({
                                    "turn": {
                                        "id": turn_id,
                                        "items": [{
                                            "type": "userMessage",
                                            "id": uuid::Uuid::new_v4().to_string(),
                                            "content": [{ "type": "text", "text": message }]
                                        }],
                                        "itemsView": "full",
                                        "status": "inProgress",
                                        "startedAt": chrono::Utc::now().timestamp(),
                                        "completedAt": null,
                                    }
                                })};
                                if let Ok(json) = serde_json::to_string(&resp) {
                                    let _ = futures_util::SinkExt::send(&mut tui_sink,
                                        tokio_tungstenite::tungstenite::Message::text(json)).await;
                                }
                            }

                            // Notify the TUI that the turn started.
                            let notif = serde_json::json!({
                                "method": "turn/started",
                                "params": {
                                    "threadId": thread_id,
                                    "turn": {
                                        "id": turn_id,
                                        "items": [],
                                        "itemsView": "full",
                                        "status": "inProgress",
                                        "startedAt": chrono::Utc::now().timestamp(),
                                        "completedAt": null,
                                    }
                                }
                            });
                            let _ = futures_util::SinkExt::send(&mut tui_sink,
                                tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                        }
                        Some(EasementEvent::Delta(delta)) => {
                            if let Some(ref turn_id) = active_turn_id {
                                let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                match delta_type {
                                    "content_block_start" => {
                                        let block_index = delta.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                                        let block = delta.get("content_block").unwrap_or(&Value::Null);
                                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");

                                        if block_type == "thinking" {
                                            in_thinking = true;
                                            let reasoning_id = uuid::Uuid::new_v4().to_string();
                                            active_reasoning_item_id = Some(reasoning_id.clone());
                                            let notif = serde_json::json!({
                                                "method": "item/started",
                                                "params": {
                                                    "threadId": thread_id,
                                                    "turnId": turn_id,
                                                    "startedAtMs": chrono::Utc::now().timestamp_millis(),
                                                    "item": { "type": "reasoning", "id": reasoning_id, "summary": [], "content": [] }
                                                }
                                            });
                                            let _ = futures_util::SinkExt::send(&mut tui_sink,
                                                tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                            // Initial header delta.
                                            let header = serde_json::json!({
                                                "method": "item/reasoning/summaryTextDelta",
                                                "params": {
                                                    "threadId": thread_id,
                                                    "turnId": turn_id,
                                                    "itemId": active_reasoning_item_id,
                                                    "delta": "**Thinking**\n\n",
                                                    "summaryIndex": 0
                                                }
                                            });
                                            let _ = futures_util::SinkExt::send(&mut tui_sink,
                                                tokio_tungstenite::tungstenite::Message::text(header.to_string())).await;
                                        } else if block_type == "tool_use" {
                                            let tool_name = block.get("name").and_then(|v| v.as_str())
                                                .expect("missing tool name").to_string();
                                            let tool_use_id = block.get("id").and_then(|v| v.as_str())
                                                .expect("missing tool_use id").to_string();
                                            trace!("puzzle", "claude", "tool_start", "index": block_index, "tool_use_id": tool_use_id, "name": tool_name);
                                            tool_id_by_index.insert(block_index, tool_use_id.clone());
                                            tool_heap.push(Reverse(ToolKey {
                                                index: block_index,
                                                tool_use_id: tool_use_id.clone(),
                                            }));
                                            tool_runs.insert(tool_use_id.clone(), ToolRun {
                                                index: block_index,
                                                tool_use_id,
                                                name: tool_name,
                                                input_json: String::new(),
                                                command: None,
                                                started: false,
                                            });
                                        } else if block_type == "text" {
                                            if let Some(ref item_id) = active_item_id {
                                                let notif = serde_json::json!({
                                                    "method": "item/started",
                                                    "params": {
                                                        "threadId": thread_id,
                                                        "turnId": turn_id,
                                                        "startedAtMs": chrono::Utc::now().timestamp_millis(),
                                                        "item": { "type": "agentMessage", "id": item_id, "text": "" }
                                                    }
                                                });
                                                let _ = futures_util::SinkExt::send(&mut tui_sink,
                                                    tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                            }
                                        }
                                    }
                                    "content_block_delta" => {
                                        let block_index = delta.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                                        let inner = delta.get("delta").unwrap_or(&Value::Null);
                                        let inner_type = inner.get("type").and_then(|v| v.as_str()).unwrap_or("");

                                        if in_thinking && inner_type == "thinking_delta" {
                                            let text = inner.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                                            if !text.is_empty() {
                                                let notif = serde_json::json!({
                                                    "method": "item/reasoning/summaryTextDelta",
                                                    "params": {
                                                        "threadId": thread_id,
                                                        "turnId": turn_id,
                                                        "itemId": active_reasoning_item_id,
                                                        "delta": text,
                                                        "summaryIndex": 0
                                                    }
                                                });
                                                let _ = futures_util::SinkExt::send(&mut tui_sink,
                                                    tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                            }
                                        } else if inner_type == "input_json_delta" {
                                            if let Some(partial) = inner.get("partial_json").and_then(|v| v.as_str()) {
                                                if let Some(tool_use_id) = tool_id_by_index.get(&block_index) {
                                                    if let Some(tool) = tool_runs.get_mut(tool_use_id) {
                                                        tool.input_json.push_str(partial);
                                                    }
                                                }
                                            }
                                        } else if !in_thinking && inner_type == "text_delta" {
                                            let text = inner.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                            if !text.is_empty() {
                                                if let Some(ref item_id) = active_item_id {
                                                    let notif = serde_json::json!({
                                                        "method": "item/agentMessage/delta",
                                                        "params": {
                                                            "threadId": thread_id,
                                                            "turnId": turn_id,
                                                            "itemId": item_id,
                                                            "delta": text
                                                        }
                                                    });
                                                    let _ = futures_util::SinkExt::send(&mut tui_sink,
                                                        tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                                }
                                            }
                                        }
                                    }
                                    "content_block_stop" => {
                                        let block_index = delta.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                                        if in_thinking {
                                            if let Some(reasoning_id) = active_reasoning_item_id.take() {
                                                let notif = serde_json::json!({
                                                    "method": "item/completed",
                                                    "params": {
                                                        "threadId": thread_id,
                                                        "turnId": turn_id,
                                                        "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                        "item": { "type": "reasoning", "id": reasoning_id, "summary": [], "content": [] }
                                                    }
                                                });
                                                let _ = futures_util::SinkExt::send(&mut tui_sink,
                                                    tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                            }
                                            in_thinking = false;
                                        } else if let Some(tool_use_id) = tool_id_by_index.remove(&block_index) {
                                            if let Some(tool) = tool_runs.get_mut(&tool_use_id) {
                                                let parsed_input = serde_json::from_str::<Value>(&tool.input_json).unwrap_or_default();
                                                let f = tool_function(&tool.name, &parsed_input).unwrap_or_default();
                                                let args = tool_args(&tool.name, &parsed_input);
                                                let command = args.get("command")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or(&f)
                                                    .to_string();
                                                trace!("puzzle", "claude", "tool_ready", "index": block_index, "tool_use_id": tool_use_id, "command": command);
                                                tool.command = Some(command);
                                            }
                                            for notif in pump_tool_runs(
                                                &thread_id,
                                                turn_id,
                                                &cwd,
                                                &mut tool_heap,
                                                &mut tool_runs,
                                                &mut tool_results,
                                            ) {
                                                send_tui(&mut tui_sink, notif).await;
                                            }
                                        }
                                    }
                                    "message_delta" => {
                                        if let Some(reason) = delta.get("delta")
                                            .and_then(|d| d.get("stop_reason"))
                                            .and_then(|v| v.as_str())
                                        {
                                            last_stop_reason = Some(reason.to_string());
                                        }
                                    }
                                    "message_stop" => {
                                        let stop_reason = last_stop_reason.take().unwrap_or_default();

                                        if stop_reason == "tool_use" {
                                            // Tool call in flight. Close the agentMessage
                                            // if one was active but keep the turn alive.
                                            if let Some(ref item_id) = active_item_id {
                                                let notif = serde_json::json!({
                                                    "method": "item/completed",
                                                    "params": {
                                                        "threadId": thread_id,
                                                        "turnId": turn_id,
                                                        "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                        "item": { "type": "agentMessage", "id": item_id, "text": "" }
                                                    }
                                                });
                                                let _ = futures_util::SinkExt::send(&mut tui_sink,
                                                    tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                            }
                                            active_item_id = Some(uuid::Uuid::new_v4().to_string());
                                        } else {
                                            if let Some(ref item_id) = active_item_id {
                                                let notif = serde_json::json!({
                                                    "method": "item/completed",
                                                    "params": {
                                                        "threadId": thread_id,
                                                        "turnId": turn_id,
                                                        "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                        "item": { "type": "agentMessage", "id": item_id, "text": "" }
                                                    }
                                                });
                                                let _ = futures_util::SinkExt::send(&mut tui_sink,
                                                    tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                            }

                                            let notif = serde_json::json!({
                                                "method": "turn/completed",
                                                "params": {
                                                    "threadId": thread_id,
                                                    "turn": {
                                                        "id": turn_id,
                                                        "items": [],
                                                        "itemsView": "full",
                                                        "status": "completed",
                                                        "startedAt": null,
                                                        "completedAt": chrono::Utc::now().timestamp(),
                                                    }
                                                }
                                            });
                                            let _ = futures_util::SinkExt::send(&mut tui_sink,
                                                tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;

                                            active_turn_id = None;
                                            active_item_id = None;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some(EasementEvent::TurnCompleted { turn_id, status }) => {
                            trace!("puzzle", "easement", "turn_completed", "turn_id": turn_id, "status": status);
                            if status == "interrupted" || status == "failed" {
                                if let Some(turn_id_done) = active_turn_id.take() {
                                    let tool_ids: Vec<String> = tool_runs.iter()
                                        .filter_map(|(tool_id, tool)| if tool.started { Some(tool_id.clone()) } else { None })
                                        .collect();
                                    for tool_id in tool_ids {
                                        let tool = tool_runs.remove(&tool_id).expect("started tool missing");
                                        let command = tool.command.unwrap_or_default();
                                        let notif = serde_json::json!({
                                            "method": "item/completed",
                                            "params": {
                                                "threadId": thread_id,
                                                "turnId": turn_id_done,
                                                "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                "item": {
                                                    "type": "commandExecution",
                                                    "id": tool_id,
                                                    "command": command, "cwd": cwd, "source": "agent",
                                                    "status": status, "commandActions": [],
                                                    "aggregatedOutput": null, "exitCode": null, "durationMs": null
                                                }
                                            }
                                        });
                                        let _ = futures_util::SinkExt::send(&mut tui_sink,
                                            tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                    }
                                    if let Some(ref item_id) = active_item_id {
                                        let notif = serde_json::json!({
                                            "method": "item/completed",
                                            "params": {
                                                "threadId": thread_id,
                                                "turnId": turn_id_done,
                                                "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                "item": { "type": "agentMessage", "id": item_id, "text": "" }
                                            }
                                        });
                                        let _ = futures_util::SinkExt::send(&mut tui_sink,
                                            tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                    }
                                    let notif = serde_json::json!({
                                        "method": "turn/completed",
                                        "params": {
                                            "threadId": thread_id,
                                            "turn": {
                                                "id": turn_id_done,
                                                "items": [], "itemsView": "full",
                                                "status": status,
                                                "startedAt": null,
                                                "completedAt": chrono::Utc::now().timestamp(),
                                            }
                                        }
                                    });
                                    let _ = futures_util::SinkExt::send(&mut tui_sink,
                                        tokio_tungstenite::tungstenite::Message::text(notif.to_string())).await;
                                    active_item_id = None;
                                    active_reasoning_item_id = None;
                                    in_thinking = false;
                                    tool_heap.clear();
                                    tool_runs.clear();
                                    tool_results.clear();
                                    tool_id_by_index.clear();
                                    last_stop_reason = None;
                                }
                            }
                        }
                        Some(EasementEvent::ToolResult { tool_use_id, output, is_error }) => {
                            trace!("puzzle", "easement", "tool_result", "tool_use_id": tool_use_id, "is_error": is_error);
                            let turn_id = active_turn_id.as_deref().unwrap_or("");
                            tool_results.insert(tool_use_id, ToolRunResult {
                                output,
                                is_error,
                                complete: true,
                            });
                            for notif in pump_tool_runs(
                                &thread_id,
                                turn_id,
                                &cwd,
                                &mut tool_heap,
                                &mut tool_runs,
                                &mut tool_results,
                            ) {
                                send_tui(&mut tui_sink, notif).await;
                            }
                        }
                        Some(EasementEvent::Usage(usage)) => {
                            let Some(sample) = parse_usage_sample(&usage) else {
                                dump!("puzzle", "easement", "usage_unparsed", "usage": usage);
                                continue;
                            };
                            let previous_display = displayed_usage_sample(&usage_samples);
                            push_usage_sample(&mut usage_samples, sample.clone());
                            let display = displayed_usage_sample(&usage_samples).unwrap_or_else(|| sample.clone());
                            trace!(
                                "puzzle", "easement", "usage",
                                "candidate_context": sample.context_size,
                                "display_context": display.context_size,
                                "uncached_input_tokens": sample.uncached_input_tokens,
                                "cached_input_tokens": sample.cached_input_tokens,
                                "cache_creation_input_tokens": sample.cache_creation_input_tokens,
                                "output_tokens": sample.output_tokens,
                            );
                            if let Some(previous) = previous_display {
                                let suppressed_large_drop = sample.context_size < previous.context_size / 2
                                    && display.context_size >= previous.context_size;
                                if suppressed_large_drop {
                                    dump!(
                                        "puzzle", "easement", "usage_drop_suppressed",
                                        "candidate_context": sample.context_size,
                                        "previous_display_context": previous.context_size,
                                        "display_context": display.context_size,
                                        "turn_id": active_turn_id.as_deref().unwrap_or(""),
                                        "transcript": transcript_id,
                                        "usage": usage,
                                    );
                                }
                            }
                            let turn_id = active_turn_id.as_deref().unwrap_or("");
                            send_tui(
                                &mut tui_sink,
                                token_usage_notification(&thread_id, turn_id, &display),
                            ).await;
                        }
                        Some(EasementEvent::UserMessage { text }) => {
                            trace!("puzzle", "easement", "user_message", "text": text);
                            let turn_id = active_turn_id.as_deref().unwrap_or("");
                            let item_id = uuid::Uuid::new_v4().to_string();
                            let notif = serde_json::json!({
                                "method": "item/completed",
                                "params": {
                                    "threadId": thread_id,
                                    "turnId": turn_id,
                                    "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                    "item": {
                                        "type": "userMessage",
                                        "id": item_id,
                                        "content": [{
                                            "type": "text",
                                            "text": text,
                                        }],
                                    },
                                },
                            });
                            send_tui(&mut tui_sink, notif).await;
                        }
                        Some(_) => {}
                        None => {
                            trace!("puzzle", "easement", "disconnected");
                            break;
                        }
                    }
                }
            }
        }
    });

    // Wait for the TUI to exit.
    let output = tui_child.wait_with_output().await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    trace!("puzzle", "lifecycle", "tui_exited",
        "status": output.status.code().unwrap_or(-1),
        "slug": slug,
        "stderr": stderr,
        "stdout": stdout,
    );

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
