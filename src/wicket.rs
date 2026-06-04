// Easement WebSocket client. Connects, sends envelopes, receives bus messages.

use std::sync::Arc;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug)]
pub enum WicketEvent {
    Delta(Value),
    Entry { data: Value, replay_id: Option<String> },
    Usage(Value),
    ToolStart(Value),
    ToolDone(Value),
    ShellResult(Value),
    UserMessage { text: String, notification: bool },
    Lifecycle(String),
    Turn(Value),
    Approval(Value),
    HistoryTerminate { replay_id: String, timestamp: String },
    Meta(Value),
    Error(String),
}

pub struct WicketClient {
    outbound_tx: mpsc::UnboundedSender<String>,
    pub event_rx: mpsc::Receiver<WicketEvent>,
    pub pinned_timestamp: Arc<RwLock<String>>,
}

impl WicketClient {
    pub async fn connect(url: &str, slug: &str) -> Result<Self, std::io::Error> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;

        let (mut sink, stream) = ws_stream.split();

        let my_slug = slug.to_string();
        let pinned_timestamp = Arc::new(RwLock::new(String::new()));
        let reader_ts = pinned_timestamp.clone();
        tracing::info!(slug = %my_slug, "websocket connected");

        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                if sink.send(Message::text(msg)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let (event_tx, event_rx) = mpsc::channel::<WicketEvent>(256);

        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(Message::Text(text)) => {
                        let msg: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        if let Some(msg_slug) = msg.get("slug").and_then(|v| v.as_str()) {
                            if msg_slug != my_slug {
                                continue;
                            }
                            let ts = reader_ts.read().await;
                            if !ts.is_empty() {
                                if let Some(msg_ts) = msg.get("timestamp").and_then(|v| v.as_str()) {
                                    if msg_ts != ts.as_str() {
                                        continue;
                                    }
                                }
                            }
                        }

                        let stream_name = msg.get("stream").and_then(|v| v.as_str()).unwrap_or("");
                        let data = msg.get("data").cloned().unwrap_or_default();
                        let replay_id = msg.get("replay_id").and_then(|v| v.as_str()).map(|s| s.to_string());

                        let event = match stream_name {
                            "delta" => WicketEvent::Delta(data),
                            "entry" => WicketEvent::Entry { data, replay_id },
                            "history_terminate" => {
                                if let Some(rid) = replay_id {
                                    let msg_ts = msg.get("timestamp").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    WicketEvent::HistoryTerminate { replay_id: rid, timestamp: msg_ts }
                                } else {
                                    continue;
                                }
                            }
                            "usage" => WicketEvent::Usage(data),
                            "tool_start" => WicketEvent::ToolStart(data),
                            "tool_done" => WicketEvent::ToolDone(data),
                            "shell_result" => WicketEvent::ShellResult(data),
                            "user_message" => {
                                let text = data.get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let notif = data.get("notification")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                WicketEvent::UserMessage { text, notification: notif }
                            }
                            "turn" => WicketEvent::Turn(data),
                            "lifecycle" => {
                                let name = data.as_str()
                                    .map(|s| s.to_string())
                                    .or_else(|| {
                                        data.as_object()
                                            .and_then(|m| m.keys().next())
                                            .map(|k| k.to_string())
                                    })
                                    .unwrap_or_else(|| format!("{}", data));
                                WicketEvent::Lifecycle(name)
                            }
                            "approval" => WicketEvent::Approval(data),
                            "meta" => WicketEvent::Meta(data),
                            "error" => {
                                let msg = data.get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown error")
                                    .to_string();
                                WicketEvent::Error(msg)
                            }
                            _ => continue,
                        };

                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Err(e) => {
                        tracing::warn!("websocket read error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(Self { outbound_tx, event_rx, pinned_timestamp })
    }

    pub fn send_raw(&self, msg: &str) -> Result<(), std::io::Error> {
        self.outbound_tx
            .send(msg.to_string())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed"))
    }

    pub fn send_envelope(&self, stream: &str, data: Value) -> Result<(), std::io::Error> {
        let envelope = serde_json::json!({ "stream": stream, "data": data });
        self.send_raw(&envelope.to_string())
    }


    pub fn request_history(&self, slug: &str, intent: &str, replay_id: &str) -> Result<(), std::io::Error> {
        self.send_envelope("history_request", serde_json::json!({
            "slug": slug,
            "intent": intent,
            "replay_id": replay_id,
        }))
    }

    pub fn send_approval(&self, allow: bool, message: Option<&str>) -> Result<(), std::io::Error> {
        let data = if allow {
            serde_json::json!({ "behavior": "allow" })
        } else {
            serde_json::json!({
                "behavior": "deny",
                "message": message.unwrap_or("denied")
            })
        };
        self.send_envelope("approval", data)
    }
}
