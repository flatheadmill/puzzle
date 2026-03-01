// Official transcript. Maintains the deduplicated record of conversation
// entries across spurts within a window. Each Easement invocation replays
// the full transcript through envelopes. The transcript layer identifies
// new content using a boundary UUID — the UUID of the first assistant
// message from the stdout event stream — and appends only new entries.
//
// On the first spurt (fresh window, empty transcript), all entries are new
// and pass through directly. On subsequent spurts, entries before the
// boundary are history replay and are skipped. The boundary is the point
// where the current spurt's new content begins.
//
// The transcript persists to transcript.jsonl in the window directory.
// Each new entry is appended as it arrives so the file is always current.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::model::{try_convert, ConversationEntry};
use crate::parser::parse_line;

pub struct Transcript {
    entries: Vec<serde_json::Value>,
    boundary_uuid: Option<String>,
    boundary_found: bool,
    buffer: Vec<serde_json::Value>,
    path: PathBuf,
}

impl Transcript {
    pub fn new(path: PathBuf) -> Self {
        Self {
            entries: Vec::new(),
            boundary_uuid: None,
            boundary_found: false,
            buffer: Vec::new(),
            path,
        }
    }

    /// Call at the start of each spurt, before spawning Easement. If the
    /// transcript is empty (first spurt), boundary detection is skipped
    /// and all envelopes pass through as new. Otherwise, envelopes buffer
    /// until the boundary UUID is found in the stream.
    pub fn begin_spurt(&mut self) {
        self.boundary_uuid = None;
        self.boundary_found = self.entries.is_empty();
        self.buffer.clear();
        tracing::info!(
            entries = self.entries.len(),
            boundary_found = self.boundary_found,
            "begin spurt (first={})", self.entries.is_empty()
        );
    }

    /// Set the boundary UUID from the first assistant stdout event.
    /// Processes any buffered envelopes and returns new UI entries.
    pub fn set_boundary(&mut self, uuid: String) -> Vec<ConversationEntry> {
        tracing::info!(
            uuid = %uuid,
            buffered = self.buffer.len(),
            "boundary uuid set"
        );
        self.boundary_uuid = Some(uuid);
        self.process_buffer()
    }

    /// Handle a transcript envelope from Easement. Returns new UI entries
    /// if the envelope contains new content (past the boundary or first
    /// spurt).
    pub fn handle_envelope(&mut self, data: serde_json::Value) -> Vec<ConversationEntry> {
        let entry_type = data.get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        if self.boundary_found {
            tracing::debug!(entry_type = %entry_type, entries = self.entries.len(), "accepting transcript entry");
            return self.accept(data);
        }

        self.buffer.push(data);
        tracing::debug!(
            entry_type = %entry_type,
            buffered = self.buffer.len(),
            has_boundary = self.boundary_uuid.is_some(),
            "buffering transcript entry"
        );

        if self.boundary_uuid.is_some() {
            return self.process_buffer();
        }

        vec![]
    }

    /// Load entries from a previous window's transcript. Populates
    /// self.entries for dedup and persists each entry to the current
    /// window's transcript.jsonl so it is self-contained. Returns
    /// parsed ConversationEntry values for the UI.
    pub fn load(&mut self, source: &std::path::Path) -> Vec<ConversationEntry> {
        let content = match std::fs::read_to_string(source) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to read previous transcript: {}", e);
                return vec![];
            }
        };

        let mut ui_entries = vec![];
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(data) => {
                    ui_entries.extend(self.accept(data));
                }
                Err(e) => {
                    tracing::warn!("transcript line parse error: {}", e);
                }
            }
        }

        tracing::info!(
            loaded = self.entries.len(),
            ui_entries = ui_entries.len(),
            source = %source.display(),
            "transcript loaded from previous window"
        );
        ui_entries
    }

    /// Number of entries in the official transcript.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The raw entries for transcript transfer in the Easement payload.
    /// When migrating to a new machine for the first time, Puzzle sends
    /// these as the transcript array. Easement writes them to a temp file,
    /// Claude forks from it into a new session. Message UUIDs are stable
    /// across forks (cli.js:497615), so deduplication still works.
    pub fn entries(&self) -> &[serde_json::Value] {
        &self.entries
    }

    fn accept(&mut self, data: serde_json::Value) -> Vec<ConversationEntry> {
        self.persist(&data);
        self.entries.push(data.clone());

        let json = serde_json::to_string(&data).unwrap_or_default();
        if let Some(entry) = parse_line(&json) {
            if let Some(ce) = try_convert(entry) {
                return vec![ce];
            }
        }

        vec![]
    }

    fn process_buffer(&mut self) -> Vec<ConversationEntry> {
        let uuid = match &self.boundary_uuid {
            Some(u) => u.clone(),
            None => return vec![],
        };

        // Find the boundary entry in the buffer.
        let boundary_pos = self.buffer.iter().position(|data| {
            data.get("uuid").and_then(|v| v.as_str()) == Some(uuid.as_str())
        });

        let boundary_pos = match boundary_pos {
            Some(pos) => pos,
            None => {
                // Boundary not found yet. Leave the buffer intact — the
                // boundary entry hasn't arrived from the tailer yet.
                tracing::debug!(
                    buffered = self.buffer.len(),
                    boundary_uuid = %uuid,
                    "boundary not in buffer yet, waiting"
                );
                return vec![];
            }
        };

        self.boundary_found = true;

        // The new turn includes a user entry before the assistant boundary
        // (the prompt that triggered the response). Scan backward to find
        // it so the user message is included in the new content.
        let start = self.buffer[..boundary_pos]
            .iter()
            .rposition(|data| {
                data.get("type").and_then(|v| v.as_str()) == Some("user")
            })
            .unwrap_or(boundary_pos);

        let skipped = start;
        let new_content: Vec<_> = self.buffer.drain(start..).collect();
        let accepted_count = new_content.len();
        self.buffer.clear(); // discard history before the new content

        tracing::info!(
            skipped,
            accepted = accepted_count,
            total_buffered = skipped + accepted_count,
            "boundary found, processing new content"
        );

        let mut results = vec![];
        for data in new_content {
            results.extend(self.accept(data));
        }
        results
    }

    fn persist(&self, data: &serde_json::Value) {
        let mut line = match serde_json::to_string(data) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("transcript persist error: {}", e);
                return;
            }
        };
        line.push('\n');

        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(line.as_bytes()) {
                    tracing::warn!("transcript write error: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("transcript open error: {}", e);
            }
        }
    }
}
