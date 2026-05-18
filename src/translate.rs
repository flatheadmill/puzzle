// Translation loop between the Codex TUI (JSON-RPC/WebSocket) and Wicket (envelope/WebSocket).

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

fn wicket_entry_to_thread_items(entry: &Value) -> Vec<Value> {
    let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let blocks = entry.get("blocks").and_then(|v| v.as_array());
    let uuid = entry.get("uuid").and_then(|v| v.as_str())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().leak());

    let Some(blocks) = blocks else { return vec![] };
    let mut items = vec![];

    for block in blocks {
        let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match (kind, btype) {
            ("user", "text") => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                items.push(serde_json::json!({
                    "type": "userMessage",
                    "id": uuid,
                    "content": [{ "type": "text", "text": text }]
                }));
            }
            ("assistant", "text") => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                items.push(serde_json::json!({
                    "type": "agentMessage",
                    "id": uuid,
                    "text": text
                }));
            }
            ("assistant", "thinking") => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                items.push(serde_json::json!({
                    "type": "reasoning",
                    "id": uuid,
                    "summary": [text],
                    "content": []
                }));
            }
            ("assistant", "tool_use") => {
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                let summary = block.get("input_summary").and_then(|v| v.as_str()).unwrap_or("");
                let home = std::env::var("HOME").unwrap_or_default();
                items.push(serde_json::json!({
                    "type": "commandExecution",
                    "id": uuid,
                    "command": format!("{} {}", name, summary),
                    "cwd": format!("{}/pane/solver", home),
                    "source": "agent",
                    "status": "completed",
                    "commandActions": [],
                    "aggregatedOutput": null,
                    "exitCode": 0,
                    "durationMs": null
                }));
            }
            ("user", "tool_result") => {
                // Tool results are part of the command execution, skip for now.
            }
            _ => {}
        }
    }

    items
}

fn build_turns_from_entries(entries: &[Value]) -> Vec<Value> {
    let mut turns: Vec<Value> = vec![];
    let mut current_items: Vec<Value> = vec![];
    let mut last_kind = "";

    for entry in entries {
        let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");

        // Start a new turn when we see a user message after assistant content.
        if kind == "user" && last_kind == "assistant" && !current_items.is_empty() {
            // Close the previous turn (assistant turn).
            // But actually, we should group user + assistant into one turn.
        }

        let items = wicket_entry_to_thread_items(entry);
        current_items.extend(items);
        last_kind = kind;
    }

    // Package all items as a single turn for now.
    if !current_items.is_empty() {
        turns.push(serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "items": current_items,
            "itemsView": "full",
            "status": "completed",
            "startedAt": 0,
            "completedAt": 0
        }));
    }

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

                            "turn/start" => {
                                let params = rpc.params.unwrap_or_default();
                                // Extract text from input: [{type: "text", text: "..."}]
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

                                let turn_id = uuid::Uuid::new_v4().to_string();
                                let item_id = uuid::Uuid::new_v4().to_string();
                                tracing::info!(message, turn_id = %turn_id, "TUI: turn/start");

                                if !message.is_empty() {
                                    wicket.send_claude_message(message)?;
                                }

                                // Respond with a Turn.
                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({
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
                                        "completedAt": null
                                    }
                                })).await?;

                                // Send turn/started notification.
                                let notif = JsonRpcNotification {
                                    jsonrpc: "2.0".into(),
                                    method: "turn/started".into(),
                                    params: serde_json::json!({
                                        "threadId": thread_id,
                                        "turnId": turn_id
                                    }),
                                };
                                let notif_json = serde_json::to_string(&notif)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&notif_json)?);
                                tui_sink.send(Message::text(notif_json)).await?;

                                // Store the active turn context for streaming.
                                active_turn_id = Some(turn_id);
                                active_item_id = Some(item_id);
                            }

                            "turn/steer" => {
                                // Interjection during an active turn.
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
                                if !message.is_empty() {
                                    wicket.send_claude_message(message)?;
                                }
                                send_response(&mut tui_sink, &exchange, id, serde_json::json!({})).await?;
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

                        // Translate streaming deltas to Codex notifications.
                        if let (Some(turn_id), Some(item_id)) = (&active_turn_id, &active_item_id) {
                            let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match delta_type {
                                "content_block_start" => {
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
                                "content_block_delta" => {
                                    let inner = delta.get("delta").unwrap_or(&Value::Null);
                                    let text = inner.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                    if !text.is_empty() {
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
                                "message_stop" => {
                                    // Stream is done. Send item/completed then turn/completed.
                                    let turn_id = active_turn_id.take().unwrap_or_default();
                                    let item_id = active_item_id.take().unwrap_or_default();

                                    // item/completed
                                    let item_notif = JsonRpcNotification {
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
                                    let json = serde_json::to_string(&item_notif)?;
                                    exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
                                    tui_sink.send(Message::text(json)).await?;

                                    // turn/completed
                                    let notif = JsonRpcNotification {
                                        jsonrpc: "2.0".into(),
                                        method: "turn/completed".into(),
                                        params: serde_json::json!({
                                            "threadId": thread_id,
                                            "turnId": turn_id,
                                            "status": "completed"
                                        }),
                                    };
                                    let json = serde_json::to_string(&notif)?;
                                    exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&json)?);
                                    tui_sink.send(Message::text(json)).await?;
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(WicketEvent::Entry(entry)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "entry", "data": &entry}));
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
                    Some(WicketEvent::Lifecycle(name)) => {
                        tracing::info!(lifecycle = %name, "wicket lifecycle");
                    }
                    Some(WicketEvent::Approval(data)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "approval", "data": &data}));
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
