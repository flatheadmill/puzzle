// Wicket client. Spawns Wicket once and holds the connection for the life
// of the window. Wicket is long-lived — it manages Easement's lifecycle,
// owns the transcript, and sends normalized entries and lifecycle events.
//
// Puzzle sends a connect payload as the first message, then envelopes:
// {"stream":"claude","data":{...}} for user messages,
// {"stream":"approval","data":{...}} for approval decisions.
//
// Wicket sends back envelopes with stream values of entry, lifecycle,
// approval, meta, and error.
//
// The SpawnTarget controls where Easement runs. It is communicated to
// Wicket per-round in the claude message, not as CLI args.

use std::process::Stdio;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use crate::model::ConversationEntry;

// -- Envelope types --

#[derive(Debug, Deserialize)]
struct InboundEnvelope {
    stream: String,
    data: Value,
}

#[derive(Debug, Serialize)]
struct OutboundEnvelope {
    stream: &'static str,
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
    Lifecycle(String),
    Approval(Value),
    Meta(Value),
    Error(String),
}

pub type EventReceiver = mpsc::Receiver<WicketEvent>;

// -- The persistent Wicket connection --

pub struct WicketConnection {
    child: Child,
    stdin: tokio::process::ChildStdin,
}

/// Write an envelope to Wicket's stdin.
async fn write_envelope(
    stdin: &mut tokio::process::ChildStdin,
    stream: &'static str,
    data: Value,
) -> Result<(), std::io::Error> {
    let envelope = OutboundEnvelope { stream, data };
    let mut line = serde_json::to_string(&envelope).expect("envelope serialization cannot fail");
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

impl WicketConnection {
    /// Spawn Wicket and send the connect payload. Returns the connection
    /// and a channel that delivers events as Wicket streams them.
    pub async fn connect(slug: &str) -> Result<(Self, EventReceiver), std::io::Error> {
        let mut cmd = Command::new("wicket");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn()?;

        let mut stdin = child
            .stdin
            .take()
            .expect("stdin was set to piped but take() returned None");
        let stdout = child
            .stdout
            .take()
            .expect("stdout was set to piped but take() returned None");

        // Send the connect payload.
        let payload = serde_json::json!({ "slug": slug });
        let mut payload_line = serde_json::to_string(&payload).unwrap();
        payload_line.push('\n');
        stdin.write_all(payload_line.as_bytes()).await?;
        stdin.flush().await?;

        tracing::info!(slug, "wicket connected");

        // Spawn the envelope reader task.
        let (tx, rx) = mpsc::channel::<WicketEvent>(256);

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                let envelope: InboundEnvelope = match serde_json::from_str(&line) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(
                            "envelope parse error: {} — line: {}",
                            e,
                            &line[..line.len().min(200)]
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
                                // Handle tagged enum format: {"round_started": ...} or string
                                if envelope.data.is_object() {
                                    // Serde serializes LifecycleEvent as {"round_started": ...}
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

                if tx.send(event).await.is_err() {
                    tracing::warn!("event channel closed");
                    break;
                }
            }
        });

        Ok((Self { child, stdin }, rx))
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

        write_envelope(&mut self.stdin, "claude", data).await?;
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

        write_envelope(&mut self.stdin, "approval", data).await?;
        tracing::info!(allow, "approval decision sent");
        Ok(())
    }

    /// Close the connection and wait for Wicket to exit.
    pub async fn shutdown(mut self) -> Result<std::process::ExitStatus, std::io::Error> {
        tracing::info!("shutting down wicket");
        drop(self.stdin);
        let status = self.child.wait().await;
        tracing::info!(?status, "wicket exited");
        status
    }
}
