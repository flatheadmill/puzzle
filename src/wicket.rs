// Wicket WebSocket client. Connects to Wicket, sends envelopes, receives events.
// Carried forward from the TUI era's claude.rs, simplified for the translator.

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Deserialize)]
struct InboundEnvelope {
    stream: String,
    data: Value,
}

#[derive(Debug)]
pub enum WicketEvent {
    Delta(Value),
    Entry(Value),
    Usage(Value),
    Lifecycle(String),
    Approval(Value),
    Meta(Value),
    Error(String),
}

pub struct WicketClient {
    outbound_tx: mpsc::UnboundedSender<String>,
    pub event_rx: mpsc::Receiver<WicketEvent>,
}

impl WicketClient {
    pub async fn connect(url: &str, slug: &str) -> Result<Self, std::io::Error> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;

        let (mut sink, stream) = ws_stream.split();

        // Send the connect payload.
        let payload = serde_json::json!({ "slug": slug });
        sink.send(Message::text(payload.to_string()))
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;

        tracing::info!(slug, "wicket connected");

        // Outbound channel.
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<String>();

        // Writer task.
        tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                if sink.send(Message::text(msg)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // Inbound channel.
        let (event_tx, event_rx) = mpsc::channel::<WicketEvent>(256);

        // Reader task.
        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(Message::Text(text)) => {
                        let envelope: InboundEnvelope = match serde_json::from_str(&text) {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!("envelope parse error: {}", e);
                                continue;
                            }
                        };

                        let event = match envelope.stream.as_str() {
                            "delta" => WicketEvent::Delta(envelope.data),
                            "entry" => WicketEvent::Entry(envelope.data),
                            "usage" => WicketEvent::Usage(envelope.data),
                            "lifecycle" => {
                                let name = envelope.data
                                    .as_str()
                                    .map(|s| s.to_string())
                                    .or_else(|| {
                                        envelope.data.as_object()
                                            .and_then(|m| m.keys().next())
                                            .map(|k| k.to_string())
                                    })
                                    .unwrap_or_else(|| format!("{}", envelope.data));
                                WicketEvent::Lifecycle(name)
                            }
                            "approval" => WicketEvent::Approval(envelope.data),
                            "meta" => WicketEvent::Meta(envelope.data),
                            "error" => {
                                let msg = envelope.data
                                    .get("message")
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
                        tracing::warn!("wicket read error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(Self { outbound_tx, event_rx })
    }

    pub fn send_envelope(&self, stream: &str, data: Value) -> Result<(), std::io::Error> {
        let envelope = serde_json::json!({ "stream": stream, "data": data });
        self.outbound_tx
            .send(envelope.to_string())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed"))?;
        Ok(())
    }

    pub fn send_claude_message(&self, message: &str) -> Result<(), std::io::Error> {
        self.send_envelope("claude", serde_json::json!({
            "message": message,
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
