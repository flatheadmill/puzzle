// Translation loop between the Codex TUI (JSON-RPC/WebSocket) and Wicket (envelope/WebSocket).
//
// Reads JSON-RPC messages from the TUI, translates to Wicket envelopes.
// Reads Wicket events, translates to JSON-RPC notifications/requests for the TUI.

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

pub async fn run(
    tui_ws: WebSocketStream<UnixStream>,
    mut wicket: WicketClient,
    exchange: ExchangeLog,
    slug: &str,
) -> color_eyre::eyre::Result<()> {
    let (mut tui_sink, mut tui_stream) = tui_ws.split();

    loop {
        tokio::select! {
            // Messages from the Codex TUI.
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

                        // Notifications have no id — acknowledge silently.
                        if id.is_none() {
                            tracing::debug!(method, "TUI notification (no response needed)");
                            continue;
                        }
                        let id = id.unwrap_or(Value::Null);

                        match method {
                            "initialize" => {
                                tracing::info!("TUI: initialize");
                                let response = JsonRpcResponse {
                                    jsonrpc: "2.0".into(),
                                    id,
                                    result: serde_json::json!({
                                        "protocolVersion": "2.0",
                                        "serverInfo": {
                                            "name": "puzzle",
                                            "version": "0.1.0"
                                        },
                                        "capabilities": {}
                                    }),
                                };
                                let resp_json = serde_json::to_string(&response)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&resp_json)?);
                                tui_sink.send(Message::text(resp_json)).await?;
                            }

                            "thread/start" | "thread/resume" => {
                                tracing::info!(method, "TUI: thread request");
                                // TODO: build response from Wicket history.
                                // For now, return an empty thread.
                                let thread_id = uuid::Uuid::new_v4().to_string();
                                let home = std::env::var("HOME").unwrap_or_default();
                                let cwd = format!("{}/pane/{}", home, slug);
                                let response = JsonRpcResponse {
                                    jsonrpc: "2.0".into(),
                                    id,
                                    result: serde_json::json!({
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
                                            "turns": []
                                        },
                                        "model": "claude-opus-4-6",
                                        "modelProvider": "anthropic",
                                        "cwd": cwd,
                                        "approvalPolicy": "on-failure",
                                        "approvalsReviewer": "user",
                                        "sandbox": { "type": "dangerFullAccess" },
                                        "instructionSources": []
                                    }),
                                };
                                let resp_json = serde_json::to_string(&response)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&resp_json)?);
                                tui_sink.send(Message::text(resp_json)).await?;
                            }

                            "account/read" => {
                                tracing::info!("TUI: account/read");
                                let response = JsonRpcResponse {
                                    jsonrpc: "2.0".into(),
                                    id,
                                    result: serde_json::json!({
                                        "account": {
                                            "type": "apiKey"
                                        },
                                        "requiresOpenaiAuth": false
                                    }),
                                };
                                let resp_json = serde_json::to_string(&response)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&resp_json)?);
                                tui_sink.send(Message::text(resp_json)).await?;
                            }

                            "model/list" => {
                                tracing::info!("TUI: model/list");
                                let response = JsonRpcResponse {
                                    jsonrpc: "2.0".into(),
                                    id,
                                    result: serde_json::json!({
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
                                    }),
                                };
                                let resp_json = serde_json::to_string(&response)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&resp_json)?);
                                tui_sink.send(Message::text(resp_json)).await?;
                            }

                            _ => {
                                tracing::info!(method, "TUI: unhandled method");
                                let error = JsonRpcError {
                                    jsonrpc: "2.0".into(),
                                    id,
                                    error: JsonRpcErrorBody {
                                        code: -32601,
                                        message: format!("not implemented: {}", method),
                                    },
                                };
                                let err_json = serde_json::to_string(&error)?;
                                exchange.log("puzzle>tui", &serde_json::from_str::<Value>(&err_json)?);
                                tui_sink.send(Message::text(err_json)).await?;
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

            // Events from Wicket.
            event = wicket.event_rx.recv() => {
                match event {
                    Some(WicketEvent::Delta(delta)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "delta", "data": &delta}));
                        // TODO: translate to ServerNotification and send to TUI.
                    }
                    Some(WicketEvent::Entry(entry)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "entry", "data": &entry}));
                        // TODO: translate to history items.
                    }
                    Some(WicketEvent::Lifecycle(name)) => {
                        tracing::info!(lifecycle = %name, "wicket lifecycle");
                    }
                    Some(WicketEvent::Approval(data)) => {
                        exchange.log("wicket>puzzle", &serde_json::json!({"type": "approval", "data": &data}));
                        // TODO: translate to ServerRequest for approval.
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
