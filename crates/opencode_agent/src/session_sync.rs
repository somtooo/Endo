//! A client-side copy of one OpenCode session, built from a message snapshot
//! plus the live event stream.
//!
//! Mirrors the server's reducer (`core/src/session/message-updater.ts`) with
//! one difference: every event is idempotent here. Text and reasoning blocks
//! are addressed by their per-message `ordinal` and tools by their ID instead
//! of "the latest one", so events that arrive while a snapshot loads can be
//! applied on top of that snapshot without duplicating content.

use crate::types::{
    AssistantContent, AssistantMessage, CompactionMessage, Event, IdleMessage, Message,
    MessageBody, MessageTime, ModelRef, ReasoningTime, SessionRevert, TextMessage, TokenUsage,
    ToolCall, ToolState, UserMessage,
};
use collections::{HashMap, HashSet};
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Running,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResyncReason {
    /// A durable event was skipped, so the copy may be missing state.
    SequenceGap { expected: u64, received: u64 },
    /// The server deleted messages; only a fresh snapshot shows which.
    HistoryRewritten,
    /// The event refers to state from before this copy was built.
    MissingState,
}

#[derive(Debug, PartialEq)]
pub enum Applied {
    Ignored,
    Changed(Vec<Change>),
    NeedsResync(ResyncReason),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Message(String),
    Activity(Activity),
    Title(String),
    /// The revert point moved. Messages from this ID onward are hidden.
    Revert(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BlockKind {
    Text,
    Reasoning,
}

/// What the server returns for a session at one moment.
pub struct Snapshot {
    pub messages: Vec<Message>,
    pub running: bool,
    pub revert: Option<SessionRevert>,
}

/// Result of replacing the copy with a fresh snapshot: what a view must redraw.
#[derive(Debug, Default, PartialEq)]
pub struct SnapshotDiff {
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

pub struct SessionTranscript {
    session_id: String,
    messages: Vec<Message>,
    index: HashMap<String, usize>,
    /// Text and reasoning blocks still streaming. Only these accept deltas;
    /// the server stores block text only when the block ends.
    open_blocks: HashSet<(String, BlockKind, usize)>,
    pending_inbox: HashMap<String, Value>,
    activity: Activity,
    revert: Option<String>,
    last_seq: Option<u64>,
}

impl SessionTranscript {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            messages: Vec::new(),
            index: HashMap::default(),
            open_blocks: HashSet::default(),
            pending_inbox: HashMap::default(),
            activity: Activity::Idle,
            revert: None,
            last_seq: None,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Every message the server still stores, including ones hidden by a
    /// staged revert.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The messages a client shows: everything before the revert point.
    pub fn visible_messages(&self) -> &[Message] {
        // Messages are in the server's order, which IDs only usually follow,
        // since a client picks the IDs of the prompts it sends.
        let end = match &self.revert {
            Some(message_id) => match self.index.get(message_id) {
                Some(position) => *position,
                None => self
                    .messages
                    .partition_point(|message| message.id < *message_id),
            },
            None => self.messages.len(),
        };
        &self.messages[..end]
    }

    pub fn contains(&self, message_id: &str) -> bool {
        self.index.contains_key(message_id)
    }

    pub fn revert(&self) -> Option<&str> {
        self.revert.as_deref()
    }

    pub fn activity(&self) -> Activity {
        self.activity
    }

    /// Replaces the copy with a fresh snapshot. Sequence tracking restarts,
    /// because the snapshot carries no sequence number of its own.
    pub fn replace_snapshot(&mut self, snapshot: Snapshot) -> SnapshotDiff {
        let Snapshot {
            messages,
            running,
            revert,
        } = snapshot;
        let previous: HashMap<String, Message> = self
            .messages
            .drain(..)
            .map(|message| (message.id.clone(), message))
            .collect();
        self.index.clear();
        self.open_blocks.clear();
        self.last_seq = None;
        self.revert = revert.map(|revert| revert.message_id);
        self.activity = if running {
            Activity::Running
        } else {
            Activity::Idle
        };

        let mut diff = SnapshotDiff::default();
        for message in messages {
            if previous.get(&message.id) != Some(&message) {
                diff.changed.push(message.id.clone());
            }
            self.track_open_blocks(&message);
            self.insert(message);
        }
        diff.removed = previous
            .into_keys()
            .filter(|id| !self.index.contains_key(id))
            .collect();
        diff.removed.sort();
        diff
    }

    pub fn apply(&mut self, event: &Event) -> Applied {
        if event.session_id() != Some(self.session_id.as_str()) {
            return Applied::Ignored;
        }
        if let Some(durable) = &event.durable {
            if let Some(last) = self.last_seq {
                if durable.seq <= last {
                    return Applied::Ignored;
                }
                if durable.seq > last + 1 {
                    return Applied::NeedsResync(ResyncReason::SequenceGap {
                        expected: last + 1,
                        received: durable.seq,
                    });
                }
            }
            self.last_seq = Some(durable.seq);
        }
        match self.reduce(event) {
            Some(Ok(changes)) if changes.is_empty() => Applied::Ignored,
            Some(Ok(changes)) => Applied::Changed(changes),
            Some(Err(reason)) => Applied::NeedsResync(reason),
            None => Applied::Ignored,
        }
    }

    fn reduce(&mut self, event: &Event) -> Option<Result<Vec<Change>, ResyncReason>> {
        let created = event.created;
        let data = &event.data;
        let result = match event.kind.as_str() {
            "session.execution.started" => Ok(self.set_activity(Activity::Running)),
            "session.execution.succeeded" => Ok(self.finish_execution(event, "succeeded")),
            "session.execution.failed" => Ok(self.finish_execution(event, "failed")),
            // Every reason ends the turn here, "shutdown" included: the
            // server also uses it for a dismissed question. A real shutdown
            // drops the event stream, and the resync on reconnect restores a
            // turn the restarted server continues.
            "session.execution.interrupted" => Ok(self.finish_execution(event, "interrupted")),
            "session.inbox.enqueued" => {
                if let (Some(inbox_id), Some(item)) =
                    (field::<String>(data, "inboxID"), data.get("item"))
                {
                    self.pending_inbox.insert(inbox_id, item.clone());
                }
                Ok(Vec::new())
            }
            "session.inbox.cancelled" => {
                if let Some(inbox_id) = field::<String>(data, "inboxID") {
                    self.pending_inbox.remove(&inbox_id);
                }
                Ok(Vec::new())
            }
            "session.inbox.delivered" => self.deliver_inbox(data),
            "session.synthetic" => Ok(self.insert_derived(
                event,
                MessageBody::Synthetic(TextMessage {
                    text: field(data, "text").unwrap_or_default(),
                    description: field(data, "description"),
                    metadata: data.get("metadata").cloned(),
                }),
            )),
            "session.agent.selected" => Ok(self.insert_derived(
                event,
                MessageBody::AgentSwitched {
                    agent: field(data, "agent").unwrap_or_default(),
                },
            )),
            "session.model.selected" => match field::<ModelRef>(data, "model") {
                Some(model) => Ok(self.insert_derived(event, MessageBody::ModelSwitched { model })),
                None => Ok(Vec::new()),
            },
            "session.renamed" => Ok(field::<String>(data, "title")
                .map(Change::Title)
                .into_iter()
                .collect()),
            "session.step.started" => Ok(self.start_step(data, created)),
            "session.step.streamed" => Ok(self.update_assistant(data, |assistant| {
                assistant.time.streamed = created;
            })),
            "session.step.ended" | "session.step.failed" => {
                let failed = event.kind == "session.step.failed";
                Ok(self.update_assistant(data, |assistant| {
                    assistant.time.completed = created;
                    assistant.finish = field(data, "finish")
                        .or_else(|| failed.then(|| "error".to_string()));
                    assistant.retry = None;
                    if failed {
                        assistant.error = data.get("error").cloned();
                    }
                    if let Some(cost) = field(data, "cost") {
                        assistant.cost = Some(cost);
                    }
                    if let Some(tokens) = field::<TokenUsage>(data, "tokens") {
                        assistant.tokens = Some(tokens);
                    }
                }))
            }
            "session.retry.scheduled" => Ok(self.update_assistant(data, |assistant| {
                assistant.retry = Some(data.clone());
            })),
            "session.text.started" => Ok(self.start_block(data, BlockKind::Text, created)),
            "session.text.delta" => Ok(self.append_block(data, BlockKind::Text)),
            "session.text.ended" => Ok(self.end_block(data, BlockKind::Text, created)),
            "session.reasoning.started" => {
                Ok(self.start_block(data, BlockKind::Reasoning, created))
            }
            "session.reasoning.delta" => Ok(self.append_block(data, BlockKind::Reasoning)),
            "session.reasoning.ended" => Ok(self.end_block(data, BlockKind::Reasoning, created)),
            "session.tool.input.started" => Ok(self.start_tool(data)),
            "session.tool.input.delta" => Ok(self.update_tool(data, |state| {
                if let ToolState::Streaming { input } = state {
                    input.push_str(&field::<String>(data, "delta").unwrap_or_default());
                    return true;
                }
                false
            })),
            "session.tool.input.ended" => Ok(self.update_tool(data, |state| {
                if let ToolState::Streaming { input } = state {
                    *input = field(data, "text").unwrap_or_default();
                    return true;
                }
                false
            })),
            "session.tool.called" => Ok(self.update_tool(data, |state| {
                if state.is_terminal() {
                    return false;
                }
                *state = ToolState::Running {
                    input: data.get("input").cloned().unwrap_or_default(),
                    metadata: Value::Object(Default::default()),
                };
                true
            })),
            "session.tool.progress" => Ok(self.update_tool(data, |state| {
                if let ToolState::Running { metadata, .. } = state {
                    *metadata = data.get("metadata").cloned().unwrap_or_default();
                    return true;
                }
                false
            })),
            "session.tool.success" => Ok(self.update_tool(data, |state| {
                let ToolState::Running { input, .. } = state else {
                    return false;
                };
                *state = ToolState::Completed {
                    input: input.take(),
                    content: field(data, "content").unwrap_or_default(),
                    metadata: data.get("metadata").cloned().unwrap_or_default(),
                };
                true
            })),
            "session.tool.failed" => Ok(self.update_tool(data, |state| {
                let input = match state {
                    ToolState::Running { input, .. } => input.take(),
                    ToolState::Streaming { .. } => Value::Object(Default::default()),
                    ToolState::Completed { .. } | ToolState::Error { .. } => return false,
                };
                *state = ToolState::Error {
                    input,
                    error: data.get("error").cloned().unwrap_or_default(),
                };
                true
            })),
            "session.message.content.updated" => {
                let content = data
                    .get("content")
                    .cloned()
                    .and_then(|content| serde_json::from_value(content).ok());
                match (field::<String>(data, "messageID"), content) {
                    (Some(message_id), Some(content)) => {
                        let changed = self
                            .assistant_mut(&message_id)
                            .map(|assistant| assistant.content = content)
                            .is_some();
                        Ok(changed
                            .then(|| Change::Message(message_id))
                            .into_iter()
                            .collect())
                    }
                    _ => Err(ResyncReason::MissingState),
                }
            }
            "session.compaction.started" => {
                let id = field::<String>(data, "inputID")
                    .unwrap_or_else(|| event.derived_message_id());
                Ok(self.insert_if_missing(Message {
                    id,
                    body: MessageBody::Compaction(CompactionMessage {
                        status: "running".to_string(),
                        summary: String::new(),
                        error: None,
                    }),
                }))
            }
            "session.compaction.delta" => Ok(self.update_running_compaction(|compaction| {
                compaction
                    .summary
                    .push_str(&field::<String>(data, "delta").unwrap_or_default());
            })),
            "session.compaction.ended" | "session.compaction.failed" => {
                let status = if event.kind == "session.compaction.ended" {
                    "completed"
                } else {
                    "failed"
                };
                let summary: Option<String> = field(data, "text");
                let error = data.get("error").cloned();
                let changes = self.update_running_compaction(|compaction| {
                    compaction.status = status.to_string();
                    if let Some(summary) = &summary {
                        compaction.summary = summary.clone();
                    }
                    compaction.error = error.clone();
                });
                if changes.is_empty() {
                    Ok(self.insert_derived(
                        event,
                        MessageBody::Compaction(CompactionMessage {
                            status: status.to_string(),
                            summary: summary.unwrap_or_default(),
                            error,
                        }),
                    ))
                } else {
                    Ok(changes)
                }
            }
            "session.revert.staged" => {
                let revert = data
                    .get("revert")
                    .and_then(|revert| field::<String>(revert, "messageID"));
                Ok(self.set_revert(revert))
            }
            "session.revert.cleared" => Ok(self.set_revert(None)),
            // Committing deletes the hidden messages on the server.
            "session.revert.committed" => Err(ResyncReason::HistoryRewritten),
            _ => return None,
        };
        Some(result)
    }

    fn set_revert(&mut self, revert: Option<String>) -> Vec<Change> {
        if self.revert == revert {
            return Vec::new();
        }
        self.revert = revert.clone();
        vec![Change::Revert(revert)]
    }

    fn set_activity(&mut self, activity: Activity) -> Vec<Change> {
        if self.activity == activity {
            return Vec::new();
        }
        self.activity = activity;
        vec![Change::Activity(activity)]
    }

    fn finish_execution(&mut self, event: &Event, outcome: &str) -> Vec<Change> {
        let mut changes = self.clear_current_retry();
        changes.extend(self.insert_derived(
            event,
            MessageBody::Idle(IdleMessage {
                outcome: outcome.to_string(),
                error: event.data.get("error").cloned(),
            }),
        ));
        changes.extend(self.set_activity(Activity::Idle));
        changes
    }

    fn clear_current_retry(&mut self) -> Vec<Change> {
        let Some(message) = self.current_assistant_mut() else {
            return Vec::new();
        };
        let id = message.id.clone();
        match message.as_assistant_mut() {
            Some(assistant) if assistant.retry.is_some() => {
                assistant.retry = None;
                vec![Change::Message(id)]
            }
            _ => Vec::new(),
        }
    }

    fn deliver_inbox(&mut self, data: &Value) -> Result<Vec<Change>, ResyncReason> {
        let Some(inbox_id) = field::<String>(data, "inboxID") else {
            return Ok(Vec::new());
        };
        let item = self.pending_inbox.remove(&inbox_id);
        if self.index.contains_key(&inbox_id) {
            return Ok(Vec::new());
        }
        let Some(item) = item else {
            // Enqueued before this copy subscribed; only a snapshot has it.
            return Err(ResyncReason::MissingState);
        };
        let payload = item.get("payload").cloned().unwrap_or_default();
        let body = match item.get("type").and_then(Value::as_str) {
            Some("user") => MessageBody::User(UserMessage {
                text: field(&payload, "text").unwrap_or_default(),
                files: field(&payload, "files").unwrap_or_default(),
                skills: field(&payload, "skills").unwrap_or_default(),
                metadata: payload.get("metadata").cloned(),
            }),
            Some("synthetic") => MessageBody::Synthetic(TextMessage {
                text: field(&payload, "text").unwrap_or_default(),
                description: field(&payload, "description"),
                metadata: payload.get("metadata").cloned(),
            }),
            _ => return Ok(Vec::new()),
        };
        Ok(self.insert_if_missing(Message { id: inbox_id, body }))
    }

    fn start_step(&mut self, data: &Value, created: Option<i64>) -> Vec<Change> {
        let Some(message_id) = field::<String>(data, "assistantMessageID") else {
            return Vec::new();
        };
        let started = field::<i64>(data, "started").or(created);
        if let Some(assistant) = self.assistant_mut(&message_id) {
            assistant.agent = field(data, "agent");
            assistant.model = field(data, "model");
            assistant.retry = None;
            assistant.error = None;
            assistant.finish = None;
            assistant.time = MessageTime {
                created: started,
                streamed: None,
                completed: None,
            };
            return vec![Change::Message(message_id)];
        }
        let mut changes = Vec::new();
        if let Some(current) = self.current_assistant_mut() {
            let id = current.id.clone();
            if let Some(assistant) = current.as_assistant_mut() {
                assistant.retry = None;
                assistant.time.completed = created;
                changes.push(Change::Message(id));
            }
        }
        changes.extend(self.insert_if_missing(Message {
            id: message_id,
            body: MessageBody::Assistant(AssistantMessage {
                agent: field(data, "agent"),
                model: field(data, "model"),
                content: Vec::new(),
                finish: None,
                error: None,
                retry: None,
                cost: None,
                tokens: None,
                time: MessageTime {
                    created: started,
                    streamed: None,
                    completed: None,
                },
            }),
        }));
        changes
    }

    fn start_block(&mut self, data: &Value, kind: BlockKind, created: Option<i64>) -> Vec<Change> {
        let (Some(message_id), Some(ordinal)) = (
            field::<String>(data, "assistantMessageID"),
            field::<usize>(data, "ordinal"),
        ) else {
            return Vec::new();
        };
        let Some(assistant) = self.assistant_mut(&message_id) else {
            return Vec::new();
        };
        if block_mut(assistant, kind, ordinal).is_some() {
            return Vec::new();
        }
        while block_count(assistant, kind) <= ordinal {
            assistant.content.push(empty_block(kind, created));
        }
        self.open_blocks.insert((message_id.clone(), kind, ordinal));
        vec![Change::Message(message_id)]
    }

    fn append_block(&mut self, data: &Value, kind: BlockKind) -> Vec<Change> {
        let (Some(message_id), Some(ordinal), Some(delta)) = (
            field::<String>(data, "assistantMessageID"),
            field::<usize>(data, "ordinal"),
            field::<String>(data, "delta"),
        ) else {
            return Vec::new();
        };
        if !self
            .open_blocks
            .contains(&(message_id.clone(), kind, ordinal))
        {
            return Vec::new();
        }
        let Some(text) = self
            .assistant_mut(&message_id)
            .and_then(|assistant| block_mut(assistant, kind, ordinal))
        else {
            return Vec::new();
        };
        text.push_str(&delta);
        vec![Change::Message(message_id)]
    }

    fn end_block(&mut self, data: &Value, kind: BlockKind, created: Option<i64>) -> Vec<Change> {
        let (Some(message_id), Some(ordinal)) = (
            field::<String>(data, "assistantMessageID"),
            field::<usize>(data, "ordinal"),
        ) else {
            return Vec::new();
        };
        let full_text: String = field(data, "text").unwrap_or_default();
        let Some(assistant) = self.assistant_mut(&message_id) else {
            return Vec::new();
        };
        while block_count(assistant, kind) <= ordinal {
            assistant.content.push(empty_block(kind, created));
        }
        let mut changed = false;
        if let Some(text) = block_mut(assistant, kind, ordinal)
            && *text != full_text
        {
            *text = full_text;
            changed = true;
        }
        if kind == BlockKind::Reasoning
            && let Some(time) = reasoning_time_mut(assistant, ordinal)
            && time.completed.is_none()
        {
            time.completed = created;
            changed = true;
        }
        self.open_blocks.remove(&(message_id.clone(), kind, ordinal));
        if changed {
            vec![Change::Message(message_id)]
        } else {
            Vec::new()
        }
    }

    fn start_tool(&mut self, data: &Value) -> Vec<Change> {
        let (Some(message_id), Some(tool_id)) = (
            field::<String>(data, "assistantMessageID"),
            field::<String>(data, "id"),
        ) else {
            return Vec::new();
        };
        let Some(assistant) = self.assistant_mut(&message_id) else {
            return Vec::new();
        };
        if tool_mut(assistant, &tool_id).is_some() {
            return Vec::new();
        }
        assistant.content.push(AssistantContent::Tool(ToolCall {
            id: tool_id,
            name: field(data, "name").unwrap_or_default(),
            state: ToolState::Streaming {
                input: String::new(),
            },
        }));
        vec![Change::Message(message_id)]
    }

    fn update_tool(
        &mut self,
        data: &Value,
        update: impl FnOnce(&mut ToolState) -> bool,
    ) -> Vec<Change> {
        let (Some(message_id), Some(tool_id)) = (
            field::<String>(data, "assistantMessageID"),
            field::<String>(data, "id"),
        ) else {
            return Vec::new();
        };
        let Some(tool) = self
            .assistant_mut(&message_id)
            .and_then(|assistant| tool_mut(assistant, &tool_id))
        else {
            return Vec::new();
        };
        if update(&mut tool.state) {
            vec![Change::Message(message_id)]
        } else {
            Vec::new()
        }
    }

    fn update_assistant(
        &mut self,
        data: &Value,
        update: impl FnOnce(&mut AssistantMessage),
    ) -> Vec<Change> {
        let Some(message_id) = field::<String>(data, "assistantMessageID") else {
            return Vec::new();
        };
        let Some(assistant) = self.assistant_mut(&message_id) else {
            return Vec::new();
        };
        let before = assistant.clone();
        update(assistant);
        if *assistant == before {
            Vec::new()
        } else {
            vec![Change::Message(message_id)]
        }
    }

    fn update_running_compaction(
        &mut self,
        update: impl FnOnce(&mut CompactionMessage),
    ) -> Vec<Change> {
        let running = self.messages.iter_mut().rev().find_map(|message| {
            let id = message.id.clone();
            match &mut message.body {
                MessageBody::Compaction(compaction) if compaction.status == "running" => {
                    Some((id, compaction))
                }
                _ => None,
            }
        });
        match running {
            Some((id, compaction)) => {
                update(compaction);
                vec![Change::Message(id)]
            }
            None => Vec::new(),
        }
    }

    fn insert_derived(&mut self, event: &Event, body: MessageBody) -> Vec<Change> {
        self.insert_if_missing(Message {
            id: event.derived_message_id(),
            body,
        })
    }

    fn insert_if_missing(&mut self, message: Message) -> Vec<Change> {
        if self.index.contains_key(&message.id) {
            return Vec::new();
        }
        let id = message.id.clone();
        self.insert(message);
        vec![Change::Message(id)]
    }

    /// New messages go at the end. The server orders messages by when they
    /// were stored, not by ID: a subagent result keeps the ID it got when
    /// the subagent started but lands when it finishes. Snapshots come in
    /// stored order and events arrive in it, as in OpenCode's own client.
    fn insert(&mut self, message: Message) {
        if let Some(&ix) = self.index.get(&message.id) {
            if let Some(existing) = self.messages.get_mut(ix) {
                *existing = message;
            }
            return;
        }
        self.index.insert(message.id.clone(), self.messages.len());
        self.messages.push(message);
    }

    fn track_open_blocks(&mut self, message: &Message) {
        let Some(assistant) = message.as_assistant() else {
            return;
        };
        if assistant.time.completed.is_some() {
            return;
        }
        let mut text_ordinal = 0;
        let mut reasoning_ordinal = 0;
        for item in &assistant.content {
            match item {
                AssistantContent::Text { text } => {
                    if text.is_empty() {
                        self.open_blocks
                            .insert((message.id.clone(), BlockKind::Text, text_ordinal));
                    }
                    text_ordinal += 1;
                }
                AssistantContent::Reasoning { time, .. } => {
                    if time.completed.is_none() {
                        self.open_blocks.insert((
                            message.id.clone(),
                            BlockKind::Reasoning,
                            reasoning_ordinal,
                        ));
                    }
                    reasoning_ordinal += 1;
                }
                AssistantContent::Tool(_) => {}
            }
        }
    }

    fn assistant_mut(&mut self, message_id: &str) -> Option<&mut AssistantMessage> {
        let ix = *self.index.get(message_id)?;
        self.messages.get_mut(ix)?.as_assistant_mut()
    }

    fn current_assistant_mut(&mut self) -> Option<&mut Message> {
        self.messages.iter_mut().rev().find(|message| {
            message
                .as_assistant()
                .is_some_and(|assistant| assistant.time.completed.is_none())
        })
    }
}

fn field<T: DeserializeOwned>(data: &Value, key: &str) -> Option<T> {
    data.get(key)
        .cloned()
        .and_then(|value| T::deserialize(value).ok())
}

fn empty_block(kind: BlockKind, created: Option<i64>) -> AssistantContent {
    match kind {
        BlockKind::Text => AssistantContent::Text {
            text: String::new(),
        },
        BlockKind::Reasoning => AssistantContent::Reasoning {
            text: String::new(),
            time: ReasoningTime {
                created,
                completed: None,
            },
        },
    }
}

fn block_count(assistant: &AssistantMessage, kind: BlockKind) -> usize {
    assistant
        .content
        .iter()
        .filter(|item| is_block(item, kind))
        .count()
}

fn is_block(item: &AssistantContent, kind: BlockKind) -> bool {
    matches!(
        (item, kind),
        (AssistantContent::Text { .. }, BlockKind::Text)
            | (AssistantContent::Reasoning { .. }, BlockKind::Reasoning)
    )
}

fn block_mut(
    assistant: &mut AssistantMessage,
    kind: BlockKind,
    ordinal: usize,
) -> Option<&mut String> {
    assistant
        .content
        .iter_mut()
        .filter(|item| is_block(item, kind))
        .nth(ordinal)
        .and_then(|item| match item {
            AssistantContent::Text { text } | AssistantContent::Reasoning { text, .. } => {
                Some(text)
            }
            AssistantContent::Tool(_) => None,
        })
}

fn reasoning_time_mut(assistant: &mut AssistantMessage, ordinal: usize) -> Option<&mut ReasoningTime> {
    assistant
        .content
        .iter_mut()
        .filter(|item| is_block(item, BlockKind::Reasoning))
        .nth(ordinal)
        .and_then(|item| match item {
            AssistantContent::Reasoning { time, .. } => Some(time),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Durable;
    use serde_json::json;

    const SESSION: &str = "ses_test";
    const ASSISTANT: &str = "msg_000000002001assistant0001";
    const USER: &str = "msg_000000001001user000000001";

    struct EventLog {
        events: Vec<Event>,
        seq: u64,
    }

    impl EventLog {
        fn new() -> Self {
            Self {
                events: Vec::new(),
                seq: 0,
            }
        }

        fn durable(&mut self, kind: &str, data: Value) {
            self.seq += 1;
            let seq = self.seq;
            self.push(kind, data, Some(seq));
        }

        fn live(&mut self, kind: &str, data: Value) {
            self.push(kind, data, None);
        }

        fn push(&mut self, kind: &str, mut data: Value, seq: Option<u64>) {
            if let Some(object) = data.as_object_mut() {
                object.insert("sessionID".into(), json!(SESSION));
            }
            let index = self.events.len() + 1;
            self.events.push(Event {
                id: format!("evt_{:012x}event{index:09}", 0x3000 + index),
                kind: kind.to_string(),
                created: Some(1_000 + index as i64),
                location: None,
                data,
                durable: seq.map(|seq| Durable {
                    aggregate_id: SESSION.to_string(),
                    seq,
                }),
            });
        }
    }

    /// One turn: a prompt, a reasoning block, a tool call, then streamed text.
    fn turn() -> Vec<Event> {
        let mut log = EventLog::new();
        log.durable(
            "session.inbox.enqueued",
            json!({"inboxID": USER, "item": {"type": "user", "payload": {"text": "hello"}}}),
        );
        log.durable("session.inbox.delivered", json!({"inboxID": USER}));
        log.durable("session.execution.started", json!({}));
        log.durable(
            "session.step.started",
            json!({"assistantMessageID": ASSISTANT, "agent": "build", "started": 2000}),
        );
        let message = json!(ASSISTANT);
        log.durable(
            "session.reasoning.started",
            json!({"assistantMessageID": message, "ordinal": 0}),
        );
        log.live(
            "session.reasoning.delta",
            json!({"assistantMessageID": message, "ordinal": 0, "delta": "thinking"}),
        );
        log.durable(
            "session.reasoning.ended",
            json!({"assistantMessageID": message, "ordinal": 0, "text": "thinking"}),
        );
        log.durable(
            "session.tool.input.started",
            json!({"assistantMessageID": message, "id": "tool_1", "name": "read"}),
        );
        log.durable(
            "session.tool.input.ended",
            json!({"assistantMessageID": message, "id": "tool_1", "text": "{}"}),
        );
        log.durable(
            "session.tool.called",
            json!({"assistantMessageID": message, "id": "tool_1", "input": {"path": "a.rs"}}),
        );
        log.durable(
            "session.tool.success",
            json!({"assistantMessageID": message, "id": "tool_1", "content": [{"type": "text", "text": "ok"}]}),
        );
        log.durable(
            "session.text.started",
            json!({"assistantMessageID": message, "ordinal": 0}),
        );
        log.live(
            "session.text.delta",
            json!({"assistantMessageID": message, "ordinal": 0, "delta": "Good"}),
        );
        log.live(
            "session.text.delta",
            json!({"assistantMessageID": message, "ordinal": 0, "delta": " day"}),
        );
        log.durable(
            "session.text.ended",
            json!({"assistantMessageID": message, "ordinal": 0, "text": "Good day"}),
        );
        log.durable(
            "session.step.ended",
            json!({"assistantMessageID": message, "finish": "stop", "cost": 0.5}),
        );
        log.durable("session.execution.succeeded", json!({}));
        log.events
    }

    fn snapshot(messages: Vec<Message>, running: bool) -> Snapshot {
        Snapshot {
            messages,
            running,
            revert: None,
        }
    }

    fn apply_all(transcript: &mut SessionTranscript, events: &[Event]) {
        for event in events {
            let applied = transcript.apply(event);
            assert!(
                !matches!(applied, Applied::NeedsResync(_)),
                "{} needed a resync: {applied:?}",
                event.kind
            );
        }
    }

    /// What the server's message list returns after `events`: the same state,
    /// minus live-only deltas, which the server never stores.
    fn server_snapshot(events: &[Event]) -> Vec<Message> {
        let mut transcript = SessionTranscript::new(SESSION);
        let durable: Vec<Event> = events
            .iter()
            .filter(|event| event.durable.is_some())
            .cloned()
            .collect();
        apply_all(&mut transcript, &durable);
        transcript.messages().to_vec()
    }

    #[test]
    fn builds_a_turn_from_events() {
        let mut transcript = SessionTranscript::new(SESSION);
        apply_all(&mut transcript, &turn());

        let kinds: Vec<&str> = transcript
            .messages()
            .iter()
            .map(|message| match &message.body {
                MessageBody::User(_) => "user",
                MessageBody::Assistant(_) => "assistant",
                MessageBody::Idle(_) => "idle",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["user", "assistant", "idle"]);
        assert_eq!(transcript.activity(), Activity::Idle);

        let assistant = transcript.messages()[1].as_assistant().expect("assistant");
        assert_eq!(assistant.finish.as_deref(), Some("stop"));
        assert_eq!(assistant.content.len(), 3);
        assert!(matches!(
            &assistant.content[0],
            AssistantContent::Reasoning { text, time } if text == "thinking" && time.completed.is_some()
        ));
        assert!(matches!(
            &assistant.content[1],
            AssistantContent::Tool(tool) if tool.state.status() == "completed"
        ));
        assert!(matches!(
            &assistant.content[2],
            AssistantContent::Text { text } if text == "Good day"
        ));
    }

    #[test]
    fn streams_text_before_the_block_ends() {
        let events = turn();
        let text_ended = events
            .iter()
            .position(|event| event.kind == "session.text.ended")
            .expect("turn has text");
        let mut transcript = SessionTranscript::new(SESSION);
        apply_all(&mut transcript, &events[..text_ended]);
        let assistant = transcript.messages()[1].as_assistant().expect("assistant");
        assert!(matches!(
            &assistant.content[2],
            AssistantContent::Text { text } if text == "Good day"
        ));
        assert_eq!(transcript.activity(), Activity::Running);
    }

    /// The core sync property: a snapshot taken at any point, followed by
    /// every event buffered since subscribing (which may overlap the
    /// snapshot), ends in the same state as following the stream throughout.
    #[test]
    fn snapshot_plus_overlapping_events_matches_live_stream() {
        let events = turn();
        let mut live = SessionTranscript::new(SESSION);
        apply_all(&mut live, &events);

        for snapshot_at in 0..=events.len() {
            for subscribed_at in 0..=snapshot_at {
                let mut transcript = SessionTranscript::new(SESSION);
                transcript.replace_snapshot(snapshot(server_snapshot(&events[..snapshot_at]), false));
                for (offset, event) in events[subscribed_at..].iter().enumerate() {
                    if let Applied::NeedsResync(_) = transcript.apply(event) {
                        // What a client does: take a fresh snapshot, which
                        // now includes this event.
                        let applied_through = subscribed_at + offset + 1;
                        transcript.replace_snapshot(snapshot(
                            server_snapshot(&events[..applied_through]),
                            true,
                        ));
                    }
                }
                assert_eq!(
                    transcript.messages(),
                    live.messages(),
                    "snapshot after {snapshot_at} events, subscribed after {subscribed_at}"
                );
                assert_eq!(transcript.activity(), Activity::Idle);
            }
        }
    }

    #[test]
    fn applying_events_twice_changes_nothing() {
        let events = turn();
        let mut transcript = SessionTranscript::new(SESSION);
        apply_all(&mut transcript, &events);
        let once = transcript.messages().to_vec();
        transcript.replace_snapshot(snapshot(once.clone(), false));
        apply_all(&mut transcript, &events);
        assert_eq!(transcript.messages(), once.as_slice());
    }

    #[test]
    fn skipped_durable_event_requests_resync() {
        let events = turn();
        let mut transcript = SessionTranscript::new(SESSION);
        transcript.apply(&events[0]);
        let next_durable = events[2..]
            .iter()
            .find(|event| event.durable.is_some())
            .expect("more durable events");
        assert_eq!(
            transcript.apply(next_durable),
            Applied::NeedsResync(ResyncReason::SequenceGap {
                expected: 2,
                received: 3
            })
        );
    }

    #[test]
    fn revert_point_follows_server_order_not_id_order() {
        let mut transcript = SessionTranscript::new(SESSION);
        apply_all(&mut transcript, &turn());
        let mut messages = transcript.messages().to_vec();
        // A prompt whose client-picked ID sorts before the one ahead of it.
        let early_id = "msg_000000000aaa000000000001".to_string();
        messages[1].id = early_id.clone();
        let mut reverted = SessionTranscript::new(SESSION);
        reverted.replace_snapshot(Snapshot {
            messages,
            running: false,
            revert: Some(SessionRevert {
                message_id: early_id,
            }),
        });
        assert_eq!(reverted.visible_messages().len(), 1);
    }

    #[test]
    fn staged_revert_hides_messages_until_cleared() {
        let mut transcript = SessionTranscript::new(SESSION);
        apply_all(&mut transcript, &turn());
        assert_eq!(transcript.visible_messages().len(), 3);

        let mut log = EventLog::new();
        log.durable(
            "session.revert.staged",
            json!({"revert": {"messageID": ASSISTANT, "files": []}}),
        );
        log.durable("session.revert.cleared", json!({}));
        log.durable("session.revert.committed", json!({"to": ASSISTANT}));
        let mut transcript_after = SessionTranscript::new(SESSION);
        transcript_after.replace_snapshot(snapshot(transcript.messages().to_vec(), false));

        assert_eq!(
            transcript_after.apply(&log.events[0]),
            Applied::Changed(vec![Change::Revert(Some(ASSISTANT.to_string()))])
        );
        assert_eq!(transcript_after.visible_messages().len(), 1);
        assert_eq!(transcript_after.messages().len(), 3);

        assert_eq!(
            transcript_after.apply(&log.events[1]),
            Applied::Changed(vec![Change::Revert(None)])
        );
        assert_eq!(transcript_after.visible_messages().len(), 3);

        assert_eq!(
            transcript_after.apply(&log.events[2]),
            Applied::NeedsResync(ResyncReason::HistoryRewritten)
        );
    }

    #[test]
    fn prompt_from_before_subscribing_requests_resync() {
        let mut log = EventLog::new();
        log.durable("session.inbox.delivered", json!({"inboxID": USER}));
        let mut transcript = SessionTranscript::new(SESSION);
        assert_eq!(
            transcript.apply(&log.events[0]),
            Applied::NeedsResync(ResyncReason::MissingState)
        );
    }

    /// A subagent result keeps the ID it got when the subagent started, so
    /// its ID sorts before messages stored earlier. It must still land last.
    #[test]
    fn keeps_delivery_order_not_id_order() {
        const EARLY_ID: &str = "msg_000000000001subagent0001";
        let mut transcript = SessionTranscript::new(SESSION);
        let events = turn();
        apply_all(&mut transcript, &events);
        let mut log = EventLog::new();
        log.seq = events.iter().filter(|event| event.durable.is_some()).count() as u64;
        log.durable(
            "session.inbox.enqueued",
            json!({"inboxID": EARLY_ID, "item": {"type": "synthetic", "payload": {"text": "done"}}}),
        );
        log.durable("session.inbox.delivered", json!({"inboxID": EARLY_ID}));
        apply_all(&mut transcript, &log.events);
        let last = transcript.messages().last().map(|message| message.id.as_str());
        assert_eq!(last, Some(EARLY_ID));
    }

    #[test]
    fn ignores_other_sessions() {
        let mut transcript = SessionTranscript::new("ses_other");
        assert_eq!(transcript.apply(&turn()[0]), Applied::Ignored);
    }

    #[test]
    fn snapshot_diff_reports_removed_and_changed_messages() {
        let mut transcript = SessionTranscript::new(SESSION);
        let events = turn();
        apply_all(&mut transcript, &events);
        let mut reverted = transcript.messages()[..1].to_vec();
        if let MessageBody::User(user) = &mut reverted[0].body {
            user.text = "edited".into();
        }
        let diff = transcript.replace_snapshot(snapshot(reverted, false));
        assert_eq!(diff.changed, vec![USER.to_string()]);
        assert_eq!(diff.removed.len(), 2);
    }
}

fn tool_mut<'a>(assistant: &'a mut AssistantMessage, tool_id: &str) -> Option<&'a mut ToolCall> {
    assistant
        .content
        .iter_mut()
        .rev()
        .find_map(|item| match item {
            AssistantContent::Tool(tool) if tool.id == tool_id => Some(tool),
            _ => None,
        })
}
