use anyhow::{Context as _, Result};
use rand::SeedableRng as _;
use rand::rngs::StdRng;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

/// A new message ID in OpenCode's ascending format (`schema/src/identifier.ts`):
/// `msg_`, 12 hex characters of `timestamp * 0x1000 + counter`, then 14 random
/// base62 characters. The server orders messages by ID, so a client-chosen ID
/// must sort the same way the server's own IDs do.
pub fn new_message_id() -> String {
    static LAST: std::sync::Mutex<(u64, u64)> = std::sync::Mutex::new((0, 0));
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    let counter = {
        let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.0 != timestamp {
            *last = (timestamp, 0);
        }
        last.1 += 1;
        last.1
    };
    format_message_id(timestamp, counter, &mut StdRng::from_os_rng())
}

fn format_message_id(timestamp: u64, counter: u64, rng: &mut impl rand::Rng) -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let value = timestamp.wrapping_mul(0x1000).wrapping_add(counter) & 0xffff_ffff_ffff;
    let mut id = format!("msg_{value:012x}");
    for _ in 0..14 {
        let byte: u8 = rng.random();
        id.push(CHARS[byte as usize % CHARS.len()] as char);
    }
    id
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub name: String,
    #[serde(default)]
    pub variants: Vec<ModelVariant>,
    #[serde(default)]
    pub limit: ModelLimit,
    /// Prices per million tokens, one entry per context-size tier.
    #[serde(default)]
    pub cost: Vec<ModelCost>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModelCost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModelLimit {
    pub context: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelVariant {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub mode: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    /// The agent's own model, used instead of the session's.
    pub model: Option<ModelRef>,
    /// The agent's prompt, when it replaces OpenCode's default.
    pub system: Option<String>,
    /// Every rule that applies to the agent, global ones included, in the
    /// order OpenCode checks them: the last match wins.
    #[serde(default)]
    pub permissions: Vec<PermissionRule>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: PermissionEffect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionEffect {
    Allow,
    Ask,
    Deny,
}

/// One of the configuration sources the service read, lowest priority first.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConfigEntry {
    Document { path: Option<String> },
    Directory { path: String },
    #[serde(other)]
    Other,
}

/// A command: invoked by name.
#[derive(Clone, Debug, Deserialize)]
pub struct NamedItem {
    pub name: String,
    pub description: Option<String>,
}

/// A skill: attached to a prompt by ID.
#[derive(Clone, Debug, Deserialize)]
pub struct Skill {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// Built-in skills have a path that doesn't exist on disk.
    pub path: PathBuf,
    #[serde(default)]
    pub content: String,
    /// Shown next to the name in Zed's menus, as for native skills:
    /// "built-in", "global", or the project folder's name.
    #[serde(skip)]
    pub scope: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub id: String,
    pub variant: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
    #[serde(default)]
    pub cache: CacheUsage,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct CacheUsage {
    #[serde(default)]
    pub read: u64,
    #[serde(default)]
    pub write: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Location {
    pub directory: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SessionTime {
    pub created: Option<i64>,
    pub updated: Option<i64>,
    pub busy: Option<i64>,
    pub idle: Option<i64>,
    pub archived: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<ModelRef>,
    pub location: Option<Location>,
    #[serde(default)]
    pub time: SessionTime,
    pub revert: Option<SessionRevert>,
}

/// A staged revert. Messages from `message_id` onward stay on the server
/// until the revert is committed, but clients hide them.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SessionRevert {
    #[serde(rename = "messageID")]
    pub message_id: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionPage {
    pub data: Vec<SessionInfo>,
    #[serde(default)]
    pub cursor: PageCursor,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PageCursor {
    pub previous: Option<String>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub id: String,
    pub body: MessageBody,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MessageBody {
    User(UserMessage),
    Synthetic(TextMessage),
    System(TextMessage),
    Assistant(AssistantMessage),
    Idle(IdleMessage),
    Shell(ShellMessage),
    Compaction(CompactionMessage),
    AgentSwitched { agent: String },
    ModelSwitched { model: ModelRef },
    /// Message kinds this client doesn't render yet. Kept so ordering and
    /// identity still match the server.
    Other { kind: String },
}

impl Message {
    pub fn from_value(value: Value) -> Result<Self> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .context("message without id")?
            .to_string();
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .context("message without type")?
            .to_string();
        let body = match kind.as_str() {
            "user" => MessageBody::User(serde_json::from_value(value)?),
            "synthetic" => MessageBody::Synthetic(serde_json::from_value(value)?),
            "system" => MessageBody::System(serde_json::from_value(value)?),
            "assistant" => MessageBody::Assistant(serde_json::from_value(value)?),
            "idle" => MessageBody::Idle(serde_json::from_value(value)?),
            "shell" => MessageBody::Shell(serde_json::from_value(value)?),
            "compaction" => MessageBody::Compaction(serde_json::from_value(value)?),
            "agent-switched" => MessageBody::AgentSwitched {
                agent: value
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            "model-switched" => MessageBody::ModelSwitched {
                model: serde_json::from_value(value.get("model").cloned().unwrap_or_default())?,
            },
            _ => MessageBody::Other { kind },
        };
        Ok(Self { id, body })
    }

    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match &self.body {
            MessageBody::Assistant(assistant) => Some(assistant),
            _ => None,
        }
    }

    pub fn as_assistant_mut(&mut self) -> Option<&mut AssistantMessage> {
        match &mut self.body {
            MessageBody::Assistant(assistant) => Some(assistant),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct UserMessage {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub files: Vec<Value>,
    #[serde(default)]
    pub skills: Vec<Value>,
    /// Zed stores how it showed the prompt here, so its mention chips can be
    /// rebuilt when the message is drawn again.
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TextMessage {
    #[serde(default)]
    pub text: String,
    pub description: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct MessageTime {
    pub created: Option<i64>,
    pub streamed: Option<i64>,
    pub completed: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct AssistantMessage {
    pub agent: Option<String>,
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub content: Vec<AssistantContent>,
    pub finish: Option<String>,
    pub error: Option<Value>,
    pub retry: Option<Value>,
    pub cost: Option<f64>,
    pub tokens: Option<TokenUsage>,
    #[serde(default)]
    pub time: MessageTime,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AssistantContent {
    Text {
        #[serde(default)]
        text: String,
    },
    Reasoning {
        #[serde(default)]
        text: String,
        #[serde(default)]
        time: ReasoningTime,
    },
    Tool(ToolCall),
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ReasoningTime {
    pub created: Option<i64>,
    pub completed: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub state: ToolState,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ToolState {
    Streaming {
        #[serde(default)]
        input: String,
    },
    Running {
        #[serde(default)]
        input: Value,
        #[serde(default)]
        metadata: Value,
    },
    Completed {
        #[serde(default)]
        input: Value,
        #[serde(default)]
        content: Vec<Value>,
        #[serde(default)]
        metadata: Value,
    },
    Error {
        #[serde(default)]
        input: Value,
        #[serde(default)]
        error: Value,
    },
}

impl TokenUsage {
    /// Tokens the step's request used, matching OpenCode's `TokenUsage.total`.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache.read + self.cache.write
    }
}

impl ToolState {
    pub fn status(&self) -> &'static str {
        match self {
            ToolState::Streaming { .. } => "streaming",
            ToolState::Running { .. } => "running",
            ToolState::Completed { .. } => "completed",
            ToolState::Error { .. } => "error",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, ToolState::Completed { .. } | ToolState::Error { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct IdleMessage {
    pub outcome: String,
    pub error: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ShellMessage {
    #[serde(rename = "shellID")]
    pub shell_id: String,
    pub command: String,
    pub status: Option<String>,
    pub output: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CompactionMessage {
    pub status: String,
    #[serde(default)]
    pub summary: String,
    pub error: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Durable {
    #[serde(rename = "aggregateID")]
    pub aggregate_id: String,
    pub seq: u64,
}

/// One frame from `GET /api/event`. `data` stays untyped here; the session
/// reducer decodes only the fields it needs for each event type, so unknown or
/// newer events never fail the stream.
#[derive(Clone, Debug, Deserialize)]
pub struct Event {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub created: Option<i64>,
    pub location: Option<Location>,
    #[serde(default)]
    pub data: Value,
    pub durable: Option<Durable>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_match_the_server_format() {
        // A real server ID: `msg_199745775001yeCxMXVQlo9rs1`.
        let id = format_message_id(1793575901045, 1, &mut StdRng::seed_from_u64(7));
        assert!(id.starts_with("msg_199745775001"), "{id}");
        assert_eq!(id.len(), "msg_199745775001yeCxMXVQlo9rs1".len());
        assert!(new_message_id() < new_message_id());
    }

    #[test]
    fn decodes_messages_from_the_server() {
        let message = Message::from_value(serde_json::json!({
            "id": "msg_1",
            "type": "assistant",
            "agent": "build",
            "content": [
                {"type": "reasoning", "text": "hm", "time": {"created": 1, "completed": 2}},
                {"type": "tool", "id": "t", "name": "shell", "executed": false,
                 "state": {"status": "running", "input": {"command": "ls"}, "metadata": {}}},
                {"type": "text", "text": "done"}
            ],
            "time": {"created": 1}
        }))
        .expect("valid message");
        let assistant = message.as_assistant().expect("assistant");
        assert_eq!(assistant.content.len(), 3);

        let unknown = Message::from_value(serde_json::json!({"id": "msg_2", "type": "future-kind"}))
            .expect("unknown kinds still decode");
        assert_eq!(
            unknown.body,
            MessageBody::Other {
                kind: "future-kind".into()
            }
        );
    }
}

impl Event {
    pub fn session_id(&self) -> Option<&str> {
        self.data
            .get("sessionID")
            .or_else(|| self.data.get("form")?.get("sessionID"))
            .and_then(Value::as_str)
    }

    /// Messages the server derives from an event take the event's ID with the
    /// `msg_` prefix (`SessionMessage.ID.fromEvent`), which lets a client build
    /// the same message without refetching.
    pub fn derived_message_id(&self) -> String {
        match self.id.strip_prefix("evt_") {
            Some(suffix) => format!("msg_{suffix}"),
            None => self.id.clone(),
        }
    }
}
