use crate::parser::{
    AssistantContentBlock, AssistantEntry, Entry, TextBlock, ThinkingBlock, ToolResultBlock,
    ToolResultContent, ToolUseBlock, UserContent, UserContentBlock, UserEntry,
};

#[derive(Debug)]
pub enum ContentBlock {
    Thinking { text: String },
    Text { text: String },
    ToolUse { name: String, input_summary: String },
    ToolResult { content: String, is_error: bool },
}

#[derive(Debug)]
pub enum EntryKind {
    User,
    Assistant,
}

#[derive(Debug)]
pub struct ConversationEntry {
    pub kind: EntryKind,
    pub blocks: Vec<ContentBlock>,
    pub timestamp: Option<String>,
    pub uuid: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Filter and convert a raw parsed Entry into a display-ready
/// ConversationEntry. Returns None for entries that should be
/// skipped (progress, system, sidechains, empty content).
pub fn try_convert(entry: Entry) -> Option<ConversationEntry> {
    match entry {
        Entry::User(user) if !user.is_sidechain => convert_user(user),
        Entry::Assistant(assistant) if !assistant.is_sidechain => convert_assistant(assistant),
        _ => None,
    }
}

fn convert_user(entry: UserEntry) -> Option<ConversationEntry> {
    let blocks = match entry.message.content {
        UserContent::Text(text) => {
            vec![ContentBlock::Text { text }]
        }
        UserContent::Blocks(content_blocks) => {
            let mut blocks = Vec::new();
            for block in content_blocks {
                match block {
                    UserContentBlock::ToolResult(ToolResultBlock {
                        content,
                        is_error,
                        ..
                    }) => {
                        let text = match content {
                            Some(ToolResultContent::Text(s)) => s,
                            Some(ToolResultContent::Blocks(_)) => "(structured content)".into(),
                            None => String::new(),
                        };
                        blocks.push(ContentBlock::ToolResult {
                            content: text,
                            is_error,
                        });
                    }
                    UserContentBlock::Text(TextBlock { text }) => {
                        blocks.push(ContentBlock::Text { text });
                    }
                    UserContentBlock::Unknown => {}
                }
            }
            blocks
        }
    };

    if blocks.is_empty() {
        return None;
    }

    Some(ConversationEntry {
        kind: EntryKind::User,
        blocks,
        timestamp: entry.timestamp,
        uuid: entry.uuid,
        input_tokens: None,
        output_tokens: None,
    })
}

fn convert_assistant(entry: AssistantEntry) -> Option<ConversationEntry> {
    let mut blocks = Vec::new();

    for block in entry.message.content {
        match block {
            AssistantContentBlock::Thinking(ThinkingBlock { thinking, .. }) => {
                blocks.push(ContentBlock::Thinking { text: thinking });
            }
            AssistantContentBlock::Text(TextBlock { text }) => {
                blocks.push(ContentBlock::Text { text });
            }
            AssistantContentBlock::ToolUse(ToolUseBlock { name, input, .. }) => {
                let summary = summarize_tool_input(&name, &input);
                blocks.push(ContentBlock::ToolUse {
                    name,
                    input_summary: summary,
                });
            }
            AssistantContentBlock::Unknown => {}
        }
    }

    if blocks.is_empty() {
        return None;
    }

    let (input_tokens, output_tokens) = entry
        .message
        .usage
        .map(|u| (u.input_tokens, u.output_tokens))
        .unwrap_or((None, None));

    Some(ConversationEntry {
        kind: EntryKind::Assistant,
        blocks,
        timestamp: entry.timestamp,
        uuid: entry.uuid,
        input_tokens,
        output_tokens,
    })
}

fn summarize_tool_input(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Read" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Write" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Edit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Task" => input
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| truncate(s, 80))
            .unwrap_or_default(),
        _ => {
            let keys: Vec<&str> = input
                .as_object()
                .map(|m| m.keys().map(|k| k.as_str()).collect())
                .unwrap_or_default();
            keys.join(", ")
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
