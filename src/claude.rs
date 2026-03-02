// Wicket client. This module spawns Wicket and speaks its envelope protocol.
// Wicket is the coordinator — it spawns Easement, handles SSH, binds the
// approval socket, and multiplexes everything into a single NDJSON envelope
// stream. Puzzle sends a JSON payload as the first line of stdin, then
// envelopes: {"stream":"claude","data":{...}} for user messages,
// {"stream":"approval","data":{...}} for approval decisions. Wicket sends
// back envelopes with stream values of stdout, transcript, meta, error,
// and approval.
//
// The SpawnTarget controls where Easement runs. Local spawns Wicket bare.
// Remote adds --remote <host>. Yolo adds --yolo. These map directly to
// Wicket CLI args — Puzzle does not know how to build SSH commands or
// where to put sockets.
//
// The struct IS the invocation — it is created, it runs, it drains, it is
// dropped. The session state lives in the JSONL transcript on disk, not in
// this struct. When the invocation is done, create a new one with the same
// session ID.
//
// The drain gate stays in Puzzle. Wicket does not interpret Claude's stdout
// events. Puzzle tracks sent/replayed counts and flips the drain flag at
// result boundaries.
//
// The payload includes a kickoff message because Claude does not create the
// transcript until the first API call. Easement sends this message
// immediately after spawning Claude. Subsequent messages go through stdin
// passthrough via send().

use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use uuid::Uuid;

// -- Envelope types --

#[derive(Debug, Deserialize)]
struct Envelope {
    stream: String,
    data: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OutboundEnvelope {
    stream: &'static str,
    data: serde_json::Value,
}

// -- Payload --

#[derive(Debug, Serialize)]
pub struct Payload {
    pub slug: String,
    #[serde(default)]
    pub yolo: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<Vec<serde_json::Value>>,
}

// -- Stdout event types --
//
// These types model what comes out of Claude's stdout, wrapped inside
// Easement's {"stream":"stdout","data":{...}} envelopes. The drain gate
// logic depends on result events and replayed user messages.

/// A single event from Claude's stdout stream, unwrapped from an Easement
/// envelope. We parse the type to decide what matters.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum StdoutEvent {
    /// The result event is the per-round completion boundary.
    Result {
        subtype: Option<String>,
        #[serde(default)]
        is_error: bool,
        duration_ms: Option<u64>,
        num_turns: Option<u64>,
        result: Option<String>,
        session_id: Option<String>,
    },

    /// The init event fires at the start of every round.
    System {
        subtype: Option<String>,
        session_id: Option<String>,
    },

    /// Assistant messages from the model.
    Assistant {
        message: serde_json::Value,
        session_id: Option<String>,
        uuid: Option<String>,
    },

    /// User messages on stdout. With --replay-user-messages, injected
    /// messages appear with "isReplay":true for counting.
    User {
        message: serde_json::Value,
        session_id: Option<String>,
        #[serde(default)]
        #[serde(rename = "isReplay")]
        is_replay: bool,
    },

    /// Rate limit check that fires once at process start.
    RateLimitEvent {
        rate_limit_info: serde_json::Value,
    },

    /// Anything we don't recognize.
    #[serde(other)]
    Unknown,
}

// -- Stdin message types --
//
// These go through Wicket → Easement → Claude. The payload line carries the
// kickoff message, so these are for subsequent messages within the same
// invocation.

#[derive(Debug, Serialize)]
struct UserMessage {
    r#type: &'static str,
    message: UserMessageContent,
    uuid: String,
}

#[derive(Debug, Serialize)]
struct UserMessageContent {
    role: &'static str,
    content: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct InterruptRequest {
    r#type: &'static str,
    request: InterruptSubtype,
}

#[derive(Debug, Serialize)]
struct InterruptSubtype {
    subtype: &'static str,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
struct KeepAlive {
    r#type: &'static str,
}

// -- Spawn target --
//
// From Puzzle's perspective, the target determines which CLI args Wicket
// gets. Local is bare. Remote adds --remote <host>. Yolo adds --yolo.
// Puzzle does not know how SSH works or where sockets go.

#[derive(Clone, Debug, PartialEq)]
pub enum SpawnTarget {
    Local,
    Remote { host: String, yolo: bool },
}

// -- The invocation --

#[allow(dead_code)]
pub struct Invocation {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    sent: u64,
    replayed: u64,
    drained: bool,
    session_id: Option<String>,
}

/// What the main loop receives from the envelope reader task. Stdout
/// events drive the drain gate. Transcript entries feed the UI. Approval
/// requests trigger the dialog.
pub enum EasementEvent {
    Stdout(StdoutEvent),
    Transcript(serde_json::Value),
    Approval(serde_json::Value),
}

pub type EventReceiver = mpsc::Receiver<EasementEvent>;

/// Write an envelope to stdin, flushing after.
async fn write_envelope(
    stdin: &mut tokio::process::ChildStdin,
    stream: &'static str,
    data: serde_json::Value,
) -> Result<(), std::io::Error> {
    let envelope = OutboundEnvelope { stream, data };
    let mut line = serde_json::to_string(&envelope)
        .expect("envelope serialization cannot fail");
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

#[allow(dead_code)]
impl Invocation {
    /// Spawn Wicket with a payload. The target controls the CLI args.
    /// The payload includes the kickoff message, so Claude begins
    /// processing immediately. The session ID is captured from stdout
    /// events via handle_event.
    pub async fn spawn(target: &SpawnTarget, payload: Payload) -> Result<(Self, EventReceiver), std::io::Error> {
        let session_id = payload.session_id.clone();

        tracing::info!(
            slug = %payload.slug,
            has_session_id = session_id.is_some(),
            has_transcript = payload.transcript.is_some(),
            target = ?target,
            "spawning wicket"
        );

        let mut cmd = Command::new("wicket");
        match target {
            SpawnTarget::Local => {}
            SpawnTarget::Remote { host, yolo } => {
                cmd.arg("--remote").arg(host);
                if *yolo {
                    cmd.arg("--yolo");
                }
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn()?;

        let mut stdin = child.stdin.take()
            .expect("stdin was set to piped but take() returned None");
        let stdout = child.stdout.take()
            .expect("stdout was set to piped but take() returned None");

        // Write the payload as the first line.
        let mut payload_line = serde_json::to_string(&payload)
            .expect("Payload serialization cannot fail");
        payload_line.push('\n');
        stdin.write_all(payload_line.as_bytes()).await?;
        stdin.flush().await?;

        // Spawn the envelope reader task.
        let (tx, rx) = mpsc::channel::<EasementEvent>(256);

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                let envelope: Envelope = match serde_json::from_str(&line) {
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

                match envelope.stream.as_str() {
                    "stdout" => {
                        match serde_json::from_value::<StdoutEvent>(envelope.data) {
                            Ok(event) => {
                                let event_desc = match &event {
                                    StdoutEvent::Result { .. } => "result",
                                    StdoutEvent::System { .. } => "system",
                                    StdoutEvent::Assistant { .. } => "assistant",
                                    StdoutEvent::User { is_replay, .. } =>
                                        if *is_replay { "user(replay)" } else { "user" },
                                    StdoutEvent::RateLimitEvent { .. } => "rate_limit",
                                    StdoutEvent::Unknown => "unknown",
                                };
                                tracing::debug!(event = event_desc, "stdout event received");
                                if tx.send(EasementEvent::Stdout(event)).await.is_err() {
                                    tracing::warn!("event channel closed");
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("stdout event parse error: {}", e);
                            }
                        }
                    }
                    "transcript" => {
                        let entry_type = envelope.data.get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        tracing::debug!(entry_type, "transcript envelope received");
                        if tx.send(EasementEvent::Transcript(envelope.data)).await.is_err() {
                            tracing::warn!("event channel closed");
                            break;
                        }
                    }
                    "approval" => {
                        tracing::info!("approval envelope received");
                        if tx.send(EasementEvent::Approval(envelope.data)).await.is_err() {
                            tracing::warn!("event channel closed");
                            break;
                        }
                    }
                    "meta" => {
                        tracing::info!("wicket meta: {}", envelope.data);
                    }
                    "error" => {
                        let msg = envelope.data.get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error");
                        tracing::error!("wicket error: {}", msg);
                    }
                    other => {
                        tracing::warn!("unknown envelope stream: {}", other);
                    }
                }
            }
        });

        // sent starts at 1 because the payload's kickoff message is sent
        // by Easement to Claude on our behalf.
        Ok((
            Self {
                child,
                stdin: Some(stdin),
                sent: 1,
                replayed: 0,
                drained: false,
                session_id,
            },
            rx,
        ))
    }

    /// Write a user message wrapped in a stdin envelope. Wicket unwraps
    /// the data field and forwards it to Easement as raw NDJSON.
    pub async fn send(&mut self, content: &str) -> Result<(), std::io::Error> {
        assert!(!self.drained, "send() called after drain — invocation is over");

        let stdin = self.stdin.as_mut()
            .expect("send() called after shutdown — invocation is over");

        let msg = UserMessage {
            r#type: "user",
            message: UserMessageContent {
                role: "user",
                content: content.to_string(),
            },
            uuid: Uuid::new_v4().to_string(),
        };

        let data = serde_json::to_value(&msg)
            .expect("UserMessage serialization cannot fail");
        write_envelope(stdin, "claude", data).await?;

        self.sent += 1;
        tracing::info!(sent = self.sent, "message sent to wicket");
        Ok(())
    }

    /// Send an interrupt wrapped in a stdin envelope.
    pub async fn interrupt(&mut self) -> Result<(), std::io::Error> {
        let stdin = self.stdin.as_mut()
            .expect("interrupt() called after shutdown");

        let msg = InterruptRequest {
            r#type: "control_request",
            request: InterruptSubtype {
                subtype: "interrupt",
            },
        };

        let data = serde_json::to_value(&msg)
            .expect("InterruptRequest serialization cannot fail");
        write_envelope(stdin, "claude", data).await?;
        Ok(())
    }

    /// Send a keep-alive heartbeat wrapped in a stdin envelope.
    pub async fn keep_alive(&mut self) -> Result<(), std::io::Error> {
        let stdin = self.stdin.as_mut()
            .expect("keep_alive() called after shutdown");

        let data = serde_json::to_value(&KeepAlive { r#type: "keep_alive" })
            .expect("KeepAlive serialization cannot fail");
        write_envelope(stdin, "claude", data).await?;
        Ok(())
    }

    /// Send an approval decision back to Wicket.
    pub async fn approve(&mut self, allow: bool, message: Option<&str>) -> Result<(), std::io::Error> {
        let stdin = self.stdin.as_mut()
            .expect("approve() called after shutdown");

        let data = if allow {
            serde_json::json!({ "behavior": "allow" })
        } else {
            serde_json::json!({
                "behavior": "deny",
                "message": message.unwrap_or("User denied permission")
            })
        };

        write_envelope(stdin, "approval", data).await?;
        tracing::info!(allow, "approval decision sent");
        Ok(())
    }

    /// Close stdin and wait for the child to exit.
    pub async fn shutdown(mut self) -> Result<std::process::ExitStatus, std::io::Error> {
        tracing::info!(
            sent = self.sent, replayed = self.replayed, drained = self.drained,
            "shutting down wicket"
        );
        self.stdin.take();
        let status = self.child.wait().await;
        tracing::info!(?status, "wicket exited");
        status
    }

    /// Process an event from the stdout channel. Updates drain gate state
    /// and captures the session ID. Returns true if the invocation just
    /// drained (sent == replayed at a result boundary).
    pub fn handle_event(&mut self, event: &StdoutEvent) -> bool {
        if self.session_id.is_none() {
            let event_session_id = match event {
                StdoutEvent::System { session_id, .. } => session_id.as_ref(),
                StdoutEvent::Result { session_id, .. } => session_id.as_ref(),
                StdoutEvent::Assistant { session_id, .. } => session_id.as_ref(),
                StdoutEvent::User { session_id, .. } => session_id.as_ref(),
                _ => None,
            };
            if let Some(id) = event_session_id {
                tracing::info!(session_id = %id, "captured session id");
                self.session_id = Some(id.clone());
            }
        }

        match event {
            StdoutEvent::User { is_replay: true, .. } => {
                self.replayed += 1;
                tracing::debug!(
                    sent = self.sent, replayed = self.replayed,
                    "user replay, drain gate: {}/{}",
                    self.replayed, self.sent
                );
            }
            StdoutEvent::Result { .. } => {
                tracing::info!(
                    sent = self.sent, replayed = self.replayed,
                    drained = (self.sent == self.replayed),
                    "result event, drain gate: {}/{}",
                    self.replayed, self.sent
                );
                if self.sent == self.replayed {
                    self.drained = true;
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    pub fn is_drained(&self) -> bool {
        self.drained
    }

    pub fn sent(&self) -> u64 {
        self.sent
    }

    pub fn replayed(&self) -> u64 {
        self.replayed
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}
