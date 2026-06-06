// Easement WebSocket client. Connects, sends packets, receives broadcasts.

use std::sync::Arc;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;

// Inbound: what Easement broadcasts to Puzzle.
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
    Lifecycle {
        slug: String,
        transcript: String,
        #[serde(flatten)]
        event: LifecycleInbound,
    },
    UserMessage {
        slug: String,
        transcript: String,
        text: String,
    },
}

#[derive(serde::Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum HistoryInbound {
    Begin { replay_id: String, last_uuid: Option<String> },
    Entry { replay_id: String, entry: Value },
}

#[derive(serde::Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum TurnInbound {
    Started { turn_id: String },
    Completed { turn_id: String, status: String },
}

#[derive(serde::Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum LifecycleInbound {
    RoundStarted,
    RoundCompleted,
    RoundInterrupted,
    RoundFailed { message: String },
}

// What Puzzle sends to the main loop after parsing.
#[derive(Debug)]
pub enum EasementEvent {
    HistoryBegin { replay_id: String, last_uuid: Option<String>, transcript: String },
    HistoryEntry { replay_id: String, entry: Value },
    Delta(Value),
    Usage(Value),
    TurnStarted { turn_id: String },
    TurnCompleted { turn_id: String, status: String },
    Lifecycle(String),
    UserMessage { text: String },
}

// Outbound: what Puzzle sends to Easement.
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
    Connect { who: String, r#where: String, tools: Vec<Value> },
}

#[derive(serde::Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum HistoryOutbound {
    Replay { slug: String, transcript: String, replay_id: String },
}

#[derive(serde::Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum TurnOutbound {
    Start { slug: String, transcript: String, message: String },
    Steer { slug: String, transcript: String, message: String, expected_turn_id: String },
}

#[derive(serde::Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
enum ShellOutbound {
    Run { slug: String, transcript: String, command: String },
}

pub struct EasementClient {
    outbound_tx: mpsc::UnboundedSender<String>,
    pub event_rx: mpsc::Receiver<EasementEvent>,
    pub pinned_transcript: Arc<RwLock<String>>,
}

fn send(tx: &mpsc::UnboundedSender<String>, msg: Outbound) {
    if let Ok(json) = serde_json::to_string(&msg) {
        let _ = tx.send(json);
    }
}

impl EasementClient {
    pub async fn connect(url: &str, slug: &str) -> Result<Self, std::io::Error> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;

        let (mut sink, stream) = ws_stream.split();

        let my_slug = slug.to_string();
        let pinned_transcript = Arc::new(RwLock::new(String::new()));
        let reader_transcript = pinned_transcript.clone();
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

        send(&outbound_tx, Outbound::Socket(SocketOutbound::Connect {
            who: "puzzle".to_string(),
            r#where: "localhost".to_string(),
            tools: vec![],
        }));

        let (event_tx, event_rx) = mpsc::channel::<EasementEvent>(256);

        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(Message::Text(text)) => {
                        let msg: Inbound = match serde_json::from_str(&text) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };

                        let (msg_slug, msg_transcript) = match &msg {
                            Inbound::History { slug, transcript, .. } => (slug.as_str(), transcript.as_str()),
                            Inbound::Delta { slug, transcript, .. } => (slug.as_str(), transcript.as_str()),
                            Inbound::Usage { slug, transcript, .. } => (slug.as_str(), transcript.as_str()),
                            Inbound::Turn { slug, transcript, .. } => (slug.as_str(), transcript.as_str()),
                            Inbound::Lifecycle { slug, transcript, .. } => (slug.as_str(), transcript.as_str()),
                            Inbound::UserMessage { slug, transcript, .. } => (slug.as_str(), transcript.as_str()),
                        };

                        if msg_slug != my_slug {
                            continue;
                        }
                        let pinned = reader_transcript.read().await;
                        if !pinned.is_empty() && msg_transcript != pinned.as_str() {
                            continue;
                        }
                        drop(pinned);

                        let event = match msg {
                            Inbound::History { transcript, event: HistoryInbound::Begin { replay_id, last_uuid }, .. } => {
                                EasementEvent::HistoryBegin { replay_id, last_uuid, transcript }
                            }
                            Inbound::History { event: HistoryInbound::Entry { replay_id, entry }, .. } => {
                                EasementEvent::HistoryEntry { replay_id, entry }
                            }
                            Inbound::Delta { event, .. } => EasementEvent::Delta(event),
                            Inbound::Usage { usage, .. } => EasementEvent::Usage(usage),
                            Inbound::Turn { event: TurnInbound::Started { turn_id }, .. } => {
                                EasementEvent::TurnStarted { turn_id }
                            }
                            Inbound::Turn { event: TurnInbound::Completed { turn_id, status }, .. } => {
                                EasementEvent::TurnCompleted { turn_id, status }
                            }
                            Inbound::Lifecycle { event, .. } => {
                                let name = match event {
                                    LifecycleInbound::RoundStarted => "round_started",
                                    LifecycleInbound::RoundCompleted => "round_completed",
                                    LifecycleInbound::RoundInterrupted => "round_interrupted",
                                    LifecycleInbound::RoundFailed { .. } => "round_failed",
                                };
                                EasementEvent::Lifecycle(name.to_string())
                            }
                            Inbound::UserMessage { text, .. } => {
                                EasementEvent::UserMessage { text }
                            }
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

        Ok(Self { outbound_tx, event_rx, pinned_transcript })
    }

    pub fn send_raw(&self, msg: &str) -> Result<(), std::io::Error> {
        self.outbound_tx
            .send(msg.to_string())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed"))
    }

    pub fn request_history(&self, slug: &str, transcript: &str, replay_id: &str) -> Result<(), std::io::Error> {
        send(&self.outbound_tx, Outbound::History(HistoryOutbound::Replay {
            slug: slug.to_string(),
            transcript: transcript.to_string(),
            replay_id: replay_id.to_string(),
        }));
        Ok(())
    }

    pub fn send_turn(&self, slug: &str, transcript: &str, message: &str) -> Result<(), std::io::Error> {
        send(&self.outbound_tx, Outbound::Turn(TurnOutbound::Start {
            slug: slug.to_string(),
            transcript: transcript.to_string(),
            message: message.to_string(),
        }));
        Ok(())
    }

    pub fn send_steer(&self, slug: &str, transcript: &str, message: &str, expected_turn_id: &str) -> Result<(), std::io::Error> {
        send(&self.outbound_tx, Outbound::Turn(TurnOutbound::Steer {
            slug: slug.to_string(),
            transcript: transcript.to_string(),
            message: message.to_string(),
            expected_turn_id: expected_turn_id.to_string(),
        }));
        Ok(())
    }

    pub fn send_shell(&self, slug: &str, transcript: &str, command: &str) -> Result<(), std::io::Error> {
        send(&self.outbound_tx, Outbound::Shell(ShellOutbound::Run {
            slug: slug.to_string(),
            transcript: transcript.to_string(),
            command: command.to_string(),
        }));
        Ok(())
    }
}
