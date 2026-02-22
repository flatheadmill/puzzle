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
    }

    /// Set the boundary UUID from the first assistant stdout event.
    /// Processes any buffered envelopes and returns new UI entries.
    pub fn set_boundary(&mut self, uuid: String) -> Vec<ConversationEntry> {
        self.boundary_uuid = Some(uuid);
        self.process_buffer()
    }

    /// Handle a transcript envelope from Easement. Returns new UI entries
    /// if the envelope contains new content (past the boundary or first
    /// spurt).
    pub fn handle_envelope(&mut self, data: serde_json::Value) -> Vec<ConversationEntry> {
        if self.boundary_found {
            return self.accept(data);
        }

        self.buffer.push(data);

        if self.boundary_uuid.is_some() {
            return self.process_buffer();
        }

        vec![]
    }

    /// Number of entries in the official transcript.
    pub fn len(&self) -> usize {
        self.entries.len()
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

        let mut results = vec![];
        let buffer = std::mem::take(&mut self.buffer);

        for data in buffer {
            if !self.boundary_found {
                let entry_uuid = data.get("uuid").and_then(|v| v.as_str());
                if entry_uuid == Some(uuid.as_str()) {
                    self.boundary_found = true;
                }
            }

            if self.boundary_found {
                results.extend(self.accept(data));
            }
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
