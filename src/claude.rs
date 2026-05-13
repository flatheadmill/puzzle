// Wicket client over WebSocket. Connects to ws://127.0.0.1:6502 and holds
// the connection for the life of the window. Wicket is long-lived — it
// manages Easement's lifecycle, owns the transcript, and sends normalized
// entries and lifecycle events.
//
// Puzzle sends a connect payload as the first WebSocket message, then
// envelopes: {"stream":"claude","data":{...}} for user messages,
// {"stream":"approval","data":{...}} for approval decisions,
// {"stream":"exit","data":{}} for graceful disconnect.
//
// Wicket sends back envelopes with stream values of entry, lifecycle,
// approval, meta, and error.

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::model::ConversationEntry;

// -- Envelope types --

#[derive(Debug, Deserialize)]
struct InboundEnvelope {
    stream: String,
    data: Value,
}

// -- Spawn target --

#[derive(Clone, Debug, PartialEq)]
pub enum SpawnTarget {
    Local,
    Remote { host: String, yolo: bool },
}

// -- Events from Wicket --

pub enum WicketEvent {
    Entry(ConversationEntry),
    Delta(Value),
    Lifecycle(String),
    Approval(Value),
    Meta(Value),
    Error(String),
}

pub type EventReceiver = mpsc::Receiver<WicketEvent>;

// -- The WebSocket connection to Wicket --

pub struct WicketConnection {
    outbound_tx: mpsc::UnboundedSender<String>,
}

impl WicketConnection {
    pub async fn connect(slug: &str) -> Result<(Self, EventReceiver), std::io::Error> {
        let url = "ws://127.0.0.1:6502";
        let (ws_stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;

        let (mut sink, stream) = ws_stream.split();

        // Send the connect payload.
        let payload = serde_json::json!({ "slug": slug });
        sink.send(Message::text(payload.to_string()))
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;

        tracing::info!(slug, "wicket connected via websocket");

        // Channel for outbound messages from Puzzle to WebSocket.
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<String>();

        // Writer task: outbound channel → WebSocket sink.
        tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                if sink.send(Message::text(msg)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // Channel for inbound events from WebSocket to Puzzle.
        let (event_tx, event_rx) = mpsc::channel::<WicketEvent>(256);

        // Reader task: WebSocket stream → events.
        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(Message::Text(text)) => {
                        let envelope: InboundEnvelope = match serde_json::from_str(&text) {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!(
                                    "envelope parse error: {}",
                                    e,
                                );
                                continue;
                            }
                        };

                        let event = match envelope.stream.as_str() {
                            "entry" => match ConversationEntry::from_value(envelope.data) {
                                Some(entry) => WicketEvent::Entry(entry),
                                None => {
                                    tracing::warn!("entry conversion returned None");
                                    continue;
                                }
                            },
                            "lifecycle" => {
                                let event_name = envelope
                                    .data
                                    .as_str()
                                    .map(|s| s.to_string())
                                    .or_else(|| {
                                        if envelope.data.is_object() {
                                            envelope
                                                .data
                                                .as_object()
                                                .and_then(|m| m.keys().next())
                                                .map(|k| k.to_string())
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or_else(|| format!("{}", envelope.data));
                                WicketEvent::Lifecycle(event_name)
                            }
                            "approval" => WicketEvent::Approval(envelope.data),
                            "delta" => WicketEvent::Delta(envelope.data),
                            "meta" => WicketEvent::Meta(envelope.data),
                            "error" => {
                                let msg = envelope
                                    .data
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown error")
                                    .to_string();
                                tracing::error!("wicket error: {}", msg);
                                WicketEvent::Error(msg)
                            }
                            other => {
                                tracing::warn!("unknown envelope stream: {}", other);
                                continue;
                            }
                        };

                        if event_tx.send(event).await.is_err() {
                            tracing::warn!("event channel closed");
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

        Ok((Self { outbound_tx }, event_rx))
    }

    /// Send a user message to start a new round or interject.
    pub async fn send(
        &mut self,
        content: &str,
        target: &SpawnTarget,
    ) -> Result<(), std::io::Error> {
        let (remote, yolo) = match target {
            SpawnTarget::Local => (None, false),
            SpawnTarget::Remote { host, yolo } => (Some(host.as_str()), *yolo),
        };

        let data = serde_json::json!({
            "message": content,
            "remote": remote,
            "yolo": yolo,
        });

        let envelope = serde_json::json!({ "stream": "claude", "data": data });
        self.outbound_tx
            .send(envelope.to_string())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed"))?;

        tracing::info!("message sent to wicket");
        Ok(())
    }

    /// Send an approval decision.
    pub async fn approve(
        &mut self,
        allow: bool,
        message: Option<&str>,
    ) -> Result<(), std::io::Error> {
        let data = if allow {
            serde_json::json!({ "behavior": "allow" })
        } else {
            serde_json::json!({
                "behavior": "deny",
                "message": message.unwrap_or("User denied permission")
            })
        };

        let envelope = serde_json::json!({ "stream": "approval", "data": data });
        self.outbound_tx
            .send(envelope.to_string())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed"))?;

        tracing::info!(allow, "approval decision sent");
        Ok(())
    }

    /// Send a log entry to Wicket for centralized logging.
    pub fn log(&self, level: &str, message: &str, fields: serde_json::Value) {
        let data = serde_json::json!({
            "level": level,
            "message": message,
            "fields": fields,
        });
        let envelope = serde_json::json!({ "stream": "log", "data": data });
        let _ = self.outbound_tx.send(envelope.to_string());
    }

    /// Graceful disconnect. Sends an explicit exit message rather than
    /// relying on socket close semantics.
    pub async fn shutdown(self) -> Result<(), std::io::Error> {
        tracing::info!("shutting down wicket connection");
        let envelope = serde_json::json!({ "stream": "exit", "data": {} });
        let _ = self.outbound_tx.send(envelope.to_string());
        // Drop self, which drops outbound_tx, which closes the writer
        // task's channel, which closes the WebSocket.
        Ok(())
    }
}
