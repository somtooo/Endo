use anyhow::{Context as _, Result};
use rand::SeedableRng as _;
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// A new message ID in OpenCode's ascending format (`schema/src/identifier.ts`):
/// `msg_`, 12 hex characters of `timestamp * 0x1000 + counter`, then 14 random
/// base62 characters. The server orders messages by when it stored them, not
/// by ID, but other clients may sort by ID, so a client-chosen ID should sort
/// the same way the server's own IDs do.
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
    /// What the model produces. Only `chat` models can run sessions.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
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

impl ModelInfo {
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }

    pub fn has_variant(&self, id: &str) -> bool {
        self.variants.iter().any(|variant| variant.id == id)
    }
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

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: PermissionEffect,
}

impl PermissionRule {
    /// The rule for `action` on every resource.
    pub fn general(action: &str, effect: PermissionEffect) -> Self {
        Self {
            action: action.to_string(),
            resource: "*".to_string(),
            effect,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionEffect {
    Allow,
    Ask,
    #[serde(other)]
    Deny,
}

/// One of the configuration sources the service read, lowest priority first.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConfigEntry {
    Document {
        path: Option<String>,
        #[serde(default)]
        info: Value,
    },
    Directory {
        path: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Command {
    pub name: String,
    pub description: Option<String>,
}

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

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub id: String,
    pub variant: Option<String>,
}

impl ModelRef {
    /// `provider/model`: how OpenCode's config file names a model, and how
    /// Zed saves favorites and defaults.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }

    /// `provider/model#variant`: how OpenCode's config file names a model
    /// with its variant.
    pub fn to_config_reference(&self) -> String {
        match &self.variant {
            Some(variant) => format!("{}#{variant}", self.reference()),
            None => self.reference(),
        }
    }
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

impl TokenUsage {
    /// Tokens the step's request used, matching OpenCode's `TokenUsage.total`.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache.read + self.cache.write
    }
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

/// Messages from `message_id` onward stay on the server until the revert is
/// committed, but clients hide them.
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
        let body = Self::decode_body(&kind, value).unwrap_or_else(|error| {
            log::warn!("Could not decode OpenCode message {id} ({kind}): {error:#}");
            MessageBody::Other { kind }
        });
        Ok(Self { id, body })
    }

    fn decode_body(kind: &str, value: Value) -> Result<MessageBody> {
        Ok(match kind {
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
            _ => MessageBody::Other {
                kind: kind.to_string(),
            },
        })
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.as_assistant()
            .into_iter()
            .flat_map(|assistant| &assistant.content)
            .filter_map(|item| match item {
                AssistantContent::Tool(tool) => Some(tool),
                _ => None,
            })
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

#[derive(Clone, Debug, PartialEq)]
pub enum MessageBody {
    User(UserMessage),
    Synthetic(TextMessage),
    System(TextMessage),
    Assistant(AssistantMessage),
    Idle(IdleMessage),
    Shell(ShellMessage),
    Compaction(CompactionMessage),
    AgentSwitched {
        agent: String,
    },
    ModelSwitched {
        model: ModelRef,
    },
    /// Message kinds this client doesn't render yet. Kept so ordering and
    /// identity still match the server.
    Other {
        kind: String,
    },
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
    #[serde(rename = "firstToken")]
    pub first_token: Option<i64>,
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
    /// A part this client doesn't know. Kept, so one new kind of part
    /// doesn't stop the whole message from loading.
    #[serde(other)]
    Other,
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
    /// A status this client doesn't know, shown as not finished.
    #[serde(other)]
    Other,
}

impl ToolState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, ToolState::Completed { .. } | ToolState::Error { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct IdleMessage {
    pub outcome: IdleOutcome,
    pub error: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdleOutcome {
    Succeeded,
    Failed,
    Interrupted,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ShellMessage {
    #[serde(rename = "shellID")]
    pub shell_id: String,
    pub command: String,
    pub status: Option<ShellStatus>,
    pub exit: Option<i64>,
    pub output: Option<ShellOutput>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellStatus {
    Running,
    Exited,
    Timeout,
    Killed,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ShellOutput {
    pub output: String,
    /// Byte position after this page.
    pub cursor: u64,
    /// Bytes captured so far.
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CompactionMessage {
    pub status: CompactionStatus,
    #[serde(default)]
    pub summary: String,
    pub error: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionStatus {
    Running,
    Completed,
    Failed,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Durable {
    #[serde(rename = "aggregateID")]
    pub aggregate_id: String,
    pub seq: u64,
}

/// One frame from `GET /api/event`. Its payload is decoded separately, by
/// [`EventFrame::decode`], so a payload Zed can't read doesn't end the stream.
#[derive(Clone, Debug, Deserialize)]
pub struct EventFrame {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub created: Option<i64>,
    pub location: Option<Location>,
    #[serde(default)]
    pub data: Value,
    pub durable: Option<Durable>,
}

impl EventFrame {
    pub fn decode(self) -> Result<Event> {
        let session_id = self.session_id().map(str::to_string);
        let kind = self.kind.clone();
        let mut tagged = serde_json::Map::new();
        tagged.insert("type".into(), Value::String(self.kind));
        tagged.insert("data".into(), self.data);
        let payload = serde_json::from_value(Value::Object(tagged))
            .with_context(|| format!("decoding OpenCode event {kind} ({})", self.id))?;
        Ok(Event {
            id: self.id,
            created: self.created,
            location: self.location,
            durable: self.durable,
            session_id,
            payload,
        })
    }

    pub fn session_id(&self) -> Option<&str> {
        self.data
            .get("sessionID")
            .or_else(|| self.data.get("form")?.get("sessionID"))
            .and_then(Value::as_str)
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub id: String,
    pub created: Option<i64>,
    pub location: Option<Location>,
    pub durable: Option<Durable>,
    pub session_id: Option<String>,
    pub payload: EventPayload,
}

/// The events Zed reads. Each payload lists only the fields Zed uses, so the
/// server can add fields freely; a missing required field fails the decode.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type")]
pub enum EventPayload {
    #[serde(rename = "session.execution.started")]
    ExecutionStarted {},
    #[serde(rename = "session.execution.succeeded")]
    ExecutionSucceeded {},
    #[serde(rename = "session.execution.failed")]
    ExecutionFailed { data: ExecutionFailed },
    #[serde(rename = "session.execution.interrupted")]
    ExecutionInterrupted { data: ExecutionInterrupted },
    #[serde(rename = "session.inbox.enqueued")]
    InboxEnqueued { data: InboxEnqueued },
    #[serde(rename = "session.inbox.delivered")]
    InboxDelivered { data: InboxRef },
    #[serde(rename = "session.inbox.cancelled")]
    InboxCancelled { data: InboxRef },
    #[serde(rename = "session.synthetic")]
    Synthetic { data: TextMessage },
    #[serde(rename = "session.agent.selected")]
    AgentSelected { data: AgentSelected },
    #[serde(rename = "session.model.selected")]
    ModelSelected { data: ModelSelected },
    #[serde(rename = "session.shell.started")]
    ShellStarted { data: ShellStarted },
    #[serde(rename = "session.shell.ended")]
    ShellEnded { data: ShellEnded },
    #[serde(rename = "session.skill.activated")]
    SkillActivated {},
    #[serde(rename = "session.moved")]
    Moved { data: Moved },
    #[serde(rename = "session.renamed")]
    Renamed { data: Renamed },
    #[serde(rename = "session.archived")]
    Archived {},
    #[serde(rename = "session.unarchived")]
    Unarchived {},
    #[serde(rename = "session.deleted")]
    Deleted {},
    #[serde(rename = "session.step.started")]
    StepStarted { data: StepStarted },
    #[serde(rename = "session.step.first-token")]
    StepFirstToken { data: StepRef },
    #[serde(rename = "session.step.streamed")]
    StepStreamed { data: StepRef },
    #[serde(rename = "session.step.ended")]
    StepEnded { data: StepEnded },
    #[serde(rename = "session.step.failed")]
    StepFailed { data: StepEnded },
    #[serde(rename = "session.text.started")]
    TextStarted { data: BlockStarted },
    #[serde(rename = "session.text.delta")]
    TextDelta { data: BlockDelta },
    #[serde(rename = "session.text.ended")]
    TextEnded { data: BlockEnded },
    #[serde(rename = "session.reasoning.started")]
    ReasoningStarted { data: BlockStarted },
    #[serde(rename = "session.reasoning.delta")]
    ReasoningDelta { data: BlockDelta },
    #[serde(rename = "session.reasoning.ended")]
    ReasoningEnded { data: BlockEnded },
    #[serde(rename = "session.tool.input.started")]
    ToolInputStarted { data: ToolInputStarted },
    #[serde(rename = "session.tool.input.delta")]
    ToolInputDelta { data: ToolInputDelta },
    #[serde(rename = "session.tool.input.ended")]
    ToolInputEnded { data: ToolInputEnded },
    #[serde(rename = "session.tool.called")]
    ToolCalled { data: ToolCalled },
    #[serde(rename = "session.tool.progress")]
    ToolProgress { data: ToolProgress },
    #[serde(rename = "session.tool.success")]
    ToolSucceeded { data: ToolSucceeded },
    #[serde(rename = "session.tool.failed")]
    ToolFailed { data: ToolFailed },
    #[serde(rename = "session.message.content.updated")]
    MessageContentUpdated { data: MessageContentUpdated },
    #[serde(rename = "session.compaction.started")]
    CompactionStarted { data: CompactionStarted },
    #[serde(rename = "session.compaction.delta")]
    CompactionDelta { data: CompactionDelta },
    #[serde(rename = "session.compaction.ended")]
    CompactionEnded { data: CompactionEnded },
    #[serde(rename = "session.compaction.failed")]
    CompactionFailed { data: CompactionFailed },
    #[serde(rename = "session.revert.staged")]
    RevertStaged { data: RevertStaged },
    #[serde(rename = "session.revert.cleared")]
    RevertCleared {},
    #[serde(rename = "session.revert.committed")]
    RevertCommitted { data: RevertCommitted },
    #[serde(rename = "permission.asked")]
    PermissionAsked { data: PermissionRequest },
    #[serde(rename = "permission.replied")]
    PermissionReplied { data: PermissionReplied },
    #[serde(rename = "form.created")]
    FormCreated { data: FormCreated },
    #[serde(rename = "form.replied", alias = "form.cancelled")]
    FormClosed { data: FormClosed },
    #[serde(
        rename = "command.updated",
        alias = "config.updated",
        alias = "skill.updated",
        alias = "agent.updated",
        alias = "model.updated",
        alias = "provider.updated",
        alias = "integration.updated",
        alias = "credential.updated"
    )]
    ProjectSetupChanged {},
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExecutionFailed {
    pub error: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExecutionInterrupted {
    pub reason: InterruptReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InterruptReason {
    User,
    /// Also sent for a dismissed question, not only when the server stops.
    Shutdown,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InboxEnqueued {
    #[serde(rename = "inboxID")]
    pub inbox_id: String,
    pub item: InboxItem,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InboxRef {
    #[serde(rename = "inboxID")]
    pub inbox_id: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InboxEntry {
    pub id: String,
    #[serde(flatten)]
    pub item: InboxItem,
}

/// A prompt waiting in a session's inbox until the session takes it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InboxItem {
    User {
        payload: UserMessage,
    },
    Synthetic {
        payload: TextMessage,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentSelected {
    pub agent: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelSelected {
    pub model: ModelRef,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ShellStarted {
    pub shell: ShellInfo,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ShellEnded {
    pub shell: ShellInfo,
    pub output: ShellOutput,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ShellInfo {
    pub id: String,
    pub command: String,
    pub status: ShellStatus,
    pub exit: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Moved {
    pub location: Location,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Renamed {
    pub title: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StepStarted {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub agent: Option<String>,
    pub model: Option<ModelRef>,
    pub started: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StepRef {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StepEnded {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub finish: Option<String>,
    pub error: Option<Value>,
    pub cost: Option<f64>,
    pub tokens: Option<TokenUsage>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BlockStarted {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub ordinal: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BlockDelta {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub ordinal: usize,
    pub delta: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BlockEnded {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub ordinal: usize,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolInputStarted {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolInputDelta {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    pub delta: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolInputEnded {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolCalled {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    pub input: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolProgress {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    pub metadata: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolSucceeded {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    pub content: Vec<Value>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolFailed {
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: String,
    pub id: String,
    pub error: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MessageContentUpdated {
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub content: Vec<AssistantContent>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CompactionStarted {
    #[serde(rename = "inputID")]
    pub input_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CompactionDelta {
    pub text: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CompactionEnded {
    pub text: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CompactionFailed {
    pub error: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RevertStaged {
    pub revert: SessionRevert,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RevertCommitted {
    /// The first message deleted. Every message stored after it goes too.
    pub to: String,
}

/// A tool call waiting for the user to allow it. Requests that aren't tied to
/// a tool call have no `source`.
#[derive(Clone, Debug, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    pub action: String,
    #[serde(default)]
    pub resources: Vec<String>,
    pub source: Option<PermissionSource>,
}

impl PermissionRequest {
    pub fn tool_call_id(&self) -> Option<&str> {
        self.source.as_ref().map(|source| source.id.as_str())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct PermissionSource {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PermissionReplied {
    #[serde(rename = "requestID")]
    pub request_id: String,
    pub reply: PermissionReply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionReply {
    Once,
    Always,
    #[serde(other)]
    Reject,
}

impl PermissionReply {
    pub const ALL: [Self; 3] = [Self::Always, Self::Once, Self::Reject];

    pub fn as_str(self) -> &'static str {
        match self {
            PermissionReply::Once => "once",
            PermissionReply::Always => "always",
            PermissionReply::Reject => "reject",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct FormCreated {
    pub form: Form,
}

/// A question the agent asks with typed fields (`Form.Info` in OpenCode's API
/// reference).
#[derive(Clone, Debug, Deserialize)]
pub struct Form {
    pub id: String,
    #[serde(default)]
    pub title: String,
    /// Free-form, so it stays JSON. Zed reads its `kind` and `tool`.
    #[serde(default)]
    pub metadata: Option<Value>,
    pub fields: Vec<FormField>,
}

impl Form {
    pub fn kind(&self) -> Option<&str> {
        self.metadata.as_ref()?.get("kind")?.as_str()
    }

    /// The tool call that asked.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.metadata.as_ref()?.get("tool")?.get("id")?.as_str()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct FormClosed {
    pub id: String,
}

/// Messages the server derives from an event take the event's ID with the
/// `msg_` prefix (`SessionMessage.ID.fromEvent`), which lets a client build
/// the same message without refetching.
pub fn derived_message_id(event_id: &str) -> String {
    match event_id.strip_prefix("evt_") {
        Some(suffix) => format!("msg_{suffix}"),
        None => event_id.to_string(),
    }
}

/// A provider or other service OpenCode can connect to.
#[derive(Clone, Debug, Deserialize)]
pub struct Integration {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub methods: Vec<IntegrationMethod>,
    #[serde(default)]
    pub connections: Vec<IntegrationConnection>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum IntegrationMethod {
    #[serde(rename = "oauth")]
    OAuth {
        id: String,
        label: String,
        #[serde(default)]
        form: Vec<FormField>,
    },
    Command {
        id: String,
        label: String,
    },
    Key {
        label: Option<String>,
        #[serde(default)]
        form: Vec<FormField>,
    },
    External {
        id: String,
        label: String,
        #[serde(default)]
        form: Vec<FormField>,
    },
    /// Read from the service's environment, so there's nothing to connect.
    Env {
        #[serde(default)]
        names: Vec<String>,
    },
    #[serde(other)]
    Other,
}

impl IntegrationMethod {
    /// The details the method asks for before it starts.
    pub fn form(&self) -> &[FormField] {
        match self {
            Self::OAuth { form, .. } | Self::Key { form, .. } | Self::External { form, .. } => form,
            Self::Command { .. } | Self::Env { .. } | Self::Other => &[],
        }
    }

    /// `None` for an API key the provider didn't name, and for methods with
    /// nothing to connect.
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::OAuth { label, .. }
            | Self::Command { label, .. }
            | Self::External { label, .. } => Some(label),
            Self::Key { label, .. } => label.as_deref(),
            Self::Env { .. } | Self::Other => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum IntegrationConnection {
    Credential {
        id: String,
        label: String,
        method: CredentialMethod,
        status: Option<ConnectionStatus>,
    },
    Env {
        name: String,
        status: Option<ConnectionStatus>,
    },
    #[serde(other)]
    Other,
}

/// How a saved account was connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialMethod {
    #[serde(rename = "oauth")]
    OAuth,
    Key,
    External,
    #[serde(other)]
    Other,
}

/// A problem the integration reported with a connection.
#[derive(Clone, Debug, Deserialize)]
pub struct ConnectionStatus {
    pub message: String,
}

/// What a form field asks for: its `type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormFieldKind {
    String,
    Number,
    Integer,
    Boolean,
    Multiselect,
    /// A link to open, not an answer.
    External,
    #[serde(other)]
    Other,
}

/// One field of a form (`Form.Field` in OpenCode's API reference). The fields
/// of all types share one struct; `kind` says which apply.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormField {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: FormFieldKind,
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    /// Answered with the default instead of being asked.
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub when: Vec<FormCondition>,
    pub placeholder: Option<String>,
    pub default: Option<Value>,
    /// Make a string field a choice.
    #[serde(default)]
    pub options: Vec<FormOption>,
    /// A choice that also takes an answer of the user's own.
    #[serde(default)]
    pub custom: bool,
    pub format: Option<String>,
    pub min_length: Option<u32>,
    pub max_length: Option<u32>,
    pub pattern: Option<String>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub min_items: Option<u64>,
    pub max_items: Option<u64>,
    /// A link the user follows instead of answering.
    pub url: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FormCondition {
    pub key: String,
    pub op: String,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FormOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OAuthAttempt {
    #[serde(rename = "attemptID")]
    pub attempt_id: String,
    pub url: String,
    pub instructions: String,
    pub mode: OAuthMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthMode {
    /// The user pastes a code back.
    Code,
    #[serde(other)]
    Auto,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CommandAttempt {
    #[serde(rename = "attemptID")]
    pub attempt_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum AttemptStatus {
    Pending {
        message: Option<String>,
    },
    Complete {},
    Failed {
        message: Option<String>,
    },
    #[serde(other)]
    Expired,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginInfo {
    pub id: Option<String>,
    pub source: PluginSource,
    pub state: PluginState,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginSource {
    Builtin,
    Package {
        target: String,
        version: Option<String>,
    },
    Local {
        path: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PluginState {
    Failed {
        #[serde(default)]
        error: String,
    },
    #[serde(other)]
    Active,
}

/// The error OpenCode records on a step or tool call that didn't finish.
#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum StepError<'a> {
    #[serde(rename = "aborted")]
    Aborted {
        #[serde(borrow)]
        message: &'a str,
    },
    #[serde(other)]
    Other,
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
    fn unknown_permissions_are_restrictive() {
        let effect: PermissionEffect = serde_json::from_str("\"future\"").expect("effect");
        let reply: PermissionReply = serde_json::from_str("\"future\"").expect("reply");
        assert_eq!(effect, PermissionEffect::Deny);
        assert_eq!(reply, PermissionReply::Reject);
    }

    #[test]
    fn malformed_message_keeps_its_kind() {
        let message = Message::from_value(
            serde_json::json!({"id":"msg", "type":"model-switched", "model": 42}),
        )
        .expect("message");
        assert_eq!(
            message.body,
            MessageBody::Other {
                kind: "model-switched".into()
            }
        );
    }

    #[test]
    fn events_preserve_unknown_types() {
        let frame = |kind: &str, data: Value| EventFrame {
            id: "evt_1".into(),
            kind: kind.into(),
            created: None,
            location: None,
            data,
            durable: None,
        };
        let event = frame(
            "session.text.delta",
            serde_json::json!({"sessionID": "ses_1", "assistantMessageID": "msg_1", "ordinal": 2, "delta": "hi"}),
        )
        .decode()
        .expect("a known event decodes");
        assert_eq!(event.session_id.as_deref(), Some("ses_1"));
        assert!(matches!(
            event.payload,
            EventPayload::TextDelta { data } if data.ordinal == 2 && data.delta == "hi"
        ));

        let unknown = frame("session.future", serde_json::json!({"sessionID": "ses_1"}))
            .decode()
            .expect("an unknown event decodes");
        assert!(matches!(unknown.payload, EventPayload::Other));

        let malformed = frame("session.text.delta", serde_json::json!({"ordinal": 2}));
        assert!(malformed.decode().is_err());
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

        let unknown =
            Message::from_value(serde_json::json!({"id": "msg_2", "type": "future-kind"}))
                .expect("unknown kinds still decode");
        assert_eq!(
            unknown.body,
            MessageBody::Other {
                kind: "future-kind".into()
            }
        );
    }
}
