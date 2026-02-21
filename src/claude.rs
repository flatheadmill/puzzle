// Claude process abstraction. This module models one invocation of the Claude
// CLI as a child process with a bidirectional NDJSON channel. The struct IS the
// invocation — it is created, it runs, it drains, it is dropped. There is no
// long-lived session manager. The session state lives in the JSONL transcript
// on disk, not in this struct. When the invocation is done, create a new one
// with --resume against the same session.
//
// The discovery that drives this design came from FIFO and coproc testing
// against the live CLI (~/code/junk/eof.zsh is the reference harness). The
// key findings:
//
// 1. --input-format stream-json works with --print. No positional prompt
//    argument is required. The process starts, hooks fire, and it waits for
//    NDJSON user messages on stdin.
//
// 2. Each round on stdout follows: system:init -> assistant/user flurry ->
//    result:success. The init event fires per round, not per process. The
//    result event is the completion boundary.
//
// 3. User messages injected mid-turn are absorbed into the current round.
//    They appear in the JSONL transcript on disk but NOT on stdout — unless
//    --replay-user-messages is set. With that flag, injected messages appear
//    on stdout with "isReplay":true, making them countable.
//
// 4. Closing stdin prematurely (before the result event) kills the process
//    before the turn completes. Stdin must stay open until the round finishes.
//    The correct shutdown: wait for result, then close stdin. The process sees
//    EOF and exits cleanly with a flushed transcript.
//
// 5. The JSONL transcript on disk and the stdout stream are different formats.
//    The transcript has queue-operation, progress, parentUuid, isSidechain,
//    slug, requestId — rich conversation record. Stdout has system:init,
//    result:success, and sparser event schemas. The result event only exists
//    on stdout. The queue-operation events only exist in the JSONL.
//
// 6. Mid-turn injection works but not between parallel tool calls. Messages
//    sent during tool execution are queued and appear on the next API call
//    within the same round. Claude sees them and can acknowledge them. But
//    they don't interrupt running tools and they don't create separate rounds.
//    Once the result event arrives, any subsequent message starts a new round
//    with fresh reasoning.
//
// 7. The JSONL transcript is the portable artifact. Stop a process on one
//    machine, resume on another with --resume <session-id>. The process is
//    ephemeral. The session is the state.
//
// 8. --resume accepts either a session ID or a .jsonl file path. When given
//    a file path, the CLI generates a new random UUID as the session ID,
//    reads the file for history (filtering to assistant/user entries and
//    applying cleanup transforms), and writes the new session to its own
//    JSONL under ~/.claude/projects/. The original file is read-only input.
//    When given a session ID, it resumes in place. This fork behavior is
//    how environment transitions work: copy the JSONL to the target, resume
//    from the file path, get a new session ID back. Subsequent resumes on
//    that machine use the session ID directly. Tested with clone.zsh and
//    touch.zsh — an empty touched file also works, producing a fresh session
//    with no history. The new session ID appears in every stdout event
//    starting from the first (system hook_started), so the Invocation
//    captures it from the stream and always exposes it to the caller.
//    (cli.js:543099-543106 — file path detection and randomUUID generation.)
//
// The concurrency model matches what main.rs already does: a spawned task
// reads the child's stdout and sends events through an mpsc channel. The main
// loop's tokio::select! gains a branch for this channel. The struct lives in
// the main loop. No mutex.

use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use uuid::Uuid;

// -- Stdout event types --
//
// The stdout stream-json output has a different schema from the JSONL transcript
// on disk. These types model what comes out of stdout, not what's in the file.
// The tailer (tailer.rs) reads the file. This module reads stdout.
//
// We deserialize loosely — serde(tag = "type") with an Unknown catch-all — so
// new event types from future CLI versions don't break us. We only care about
// a few event types: result (drain signal), system:init (round start marker),
// and user messages with isReplay (for counting injected messages).

/// A single event from the child's stdout stream. The CLI emits one NDJSON
/// line per event. We parse the type to decide what matters.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // fields parsed for completeness; available as rendering gets richer
pub enum StdoutEvent {
    /// The result event is the per-round completion boundary. When Puzzle sees
    /// this, the round is done. If nothing is queued on Puzzle's side, the
    /// process is idle and stdin can be closed. The result carries cumulative
    /// cost, usage, and duration for the entire process lifetime.
    Result {
        subtype: Option<String>,
        #[serde(default)]
        is_error: bool,
        duration_ms: Option<u64>,
        num_turns: Option<u64>,
        result: Option<String>,
        session_id: Option<String>,
    },

    /// The init event fires at the start of every round — each time a user
    /// message triggers an API call. It carries session metadata, tools, model.
    /// The very first round also gets hook events and a rate_limit_event before
    /// this, but subsequent rounds just get init.
    System {
        subtype: Option<String>,
        session_id: Option<String>,
    },

    /// Assistant messages from the model. These include text responses, tool_use
    /// blocks, and thinking blocks. They appear between init and result. Multiple
    /// assistant events can occur per round (text + tool calls, or parallel tool
    /// calls sharing the same message id).
    Assistant {
        message: serde_json::Value,
        session_id: Option<String>,
    },

    /// User messages on stdout. Without --replay-user-messages, only tool_result
    /// messages appear (the CLI's internal tool result delivery). With the flag,
    /// injected user messages also appear with "isReplay":true. This is how we
    /// count absorbed messages.
    User {
        message: serde_json::Value,
        session_id: Option<String>,
        #[serde(default)]
        #[serde(rename = "isReplay")]
        is_replay: bool,
    },

    /// Rate limit check that fires once at process start before the first round.
    RateLimitEvent {
        rate_limit_info: serde_json::Value,
    },

    /// Anything we don't recognize. Future CLI versions may add event types.
    /// We ignore them rather than crashing.
    #[serde(other)]
    Unknown,
}

// -- Stdin message types --
//
// The NDJSON protocol for writing to the child's stdin. Three message types
// matter. Malformed JSON kills the child (process.exit(1) in the CLI source),
// so serialization must be correct. These types are not configurable — they
// are the protocol.

/// A user message written to stdin. This is what triggers a round.
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

#[allow(dead_code)] // interrupt and keep_alive not yet wired; API surface for mid-turn control
/// An interrupt request. Cancels the current turn via an abort controller in
/// the CLI. After the turn unwinds, the executor clears its running flag and
/// processes the next queued command. Pattern for urgent intervention: send
/// interrupt, then send user message with the warning.
#[derive(Debug, Serialize)]
struct InterruptRequest {
    r#type: &'static str,
    request: InterruptSubtype,
}

#[derive(Debug, Serialize)]
struct InterruptSubtype {
    subtype: &'static str,
}

/// A keep-alive heartbeat. Accepted and silently ignored by the CLI parser.
/// Puzzle can send these periodically to keep the pipe active during long
/// idle periods.
#[derive(Debug, Serialize)]
#[allow(dead_code)]
struct KeepAlive {
    r#type: &'static str,
}

// -- The invocation --
//
// One struct, one child process. Calling spawn() starts the child. Messages
// go in via send(). Events come out via the mpsc channel returned from
// spawn(). When the result event arrives and nothing is queued, call
// shutdown() to close stdin. The child exits. The struct is done. Drop it.
//
// If you call send() after shutdown(), that is a bug. The invocation is over.
// New messages need a new Invocation with --resume against the same session.
//
// The invocation flags are internal. They are how Puzzle talks to Claude:
//   --print                       headless mode, no TUI
//   --input-format stream-json    NDJSON on stdin
//   --output-format stream-json   NDJSON on stdout
//   --replay-user-messages        injected messages appear on stdout
//   --verbose                     required with --output-format stream-json
//   --max-thinking-tokens 31999   hidden flag, enables extended thinking
//   --resume <target>             session ID or .jsonl file path
//
// The --allowed-tools flag may be added later to restrict the tool set.
// The --permission-prompt-tool flag is for the MCP approval relay (step 012).

/// What to pass to --resume. A session ID resumes in place. A file path
/// forks — the CLI reads the file for history, generates a new session ID,
/// and writes to its own JSONL under ~/.claude/projects/. An empty touched
/// file works too, producing a fresh session with no history.
#[allow(dead_code)] // FilePath variant is the fork mechanism; not yet wired in main
pub enum ResumeTarget {
    /// A session UUID. Resumes the existing session in place.
    SessionId(String),
    /// A .jsonl file path. The CLI forks: reads history from the file,
    /// creates a new session with a new UUID. The file is read-only input.
    FilePath(String),
}

#[allow(dead_code)] // sent/replayed/model not yet read externally; API surface for future use
pub struct Invocation {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    sent: u64,
    replayed: u64,
    drained: bool,
    session_id: Option<String>,
    model: Option<String>,
}

/// What the main loop receives from the stdout reader task. This is the
/// channel type — it carries parsed events from the child's stdout.
pub type EventReceiver = mpsc::Receiver<StdoutEvent>;

#[allow(dead_code)] // interrupt, keep_alive, is_drained, sent, replayed: API surface for future use
impl Invocation {
    /// Create and spawn a new invocation. Returns the invocation and a channel
    /// receiver for stdout events. The caller (main loop) reads from the
    /// receiver in its tokio::select! loop.
    ///
    /// The child process starts immediately. Hooks fire. The process waits for
    /// the first user message on stdin. Nothing happens until send() is called.
    ///
    /// The resume target determines what --resume gets. A session ID resumes
    /// in place. A file path forks into a new session. Either way, the CLI
    /// emits a session ID in stdout events starting from the first event.
    /// The Invocation captures it via handle_event and exposes it through
    /// session_id(). The caller always gets the session ID back.
    pub fn spawn(
        target: ResumeTarget,
        model: Option<String>,
    ) -> Result<(Self, EventReceiver), std::io::Error> {
        let resume_arg = match &target {
            ResumeTarget::SessionId(id) => id.clone(),
            ResumeTarget::FilePath(path) => path.clone(),
        };

        let mut cmd = Command::new("claude");
        cmd.arg("--print")
            .arg("--input-format").arg("stream-json")
            .arg("--output-format").arg("stream-json")
            .arg("--replay-user-messages")
            .arg("--verbose")
            .arg("--max-thinking-tokens").arg("31999")
            .arg("--resume").arg(&resume_arg);

        if let Some(ref m) = model {
            cmd.arg("--model").arg(m);
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take()
            .expect("stdout was set to piped but take() returned None");

        // Spawn the stdout reader task. It reads lines, parses NDJSON, and
        // sends events through the channel. This matches the pattern in main.rs
        // where the tailer and run_claude_print both use tokio::spawn with mpsc.
        //
        // The channel buffer is 256 — same as the tailer channel. Events arrive
        // at the rate the CLI produces them, which is bounded by API response
        // time and tool execution. 256 is generous.
        let (tx, rx) = mpsc::channel::<StdoutEvent>(256);

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                match serde_json::from_str::<StdoutEvent>(&line) {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            // receiver dropped, main loop is done with us
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("stdout parse error: {} — line: {}", e, &line[..line.len().min(200)]);
                    }
                }
            }
            // stdout closed means child exited or pipe broke. the task ends.
            // the channel drops. the main loop sees None from rx.recv().
        });

        // if resuming by session ID, we already know it. if resuming by
        // file path, the CLI generates a new one and we'll capture it from
        // the first stdout event in handle_event.
        let session_id = match target {
            ResumeTarget::SessionId(id) => Some(id),
            ResumeTarget::FilePath(_) => None,
        };

        Ok((
            Self {
                child,
                stdin,
                sent: 0,
                replayed: 0,
                drained: false,
                session_id,
                model,
            },
            rx,
        ))
    }

    /// Write a user message to the child's stdin. This is what starts a round
    /// or injects a message into an ongoing round. The message is serialized as
    /// NDJSON with a trailing newline. Sent count increments.
    ///
    /// If the round is idle (after a result event), this message starts a new
    /// round. If a round is in progress, this message is queued by the CLI and
    /// absorbed into the current round — it appears on the next API call within
    /// the round. Claude sees it but it doesn't create a separate result event.
    ///
    /// Panics if called after drain or shutdown. The invocation is over.
    /// Create a new one.
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

        let mut serialized = serde_json::to_string(&msg)
            .expect("UserMessage serialization cannot fail");
        serialized.push('\n');

        stdin.write_all(serialized.as_bytes()).await?;
        stdin.flush().await?;

        self.sent += 1;
        Ok(())
    }

    /// Send an interrupt. Cancels the current turn. After the turn unwinds,
    /// the CLI processes the next queued command. Use this for urgent
    /// intervention: interrupt, then send a user message with the warning.
    pub async fn interrupt(&mut self) -> Result<(), std::io::Error> {
        let stdin = self.stdin.as_mut()
            .expect("interrupt() called after shutdown");

        let msg = InterruptRequest {
            r#type: "control_request",
            request: InterruptSubtype {
                subtype: "interrupt",
            },
        };

        let mut serialized = serde_json::to_string(&msg)
            .expect("InterruptRequest serialization cannot fail");
        serialized.push('\n');

        stdin.write_all(serialized.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Send a keep-alive heartbeat. The CLI accepts and ignores it. Use during
    /// long idle periods to keep the pipe active.
    pub async fn keep_alive(&mut self) -> Result<(), std::io::Error> {
        let stdin = self.stdin.as_mut()
            .expect("keep_alive() called after shutdown");

        let mut serialized = serde_json::to_string(&KeepAlive { r#type: "keep_alive" })
            .expect("KeepAlive serialization cannot fail");
        serialized.push('\n');

        stdin.write_all(serialized.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Close stdin and wait for the child to exit. This is the shutdown
    /// sequence. Closing stdin sends EOF to the CLI's NDJSON parser. The
    /// parser's for-await loop ends. Active turns complete before shutdown.
    /// The transcript is flushed to disk.
    ///
    /// Only call this after the result event has arrived and nothing is queued
    /// on Puzzle's side. Closing stdin while a turn is in progress kills the
    /// turn. Closing stdin while an approval is pending causes the permission
    /// to be denied (EOF rejects pending permission requests on the structured
    /// input stream).
    ///
    /// After this call, the invocation is over. The struct should be dropped.
    /// Create a new Invocation with --resume for the next round.
    pub async fn shutdown(mut self) -> Result<std::process::ExitStatus, std::io::Error> {
        // Drop stdin to close the write end of the pipe. The child sees EOF.
        self.stdin.take();

        // Wait for the child to exit. It will finish any active turn first,
        // run cleanup callbacks, flush the transcript writer, and exit.
        self.child.wait().await
    }

    /// Process an event from the stdout channel. The main loop calls this for
    /// every event it pulls from the receiver. This is the adapter — the main
    /// loop doesn't interpret events for counting purposes, it passes them
    /// through and the struct updates its own state.
    ///
    /// Session ID capture: every stdout event carries a session_id. On the
    /// first event that has one, the Invocation stores it. For session ID
    /// resumes this confirms what we already knew. For file path resumes
    /// (forks), this is how the caller discovers the new session identity.
    ///
    /// The drain gate: when a result event arrives, check if sent == replayed.
    /// If they match, every message has been consumed. Flip drained to true.
    /// No more sends are accepted. The main loop should call shutdown().
    ///
    /// If sent != replayed at result time, some messages are still in flight.
    /// They'll trigger another round. Wait for the next result.
    ///
    /// Returns true if the invocation just drained (the gate flipped on this
    /// event). The main loop can use this to trigger shutdown.
    pub fn handle_event(&mut self, event: &StdoutEvent) -> bool {
        // capture session_id from the first event that carries one.
        if self.session_id.is_none() {
            let event_session_id = match event {
                StdoutEvent::System { session_id, .. } => session_id.as_ref(),
                StdoutEvent::Result { session_id, .. } => session_id.as_ref(),
                StdoutEvent::Assistant { session_id, .. } => session_id.as_ref(),
                StdoutEvent::User { session_id, .. } => session_id.as_ref(),
                _ => None,
            };
            if let Some(id) = event_session_id {
                self.session_id = Some(id.clone());
            }
        }

        match event {
            StdoutEvent::User { is_replay: true, .. } => {
                self.replayed += 1;
            }
            StdoutEvent::Result { .. } => {
                if self.sent == self.replayed {
                    self.drained = true;
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    /// Whether the invocation has drained. Sent == replayed at a result
    /// boundary. No more sends accepted. Shutdown is safe.
    pub fn is_drained(&self) -> bool {
        self.drained
    }

    /// How many user messages have been written to stdin via send().
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// How many isReplay user messages have appeared on stdout.
    pub fn replayed(&self) -> u64 {
        self.replayed
    }

    /// The session ID for this invocation. Captured from the first stdout
    /// event that carries one. For session ID resumes, this is the same ID
    /// that was passed in. For file path resumes (forks), this is the new
    /// UUID the CLI generated. Always available after the first event.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}
