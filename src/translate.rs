// Translation loop between the Codex TUI (JSON-RPC/WebSocket) and Wicket (envelope/WebSocket).

use codex_app_server_protocol::{
    ServerNotification,
    ReasoningSummaryTextDeltaNotification,
    ItemStartedNotification,
    ItemCompletedNotification,
    ThreadItem,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::exchange::ExchangeLog;
use crate::wicket::{WicketClient, WicketEvent};

#[derive(Deserialize, Debug)]
struct JsonRpcMessage {
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
    result: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    result: Value,
}

#[derive(Serialize)]
struct JsonRpcNotification {
    jsonrpc: String,
    method: String,
    params: Value,
}

#[derive(Serialize)]
struct JsonRpcError {
    jsonrpc: String,
    id: Value,
    error: JsonRpcErrorBody,
}

#[derive(Serialize)]
struct JsonRpcErrorBody {
    code: i32,
    message: String,
}

fn parse_patch_to_changes(patch: &str) -> Vec<Value> {
    let mut changes: Vec<Value> = vec![];
    let mut current_path: Option<String> = None;
    let mut current_kind: Option<Value> = None;
    let mut current_diff = String::new();

    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            if let (Some(p), Some(k)) = (current_path.take(), current_kind.take()) {
                changes.push(serde_json::json!({ "path": p, "kind": k, "diff": current_diff.trim_end() }));
                current_diff.clear();
            }
            current_path = Some(path.trim().to_string());
            current_kind = Some(serde_json::json!({ "type": "add" }));
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            if let (Some(p), Some(k)) = (current_path.take(), current_kind.take()) {
                changes.push(serde_json::json!({ "path": p, "kind": k, "diff": current_diff.trim_end() }));
                current_diff.clear();
            }
            current_path = Some(path.trim().to_string());
            current_kind = Some(serde_json::json!({ "type": "delete" }));
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            if let (Some(p), Some(k)) = (current_path.take(), current_kind.take()) {
                changes.push(serde_json::json!({ "path": p, "kind": k, "diff": current_diff.trim_end() }));
                current_diff.clear();
            }
            current_path = Some(path.trim().to_string());
            current_kind = Some(serde_json::json!({ "type": "update", "movePath": null }));
        } else if line == "*** Begin Patch" || line == "*** End Patch" {
            // skip markers
        } else if current_path.is_some() {
            current_diff.push_str(line);
            current_diff.push('\n');
        }
    }
    if let (Some(p), Some(k)) = (current_path.take(), current_kind.take()) {
        changes.push(serde_json::json!({ "path": p, "kind": k, "diff": current_diff.trim_end() }));
    }

    // Convert to unified diff for update changes.
    for c in changes.iter_mut() {
        let path = c.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let raw_diff = c.get("diff").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let kind_type = c.get("kind").and_then(|v| v.get("type")).and_then(|v| v.as_str()).unwrap_or("");
        if !raw_diff.is_empty() && kind_type == "update" {
            let mut old_count = 0usize;
            let mut new_count = 0usize;
            let mut diff_lines = String::new();
            for dl in raw_diff.lines() {
                if dl.starts_with("@@") { continue; }
                if dl.starts_with('-') { old_count += 1; }
                else if dl.starts_with('+') { new_count += 1; }
                else if dl.starts_with(' ') || !dl.is_empty() { old_count += 1; new_count += 1; }
                diff_lines.push_str(dl);
                diff_lines.push('\n');
            }
            let unified = format!(
                "--- a/{}\n+++ b/{}\n@@ -1,{} +1,{} @@\n{}",
                path, path, old_count, new_count, diff_lines.trim_end()
            );
            c.as_object_mut().map(|m| m.insert("diff".into(), serde_json::json!(unified)));
        }
    }

    changes
}


fn is_interrupt_marker(entry: &Value) -> bool {
    let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if kind != "user" { return false; }
    entry.get("blocks")
        .and_then(|v| v.as_array())
        .and_then(|blocks| blocks.first())
        .and_then(|b| b.get("text"))
        .and_then(|v| v.as_str())
        .map(|t| t.starts_with("[Request interrupted by user"))
        .unwrap_or(false)
}

fn is_user_text_entry(entry: &Value) -> bool {
    let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if kind != "user" { return false; }
    entry.get("blocks")
        .and_then(|v| v.as_array())
        .map(|blocks| blocks.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some("text")))
        .unwrap_or(false)
}

fn flush_turn(turns: &mut Vec<Value>, items: &mut Vec<Value>, status: &str) {
    if items.is_empty() { return; }
    turns.push(serde_json::json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "items": items.drain(..).collect::<Vec<_>>(),
        "itemsView": "full",
        "status": status,
        "error": null,
        "startedAt": 0,
        "completedAt": 0,
        "durationMs": null
    }));
}

fn build_turns_from_entries(entries: &[Value]) -> Vec<Value> {
    let mut turns: Vec<Value> = vec![];
    let mut current_items: Vec<Value> = vec![];
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = format!("{}/pane/solver", home);

    let mut i = 0;
    while i < entries.len() {
        let entry = &entries[i];

        if is_interrupt_marker(entry) {
            flush_turn(&mut turns, &mut current_items, "interrupted");
            i += 1;
            continue;
        }

        let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let blocks = entry.get("blocks").and_then(|v| v.as_array());
        let uuid = entry.get("uuid").and_then(|v| v.as_str())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().leak());

        if is_user_text_entry(entry) && !current_items.is_empty() {
            flush_turn(&mut turns, &mut current_items, "completed");
        }

        if let Some(blocks) = blocks {
            for block in blocks {
                let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match (kind, btype) {
                    ("user", "text") => {
                        let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        current_items.push(serde_json::json!({
                            "type": "userMessage",
                            "id": uuid,
                            "content": [{ "type": "text", "text": text }]
                        }));
                    }
                    ("assistant", "text") => {
                        let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        current_items.push(serde_json::json!({
                            "type": "agentMessage",
                            "id": uuid,
                            "text": text
                        }));
                    }
                    ("assistant", "thinking") => {
                        let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        let item = ThreadItem::Reasoning {
                            id: uuid.to_string(),
                            summary: vec![format!("**Thinking**\n\n{}", text)],
                            content: vec![],
                        };
                        current_items.push(serde_json::to_value(&item).unwrap_or_default());
                    }
                    ("assistant", "tool_use") => {
                        let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                        if !name.contains("wicket") {
                            continue;
                        }

                        let mut tool_output = String::new();
                        if i + 1 < entries.len() {
                            let next = &entries[i + 1];
                            if next.get("kind").and_then(|v| v.as_str()) == Some("user") {
                                if let Some(next_blocks) = next.get("blocks").and_then(|v| v.as_array()) {
                                    for nb in next_blocks {
                                        if nb.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                                            tool_output = nb.get("content")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                        }
                                    }
                                }
                            }
                        }

                        if name.contains("apply_patch") {
                            let patch = block.get("input")
                                .and_then(|input| input.get("patch"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let changes = parse_patch_to_changes(patch);
                            current_items.push(serde_json::json!({
                                "type": "fileChange",
                                "id": uuid,
                                "changes": changes,
                                "status": "completed"
                            }));
                        } else {
                            let command = block.get("input")
                                .and_then(|input| input.get("command"))
                                .and_then(|v| v.as_str())
                                .unwrap_or(name);
                            current_items.push(serde_json::json!({
                                "type": "commandExecution",
                                "id": uuid,
                                "command": command,
                                "cwd": cwd,
                                "source": "agent",
                                "status": "completed",
                                "commandActions": [],
                                "aggregatedOutput": if tool_output.is_empty() { Value::Null } else { Value::String(tool_output.clone()) },
                                "exitCode": 0,
                                "durationMs": null
                            }));
                        }
                    }
                    ("user", "tool_result") => {}
                    _ => {}
                }
            }
        }
        i += 1;
    }

    flush_turn(&mut turns, &mut current_items, "completed");
    turns
}

async fn send_response(
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<UnixStream>, Message>,
    exchange: &ExchangeLog,
    id: Value,
    result: Value,
) -> color_eyre::eyre::Result<()> {
    let response = JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result,
    };
    let json = serde_json::to_string(&response)?;
    exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
    sink.send(Message::text(json)).await?;
    Ok(())
}

async fn send_error(
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<UnixStream>, Message>,
    exchange: &ExchangeLog,
    id: Value,
    code: i32,
    message: String,
) -> color_eyre::eyre::Result<()> {
    let error = JsonRpcError {
        jsonrpc: "2.0".into(),
        id,
        error: JsonRpcErrorBody { code, message },
    };
    let json = serde_json::to_string(&error)?;
    exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
    sink.send(Message::text(json)).await?;
    Ok(())
}

async fn send_notification(
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<UnixStream>, Message>,
    exchange: &ExchangeLog,
    notif: ServerNotification,
) -> color_eyre::eyre::Result<()> {
    let mut obj = serde_json::to_value(&notif)?;
    if let Some(map) = obj.as_object_mut() {
        map.insert("jsonrpc".into(), serde_json::json!("2.0"));
    }
    let json = serde_json::to_string(&obj)?;
    exchange.log("puzzle>tui", &obj);
    sink.send(Message::text(json)).await?;
    Ok(())
}

struct PendingShell {
    turn_id: String,
    item_id: String,
}

pub async fn run(
    tui_ws: WebSocketStream<UnixStream>,
    mut wicket: WicketClient,
    exchange: ExchangeLog,
    slug: &str,
    history: Vec<Value>,
    initial_usage: Option<Value>,
) -> color_eyre::eyre::Result<()> {
    let (mut tui_sink, mut tui_stream) = tui_ws.split();

    let thread_id = uuid::Uuid::new_v4().to_string();
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = format!("{}/pane/{}", home, slug);
    let mut active_turn_id: Option<String> = None;
    let mut active_item_id: Option<String> = None;
    let mut active_tool_item_id: Option<String> = None;
    let mut active_tool_name: Option<String> = None;
    let mut completed_tool_name: Option<String> = None;
    let mut completed_tool_path: Option<String> = None;
    let mut active_reasoning_item_id: Option<String> = None;
    let mut in_tool_use: bool = false;
    let mut turn_interrupted: bool = false;
    let mut in_thinking: bool = false;
    let mut tool_input_json: String = String::new();
    let mut last_stop_reason: Option<String> = None;
    let mut pending_shell: Option<PendingShell> = None;
    let mut pending_approval_request_id: Option<Value> = None;
    let mut pending_turn_start_response: Option<Value> = None;
    let mut next_server_request_id: i64 = 1000;

    loop {
        tokio::select! {
            msg = tui_stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(raw) = serde_json::from_str::<Value>(&text) {
                            exchange.log("tui>puzzle", &raw);
                        }

                        let rpc: JsonRpcMessage = match serde_json::from_str(&text) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::warn!("bad JSON-RPC from TUI: {}", e);
                                continue;
                            }
                        };

                        if rpc.method.is_none() && rpc.result.is_some() {
                            if let Some(ref pending_id) = pending_approval_request_id {
                                if rpc.id.as_ref() == Some(pending_id) {
                                    let result = rpc.result.unwrap_or_default();
                                    let decision = result.get("decision")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("decline");
                                    tracing::info!(decision = %decision, "approval response from TUI");
                                    let allow = matches!(decision, "accept" | "acceptForSession" | "acceptWithExecpolicyAmendment");
                                    wicket.send_approval(allow, if allow { None } else { Some("User denied") })?;
                                    pending_approval_request_id = None;
                                }
                            }
                            continue;
                        }

                        let method = rpc.method.as_deref().unwrap_or("");
                        let id = rpc.id.clone();

                        if id.is_none() {
                            tracing::debug!(method, "TUI notification");
                            continue;
                        }
                        let id = id.unwrap_or(Value::Null);

                        match method {
                            "initialize" => {
                                tracing::info!("TUI: initialize");
                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({
                                    "protocolVersion": "2.0",
                                    "serverInfo": { "name": "puzzle", "version": "0.1.0" },
                                    "capabilities": {}
                                })).await?;
                            }

                            "account/read" => {
                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({
                                    "account": { "type": "apiKey" },
                                    "requiresOpenaiAuth": false
                                })).await?;
                            }

                            "model/list" => {
                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({
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
                                })).await?;
                            }

                            "thread/start" | "thread/resume" => {
                                tracing::info!(method, entries = history.len(), "TUI: thread request");
                                let turns = build_turns_from_entries(&history);
                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({
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
                                })).await?;

                                if let Some(ref usage) = initial_usage {
                                    let input = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                    let cached = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                    let output = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                    let total = input + cached + output;
                                    let notif = JsonRpcNotification {
                                        jsonrpc: "2.0".into(),
                                        method: "thread/tokenUsage/updated".into(),
                                        params: serde_json::json!({
                                            "threadId": thread_id,
                                            "turnId": "",
                                            "tokenUsage": {
                                                "total": {
                                                    "totalTokens": total,
                                                    "inputTokens": input + cached,
                                                    "cachedInputTokens": cached,
                                                    "outputTokens": output,
                                                    "reasoningOutputTokens": 0
                                                },
                                                "last": {
                                                    "totalTokens": total,
                                                    "inputTokens": input + cached,
                                                    "cachedInputTokens": cached,
                                                    "outputTokens": output,
                                                    "reasoningOutputTokens": 0
                                                },
                                                "modelContextWindow": 1000000
                                            }
                                        }),
                                    };
                                    let json = serde_json::to_string(&notif).unwrap_or_default();
                                    let _ = tui_sink.send(Message::text(json)).await;
                                }
                            }

                            "skills/list" | "hooks/list" | "plugin/list" => {
                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({
                                    "data": []
                                })).await?;
                            }

                            "thread/unsubscribe" => {
                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({})).await?;
                            }

                            "turn/interrupt" => {
                                tracing::info!("turn interrupt");
                                turn_interrupted = true;
                                wicket.send_envelope("interrupt", serde_json::json!({}))?;
                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({})).await?;
                            }

                            "thread/shellCommand" => {
                                let params = rpc.params.unwrap_or_default();
                                let command = params.get("command")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                tracing::info!(command = %command, "shell command");

                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({})).await?;

                                let shell_turn_id = uuid::Uuid::new_v4().to_string();
                                let shell_item_id = uuid::Uuid::new_v4().to_string();
                                let now = chrono::Utc::now().timestamp();

                                let started_notif = JsonRpcNotification {
                                    jsonrpc: "2.0".into(),
                                    method: "turn/started".into(),
                                    params: serde_json::json!({
                                        "threadId": thread_id,
                                        "turn": {
                                            "id": shell_turn_id,
                                            "items": [],
                                            "itemsView": "full",
                                            "status": "inProgress",
                                            "error": null,
                                            "startedAt": now,
                                            "completedAt": null,
                                            "durationMs": null
                                        }
                                    }),
                                };
                                let json = serde_json::to_string(&started_notif)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json).unwrap_or_default());
                                tui_sink.send(Message::text(json)).await?;

                                let item_notif = JsonRpcNotification {
                                    jsonrpc: "2.0".into(),
                                    method: "item/started".into(),
                                    params: serde_json::json!({
                                        "threadId": thread_id,
                                        "turnId": shell_turn_id,
                                        "startedAtMs": chrono::Utc::now().timestamp_millis(),
                                        "item": {
                                            "type": "commandExecution",
                                            "id": shell_item_id,
                                            "command": command,
                                            "cwd": cwd,
                                            "source": "userShell",
                                            "status": "inProgress",
                                            "commandActions": [],
                                            "aggregatedOutput": null,
                                            "exitCode": null,
                                            "durationMs": null
                                        }
                                    }),
                                };
                                let json = serde_json::to_string(&item_notif)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json).unwrap_or_default());
                                tui_sink.send(Message::text(json)).await?;

                                wicket.send_envelope("shell", serde_json::json!({
                                    "command": command,
                                    "turn_id": shell_turn_id,
                                    "item_id": shell_item_id
                                }))?;

                                pending_shell = Some(PendingShell {
                                    turn_id: shell_turn_id,
                                    item_id: shell_item_id,
                                });
                            }

                            "turn/start" => {
                                let params = rpc.params.unwrap_or_default();
                                let message = params.get("input")
                                    .and_then(|input| input.as_array())
                                    .and_then(|items| items.iter().find_map(|item| {
                                        if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                            item.get("text").and_then(|t| t.as_str())
                                        } else {
                                            None
                                        }
                                    }))
                                    .unwrap_or("");

                                tracing::info!(message, "TUI: turn/start, forwarding to wicket");

                                if !message.is_empty() {
                                    wicket.send_envelope("turn", serde_json::json!({ "message": message }))?;
                                }

                                // Stash the pending turn/start response ID. We respond
                                // when Wicket's turn:started broadcast arrives with
                                // the Wicket-generated turn ID.
                                pending_turn_start_response = Some(id);
                            }

                            "turn/steer" => {
                                let params = rpc.params.unwrap_or_default();
                                let expected_turn = params.get("expectedTurnId")
                                    .and_then(|v| v.as_str());
                                let message = params.get("input")
                                    .and_then(|input| input.as_array())
                                    .and_then(|items| items.iter().find_map(|item| {
                                        if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                            item.get("text").and_then(|t| t.as_str())
                                        } else {
                                            None
                                        }
                                    }))
                                    .unwrap_or("");
                                tracing::info!(
                                    expected_turn = ?expected_turn,
                                    active_turn = ?active_turn_id,
                                    message = %message,
                                    "turn/steer received"
                                );

                                if active_turn_id.is_none() {
                                    send_error(&mut tui_sink, &exchange, id, -32000,
                                        "no active turn to steer".to_string()).await?;
                                } else if expected_turn.is_some() && expected_turn != active_turn_id.as_deref() {
                                    let actual = active_turn_id.as_deref().unwrap_or("");
                                    let expected = expected_turn.unwrap_or("");
                                    send_error(&mut tui_sink, &exchange, id, -32000,
                                        format!("expected active turn id `{}` but found `{}`", expected, actual)).await?;
                                } else {
                                    if !message.is_empty() {
                                        wicket.send_envelope("steer", serde_json::json!({ "message": message }))?;
                                    }
                                    let tid = active_turn_id.as_deref().unwrap_or("");
                                    send_response(&mut tui_sink, &exchange, id, serde_json::json!({
                                        "turnId": tid
                                    })).await?;
                                }
                            }

                            _ => {
                                tracing::info!(method, "TUI: unhandled method");
                                send_error(&mut tui_sink, &exchange, id, -32601,
                                    format!("not implemented: {}", method)).await?;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("TUI disconnected");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("TUI websocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }

            event = wicket.event_rx.recv() => {
                match event {
                    Some(WicketEvent::Delta(delta)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "delta", "data": &delta}));

                        if let Some(turn_id) = &active_turn_id {
                            let turn_id = turn_id.clone();
                            let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match delta_type {
                                "content_block_start" => {
                                    let block = delta.get("content_block").unwrap_or(&Value::Null);
                                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");

                                    if block_type == "thinking" {
                                        tracing::info!("THINKING block detected");
                                        in_thinking = true;
                                        let reasoning_id = uuid::Uuid::new_v4().to_string();
                                        active_reasoning_item_id = Some(reasoning_id.clone());
                                        send_notification(&mut tui_sink, &exchange,
                                            ServerNotification::ItemStarted(ItemStartedNotification {
                                                thread_id: thread_id.clone(),
                                                turn_id: turn_id.clone(),
                                                started_at_ms: chrono::Utc::now().timestamp_millis(),
                                                item: ThreadItem::Reasoning {
                                                    id: reasoning_id.clone(),
                                                    summary: vec![],
                                                    content: vec![],
                                                },
                                            })
                                        ).await?;
                                        send_notification(&mut tui_sink, &exchange,
                                            ServerNotification::ReasoningSummaryTextDelta(
                                                ReasoningSummaryTextDeltaNotification {
                                                    thread_id: thread_id.clone(),
                                                    turn_id: turn_id.clone(),
                                                    item_id: reasoning_id.clone(),
                                                    delta: "**Thinking**\n\n".to_string(),
                                                    summary_index: 0,
                                                }
                                            )
                                        ).await?;
                                    } else if block_type == "tool_use" {
                                        let tool_name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                        let is_wicket_tool = tool_name.contains("wicket");

                                        in_tool_use = true;
                                        tool_input_json.clear();
                                        active_tool_name = Some(tool_name.to_string());

                                        if is_wicket_tool {
                                            let tool_item_id = uuid::Uuid::new_v4().to_string();
                                            active_tool_item_id = Some(tool_item_id.clone());
                                        }
                                    } else if block_type == "text" {
                                        if let Some(tool_item_id) = active_tool_item_id.take() {
                                            let completed_notif = JsonRpcNotification {
                                                jsonrpc: "2.0".into(),
                                                method: "item/completed".into(),
                                                params: serde_json::json!({
                                                    "threadId": thread_id,
                                                    "turnId": turn_id,
                                                    "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                    "item": {
                                                        "type": "commandExecution",
                                                        "id": tool_item_id,
                                                        "command": "",
                                                        "cwd": cwd,
                                                        "source": "agent",
                                                        "status": "completed",
                                                        "commandActions": [],
                                                        "aggregatedOutput": null,
                                                        "exitCode": 0,
                                                        "durationMs": null
                                                    }
                                                }),
                                            };
                                            let json = serde_json::to_string(&completed_notif)?;
                                            tui_sink.send(Message::text(json)).await?;
                                        }
                                        if let Some(item_id) = &active_item_id {
                                            let notif = JsonRpcNotification {
                                                jsonrpc: "2.0".into(),
                                                method: "item/started".into(),
                                                params: serde_json::json!({
                                                    "threadId": thread_id,
                                                    "turnId": turn_id,
                                                    "startedAtMs": chrono::Utc::now().timestamp_millis(),
                                                    "item": {
                                                        "type": "agentMessage",
                                                        "id": item_id,
                                                        "text": ""
                                                    }
                                                }),
                                            };
                                            let json = serde_json::to_string(&notif)?;
                                            exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
                                            tui_sink.send(Message::text(json)).await?;
                                        }
                                    }
                                }
                                "content_block_delta" => {
                                    let inner = delta.get("delta").unwrap_or(&Value::Null);
                                    let inner_type = inner.get("type").and_then(|v| v.as_str()).unwrap_or("");

                                    if in_thinking && inner_type == "thinking_delta" {
                                        let text = inner.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                                        if !text.is_empty() {
                                            if let Some(ref reasoning_id) = active_reasoning_item_id {
                                                send_notification(&mut tui_sink, &exchange,
                                                    ServerNotification::ReasoningSummaryTextDelta(
                                                        ReasoningSummaryTextDeltaNotification {
                                                            thread_id: thread_id.clone(),
                                                            turn_id: turn_id.clone(),
                                                            item_id: reasoning_id.clone(),
                                                            delta: text.to_string(),
                                                            summary_index: 0,
                                                        }
                                                    )
                                                ).await?;
                                            }
                                        }
                                    } else if in_tool_use && inner_type == "input_json_delta" {
                                        if let Some(partial) = inner.get("partial_json").and_then(|v| v.as_str()) {
                                            tool_input_json.push_str(partial);
                                        }
                                    } else if !in_tool_use && !in_thinking {
                                        let text = inner.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                        if !text.is_empty() {
                                            if let Some(item_id) = &active_item_id {
                                                let notif = JsonRpcNotification {
                                                    jsonrpc: "2.0".into(),
                                                    method: "item/agentMessage/delta".into(),
                                                    params: serde_json::json!({
                                                        "threadId": thread_id,
                                                        "turnId": turn_id,
                                                        "itemId": item_id,
                                                        "delta": text
                                                    }),
                                                };
                                                let json = serde_json::to_string(&notif)?;
                                                tui_sink.send(Message::text(json)).await?;
                                            }
                                        }
                                    }
                                }
                                "content_block_stop" => {
                                    if in_thinking {
                                        if let Some(reasoning_id) = active_reasoning_item_id.take() {
                                            send_notification(&mut tui_sink, &exchange,
                                                ServerNotification::ItemCompleted(ItemCompletedNotification {
                                                    thread_id: thread_id.clone(),
                                                    turn_id: turn_id.clone(),
                                                    completed_at_ms: chrono::Utc::now().timestamp_millis(),
                                                    item: ThreadItem::Reasoning {
                                                        id: reasoning_id,
                                                        summary: vec![],
                                                        content: vec![],
                                                    },
                                                })
                                            ).await?;
                                        }
                                        in_thinking = false;
                                    } else if in_tool_use {
                                        if let Some(ref tool_item_id) = active_tool_item_id {
                                            let tool = active_tool_name.as_deref().unwrap_or("");
                                            let is_patch = tool.contains("apply_patch");
                                            let is_view_image = tool.contains("view_image");

                                            if is_view_image {
                                                let image_path = serde_json::from_str::<Value>(&tool_input_json)
                                                    .ok()
                                                    .and_then(|input| input.get("path").and_then(|v| v.as_str()).map(|s| s.to_string()))
                                                    .unwrap_or_default();

                                                let started_notif = JsonRpcNotification {
                                                    jsonrpc: "2.0".into(),
                                                    method: "item/started".into(),
                                                    params: serde_json::json!({
                                                        "threadId": thread_id,
                                                        "turnId": turn_id,
                                                        "startedAtMs": chrono::Utc::now().timestamp_millis(),
                                                        "item": {
                                                            "type": "imageView",
                                                            "id": tool_item_id,
                                                            "path": image_path
                                                        }
                                                    }),
                                                };
                                                let json = serde_json::to_string(&started_notif)?;
                                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
                                                tui_sink.send(Message::text(json)).await?;
                                            } else if is_patch {
                                                let patch = serde_json::from_str::<Value>(&tool_input_json)
                                                    .ok()
                                                    .and_then(|input| input.get("patch").and_then(|v| v.as_str()).map(|s| s.to_string()))
                                                    .unwrap_or_default();

                                                let changes = parse_patch_to_changes(&patch);

                                                let started_notif = JsonRpcNotification {
                                                    jsonrpc: "2.0".into(),
                                                    method: "item/started".into(),
                                                    params: serde_json::json!({
                                                        "threadId": thread_id,
                                                        "turnId": turn_id,
                                                        "startedAtMs": chrono::Utc::now().timestamp_millis(),
                                                        "item": {
                                                            "type": "fileChange",
                                                            "id": tool_item_id,
                                                            "changes": changes,
                                                            "status": "inProgress"
                                                        }
                                                    }),
                                                };
                                                let json = serde_json::to_string(&started_notif)?;
                                                tui_sink.send(Message::text(json)).await?;

                                                let patch_notif = JsonRpcNotification {
                                                    jsonrpc: "2.0".into(),
                                                    method: "item/fileChange/patchUpdated".into(),
                                                    params: serde_json::json!({
                                                        "threadId": thread_id,
                                                        "turnId": turn_id,
                                                        "itemId": tool_item_id,
                                                        "changes": changes
                                                    }),
                                                };
                                                let json = serde_json::to_string(&patch_notif)?;
                                                tui_sink.send(Message::text(json)).await?;
                                            } else {
                                                let command = serde_json::from_str::<Value>(&tool_input_json)
                                                    .ok()
                                                    .and_then(|input| input.get("command").and_then(|v| v.as_str()).map(|s| s.to_string()))
                                                    .unwrap_or_else(|| tool_input_json.clone());

                                                let started_notif = JsonRpcNotification {
                                                    jsonrpc: "2.0".into(),
                                                    method: "item/started".into(),
                                                    params: serde_json::json!({
                                                        "threadId": thread_id,
                                                    "turnId": turn_id,
                                                    "startedAtMs": chrono::Utc::now().timestamp_millis(),
                                                    "item": {
                                                        "type": "commandExecution",
                                                        "id": tool_item_id,
                                                        "command": command,
                                                        "cwd": cwd,
                                                        "source": "agent",
                                                        "status": "inProgress",
                                                        "commandActions": [],
                                                        "aggregatedOutput": null,
                                                        "exitCode": null,
                                                        "durationMs": null
                                                    }
                                                }),
                                            };
                                            let json = serde_json::to_string(&started_notif)?;
                                            exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
                                            tui_sink.send(Message::text(json)).await?;
                                            }
                                        }
                                        in_tool_use = false;
                                        completed_tool_name = active_tool_name.take();
                                        if completed_tool_name.as_deref().map(|n| n.contains("view_image")).unwrap_or(false) {
                                            completed_tool_path = serde_json::from_str::<Value>(&tool_input_json)
                                                .ok()
                                                .and_then(|input| input.get("path").and_then(|v| v.as_str()).map(|s| s.to_string()));
                                        } else {
                                            completed_tool_path = None;
                                        }
                                    }
                                }
                                "message_delta" => {
                                    let inner = delta.get("delta").unwrap_or(&Value::Null);
                                    if let Some(reason) = inner.get("stop_reason").and_then(|v| v.as_str()) {
                                        last_stop_reason = Some(reason.to_string());
                                    }
                                }
                                "message_stop" => {
                                    let stop_reason = last_stop_reason.take().unwrap_or_default();

                                    if stop_reason == "tool_use" {
                                        // Tool call in flight — don't end the turn.
                                        // Complete the agentMessage item if one was active.
                                        if let Some(item_id) = &active_item_id {
                                            let notif = JsonRpcNotification {
                                                jsonrpc: "2.0".into(),
                                                method: "item/completed".into(),
                                                params: serde_json::json!({
                                                    "threadId": thread_id,
                                                    "turnId": turn_id,
                                                    "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                    "item": {
                                                        "type": "agentMessage",
                                                        "id": item_id,
                                                        "text": ""
                                                    }
                                                }),
                                            };
                                            let json = serde_json::to_string(&notif)?;
                                            tui_sink.send(Message::text(json)).await?;
                                        }
                                        active_item_id = Some(uuid::Uuid::new_v4().to_string());
                                    } else {
                                        if let Some(tool_item_id) = active_tool_item_id.take() {
                                            let completed_notif = JsonRpcNotification {
                                                jsonrpc: "2.0".into(),
                                                method: "item/completed".into(),
                                                params: serde_json::json!({
                                                    "threadId": thread_id,
                                                    "turnId": turn_id,
                                                    "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                    "item": {
                                                        "type": "commandExecution",
                                                        "id": tool_item_id,
                                                        "command": "",
                                                        "cwd": cwd,
                                                        "source": "agent",
                                                        "status": "completed",
                                                        "commandActions": [],
                                                        "aggregatedOutput": null,
                                                        "exitCode": 0,
                                                        "durationMs": null
                                                    }
                                                }),
                                            };
                                            let json = serde_json::to_string(&completed_notif)?;
                                            tui_sink.send(Message::text(json)).await?;
                                        }
                                        let turn_id_done = active_turn_id.take().unwrap_or_default();
                                        let item_id_done = active_item_id.take().unwrap_or_default();

                                        let item_notif = JsonRpcNotification {
                                            jsonrpc: "2.0".into(),
                                            method: "item/completed".into(),
                                            params: serde_json::json!({
                                                "threadId": thread_id,
                                                "turnId": turn_id_done,
                                                "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                "item": {
                                                    "type": "agentMessage",
                                                    "id": item_id_done,
                                                    "text": ""
                                                }
                                            }),
                                        };
                                        let json = serde_json::to_string(&item_notif)?;
                                        exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
                                        tui_sink.send(Message::text(json)).await?;

                                        let now = chrono::Utc::now().timestamp();
                                        let turn_status = if turn_interrupted { "interrupted" } else { "completed" };
                                        turn_interrupted = false;
                                        let notif = JsonRpcNotification {
                                            jsonrpc: "2.0".into(),
                                            method: "turn/completed".into(),
                                            params: serde_json::json!({
                                                "threadId": thread_id,
                                                "turn": {
                                                    "id": turn_id_done,
                                                    "items": [],
                                                    "itemsView": "full",
                                                    "status": turn_status,
                                                    "error": null,
                                                    "startedAt": now,
                                                    "completedAt": now,
                                                    "durationMs": null
                                                }
                                            }),
                                        };
                                        let json = serde_json::to_string(&notif)?;
                                        exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
                                        tui_sink.send(Message::text(json)).await?;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(WicketEvent::Entry(entry)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "entry", "data": &entry}));
                    }
                    Some(WicketEvent::ToolStart(data)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "tool_start", "data": &data}));
                    }
                    Some(WicketEvent::ToolDone(data)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "tool_done", "data": &data}));

                        if let Some(tool_item_id) = active_tool_item_id.take() {
                            let turn_id = active_turn_id.as_deref().unwrap_or("");
                            let exit_code = data.get("exit_code").and_then(|v| v.as_i64());
                            let is_image = completed_tool_name.as_deref().map(|n| n.contains("view_image")).unwrap_or(false);

                            if is_image {
                                let image_path = completed_tool_path.take().unwrap_or_default();
                                let completed_notif = JsonRpcNotification {
                                    jsonrpc: "2.0".into(),
                                    method: "item/completed".into(),
                                    params: serde_json::json!({
                                        "threadId": thread_id,
                                        "turnId": turn_id,
                                        "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                        "item": {
                                            "type": "imageView",
                                            "id": tool_item_id,
                                            "path": image_path
                                        }
                                    }),
                                };
                                let json = serde_json::to_string(&completed_notif)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
                                tui_sink.send(Message::text(json)).await?;
                                completed_tool_name = None;
                            } else {
                                let output = data.get("output").and_then(|v| v.as_str()).unwrap_or("");

                                let output_notif = JsonRpcNotification {
                                    jsonrpc: "2.0".into(),
                                    method: "item/commandExecution/delta".into(),
                                    params: serde_json::json!({
                                        "threadId": thread_id,
                                        "turnId": turn_id,
                                        "itemId": tool_item_id,
                                        "delta": output
                                    }),
                                };
                                let json = serde_json::to_string(&output_notif)?;
                                tui_sink.send(Message::text(json)).await?;

                                let status = if exit_code == Some(0) { "completed" } else { "failed" };
                                let completed_notif = JsonRpcNotification {
                                    jsonrpc: "2.0".into(),
                                    method: "item/completed".into(),
                                    params: serde_json::json!({
                                        "threadId": thread_id,
                                        "turnId": turn_id,
                                        "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                        "item": {
                                            "type": "commandExecution",
                                            "id": tool_item_id,
                                            "command": "",
                                            "cwd": cwd,
                                            "source": "agent",
                                            "status": status,
                                        "commandActions": [],
                                        "aggregatedOutput": output,
                                        "exitCode": exit_code,
                                        "durationMs": null
                                    }
                                }),
                            };
                                let json = serde_json::to_string(&completed_notif)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
                                tui_sink.send(Message::text(json)).await?;
                                completed_tool_name = None;
                            }
                        }
                    }
                    Some(WicketEvent::ShellResult(data)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "shell_result", "data": &data}));
                        if let Some(shell) = pending_shell.take() {
                            let output = data.get("output").and_then(|v| v.as_str()).unwrap_or("");
                            let exit_code = data.get("exit_code").and_then(|v| v.as_i64());
                            let status = if exit_code == Some(0) { "completed" } else { "failed" };

                            if !output.is_empty() {
                                let delta_notif = JsonRpcNotification {
                                    jsonrpc: "2.0".into(),
                                    method: "item/commandExecution/outputDelta".into(),
                                    params: serde_json::json!({
                                        "threadId": thread_id,
                                        "turnId": shell.turn_id,
                                        "itemId": shell.item_id,
                                        "delta": output
                                    }),
                                };
                                let json = serde_json::to_string(&delta_notif)?;
                                tui_sink.send(Message::text(json)).await?;
                            }

                            let completed_notif = JsonRpcNotification {
                                jsonrpc: "2.0".into(),
                                method: "item/completed".into(),
                                params: serde_json::json!({
                                    "threadId": thread_id,
                                    "turnId": shell.turn_id,
                                    "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                    "item": {
                                        "type": "commandExecution",
                                        "id": shell.item_id,
                                        "command": "",
                                        "cwd": cwd,
                                        "source": "userShell",
                                        "status": status,
                                        "commandActions": [],
                                        "aggregatedOutput": output,
                                        "exitCode": exit_code,
                                        "durationMs": null
                                    }
                                }),
                            };
                            let json = serde_json::to_string(&completed_notif)?;
                            tui_sink.send(Message::text(json)).await?;

                            let now = chrono::Utc::now().timestamp();
                            let turn_notif = JsonRpcNotification {
                                jsonrpc: "2.0".into(),
                                method: "turn/completed".into(),
                                params: serde_json::json!({
                                    "threadId": thread_id,
                                    "turn": {
                                        "id": shell.turn_id,
                                        "items": [],
                                        "itemsView": "full",
                                        "status": status,
                                        "error": null,
                                        "startedAt": now,
                                        "completedAt": now,
                                        "durationMs": null
                                    }
                                }),
                            };
                            let json = serde_json::to_string(&turn_notif)?;
                            tui_sink.send(Message::text(json)).await?;
                        }
                    }
                    Some(WicketEvent::Usage(usage)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "usage", "data": &usage}));

                        let input = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        let cached = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        let output = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        let total = input + cached + output;

                        let notif = JsonRpcNotification {
                            jsonrpc: "2.0".into(),
                            method: "thread/tokenUsage/updated".into(),
                            params: serde_json::json!({
                                "threadId": thread_id,
                                "turnId": active_turn_id.as_deref().unwrap_or(""),
                                "tokenUsage": {
                                    "total": {
                                        "totalTokens": total,
                                        "inputTokens": input + cached,
                                        "cachedInputTokens": cached,
                                        "outputTokens": output,
                                        "reasoningOutputTokens": 0
                                    },
                                    "last": {
                                        "totalTokens": total,
                                        "inputTokens": input + cached,
                                        "cachedInputTokens": cached,
                                        "outputTokens": output,
                                        "reasoningOutputTokens": 0
                                    },
                                    "modelContextWindow": 1000000
                                }
                            }),
                        };
                        let json = serde_json::to_string(&notif).unwrap_or_default();
                        let _ = tui_sink.send(Message::text(json)).await;
                    }
                    Some(WicketEvent::Turn(data)) => {
                        let event = data.get("event").and_then(|v| v.as_str()).unwrap_or("");
                        let tid = data.get("turn_id").and_then(|v| v.as_str()).unwrap_or("");
                        tracing::info!(event = %event, turn_id = %tid, "wicket turn event");

                        match event {
                            "started" => {
                                if active_turn_id.is_some() {
                                    tracing::debug!("turn_started from wicket ignored, already have active turn");
                                } else {
                                    let message = data.get("message").and_then(|v| v.as_str()).unwrap_or("");
                                    let item_id = uuid::Uuid::new_v4().to_string();
                                    active_turn_id = Some(tid.to_string());
                                    active_item_id = Some(item_id.clone());

                                    let is_tui_initiated = pending_turn_start_response.is_some();
                                    tracing::info!(turn_id = %tid, tui_initiated = is_tui_initiated, "turn started");

                                    if let Some(response_id) = pending_turn_start_response.take() {
                                        send_response(&mut tui_sink, &exchange, response_id, serde_json::json!({
                                            "turn": {
                                                "id": tid,
                                                "items": [{
                                                    "type": "userMessage",
                                                    "id": uuid::Uuid::new_v4().to_string(),
                                                    "content": [{ "type": "text", "text": message }]
                                                }],
                                                "itemsView": "full",
                                                "status": "inProgress",
                                                "startedAt": chrono::Utc::now().timestamp(),
                                                "completedAt": null
                                            }
                                        })).await?;
                                    } else {
                                        let user_msg_id = uuid::Uuid::new_v4().to_string();
                                        let user_item_notif = JsonRpcNotification {
                                            jsonrpc: "2.0".into(),
                                            method: "item/completed".into(),
                                            params: serde_json::json!({
                                                "threadId": thread_id,
                                                "turnId": tid,
                                                "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                                "item": {
                                                    "type": "userMessage",
                                                    "id": user_msg_id,
                                                    "content": [{ "type": "text", "text": message }]
                                                }
                                            }),
                                        };
                                        let json = serde_json::to_string(&user_item_notif).unwrap_or_default();
                                        exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json).unwrap_or_default());
                                        let _ = tui_sink.send(Message::text(json)).await;
                                    }

                                    let notif = JsonRpcNotification {
                                        jsonrpc: "2.0".into(),
                                        method: "turn/started".into(),
                                        params: serde_json::json!({
                                            "threadId": thread_id,
                                            "turn": {
                                                "id": tid,
                                                "items": [],
                                                "itemsView": "full",
                                                "status": "inProgress",
                                                "error": null,
                                                "startedAt": chrono::Utc::now().timestamp(),
                                                "completedAt": null,
                                                "durationMs": null
                                            }
                                        }),
                                    };
                                    let json = serde_json::to_string(&notif).unwrap_or_default();
                                    exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json).unwrap_or_default());
                                    let _ = tui_sink.send(Message::text(json)).await;

                                    let item_notif = JsonRpcNotification {
                                        jsonrpc: "2.0".into(),
                                        method: "item/started".into(),
                                        params: serde_json::json!({
                                            "threadId": thread_id,
                                            "turnId": tid,
                                            "startedAtMs": chrono::Utc::now().timestamp_millis(),
                                            "item": {
                                                "type": "agentMessage",
                                                "id": item_id,
                                                "text": ""
                                            }
                                        }),
                                    };
                                    let json = serde_json::to_string(&item_notif).unwrap_or_default();
                                    let _ = tui_sink.send(Message::text(json)).await;
                                }
                            }
                            "completed" => {
                                let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("completed");
                                if let Some(ref current) = active_turn_id {
                                    if current == tid {
                                        tracing::debug!("turn completed from wicket for active turn, letting delta handler finish");
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(WicketEvent::Lifecycle(name)) => {
                        tracing::info!(lifecycle = %name, "wicket lifecycle");
                        if name == "round_interrupted" {
                            tracing::info!(active_turn = ?active_turn_id, "handling round_interrupted");
                            if let Some(turn_id_done) = active_turn_id.take() {
                                let item_id_done = active_item_id.take().unwrap_or_default();
                                active_tool_item_id = None;
                                active_tool_name = None;
                                active_reasoning_item_id = None;
                                in_tool_use = false;
                                in_thinking = false;
                                turn_interrupted = false;

                                let item_notif = JsonRpcNotification {
                                    jsonrpc: "2.0".into(),
                                    method: "item/completed".into(),
                                    params: serde_json::json!({
                                        "threadId": thread_id,
                                        "turnId": turn_id_done,
                                        "completedAtMs": chrono::Utc::now().timestamp_millis(),
                                        "item": {
                                            "type": "agentMessage",
                                            "id": item_id_done,
                                            "text": ""
                                        }
                                    }),
                                };
                                let json = serde_json::to_string(&item_notif).unwrap_or_default();
                                let _ = tui_sink.send(Message::text(json)).await;

                                let now = chrono::Utc::now().timestamp();
                                let notif = JsonRpcNotification {
                                    jsonrpc: "2.0".into(),
                                    method: "turn/completed".into(),
                                    params: serde_json::json!({
                                        "threadId": thread_id,
                                        "turn": {
                                            "id": turn_id_done,
                                            "items": [],
                                            "itemsView": "full",
                                            "status": "interrupted",
                                            "error": null,
                                            "startedAt": now,
                                            "completedAt": now,
                                            "durationMs": null
                                        }
                                    }),
                                };
                                let json = serde_json::to_string(&notif).unwrap_or_default();
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json).unwrap_or_default());
                                let _ = tui_sink.send(Message::text(json)).await;
                            }
                        }
                    }
                    Some(WicketEvent::Approval(data)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "approval", "data": &data}));

                        let tool_name = data.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
                        let command = data.get("input")
                            .and_then(|v| v.get("command"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(tool_name);
                        let reason = data.get("input")
                            .and_then(|v| v.get("reason"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        let request_id = next_server_request_id;
                        next_server_request_id += 1;
                        pending_approval_request_id = Some(serde_json::json!(request_id));

                        let turn_id = active_turn_id.as_deref().unwrap_or("");
                        let item_id = active_tool_item_id.as_deref()
                            .or(active_item_id.as_deref())
                            .unwrap_or("");

                        let request = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "method": "item/commandExecution/requestApproval",
                            "params": {
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "itemId": item_id,
                                "startedAtMs": chrono::Utc::now().timestamp_millis(),
                                "command": command,
                                "reason": reason,
                            }
                        });
                        let json = serde_json::to_string(&request).unwrap_or_default();
                        exchange.log("puzzle>tui", &request);
                        tracing::info!(command = %command, "sending approval request to TUI");
                        let _ = tui_sink.send(Message::text(json)).await;
                    }
                    Some(WicketEvent::Meta(data)) => {
                        tracing::debug!("wicket meta: {}", data);
                    }
                    Some(WicketEvent::Error(msg)) => {
                        tracing::error!("wicket error: {}", msg);
                    }
                    None => {
                        tracing::info!("wicket disconnected");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
