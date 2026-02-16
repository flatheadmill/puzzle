use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "kebab-case")]
pub enum Entry {
    User(UserEntry),
    Assistant(AssistantEntry),
    System(SystemEntry),
    Progress(ProgressEntry),
    Summary(SummaryEntry),
    FileHistorySnapshot(FileHistorySnapshotEntry),
    QueueOperation(QueueOperationEntry),
    CustomTitle(CustomTitleEntry),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserEntry {
    pub message: UserMessage,
    pub uuid: Option<String>,
    pub timestamp: Option<String>,
    pub parent_uuid: Option<String>,
    #[serde(default)]
    pub is_sidechain: bool,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UserMessage {
    pub role: String,
    pub content: UserContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<UserContentBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum UserContentBlock {
    ToolResult(ToolResultBlock),
    Text(TextBlock),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct ToolResultBlock {
    pub tool_use_id: Option<String>,
    pub content: Option<ToolResultContent>,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<Value>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantEntry {
    pub message: AssistantMessage,
    pub uuid: Option<String>,
    pub timestamp: Option<String>,
    pub parent_uuid: Option<String>,
    #[serde(default)]
    pub is_sidechain: bool,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssistantMessage {
    pub role: Option<String>,
    pub content: Vec<AssistantContentBlock>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum AssistantContentBlock {
    Thinking(ThinkingBlock),
    Text(TextBlock),
    ToolUse(ToolUseBlock),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct ThinkingBlock {
    pub thinking: String,
    pub signature: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TextBlock {
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct ToolUseBlock {
    pub id: Option<String>,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemEntry {
    pub subtype: Option<String>,
    #[serde(flatten)]
    pub extra: Value,
}

#[derive(Debug, Deserialize)]
pub struct ProgressEntry {
    #[serde(flatten)]
    pub extra: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryEntry {
    pub summary: Option<String>,
    pub leaf_uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FileHistorySnapshotEntry {
    #[serde(flatten)]
    pub extra: Value,
}

#[derive(Debug, Deserialize)]
pub struct QueueOperationEntry {
    #[serde(flatten)]
    pub extra: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomTitleEntry {
    pub custom_title: Option<String>,
    pub session_id: Option<String>,
}

pub fn parse_line(line: &str) -> Option<Entry> {
    match serde_json::from_str::<Entry>(line) {
        Ok(entry) => Some(entry),
        Err(e) => {
            eprintln!("parse error: {}", e);
            None
        }
    }
}
