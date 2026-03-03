// Model layer. Receives normalized entries from Wicket's wire format and
// converts to display-ready types for the renderer. The filtering, parsing,
// and normalization all happen in Wicket now — Puzzle just deserializes
// and adds UI state (collapsed flag on tool results).

use serde::Deserialize;
use serde_json::Value;

// -- Wire types from Wicket --

#[derive(Debug, Deserialize)]
struct WireEntry {
    kind: String,
    blocks: Vec<WireBlock>,
    #[allow(dead_code)]
    uuid: Option<String>,
    #[allow(dead_code)]
    seq: Option<u64>,
    #[allow(dead_code)]
    timestamp: Option<String>,
    #[allow(dead_code)]
    input_tokens: Option<u64>,
    #[allow(dead_code)]
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum WireBlock {
    Thinking { text: String },
    Text { text: String },
    ToolUse {
        name: String,
        input_summary: String,
        #[allow(dead_code)]
        input: Option<Value>,
    },
    ToolResult {
        content: String,
        is_error: bool,
    },
}

// -- Display types for the renderer --

#[derive(Debug)]
pub enum ContentBlock {
    Thinking { text: String },
    Text { text: String },
    ToolUse { name: String, input_summary: String },
    ToolResult { content: String, is_error: bool, collapsed: bool },
}

#[derive(Debug)]
pub enum EntryKind {
    User,
    Assistant,
}

#[allow(dead_code)]
pub struct ConversationEntry {
    pub kind: EntryKind,
    pub blocks: Vec<ContentBlock>,
    pub uuid: Option<String>,
    pub seq: Option<u64>,
    pub timestamp: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl ConversationEntry {
    /// Deserialize from Wicket's wire format (serde_json::Value).
    pub fn from_value(value: serde_json::Value) -> Option<Self> {
        let wire: WireEntry = serde_json::from_value(value).ok()?;

        let kind = match wire.kind.as_str() {
            "user" => EntryKind::User,
            "assistant" => EntryKind::Assistant,
            _ => return None,
        };

        let blocks: Vec<ContentBlock> = wire
            .blocks
            .into_iter()
            .map(|b| match b {
                WireBlock::Thinking { text } => ContentBlock::Thinking { text },
                WireBlock::Text { text } => ContentBlock::Text { text },
                WireBlock::ToolUse {
                    name,
                    input_summary,
                    ..
                } => ContentBlock::ToolUse {
                    name,
                    input_summary,
                },
                WireBlock::ToolResult { content, is_error } => ContentBlock::ToolResult {
                    content,
                    is_error,
                    collapsed: true,
                },
            })
            .collect();

        if blocks.is_empty() {
            return None;
        }

        Some(ConversationEntry {
            kind,
            blocks,
            uuid: wire.uuid,
            seq: wire.seq,
            timestamp: wire.timestamp,
            input_tokens: wire.input_tokens,
            output_tokens: wire.output_tokens,
        })
    }
}
