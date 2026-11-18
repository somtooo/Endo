//! The messages of one OpenCode session, built from a message snapshot
//! plus the live event stream.
//!
//! Each event changes the messages the way the server changes the messages
//! it stores, with one difference: every event is idempotent here. Text and reasoning blocks
//! are addressed by their per-message `ordinal` and tools by their ID instead
//! of "the latest one", so events that arrive while a snapshot loads can be
//! applied on top of that snapshot without duplicating content.

use crate::types::{
    AssistantContent, AssistantMessage, BlockDelta, BlockEnded, BlockStarted, CompactionMessage,
    CompactionStatus, Event, EventPayload, IdleMessage, IdleOutcome, InboxEntry, InboxItem,
    InterruptReason, Message, MessageBody, MessageTime, ReasoningTime, SessionRevert, ShellEnded,
    ShellMessage, StepEnded, StepStarted, ToolCall, ToolInputStarted, ToolState,
    derived_message_id,
};
use collections::{HashMap, HashSet};
use serde_json::Value;

pub struct SessionMessages {
    session_id: String,
    messages: Vec<Message>,
    message_positions: HashMap<String, usize>,
    /// Only these accept deltas; the server stores block text only when the
    /// block ends.
    open_blocks: HashSet<(String, BlockKind, usize)>,
    pending_inbox: HashMap<String, InboxItem>,
    revert_message_id: Option<String>,
    last_sequence: Option<u64>,
}

impl SessionMessages {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            messages: Vec::new(),
            message_positions: HashMap::default(),
            open_blocks: HashSet::default(),
            pending_inbox: HashMap::default(),
            revert_message_id: None,
            last_sequence: None,
        }
    }

    pub fn apply(&mut self, event: Event) -> MessageStatus {
        if event.session_id.as_deref() != Some(self.session_id.as_str()) {
            return MessageStatus::Unchanged;
        }
        // Numbers can skip: OpenCode numbers internal events too, such as
        // usage records, and leaves them out of the stream. A lost event
        // only comes with a dropped stream or one that doesn't decode, and
        // both resync.
        if let Some(durable) = &event.durable {
            if self.last_sequence.is_some_and(|last| durable.seq <= last) {
                return MessageStatus::Unchanged;
            }
            self.last_sequence = Some(durable.seq);
        }
        let streamed_content = match &event.payload {
            EventPayload::TextDelta { data } => Some(ContentId::Text {
                ordinal: data.ordinal,
            }),
            EventPayload::ReasoningDelta { data } => Some(ContentId::Reasoning {
                ordinal: data.ordinal,
            }),
            EventPayload::ToolInputDelta { data } => Some(ContentId::Tool {
                id: data.id.clone(),
            }),
            _ => None,
        };
        match self.reduce(event) {
            Ok(changed) if changed.is_empty() => MessageStatus::Unchanged,
            Ok(mut changed) => match streamed_content {
                Some(content) if changed.len() == 1 => MessageStatus::Streamed {
                    message_id: changed.remove(0),
                    content,
                },
                _ => MessageStatus::Changed(changed),
            },
            Err(reason) => MessageStatus::NeedsResync(reason),
        }
    }

    /// Returns the messages that are new or changed. Sequence tracking
    /// restarts, because the snapshot carries no sequence number of its own.
    pub fn replace_snapshot(&mut self, snapshot: Snapshot) -> Vec<String> {
        let previous: HashMap<String, Message> = self
            .messages
            .drain(..)
            .map(|message| (message.id.clone(), message))
            .collect();
        self.message_positions.clear();
        self.open_blocks.clear();
        self.last_sequence = None;
        self.revert_message_id = snapshot.revert.map(|revert| revert.message_id);
        self.pending_inbox = snapshot
            .inbox
            .into_iter()
            .map(|entry| (entry.id, entry.item))
            .collect();
        let mut changed = Vec::new();
        for mut message in snapshot.messages {
            // An open block's text is empty in the snapshot, so this has to
            // run before the streamed text fills it in.
            self.track_open_blocks(&message);
            if let Some(earlier) = previous.get(&message.id) {
                keep_streamed_text(&mut message, earlier);
            }
            if previous.get(&message.id) != Some(&message) {
                changed.push(message.id.clone());
            }
            self.insert(message);
        }
        changed
    }

    pub fn message(&self, message_id: &str) -> Option<&Message> {
        self.messages.get(*self.message_positions.get(message_id)?)
    }

    pub fn tool(&self, tool_id: &str) -> Option<&ToolCall> {
        self.messages.iter().rev().find_map(|message| {
            message
                .as_assistant()?
                .content
                .iter()
                .find_map(|item| match item {
                    AssistantContent::Tool(tool) if tool.id == tool_id => Some(tool),
                    _ => None,
                })
        })
    }

    /// Every message the server still stores, including ones hidden by a
    /// staged revert.
    #[cfg(test)]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    fn reduce(&mut self, event: Event) -> Result<Vec<String>, ResyncReason> {
        let Event {
            id: event_id,
            created,
            payload,
            ..
        } = event;
        Ok(match payload {
            EventPayload::ExecutionSucceeded {} => self.insert_derived(
                &event_id,
                MessageBody::Idle(IdleMessage {
                    outcome: IdleOutcome::Succeeded,
                    error: None,
                }),
            ),
            EventPayload::ExecutionFailed { data } => self.insert_derived(
                &event_id,
                MessageBody::Idle(IdleMessage {
                    outcome: IdleOutcome::Failed,
                    error: data.error,
                }),
            ),
            // Like the server, no idle message is stored for "shutdown", which
            // the server also sends for a dismissed question.
            EventPayload::ExecutionInterrupted { data }
                if data.reason == InterruptReason::Shutdown =>
            {
                Vec::new()
            }
            EventPayload::ExecutionInterrupted { .. } => self.insert_derived(
                &event_id,
                MessageBody::Idle(IdleMessage {
                    outcome: IdleOutcome::Interrupted,
                    error: None,
                }),
            ),
            EventPayload::InboxEnqueued { data } => {
                self.pending_inbox.insert(data.inbox_id, data.item);
                Vec::new()
            }
            EventPayload::InboxCancelled { data } => {
                self.pending_inbox.remove(&data.inbox_id);
                Vec::new()
            }
            EventPayload::InboxDelivered { data } => self.deliver_inbox(data.inbox_id)?,
            EventPayload::Synthetic { data } => {
                self.insert_derived(&event_id, MessageBody::Synthetic(data))
            }
            EventPayload::AgentSelected { data } => {
                self.insert_derived(&event_id, MessageBody::AgentSwitched { agent: data.agent })
            }
            EventPayload::ModelSelected { data } => {
                self.insert_derived(&event_id, MessageBody::ModelSwitched { model: data.model })
            }
            EventPayload::ShellStarted { data } => self.insert_derived(
                &event_id,
                MessageBody::Shell(ShellMessage {
                    shell_id: data.shell.id,
                    command: data.shell.command,
                    status: Some(data.shell.status),
                    exit: None,
                    output: None,
                }),
            ),
            EventPayload::ShellEnded { data } => self.end_shell(data),
            // Not shown yet, but kept so the copy lists what the server
            // stores, in the same order.
            EventPayload::SkillActivated {} => self.insert_derived(
                &event_id,
                MessageBody::Other {
                    kind: "skill".to_string(),
                },
            ),
            EventPayload::Moved { .. } => self.insert_derived(
                &event_id,
                MessageBody::Other {
                    kind: "location-switched".to_string(),
                },
            ),
            EventPayload::StepStarted { data } => self.start_step(data, created),
            EventPayload::StepStreamed { data } => {
                self.update_assistant(data.assistant_message_id, |assistant| {
                    let changed = assistant.time.streamed != created;
                    assistant.time.streamed = created;
                    changed
                })
            }
            EventPayload::StepEnded { data } => self.end_step(data, created, false),
            EventPayload::StepFailed { data } => self.end_step(data, created, true),
            EventPayload::TextStarted { data } => self.start_block(data, BlockKind::Text, created),
            EventPayload::TextDelta { data } => self.append_block(data, BlockKind::Text),
            EventPayload::TextEnded { data } => self.end_block(data, BlockKind::Text, created),
            EventPayload::ReasoningStarted { data } => {
                self.start_block(data, BlockKind::Reasoning, created)
            }
            EventPayload::ReasoningDelta { data } => self.append_block(data, BlockKind::Reasoning),
            EventPayload::ReasoningEnded { data } => {
                self.end_block(data, BlockKind::Reasoning, created)
            }
            EventPayload::ToolInputStarted { data } => self.start_tool(data),
            EventPayload::ToolInputDelta { data } => {
                self.update_tool(data.assistant_message_id, &data.id, |state| {
                    let ToolState::Streaming { input } = state else {
                        return false;
                    };
                    input.push_str(&data.delta);
                    true
                })
            }
            EventPayload::ToolInputEnded { data } => {
                self.update_tool(data.assistant_message_id, &data.id, |state| {
                    let ToolState::Streaming { input } = state else {
                        return false;
                    };
                    let changed = *input != data.text;
                    *input = data.text;
                    changed
                })
            }
            EventPayload::ToolCalled { data } => {
                self.update_tool(data.assistant_message_id, &data.id, |state| {
                    if state.is_terminal() {
                        return false;
                    }
                    *state = ToolState::Running {
                        input: data.input,
                        metadata: Value::Object(Default::default()),
                    };
                    true
                })
            }
            EventPayload::ToolProgress { data } => {
                self.update_tool(data.assistant_message_id, &data.id, |state| {
                    let ToolState::Running { metadata, .. } = state else {
                        return false;
                    };
                    *metadata = data.metadata;
                    true
                })
            }
            EventPayload::ToolSucceeded { data } => {
                self.update_tool(data.assistant_message_id, &data.id, |state| {
                    let ToolState::Running { input, .. } = state else {
                        return false;
                    };
                    *state = ToolState::Completed {
                        input: input.take(),
                        content: data.content,
                        metadata: data.metadata,
                    };
                    true
                })
            }
            EventPayload::ToolFailed { data } => {
                self.update_tool(data.assistant_message_id, &data.id, |state| {
                    let input = match state {
                        ToolState::Running { input, .. } => input.take(),
                        ToolState::Streaming { .. } => Value::Object(Default::default()),
                        ToolState::Completed { .. }
                        | ToolState::Error { .. }
                        | ToolState::Other => {
                            return false;
                        }
                    };
                    *state = ToolState::Error {
                        input,
                        error: data.error,
                    };
                    true
                })
            }
            EventPayload::MessageContentUpdated { data } => {
                if !self.contains(&data.message_id) {
                    return Err(ResyncReason::MissingState);
                }
                self.update_assistant(data.message_id, |assistant| {
                    let changed = assistant.content != data.content;
                    assistant.content = data.content;
                    changed
                })
            }
            EventPayload::CompactionStarted { data } => self.insert(Message {
                id: data
                    .input_id
                    .unwrap_or_else(|| derived_message_id(&event_id)),
                body: MessageBody::Compaction(CompactionMessage {
                    status: CompactionStatus::Running,
                    summary: String::new(),
                    error: None,
                }),
            }),
            EventPayload::CompactionDelta { data } => {
                self.update_running_compaction(|compaction| compaction.summary.push_str(&data.text))
            }
            EventPayload::CompactionEnded { data } => self.end_compaction(
                &event_id,
                CompactionStatus::Completed,
                Some(data.text),
                None,
            ),
            EventPayload::CompactionFailed { data } => {
                self.end_compaction(&event_id, CompactionStatus::Failed, None, data.error)
            }
            EventPayload::RevertStaged { data } => self.set_revert(Some(data.revert.message_id)),
            EventPayload::RevertCleared {} => self.set_revert(None),
            EventPayload::RevertCommitted { data } => self.commit_revert(data.to)?,
            EventPayload::ExecutionStarted {}
            | EventPayload::Renamed { .. }
            | EventPayload::PermissionAsked { .. }
            | EventPayload::PermissionReplied { .. }
            | EventPayload::FormCreated { .. }
            | EventPayload::FormClosed { .. }
            | EventPayload::Archived {}
            | EventPayload::Unarchived {}
            | EventPayload::Deleted {}
            | EventPayload::ProjectSetupChanged {}
            | EventPayload::Other => Vec::new(),
        })
    }

    fn deliver_inbox(&mut self, inbox_id: String) -> Result<Vec<String>, ResyncReason> {
        let item = self.pending_inbox.remove(&inbox_id);
        if self.contains(&inbox_id) {
            return Ok(Vec::new());
        }
        let Some(item) = item else {
            // Enqueued before this copy subscribed; only a snapshot has it.
            return Err(ResyncReason::MissingState);
        };
        let body = match item {
            InboxItem::User { payload } => MessageBody::User(payload),
            InboxItem::Synthetic { payload } => MessageBody::Synthetic(payload),
            InboxItem::Other => return Ok(Vec::new()),
        };
        Ok(self.insert(Message { id: inbox_id, body }))
    }

    fn end_shell(&mut self, data: ShellEnded) -> Vec<String> {
        let Some(message) = self.messages.iter_mut().rev().find(|message| {
            matches!(&message.body, MessageBody::Shell(started) if started.shell_id == data.shell.id)
        }) else {
            return Vec::new();
        };
        let MessageBody::Shell(started) = &mut message.body else {
            return Vec::new();
        };
        started.status = Some(data.shell.status);
        started.exit = data.shell.exit;
        started.output = Some(data.output);
        vec![message.id.clone()]
    }

    fn start_step(&mut self, data: StepStarted, created: Option<i64>) -> Vec<String> {
        let time = MessageTime {
            created: data.started.or(created),
            streamed: None,
            completed: None,
        };
        if self.contains(&data.assistant_message_id) {
            return self.update_assistant(data.assistant_message_id, |assistant| {
                let changed = assistant.agent != data.agent
                    || assistant.model != data.model
                    || assistant.error.is_some()
                    || assistant.finish.is_some()
                    || assistant.time != time;
                assistant.agent = data.agent;
                assistant.model = data.model;
                assistant.error = None;
                assistant.finish = None;
                assistant.time = time;
                changed
            });
        }
        let mut changed = Vec::new();
        if let Some(current) = self.find_running_assistant() {
            let id = current.id.clone();
            if let Some(assistant) = current.as_assistant_mut() {
                assistant.time.completed = created;
                changed.push(id);
            }
        }
        changed.extend(self.insert(Message {
            id: data.assistant_message_id,
            body: MessageBody::Assistant(AssistantMessage {
                agent: data.agent,
                model: data.model,
                content: Vec::new(),
                finish: None,
                error: None,
                cost: None,
                tokens: None,
                time,
            }),
        }));
        changed
    }

    fn end_step(&mut self, data: StepEnded, created: Option<i64>, failed: bool) -> Vec<String> {
        self.update_assistant(data.assistant_message_id, |assistant| {
            let finish = data.finish.or_else(|| failed.then(|| "error".to_string()));
            let error = if failed {
                data.error
            } else {
                assistant.error.clone()
            };
            let cost = data.cost.or(assistant.cost);
            let tokens = data.tokens.or_else(|| assistant.tokens.clone());
            let changed = assistant.time.completed != created
                || assistant.finish != finish
                || assistant.error != error
                || assistant.cost != cost
                || assistant.tokens != tokens;
            assistant.time.completed = created;
            assistant.finish = finish;
            assistant.error = error;
            assistant.cost = cost;
            assistant.tokens = tokens;
            changed
        })
    }

    fn start_block(
        &mut self,
        data: BlockStarted,
        kind: BlockKind,
        created: Option<i64>,
    ) -> Vec<String> {
        let BlockStarted {
            assistant_message_id,
            ordinal,
        } = data;
        let changed = self.update_assistant(assistant_message_id.clone(), |assistant| {
            if block_mut(assistant, kind, ordinal).is_some() {
                return false;
            }
            ensure_block(assistant, kind, ordinal, created).is_some()
        });
        if !changed.is_empty() {
            self.open_blocks
                .insert((assistant_message_id, kind, ordinal));
        }
        changed
    }

    fn append_block(&mut self, data: BlockDelta, kind: BlockKind) -> Vec<String> {
        let key = (data.assistant_message_id, kind, data.ordinal);
        if !self.open_blocks.contains(&key) {
            return Vec::new();
        }
        let (message_id, kind, ordinal) = key;
        self.update_assistant(message_id, |assistant| {
            let Some(text) = block_mut(assistant, kind, ordinal).and_then(block_text_mut) else {
                return false;
            };
            text.push_str(&data.delta);
            true
        })
    }

    fn end_block(
        &mut self,
        data: BlockEnded,
        kind: BlockKind,
        created: Option<i64>,
    ) -> Vec<String> {
        let key = (data.assistant_message_id, kind, data.ordinal);
        self.open_blocks.remove(&key);
        let (message_id, kind, ordinal) = key;
        self.update_assistant(message_id, |assistant| {
            match ensure_block(assistant, kind, ordinal, created) {
                Some(AssistantContent::Text { text }) => {
                    let changed = *text != data.text;
                    *text = data.text;
                    changed
                }
                Some(AssistantContent::Reasoning { text, time }) => {
                    let changed = *text != data.text || time.completed.is_none();
                    *text = data.text;
                    if time.completed.is_none() {
                        time.completed = created;
                    }
                    changed
                }
                Some(AssistantContent::Tool(_) | AssistantContent::Other) | None => false,
            }
        })
    }

    fn start_tool(&mut self, data: ToolInputStarted) -> Vec<String> {
        self.update_assistant(data.assistant_message_id, |assistant| {
            if tool_mut(assistant, &data.id).is_some() {
                return false;
            }
            assistant.content.push(AssistantContent::Tool(ToolCall {
                id: data.id,
                name: data.name,
                state: ToolState::Streaming {
                    input: String::new(),
                },
            }));
            true
        })
    }

    fn update_tool(
        &mut self,
        message_id: String,
        tool_id: &str,
        update: impl FnOnce(&mut ToolState) -> bool,
    ) -> Vec<String> {
        self.update_assistant(message_id, |assistant| {
            tool_mut(assistant, tool_id).is_some_and(|tool| update(&mut tool.state))
        })
    }

    pub fn contains(&self, message_id: &str) -> bool {
        self.message_positions.contains_key(message_id)
    }

    /// `update` returns whether it changed the message.
    fn update_assistant(
        &mut self,
        message_id: String,
        update: impl FnOnce(&mut AssistantMessage) -> bool,
    ) -> Vec<String> {
        let changed = self.assistant_mut(&message_id).is_some_and(update);
        if changed {
            vec![message_id]
        } else {
            Vec::new()
        }
    }

    fn update_running_compaction(
        &mut self,
        update: impl FnOnce(&mut CompactionMessage),
    ) -> Vec<String> {
        let running = self
            .messages
            .iter_mut()
            .rev()
            .find_map(|message| match &mut message.body {
                MessageBody::Compaction(compaction)
                    if compaction.status == CompactionStatus::Running =>
                {
                    Some((&message.id, compaction))
                }
                _ => None,
            });
        match running {
            Some((id, compaction)) => {
                update(compaction);
                vec![id.clone()]
            }
            None => Vec::new(),
        }
    }

    fn end_compaction(
        &mut self,
        event_id: &str,
        status: CompactionStatus,
        summary: Option<String>,
        error: Option<Value>,
    ) -> Vec<String> {
        let changed = self.update_running_compaction(|compaction| {
            compaction.status = status;
            if let Some(summary) = &summary {
                compaction.summary = summary.clone();
            }
            compaction.error = error.clone();
        });
        if !changed.is_empty() {
            return changed;
        }
        // Replayed after a snapshot that already has this compaction
        // finished: the server keeps it under the started event's ID, so
        // adding another would show it twice.
        let latest_finished = self
            .messages
            .iter()
            .rev()
            .find_map(|message| match &message.body {
                MessageBody::Compaction(compaction) => Some(compaction),
                _ => None,
            });
        let already_finished = latest_finished.is_some_and(|compaction| {
            compaction.status == status
                && summary
                    .as_ref()
                    .is_none_or(|summary| *summary == compaction.summary)
        });
        if already_finished {
            return Vec::new();
        }
        self.insert_derived(
            event_id,
            MessageBody::Compaction(CompactionMessage {
                status,
                summary: summary.unwrap_or_default(),
                error,
            }),
        )
    }

    /// Returns the messages that were hidden or shown.
    fn set_revert(&mut self, revert_message_id: Option<String>) -> Vec<String> {
        if self.revert_message_id == revert_message_id {
            return Vec::new();
        }
        let before = self.visible_messages().len();
        self.revert_message_id = revert_message_id;
        let after = self.visible_messages().len();
        self.messages[before.min(after)..before.max(after)]
            .iter()
            .map(|message| message.id.clone())
            .collect()
    }

    /// The messages a client shows: everything before the revert point.
    pub fn visible_messages(&self) -> &[Message] {
        // Messages are in the server's order, which IDs only usually follow,
        // since a client picks the IDs of the prompts it sends.
        let end = match &self.revert_message_id {
            Some(message_id) => match self.message_positions.get(message_id) {
                Some(position) => *position,
                None => self
                    .messages
                    .iter()
                    .position(|message| message.id >= *message_id)
                    .unwrap_or(self.messages.len()),
            },
            None => self.messages.len(),
        };
        &self.messages[..end]
    }

    /// Committing a revert deletes `to` and every message stored after it.
    fn commit_revert(&mut self, to: String) -> Result<Vec<String>, ResyncReason> {
        let Some(&position) = self.message_positions.get(&to) else {
            return Err(ResyncReason::HistoryRewritten);
        };
        let removed: Vec<String> = self
            .messages
            .drain(position..)
            .map(|message| message.id)
            .collect();
        for id in &removed {
            self.message_positions.remove(id);
        }
        self.open_blocks
            .retain(|(message_id, ..)| self.message_positions.contains_key(message_id));
        self.revert_message_id = None;
        Ok(removed)
    }

    fn insert_derived(&mut self, event_id: &str, body: MessageBody) -> Vec<String> {
        self.insert(Message {
            id: derived_message_id(event_id),
            body,
        })
    }

    /// New messages go at the end. The server orders messages by when they
    /// were stored, not by ID: a subagent result keeps the ID it got when
    /// the subagent started but lands when it finishes. Snapshots come in
    /// stored order and events arrive in it, as in OpenCode's own client.
    fn insert(&mut self, message: Message) -> Vec<String> {
        if self.contains(&message.id) {
            return Vec::new();
        }
        let id = message.id.clone();
        self.message_positions
            .insert(id.clone(), self.messages.len());
        self.messages.push(message);
        vec![id]
    }

    fn track_open_blocks(&mut self, message: &Message) {
        let Some(assistant) = message.as_assistant() else {
            return;
        };
        if assistant.time.completed.is_some() {
            return;
        }
        for kind in [BlockKind::Text, BlockKind::Reasoning] {
            let blocks = assistant.content.iter().filter(|item| kind.matches(item));
            for (ordinal, block) in blocks.enumerate() {
                let open = match block {
                    AssistantContent::Text { text } => text.is_empty(),
                    AssistantContent::Reasoning { time, .. } => time.completed.is_none(),
                    AssistantContent::Tool(_) | AssistantContent::Other => false,
                };
                if open {
                    self.open_blocks.insert((message.id.clone(), kind, ordinal));
                }
            }
        }
    }

    fn assistant_mut(&mut self, message_id: &str) -> Option<&mut AssistantMessage> {
        let position = *self.message_positions.get(message_id)?;
        self.messages.get_mut(position)?.as_assistant_mut()
    }

    fn find_running_assistant(&mut self) -> Option<&mut Message> {
        self.messages.iter_mut().rev().find(|message| {
            message
                .as_assistant()
                .is_some_and(|assistant| assistant.time.completed.is_none())
        })
    }
}

pub struct Snapshot {
    pub messages: Vec<Message>,
    pub revert: Option<SessionRevert>,
    pub inbox: Vec<InboxEntry>,
}

#[derive(Debug, PartialEq)]
pub enum MessageStatus {
    Unchanged,
    /// The messages whose content or visibility changed.
    Changed(Vec<String>),
    /// Streamed content was added to one item of a message. Nothing else in
    /// the message changed, nor which messages there are.
    Streamed {
        message_id: String,
        content: ContentId,
    },
    NeedsResync(ResyncReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentId {
    Text { ordinal: usize },
    Reasoning { ordinal: usize },
    Tool { id: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResyncReason {
    /// The server deleted messages from one this copy doesn't have.
    HistoryRewritten,
    /// The event refers to state from before this copy was built.
    MissingState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BlockKind {
    Text,
    Reasoning,
}

impl BlockKind {
    fn matches(self, item: &AssistantContent) -> bool {
        matches!(
            (item, self),
            (AssistantContent::Text { .. }, BlockKind::Text)
                | (AssistantContent::Reasoning { .. }, BlockKind::Reasoning)
        )
    }
}

/// The server stores a block's text only when the block ends, so a snapshot
/// taken mid-block has it empty.
fn keep_streamed_text(message: &mut Message, earlier: &Message) {
    let (Some(assistant), Some(earlier)) = (message.as_assistant_mut(), earlier.as_assistant())
    else {
        return;
    };
    if assistant.time.completed.is_some() {
        return;
    }
    for kind in [BlockKind::Text, BlockKind::Reasoning] {
        let streamed = earlier.content.iter().filter(|item| kind.matches(item));
        for (ordinal, item) in streamed.enumerate() {
            let (AssistantContent::Text { text } | AssistantContent::Reasoning { text, .. }) = item
            else {
                continue;
            };
            if let Some(stored) = block_mut(assistant, kind, ordinal).and_then(block_text_mut)
                && stored.is_empty()
            {
                stored.clone_from(text);
            }
        }
    }
}

fn block_mut(
    assistant: &mut AssistantMessage,
    kind: BlockKind,
    ordinal: usize,
) -> Option<&mut AssistantContent> {
    assistant
        .content
        .iter_mut()
        .filter(|item| kind.matches(item))
        .nth(ordinal)
}

/// The block, after adding empty ones up to it. Blocks can start out of
/// order, and a block's end can arrive without its start.
fn ensure_block(
    assistant: &mut AssistantMessage,
    kind: BlockKind,
    ordinal: usize,
    created: Option<i64>,
) -> Option<&mut AssistantContent> {
    let count = assistant
        .content
        .iter()
        .filter(|item| kind.matches(item))
        .count();
    for _ in count..=ordinal {
        assistant.content.push(match kind {
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
        });
    }
    block_mut(assistant, kind, ordinal)
}

fn block_text_mut(item: &mut AssistantContent) -> Option<&mut String> {
    match item {
        AssistantContent::Text { text } | AssistantContent::Reasoning { text, .. } => Some(text),
        AssistantContent::Tool(_) | AssistantContent::Other => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Durable, EventFrame, ShellOutput, ShellStatus, UserMessage};
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
            let frame = EventFrame {
                id: format!("evt_{:012x}event{index:09}", 0x3000 + index),
                kind: kind.to_string(),
                created: Some(1_000 + index as i64),
                location: None,
                data,
                durable: seq.map(|seq| Durable {
                    aggregate_id: SESSION.to_string(),
                    seq,
                }),
            };
            self.events
                .push(frame.decode().expect("test events decode"));
        }
    }

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

    fn snapshot(messages: Vec<Message>) -> Snapshot {
        Snapshot {
            messages,
            revert: None,
            inbox: Vec::new(),
        }
    }

    fn apply_all(session: &mut SessionMessages, events: &[Event]) {
        for event in events {
            let applied = session.apply(event.clone());
            assert!(
                !matches!(applied, MessageStatus::NeedsResync(_)),
                "{:?} needed a resync: {applied:?}",
                event.payload
            );
        }
    }

    /// What the server's message list returns after `events`: the same state,
    /// minus live-only deltas, which the server never stores.
    fn server_snapshot(events: &[Event]) -> Vec<Message> {
        let mut session = SessionMessages::new(SESSION);
        let durable: Vec<Event> = events
            .iter()
            .filter(|event| event.durable.is_some())
            .cloned()
            .collect();
        apply_all(&mut session, &durable);
        session.messages().to_vec()
    }

    #[test]
    fn builds_a_turn_from_events() {
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, &turn());

        let kinds: Vec<&str> = session
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

        let assistant = session.messages()[1].as_assistant().expect("assistant");
        assert_eq!(assistant.finish.as_deref(), Some("stop"));
        assert_eq!(assistant.content.len(), 3);
        assert!(matches!(
            &assistant.content[0],
            AssistantContent::Reasoning { text, time } if text == "thinking" && time.completed.is_some()
        ));
        assert!(matches!(
            &assistant.content[1],
            AssistantContent::Tool(tool) if matches!(tool.state, ToolState::Completed { .. })
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
            .position(|event| matches!(event.payload, EventPayload::TextEnded { .. }))
            .expect("turn has text");
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, &events[..text_ended]);
        let assistant = session.messages()[1].as_assistant().expect("assistant");
        assert!(matches!(
            &assistant.content[2],
            AssistantContent::Text { text } if text == "Good day"
        ));
    }

    /// The core sync property: a snapshot taken at any point, followed by
    /// every event buffered since subscribing (which may overlap the
    /// snapshot), ends in the same state as following the stream throughout.
    #[test]
    fn snapshot_plus_overlapping_events_matches_live_stream() {
        let events = turn();
        let mut live = SessionMessages::new(SESSION);
        apply_all(&mut live, &events);

        for snapshot_at in 0..=events.len() {
            for subscribed_at in 0..=snapshot_at {
                let mut session = SessionMessages::new(SESSION);
                session.replace_snapshot(snapshot(server_snapshot(&events[..snapshot_at])));
                for (offset, event) in events[subscribed_at..].iter().enumerate() {
                    if let MessageStatus::NeedsResync(_) = session.apply(event.clone()) {
                        // What a client does: take a fresh snapshot, which
                        // now includes this event.
                        let applied_through = subscribed_at + offset + 1;
                        session.replace_snapshot(snapshot(server_snapshot(
                            &events[..applied_through],
                        )));
                    }
                }
                assert_eq!(
                    session.messages(),
                    live.messages(),
                    "snapshot after {snapshot_at} events, subscribed after {subscribed_at}"
                );
            }
        }
    }

    #[test]
    fn applying_events_twice_changes_nothing() {
        let events = turn();
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, &events);
        let once = session.messages().to_vec();
        session.replace_snapshot(snapshot(once.clone()));
        apply_all(&mut session, &events);
        assert_eq!(session.messages(), once.as_slice());
    }

    #[test]
    fn a_skipped_sequence_number_is_not_a_lost_event() {
        let mut expected = SessionMessages::new(SESSION);
        apply_all(&mut expected, &turn());

        // As when OpenCode numbers an internal event it doesn't stream.
        let mut events = turn();
        for event in events.iter_mut().skip(3) {
            if let Some(durable) = &mut event.durable {
                durable.seq += 1;
            }
        }
        let mut session = SessionMessages::new(SESSION);
        for event in events {
            assert!(!matches!(
                session.apply(event),
                MessageStatus::NeedsResync(_)
            ));
        }
        assert_eq!(session.messages(), expected.messages());
    }

    fn read_assistant_text(session: &SessionMessages) -> String {
        session
            .messages()
            .iter()
            .filter_map(Message::as_assistant)
            .flat_map(|assistant| &assistant.content)
            .filter_map(|item| match item {
                AssistantContent::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn deltas_report_the_part_they_change() {
        let mut session = SessionMessages::new(SESSION);
        let parts: Vec<(String, ContentId)> = turn()
            .into_iter()
            .filter_map(|event| match session.apply(event) {
                MessageStatus::Streamed {
                    message_id,
                    content,
                } => Some((message_id, content)),
                _ => None,
            })
            .collect();
        let assistant = ASSISTANT.to_string();
        assert_eq!(
            parts,
            [
                (assistant.clone(), ContentId::Reasoning { ordinal: 0 }),
                (assistant.clone(), ContentId::Text { ordinal: 0 }),
                (assistant, ContentId::Text { ordinal: 0 }),
            ]
        );
    }

    #[test]
    fn a_snapshot_mid_block_keeps_the_text_streamed_so_far() {
        let events = turn();
        // Up to the second text delta, before the block ends.
        let streamed = &events[..14];
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, streamed);
        assert_eq!(read_assistant_text(&session), "Good day");

        session.replace_snapshot(snapshot(server_snapshot(streamed)));
        assert_eq!(read_assistant_text(&session), "Good day");
    }

    #[test]
    fn a_block_keeps_streaming_after_a_snapshot() {
        let events = turn();
        // Up to the first text delta, before the block ends.
        let streamed = &events[..13];
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, streamed);
        session.replace_snapshot(snapshot(server_snapshot(streamed)));
        assert_eq!(read_assistant_text(&session), "Good");

        apply_all(&mut session, &events[13..14]);
        assert_eq!(read_assistant_text(&session), "Good day");
    }

    #[test]
    fn replaying_a_compaction_over_a_snapshot_keeps_one_message() {
        let mut log = EventLog::new();
        log.durable(
            "session.compaction.started",
            json!({"inputID": "msg_000000003000compaction01"}),
        );
        log.durable("session.compaction.ended", json!({"text": "summary"}));
        let mut session = SessionMessages::new(SESSION);
        session.replace_snapshot(snapshot(server_snapshot(&log.events)));
        assert_eq!(session.messages().len(), 1);

        // Events buffered during the resync are replayed on top of it.
        apply_all(&mut session, &log.events);
        assert_eq!(session.messages().len(), 1);
    }

    #[test]
    fn a_compaction_summary_streams_in() {
        let mut log = EventLog::new();
        log.durable("session.compaction.started", json!({"reason": "auto"}));
        log.live("session.compaction.delta", json!({"text": "Sum"}));
        log.live("session.compaction.delta", json!({"text": "mary"}));
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, &log.events);
        let summary = match &session.messages()[0].body {
            MessageBody::Compaction(compaction) => compaction.summary.clone(),
            other => panic!("expected a compaction, got {other:?}"),
        };
        assert_eq!(summary, "Summary");
    }

    #[test]
    fn a_shutdown_interrupt_adds_no_idle_message() {
        let mut log = EventLog::new();
        log.durable("session.execution.started", json!({}));
        log.durable(
            "session.execution.interrupted",
            json!({"reason": "shutdown"}),
        );
        log.durable("session.execution.started", json!({}));
        log.durable("session.execution.interrupted", json!({"reason": "user"}));
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, &log.events[..2]);
        assert!(session.messages().is_empty());

        apply_all(&mut session, &log.events[2..]);
        assert_eq!(session.messages().len(), 1);
    }

    #[test]
    fn shell_events_add_a_shell_message_and_finish_it() {
        let mut log = EventLog::new();
        log.durable(
            "session.shell.started",
            json!({"shell": {"id": "sh_1", "command": "ls", "status": "running"}}),
        );
        log.durable(
            "session.shell.ended",
            json!({
                "shell": {"id": "sh_1", "command": "ls", "status": "exited"},
                "output": {"output": "a.rs", "cursor": 4, "size": 4, "truncated": false},
            }),
        );
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, &log.events[..1]);
        let shell = |session: &SessionMessages| match &session.messages()[0].body {
            MessageBody::Shell(shell) => shell.clone(),
            other => panic!("expected a shell message, got {other:?}"),
        };
        assert_eq!(shell(&session).status, Some(ShellStatus::Running));

        apply_all(&mut session, &log.events[1..]);
        assert_eq!(session.messages().len(), 1);
        assert_eq!(shell(&session).status, Some(ShellStatus::Exited));
        assert_eq!(
            shell(&session).output,
            Some(ShellOutput {
                output: "a.rs".into(),
                cursor: 4,
                size: 4,
            })
        );
    }

    #[test]
    fn revert_point_follows_server_order_not_id_order() {
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, &turn());
        let mut messages = session.messages().to_vec();
        // A prompt whose client-picked ID sorts before the one ahead of it.
        let early_id = "msg_000000000aaa000000000001".to_string();
        messages[1].id = early_id.clone();
        let mut reverted = SessionMessages::new(SESSION);
        reverted.replace_snapshot(Snapshot {
            messages,
            revert: Some(SessionRevert {
                message_id: early_id,
            }),
            inbox: Vec::new(),
        });
        assert_eq!(reverted.visible_messages().len(), 1);
    }

    #[test]
    fn a_missing_revert_point_cuts_at_the_first_later_id() {
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, &turn());
        let mut messages = session.messages().to_vec();
        for (message, id) in messages.iter_mut().zip(["msg_3", "msg_1", "msg_2"]) {
            message.id = id.to_string();
        }
        let mut reverted = SessionMessages::new(SESSION);
        reverted.replace_snapshot(Snapshot {
            messages,
            revert: Some(SessionRevert {
                message_id: "msg_25".to_string(),
            }),
            inbox: Vec::new(),
        });
        assert!(reverted.visible_messages().is_empty());
    }

    #[test]
    fn staged_revert_hides_messages_until_cleared() {
        let mut session = SessionMessages::new(SESSION);
        apply_all(&mut session, &turn());
        assert_eq!(session.visible_messages().len(), 3);

        let mut log = EventLog::new();
        log.durable(
            "session.revert.staged",
            json!({"revert": {"messageID": ASSISTANT, "files": []}}),
        );
        log.durable("session.revert.cleared", json!({}));
        log.durable("session.revert.committed", json!({"to": ASSISTANT}));
        let mut session_after = SessionMessages::new(SESSION);
        session_after.replace_snapshot(snapshot(session.messages().to_vec()));

        let hidden = session.messages()[1..]
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            session_after.apply(log.events[0].clone()),
            MessageStatus::Changed(hidden.clone())
        );
        assert_eq!(session_after.visible_messages().len(), 1);
        assert_eq!(session_after.messages().len(), 3);

        assert_eq!(
            session_after.apply(log.events[1].clone()),
            MessageStatus::Changed(hidden.clone())
        );
        assert_eq!(session_after.visible_messages().len(), 3);

        // Committing deletes the messages here too, with no resync.
        assert_eq!(
            session_after.apply(log.events[2].clone()),
            MessageStatus::Changed(hidden)
        );
        assert_eq!(session_after.messages().len(), 1);
        assert_eq!(session_after.visible_messages().len(), 1);
    }

    #[test]
    fn prompt_from_before_subscribing_requests_resync() {
        let mut log = EventLog::new();
        log.durable("session.inbox.delivered", json!({"inboxID": USER}));
        let mut session = SessionMessages::new(SESSION);
        assert_eq!(
            session.apply(log.events[0].clone()),
            MessageStatus::NeedsResync(ResyncReason::MissingState)
        );
    }

    /// A subagent result keeps the ID it got when the subagent started, so
    /// its ID sorts before messages stored earlier. It must still land last.
    #[test]
    fn keeps_delivery_order_not_id_order() {
        const EARLY_ID: &str = "msg_000000000001subagent0001";
        let mut session = SessionMessages::new(SESSION);
        let events = turn();
        apply_all(&mut session, &events);
        let mut log = EventLog::new();
        log.seq = events
            .iter()
            .filter(|event| event.durable.is_some())
            .count() as u64;
        log.durable(
            "session.inbox.enqueued",
            json!({"inboxID": EARLY_ID, "item": {"type": "synthetic", "payload": {"text": "done"}}}),
        );
        log.durable("session.inbox.delivered", json!({"inboxID": EARLY_ID}));
        apply_all(&mut session, &log.events);
        let last = session.messages().last().map(|message| message.id.as_str());
        assert_eq!(last, Some(EARLY_ID));
    }

    #[test]
    fn ignores_other_sessions() {
        let mut session = SessionMessages::new("ses_other");
        assert_eq!(session.apply(turn()[0].clone()), MessageStatus::Unchanged);
    }

    #[test]
    fn snapshot_diff_reports_removed_and_changed_messages() {
        let mut session = SessionMessages::new(SESSION);
        let events = turn();
        apply_all(&mut session, &events);
        let mut reverted = session.messages()[..1].to_vec();
        if let MessageBody::User(user) = &mut reverted[0].body {
            user.text = "edited".into();
        }
        let changed = session.replace_snapshot(snapshot(reverted));
        assert_eq!(changed, vec![USER.to_string()]);
        assert_eq!(session.messages().len(), 1);
    }

    #[test]
    fn committing_a_revert_of_a_message_the_copy_lacks_resyncs() {
        let mut log = EventLog::new();
        log.durable("session.revert.committed", json!({"to": USER}));
        let mut session = SessionMessages::new(SESSION);
        assert_eq!(
            session.apply(log.events[0].clone()),
            MessageStatus::NeedsResync(ResyncReason::HistoryRewritten)
        );
    }

    #[test]
    fn a_prompt_queued_before_the_snapshot_is_delivered_without_a_resync() {
        let mut log = EventLog::new();
        log.durable("session.inbox.delivered", json!({"inboxID": USER}));
        let mut session = SessionMessages::new(SESSION);
        session.replace_snapshot(Snapshot {
            messages: Vec::new(),
            revert: None,
            inbox: vec![InboxEntry {
                id: USER.to_string(),
                item: InboxItem::User {
                    payload: UserMessage {
                        text: "hello".into(),
                        files: Vec::new(),
                        skills: Vec::new(),
                        metadata: None,
                    },
                },
            }],
        });
        assert_eq!(
            session.apply(log.events[0].clone()),
            MessageStatus::Changed(vec![USER.to_string()])
        );
    }

    #[test]
    fn a_content_update_for_a_missing_message_resyncs() {
        let mut log = EventLog::new();
        log.durable(
            "session.message.content.updated",
            json!({"messageID": ASSISTANT, "content": []}),
        );
        let mut session = SessionMessages::new(SESSION);
        assert_eq!(
            session.apply(log.events[0].clone()),
            MessageStatus::NeedsResync(ResyncReason::MissingState)
        );
    }
}
