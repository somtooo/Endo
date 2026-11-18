//! Translates OpenCode messages into `AcpThread` entries.
//!
//! Every entry is keyed, so translating a message again updates it in place.
//! One OpenCode assistant message holds reasoning, text and tool calls in
//! order; each becomes its own keyed chunk or tool call entry, which keeps
//! Zed's layout (a tool call splits the surrounding assistant text) the same
//! whether the message streams in or loads from a snapshot.

use crate::connection::Activity;
use crate::session_messages::ContentId;
use crate::shell_terminals::{ShellTerminals, background_shell_id, terminal_bytes, terminal_id};
use crate::types::{
    AssistantContent, AssistantMessage, CompactionStatus, IdleOutcome, Message, MessageBody,
    ShellStatus, Skill, StepError, TextMessage, ToolCall, ToolState, UserMessage,
};
use acp_thread::{
    AcpThread, ClientUserMessageId, DisplayTerminalOutput, DisplayTerminalPatch, MentionUri,
    SUBAGENT_SESSION_INFO_META_KEY,
};
use action_log::ExternalEditKind;
use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::v2::schema as acp;
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use gpui::Context;
use http_client::Url;
use serde::Deserialize;
use serde_json::Value;
use std::ops::Range;
use std::path::{Path, PathBuf};
use util::paths::PathStyle;

pub(crate) fn to_message_id(client_id: &ClientUserMessageId) -> String {
    client_id.message_id().0.to_string()
}

/// The agent panel expects at most one new entry per effect flush, so these
/// are applied one entry at a time.
pub(crate) enum Upsert {
    LocalUser(ClientUserMessageId, Vec<acp::ContentBlock>),
    Assistant(acp::AgentMessage),
    Thought(acp::AgentThought),
    Tool(acp::ToolCallUpdate),
    /// Must come before the tool call that references it.
    Terminal(acp::TerminalId, DisplayTerminalPatch),
    Plan(acp::PlanItems),
    Compaction(acp::CompactionUpdate),
}

impl Upsert {
    pub(crate) fn apply(self, thread: &mut AcpThread, cx: &mut Context<AcpThread>) -> Result<()> {
        match self {
            Upsert::LocalUser(id, content) => thread.upsert_local_user_message(id, content, cx),
            Upsert::Assistant(message) => thread.upsert_assistant_message(message, cx),
            Upsert::Thought(thought) => thread.upsert_thought(thought, cx),
            Upsert::Tool(tool_call) => thread.upsert_wire_tool_call(tool_call, cx),
            Upsert::Terminal(terminal_id, patch) => {
                thread.upsert_display_terminal(terminal_id, patch, cx)
            }
            Upsert::Plan(plan) => {
                thread.upsert_plan_items(plan, None, cx);
                Ok(())
            }
            Upsert::Compaction(update) => {
                thread.upsert_context_compaction_update(update, cx);
                Ok(())
            }
        }
    }
}

pub(crate) struct TranslateContext<'a> {
    /// Tool calls waiting for the user's approval. Their status is left
    /// alone so translating again doesn't clear the approval prompt.
    pub tool_call_ids_awaiting_permission: &'a HashSet<String>,
    /// Of every session, so a background subagent's card keeps spinning
    /// after its tool call returns.
    pub session_activity: &'a HashMap<String, Activity>,
    pub shell_terminals: &'a ShellTerminals,
    pub skills: &'a [Skill],
    /// The session's directory, which relative paths in tool results are
    /// relative to.
    pub directory: &'a Path,
}

/// Renders one item of an assistant message's reply, for a delta that
/// changed only that item.
pub(crate) fn translate_reply(
    message: &Message,
    content_id: &ContentId,
    context: &TranslateContext,
) -> Vec<Upsert> {
    let Some(assistant) = message.as_assistant() else {
        return Vec::new();
    };
    enumerate_reply_entries(assistant)
        .find(|(candidate, _)| candidate == content_id)
        .map(|(content_id, item)| translate_content(&message.id, &content_id, item, context))
        .unwrap_or_default()
}

/// Each item of the reply with its ID. Single items and whole messages are
/// numbered here, so both name the same entries.
fn enumerate_reply_entries(
    assistant: &AssistantMessage,
) -> impl Iterator<Item = (ContentId, &AssistantContent)> {
    let (mut texts, mut reasonings) = (0, 0);
    assistant.content.iter().filter_map(move |item| {
        let content_id = match item {
            AssistantContent::Text { .. } => {
                texts += 1;
                ContentId::Text { ordinal: texts - 1 }
            }
            AssistantContent::Reasoning { .. } => {
                reasonings += 1;
                ContentId::Reasoning {
                    ordinal: reasonings - 1,
                }
            }
            AssistantContent::Tool(tool) => ContentId::Tool {
                id: tool.id.clone(),
            },
            AssistantContent::Other => return None,
        };
        Some((content_id, item))
    })
}

fn translate_content(
    message_id: &str,
    content_id: &ContentId,
    item: &AssistantContent,
    context: &TranslateContext,
) -> Vec<Upsert> {
    match item {
        AssistantContent::Text { text } if !text.is_empty() => {
            vec![Upsert::Assistant(
                acp::AgentMessage::new(EntryKey::Content(content_id).to_entry_id(message_id))
                    .content(vec![text_block(text)]),
            )]
        }
        AssistantContent::Reasoning { text, .. } if !text.is_empty() => {
            vec![Upsert::Thought(
                acp::AgentThought::new(EntryKey::Content(content_id).to_entry_id(message_id))
                    .content(vec![text_block(text)]),
            )]
        }
        AssistantContent::Tool(tool) => upsert_terminal(tool, context)
            .into_iter()
            .chain(upsert_plan(tool))
            .chain([Upsert::Tool(translate_tool_call(tool, context))])
            .collect(),
        AssistantContent::Text { .. }
        | AssistantContent::Reasoning { .. }
        | AssistantContent::Other => Vec::new(),
    }
}

pub(crate) fn translate_message(
    message: &Message,
    context: &TranslateContext,
) -> Result<Vec<Upsert>> {
    let upserts = match &message.body {
        MessageBody::User(user) => {
            let content = match crate::prompt::decode_display_metadata(user.metadata.as_ref()) {
                Some(mut content) => {
                    content.extend(user.files.iter().filter_map(embed_image));
                    content
                }
                None => rebuild_user_message(user, context.skills),
            };
            vec![Upsert::LocalUser(
                to_client_message_id(&message.id)?,
                content,
            )]
        }
        // Server-injected input (loaded instructions, subagent results).
        // Shown as a notice in the agent's reply, like the TUI does, not as
        // a user message.
        MessageBody::Synthetic(synthetic) => write_notice(synthetic)
            .map(|notice| upsert_notice(&message.id, &notice))
            .into_iter()
            .collect(),
        MessageBody::Assistant(assistant) => {
            let mut upserts: Vec<Upsert> = enumerate_reply_entries(assistant)
                .flat_map(|(content_id, item)| {
                    translate_content(&message.id, &content_id, item, context)
                })
                .collect();
            let error_id = EntryKey::Error.to_entry_id(&message.id);
            if let Some(interruption) = TurnInterruption::detect(assistant) {
                upserts.push(upsert_notice(&error_id, interruption.text()));
            } else if let Some(error) = &assistant.error {
                upserts.push(Upsert::Assistant(acp::AgentMessage::new(error_id).content(
                    vec![text_block(&format!(
                        "Error: {}",
                        extract_error_message(error)
                    ))],
                )));
            }
            upserts
        }
        MessageBody::Idle(idle) if idle.outcome == IdleOutcome::Failed => {
            let error_id = EntryKey::Error.to_entry_id(&message.id);
            let error = idle
                .error
                .as_ref()
                .map(extract_error_message)
                .unwrap_or_default();
            vec![Upsert::Assistant(acp::AgentMessage::new(error_id).content(
                vec![text_block(&format!("The agent stopped: {error}"))],
            ))]
        }
        MessageBody::Idle(idle) if idle.outcome == IdleOutcome::Interrupted => {
            vec![upsert_notice(
                &EntryKey::Error.to_entry_id(&message.id),
                "Stopped",
            )]
        }
        MessageBody::Compaction(compaction) => {
            let status = match compaction.status {
                CompactionStatus::Running => acp::CompactionStatus::InProgress,
                CompactionStatus::Completed => acp::CompactionStatus::Completed,
                CompactionStatus::Failed | CompactionStatus::Other => acp::CompactionStatus::Failed,
            };
            let mut update = acp::CompactionUpdate::new(message.id.as_str(), status);
            if !compaction.summary.is_empty() {
                update = update.summary(vec![text_block(&compaction.summary)]);
            }
            if let Some(error) = &compaction.error {
                update = update.error(extract_error_message(error));
            }
            vec![Upsert::Compaction(update)]
        }
        MessageBody::Shell(shell) => {
            // Shown like the agent's shell calls: Zed's command card shows
            // output only from a terminal.
            let terminal_id = acp::TerminalId::new(EntryKey::Terminal.to_entry_id(&message.id));
            let command = DisplayedCommand {
                command: Some(&shell.command),
                cwd: None,
                // The stored output is a page of the command's output.
                finished: shell.output.as_ref().map(|output| FinishedCommand {
                    output: Some(&output.output),
                    exit_code: shell.exit,
                }),
            };
            vec![
                command.upsert(terminal_id.clone()),
                Upsert::Tool(
                    acp::ToolCallUpdate::new(message.id.as_str())
                        .name("shell")
                        .title(shell.command.clone())
                        .kind(acp::ToolKind::Execute)
                        .status(match shell.status {
                            Some(ShellStatus::Running) => acp::ToolCallStatus::InProgress,
                            Some(ShellStatus::Exited)
                                if shell.exit.is_none_or(|exit| exit == 0) =>
                            {
                                acp::ToolCallStatus::Completed
                            }
                            // A non-zero exit, a timeout, or a killed command.
                            _ => acp::ToolCallStatus::Failed,
                        })
                        .content(vec![acp::ToolCallContent::Terminal(acp::Terminal::new(
                            terminal_id.0.to_string(),
                        ))]),
                ),
            ]
        }
        MessageBody::Idle(_)
        | MessageBody::System(_)
        | MessageBody::AgentSwitched { .. }
        | MessageBody::ModelSwitched { .. }
        | MessageBody::Other { .. } => Vec::new(),
    };
    Ok(upserts)
}

pub(crate) fn to_client_message_id(message_id: &str) -> Result<ClientUserMessageId> {
    serde_json::from_value(Value::String(message_id.to_string()))
        .context("building a client message ID")
}

/// `None` for a reminder addressed only to the model, such as the plan mode
/// instructions. The notices shown for instructions and finished work carry
/// a description or a source, so they aren't these.
fn write_notice(synthetic: &TextMessage) -> Option<String> {
    let metadata = synthetic.metadata.as_ref();
    let has_field = |name: &str| metadata.is_some_and(|metadata| metadata.get(name).is_some());
    let field = |name: &str| {
        metadata
            .and_then(|metadata| metadata.get(name))
            .and_then(Value::as_str)
    };
    let description = synthetic.description.as_deref().unwrap_or_default();
    if synthetic.text.trim_start().starts_with("<system-reminder>")
        && description.is_empty()
        && !has_field("source")
        && !has_field("instruction")
    {
        return None;
    }
    let notice = if field("source") == Some("shell") {
        let state = field("state").unwrap_or("finished");
        format!("↳ Background command {state} · {description}")
    } else if field("source") == Some("subagent") {
        let agent = field("agent").unwrap_or("Subagent");
        let state = field("state").unwrap_or("finished");
        format!("↳ {agent} {state} · {description}")
    } else if has_field("instruction") {
        format!("Instructions: {description}")
    } else if !description.is_empty() {
        description.to_string()
    } else {
        synthetic
            .text
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    };
    Some(notice.trim().to_string())
}

fn upsert_notice(entry_id: &str, text: &str) -> Upsert {
    Upsert::Assistant(
        acp::AgentMessage::new(entry_id).content(vec![text_block(&format!("*{text}*"))]),
    )
}

/// Which entry shows a part of a message. Translating the message again
/// names the same entries, so they're updated in place.
pub(crate) enum EntryKey<'a> {
    Content(&'a ContentId),
    /// The error or notice that ends the message.
    Error,
    /// The terminal showing a shell command's output.
    Terminal,
}

impl EntryKey<'_> {
    pub(crate) fn to_entry_id(&self, message_id: &str) -> String {
        match self {
            Self::Content(ContentId::Text { ordinal }) => format!("{message_id}/t{ordinal}"),
            Self::Content(ContentId::Reasoning { ordinal }) => format!("{message_id}/r{ordinal}"),
            Self::Content(ContentId::Tool { id }) => id.clone(),
            Self::Error => format!("{message_id}/error"),
            Self::Terminal => format!("{message_id}/terminal"),
        }
    }
}

enum TurnInterruption {
    Stopped,
    Declined,
}

impl TurnInterruption {
    fn detect(assistant: &AssistantMessage) -> Option<Self> {
        let StepError::Aborted {
            message: STEP_INTERRUPTED,
        } = StepError::deserialize(assistant.error.as_ref()?).ok()?
        else {
            return None;
        };
        let declined = assistant.content.iter().any(|item| {
            let AssistantContent::Tool(ToolCall {
                state: ToolState::Error { error, .. },
                ..
            }) = item
            else {
                return false;
            };
            matches!(
                StepError::deserialize(error),
                Ok(StepError::Aborted {
                    message: "The user declined this tool call"
                        | "The user dismissed this question",
                })
            )
        });
        Some(if declined {
            Self::Declined
        } else {
            Self::Stopped
        })
    }

    fn text(&self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::Declined => "You declined, so the agent stopped.",
        }
    }
}

const SUBAGENT_CANCELED_OUTPUT: &str = "User canceled";

pub(crate) fn translate_tool_call(
    tool: &ToolCall,
    context: &TranslateContext,
) -> acp::ToolCallUpdate {
    let family = ToolFamily::from_tool_name(&tool.name);
    let empty = Value::Null;
    let (input, mut status, output, metadata) = match &tool.state {
        ToolState::Streaming { input } => (
            serde_json::from_str(input).unwrap_or(Value::Null),
            acp::ToolCallStatus::Pending,
            None,
            &empty,
        ),
        ToolState::Other => (Value::Null, acp::ToolCallStatus::Pending, None, &empty),
        ToolState::Running { input, metadata } => (
            input.clone(),
            acp::ToolCallStatus::InProgress,
            None,
            metadata,
        ),
        ToolState::Completed {
            input,
            content,
            metadata,
        } => (
            input.clone(),
            acp::ToolCallStatus::Completed,
            Some(join_output_text(content)),
            metadata,
        ),
        ToolState::Error { input, error } => (
            input.clone(),
            acp::ToolCallStatus::Failed,
            Some(extract_error_message(error)),
            &empty,
        ),
    };
    let mut update = acp::ToolCallUpdate::new(tool.id.as_str())
        .name(tool.name.clone())
        .title(summarize_tool_call(&tool.name, &input))
        .kind(family.to_acp_kind())
        .raw_input(input.clone())
        .locations(locate_tool_call(
            &tool.name,
            &input,
            output.as_deref(),
            context.directory,
        ));

    let mut content = Vec::new();
    if continues_subagent(&tool.name, &input) {
        // Sends more work to a subagent another call already shows, so it
        // isn't a new card, like the TUI's "Continue subagent" line.
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        update = update
            .title(format!("Continue subagent: {description}"))
            .kind(acp::ToolKind::Other);
    } else if let Some(child_id) = find_subagent_session_id(&tool.name, &input, metadata) {
        // A background subagent's call returns at once, so the card follows
        // the subagent session itself.
        if status == acp::ToolCallStatus::Completed {
            match context.session_activity.get(child_id) {
                Some(Activity::Running) => status = acp::ToolCallStatus::InProgress,
                // What a stopped native subagent reports, which Zed's card
                // shows as cancelled.
                Some(Activity::Interrupted) => content.push(tool_text(SUBAGENT_CANCELED_OUTPUT)),
                Some(Activity::Idle) | None => {}
            }
        }
        update = update.meta(build_subagent_meta(child_id));
    } else if family == ToolFamily::Shell {
        if let Some(shell_id) = background_shell_id(tool) {
            status = match context.shell_terminals.outcome(shell_id) {
                None => acp::ToolCallStatus::InProgress,
                Some(outcome) if outcome.completed => acp::ToolCallStatus::Completed,
                Some(_) => acp::ToolCallStatus::Failed,
            };
        }
        content.push(acp::ToolCallContent::Terminal(acp::Terminal::new(
            terminal_id(tool).0.to_string(),
        )));
        // A streamed terminal keeps its output, so the error goes below it.
        if let ToolState::Error { error, .. } = &tool.state
            && context.shell_terminals.has_streamed(&tool.id)
        {
            content.push(tool_text(&extract_error_message(error)));
        }
    } else if let Some(diff) = build_file_diff(metadata, context.directory) {
        content.push(acp::ToolCallContent::Diff(diff));
    } else if let Some(output) = output {
        // A stopped foreground subagent's call fails with this error.
        if family == ToolFamily::Subagent && output.starts_with("Subagent cancelled") {
            content.push(tool_text(SUBAGENT_CANCELED_OUTPUT));
        } else {
            content.push(tool_text(&output));
        }
    }
    if !content.is_empty() {
        update = update.content(content);
    }
    if !context.tool_call_ids_awaiting_permission.contains(&tool.id) {
        update = update.status(status);
    }
    update
}

fn continues_subagent(tool_name: &str, input: &Value) -> bool {
    ToolFamily::from_tool_name(tool_name) == ToolFamily::Subagent
        && input.get("sessionID").and_then(Value::as_str).is_some()
}

/// The child session a subagent tool call starts, once the server reports
/// it. Calls that continue an existing subagent don't count.
pub(crate) fn find_subagent_session_id<'a>(
    tool_name: &str,
    input: &Value,
    metadata: &'a Value,
) -> Option<&'a str> {
    if ToolFamily::from_tool_name(tool_name) != ToolFamily::Subagent
        || continues_subagent(tool_name, input)
    {
        return None;
    }
    metadata.get("sessionID").and_then(Value::as_str)
}

fn build_subagent_meta(child_id: &str) -> acp::Meta {
    let mut meta = acp::Meta::new();
    meta.insert(
        SUBAGENT_SESSION_INFO_META_KEY.into(),
        serde_json::json!({ "session_id": child_id, "message_start_index": 0 }),
    );
    meta
}

/// What a tool does, from its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolFamily {
    Shell,
    Subagent,
    Edit,
    Todo,
    Read,
    Search,
    Fetch,
    Other,
}

impl ToolFamily {
    pub(crate) fn from_tool_name(tool_name: &str) -> Self {
        match tool_name {
            "shell" | "bash" => Self::Shell,
            "subagent" | "task" => Self::Subagent,
            "edit" | "write" | "patch" | "multiedit" | "apply_patch" => Self::Edit,
            "todowrite" => Self::Todo,
            "read" | "list" | "ls" => Self::Read,
            "grep" | "glob" | "find" | "codesearch" => Self::Search,
            "webfetch" | "websearch" => Self::Fetch,
            _ => Self::Other,
        }
    }

    fn to_acp_kind(self) -> acp::ToolKind {
        match self {
            Self::Read => acp::ToolKind::Read,
            Self::Edit => acp::ToolKind::Edit,
            Self::Shell => acp::ToolKind::Execute,
            Self::Search => acp::ToolKind::Search,
            Self::Fetch => acp::ToolKind::Fetch,
            Self::Subagent | Self::Todo => acp::ToolKind::Think,
            Self::Other => acp::ToolKind::Other,
        }
    }
}

/// OpenCode runs shell commands itself, so Zed shows them in a display-only
/// terminal: the command, its output once it finishes, and the exit code.
fn upsert_terminal(tool: &ToolCall, context: &TranslateContext) -> Option<Upsert> {
    if ToolFamily::from_tool_name(&tool.name) != ToolFamily::Shell {
        return None;
    }
    // A background command's call returns at once; the terminal stays
    // running until its completion notice arrives.
    let background = background_shell_id(tool)
        .map(|shell_id| context.shell_terminals.outcome(shell_id).cloned());
    let (input, output, exit_code) = match &tool.state {
        ToolState::Completed { input, .. } if background.is_some() => match background.flatten() {
            Some(outcome) => (input, Some(outcome.output), outcome.exit),
            None => (input, None, None),
        },
        ToolState::Streaming { .. } | ToolState::Other => return None,
        ToolState::Running { input, .. } => (input, None, None),
        ToolState::Completed {
            input,
            content,
            metadata,
        } => (
            input,
            Some(join_output_text(content)),
            metadata.get("exit").and_then(Value::as_i64),
        ),
        ToolState::Error { input, error } => (input, Some(extract_error_message(error)), None),
    };
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let command = DisplayedCommand {
        command: field("command").or(field("cmd")),
        cwd: field("workdir")
            .or(field("cwd"))
            .filter(|cwd| cwd.starts_with('/')),
        finished: output.as_deref().map(|output| FinishedCommand {
            // Output streamed into the terminal is already there.
            output: (!context.shell_terminals.has_streamed(&tool.id)).then_some(output),
            exit_code,
        }),
    };
    Some(command.upsert(terminal_id(tool)))
}

/// A shell command shown in a display-only terminal.
struct DisplayedCommand<'a> {
    command: Option<&'a str>,
    cwd: Option<&'a str>,
    finished: Option<FinishedCommand<'a>>,
}

struct FinishedCommand<'a> {
    /// Unset to leave the terminal's output as it is.
    output: Option<&'a str>,
    exit_code: Option<i64>,
}

impl DisplayedCommand<'_> {
    fn upsert(self, terminal_id: acp::TerminalId) -> Upsert {
        let mut patch = DisplayTerminalPatch::default();
        if let Some(command) = self.command {
            patch.command = MaybeUndefined::Value(command.to_string());
        }
        if let Some(cwd) = self.cwd {
            patch.cwd = MaybeUndefined::Value(PathBuf::from(cwd).into());
        }
        if let Some(finished) = self.finished {
            if let Some(output) = finished.output {
                patch.output = MaybeUndefined::Value(DisplayTerminalOutput {
                    data: terminal_bytes(output),
                    meta: None,
                });
            }
            patch.exit_status =
                MaybeUndefined::Value(acp::TerminalExitStatus::new().exit_code(finished.exit_code));
        }
        Upsert::Terminal(terminal_id, patch)
    }
}

/// Each call sends the whole list, so the latest call replaces it.
fn upsert_plan(tool: &ToolCall) -> Option<Upsert> {
    if tool.name != "todowrite" {
        return None;
    }
    let input = match &tool.state {
        ToolState::Running { input, .. } | ToolState::Completed { input, .. } => input,
        _ => return None,
    };
    let entries = input
        .get("todos")?
        .as_array()?
        .iter()
        .filter_map(|todo| {
            let content = todo.get("content").and_then(Value::as_str)?;
            let status = match todo.get("status").and_then(Value::as_str) {
                Some("in_progress") => acp::PlanEntryStatus::InProgress,
                Some("completed") => acp::PlanEntryStatus::Completed,
                Some("cancelled") => acp::PlanEntryStatus::Cancelled,
                _ => acp::PlanEntryStatus::Pending,
            };
            Some(acp::PlanEntry::new(
                content,
                acp::PlanEntryPriority::Medium,
                status,
            ))
        })
        .collect();
    Some(Upsert::Plan(acp::PlanItems::new("todo", entries)))
}

/// One file an edit tool changed, as its metadata reports it.
pub(crate) struct ChangedFile<'a> {
    pub path: String,
    pub kind: ExternalEditKind,
    pub patch: &'a str,
}

/// The files a finished edit tool changed. Files whose path can't be told
/// are left out.
pub(crate) fn parse_changed_files<'a>(
    metadata: &'a Value,
    directory: &Path,
) -> Vec<ChangedFile<'a>> {
    let Some(files) = metadata.get("files").and_then(Value::as_array) else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(|file| {
            let patch = file
                .get("patch")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let kind = match file.get("status").and_then(Value::as_str) {
                Some("added") => ExternalEditKind::Created,
                Some("deleted") => ExternalEditKind::Deleted,
                _ => ExternalEditKind::Modified,
            };
            Some(ChangedFile {
                path: resolve_changed_file_path(file, patch, directory)?,
                kind,
                patch,
            })
        })
        .collect()
}

/// Both the patch header and `file` follow the path the model passed, so
/// either can be relative to the session's directory.
fn resolve_changed_file_path(file: &Value, patch: &str, directory: &Path) -> Option<String> {
    let header_path = find_patch_header_path(patch);
    let file_path = file.get("file").and_then(Value::as_str);
    header_path
        .into_iter()
        .chain(file_path)
        .find(|path| path.starts_with('/'))
        .map(str::to_string)
        .or_else(|| {
            let relative = header_path.or(file_path)?;
            let joined = directory.join(relative);
            joined
                .is_absolute()
                .then(|| joined.to_string_lossy().into_owned())
        })
}

fn find_patch_header_path(patch: &str) -> Option<&str> {
    patch.lines().find_map(|line| {
        line.strip_prefix("Index: ")
            .or_else(|| line.strip_prefix("+++ "))
            .filter(|path| *path != "/dev/null")
    })
}

/// Edit tools report each changed file as a unified diff. Zed renders git
/// patches natively, so convert the headers and keep the hunks. A file with
/// no hunks (such as a new empty file) is still listed as a change.
fn build_file_diff(metadata: &Value, directory: &Path) -> Option<acp::Diff> {
    let mut text = String::new();
    let mut changes = Vec::new();
    for file in parse_changed_files(metadata, directory) {
        let path = file.path.as_str();
        let (old_path, new_path, change) = match file.kind {
            ExternalEditKind::Created => {
                ("/dev/null", path, acp::DiffChange::add(PathBuf::from(path)))
            }
            ExternalEditKind::Deleted => (
                path,
                "/dev/null",
                acp::DiffChange::delete(PathBuf::from(path)),
            ),
            ExternalEditKind::Modified => {
                (path, path, acp::DiffChange::modify(PathBuf::from(path)))
            }
        };
        changes.push(change);
        let Some(hunks) = strip_patch_header(file.patch) else {
            continue;
        };
        text.push_str(&format!(
            "diff --git {path} {path}\n--- {old_path}\n+++ {new_path}\n"
        ));
        text.push_str(hunks);
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    if changes.is_empty() {
        return None;
    }
    Some(if text.is_empty() {
        acp::Diff::new(changes)
    } else {
        acp::Diff::patch(text, changes)
    })
}

pub(crate) fn strip_patch_header(patch: &str) -> Option<&str> {
    let start = patch
        .find("\n@@")
        .map(|ix| ix + 1)
        .or_else(|| patch.starts_with("@@").then_some(0))?;
    Some(&patch[start..])
}

/// The keyed errors and notices already shown in the same turn.
pub(crate) fn find_repeated_notices(messages: &[Message]) -> HashSet<String> {
    let mut repeated = HashSet::default();
    let mut last_step_error = None;
    let mut interruption_shown = false;
    for message in messages {
        match &message.body {
            // A new turn's failure isn't a repeat of the last turn's.
            MessageBody::User(_) => {
                last_step_error = None;
                interruption_shown = false;
            }
            MessageBody::Assistant(assistant) => {
                if TurnInterruption::detect(assistant).is_some() {
                    if interruption_shown {
                        repeated.insert(EntryKey::Error.to_entry_id(&message.id));
                    }
                    interruption_shown = true;
                }
                last_step_error = assistant.error.as_ref().map(extract_error_message);
            }
            MessageBody::Idle(idle) if idle.outcome == IdleOutcome::Failed => {
                if idle.error.as_ref().map(extract_error_message) == last_step_error
                    && last_step_error.is_some()
                {
                    repeated.insert(EntryKey::Error.to_entry_id(&message.id));
                }
                interruption_shown = false;
            }
            MessageBody::Idle(idle) if idle.outcome == IdleOutcome::Interrupted => {
                if interruption_shown {
                    repeated.insert(EntryKey::Error.to_entry_id(&message.id));
                }
                interruption_shown = false;
            }
            _ => {}
        }
    }
    repeated
}

fn summarize_tool_call(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    // The subagent card shows the title as its label.
    if ToolFamily::from_tool_name(name) == ToolFamily::Subagent
        && let Some(description) = field("description")
    {
        return description.to_string();
    }
    let detail = match name {
        "shell" | "bash" => field("command").or(field("description")),
        "grep" | "glob" | "find" => field("pattern"),
        "webfetch" => field("url"),
        "websearch" => field("query"),
        _ => field("filePath").or(field("path")),
    };
    match detail {
        Some(detail) => format!("{name}: {detail}"),
        None => name.to_string(),
    }
}

/// Models may pass a path relative to the session's directory. Directories
/// aren't locations: Zed would try to open them as files. The read tool also
/// reads directories, and then its output starts with "Read directory".
fn locate_tool_call(
    tool_name: &str,
    input: &Value,
    output: Option<&str>,
    directory: &Path,
) -> Vec<acp::ToolCallLocation> {
    let names_directory = matches!(tool_name, "list" | "ls" | "glob" | "grep")
        || output.is_some_and(|output| output.starts_with("Read directory "));
    if names_directory {
        return Vec::new();
    }
    ["filePath", "path"]
        .iter()
        .filter_map(|key| input.get(*key).and_then(Value::as_str))
        .map(|path| directory.join(path))
        // A folder can't be opened as a buffer, and the output may not say
        // it was one.
        .filter(|path| path.is_absolute() && !path.is_dir())
        .take(1)
        .map(acp::ToolCallLocation::new)
        .collect()
}

fn join_output_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_error_message(error: &Value) -> String {
    match error {
        Value::String(text) => text.clone(),
        Value::Object(object) => object
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()),
        other => other.to_string(),
    }
}

/// The error OpenCode records on a step the user stopped.
const STEP_INTERRUPTED: &str = "Step interrupted";

/// Rebuilds chips for a prompt Zed didn't send, from the mentions OpenCode's
/// own clients record. Like the TUI, a mention is found by its text: its
/// position is in the sending client's units (the TUI counts terminal
/// columns), so it only decides which copy of a repeated text is meant.
/// Attachments without a mention found in the text follow it.
fn rebuild_user_message(user: &UserMessage, skills: &[Skill]) -> Vec<acp::ContentBlock> {
    let mut attachments = Vec::new();
    for file in &user.files {
        if let Some(block) = embed_image(file).or_else(|| link_file(file)) {
            attachments.push(Attachment {
                mention: Mention::parse(file),
                block,
            });
        }
    }
    for skill in &user.skills {
        if let Some(block) = link_skill(skill, skills) {
            attachments.push(Attachment {
                mention: Mention::parse(skill),
                block,
            });
        }
    }
    place_mentions(&user.text, attachments)
}

struct Attachment {
    mention: Option<Mention>,
    block: acp::ContentBlock,
}

/// Where the sending client put an attachment in the prompt's text.
struct Mention {
    text: String,
    /// In the sending client's units.
    start: f64,
}

impl Mention {
    fn parse(attachment: &Value) -> Option<Self> {
        let mention = attachment.get("mention")?;
        let text = mention.get("text")?.as_str()?;
        if text.is_empty() {
            return None;
        }
        let start = mention.get("start").and_then(Value::as_f64).unwrap_or(0.0);
        Some(Self {
            text: text.to_string(),
            start,
        })
    }
}

/// OpenCode's clients give a line range as `?start=&end=` (1-based), which
/// Zed shows as a selection.
fn link_file(file: &Value) -> Option<acp::ContentBlock> {
    let uri = find_file_uri(file)?;
    let url = Url::parse(uri).ok().filter(|url| url.scheme() == "file")?;
    let mention = if url.fragment().is_some() {
        MentionUri::parse(uri, PathStyle::local()).ok()?
    } else {
        let abs_path = url.to_file_path().ok()?;
        let line = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.parse::<u32>().ok())
                .and_then(|line| line.checked_sub(1))
        };
        match line("start") {
            Some(start) => MentionUri::Selection {
                abs_path: Some(abs_path),
                line_range: start..=line("end").filter(|end| *end >= start).unwrap_or(start),
                column: None,
            },
            None if uri.ends_with('/')
                || file.get("mime").and_then(Value::as_str) == Some("application/x-directory") =>
            {
                MentionUri::Directory { abs_path }
            }
            None => MentionUri::File { abs_path },
        }
    };
    Some(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
        mention.name(),
        mention.to_uri().to_string(),
    )))
}

fn place_mentions(text: &str, attachments: Vec<Attachment>) -> Vec<acp::ContentBlock> {
    // Trailing chips keep the message's attachment order.
    let mut groups: HashMap<String, Vec<(usize, Mention, acp::ContentBlock)>> = HashMap::default();
    let mut trailing = Vec::new();
    for (order, attachment) in attachments.into_iter().enumerate() {
        match attachment.mention {
            Some(mention) => groups.entry(mention.text.clone()).or_default().push((
                order,
                mention,
                attachment.block,
            )),
            None => trailing.push((order, attachment.block)),
        }
    }

    // Longer texts claim their spots first, so `@a.ts#1-5` isn't split by
    // a mention of `@a.ts`.
    let mut mention_texts: Vec<String> = groups.keys().cloned().collect();
    mention_texts.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    let mut claimed: Vec<Range<usize>> = Vec::new();
    let mut placed: Vec<(Range<usize>, acp::ContentBlock)> = Vec::new();
    for mention_text in mention_texts {
        let mut items = groups.remove(&mention_text).unwrap_or_default();
        items.sort_by(|left, right| {
            left.1
                .start
                .total_cmp(&right.1.start)
                .then(left.0.cmp(&right.0))
        });
        let candidates: Vec<Range<usize>> = text
            .match_indices(mention_text.as_str())
            .map(|(start, matched)| start..start + matched.len())
            .collect();
        let available: Vec<Range<usize>> = candidates
            .iter()
            .filter(|candidate| {
                !claimed
                    .iter()
                    .any(|range| candidate.start < range.end && range.start < candidate.end)
            })
            .cloned()
            .collect();
        claimed.extend(candidates);

        // Keep the mentions in order, each at the copy nearest its recorded
        // position, leaving enough copies for the ones after it.
        let placeable = items.len().min(available.len());
        let mut next = 0;
        for (index, (order, mention, block)) in items.into_iter().enumerate() {
            if index >= placeable {
                trailing.push((order, block));
                continue;
            }
            let last = available.len() - (placeable - index);
            let nearest = (next..=last)
                .min_by(|left, right| {
                    let distance = |candidate: usize| {
                        let offset = text[..available[candidate].start].chars().count();
                        (offset as f64 - mention.start).abs()
                    };
                    distance(*left).total_cmp(&distance(*right))
                })
                .unwrap_or(next);
            placed.push((available[nearest].clone(), block));
            next = nearest + 1;
        }
    }

    placed.sort_by_key(|(range, _)| range.start);
    let mut content = Vec::new();
    let mut cursor = 0;
    for (range, block) in placed {
        if range.start > cursor {
            content.push(text_block(&text[cursor..range.start]));
        }
        content.push(block);
        cursor = range.end;
    }
    if cursor < text.len() || content.is_empty() {
        content.push(text_block(&text[cursor..]));
    }
    trailing.sort_by_key(|(order, _)| *order);
    content.extend(trailing.into_iter().map(|(_, block)| block));
    content
}

fn link_skill(attachment: &Value, skills: &[Skill]) -> Option<acp::ContentBlock> {
    let id = attachment.get("id")?.as_str()?;
    let skill = skills.iter().find(|skill| skill.id == id)?;
    let uri = MentionUri::Skill {
        name: skill.id.clone(),
        source: skill.scope.clone(),
        skill_file_path: skill.path.clone(),
    };
    Some(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
        uri.name(),
        uri.to_uri().to_string(),
    )))
}

/// Stored files carry `data` and `mime`; files still in the inbox carry the
/// `data:` URI Zed sent.
fn embed_image(file: &Value) -> Option<acp::ContentBlock> {
    let (mime_type, data) = match (
        file.get("mime").and_then(Value::as_str),
        file.get("data").and_then(Value::as_str),
    ) {
        (Some(mime_type), Some(data)) => (mime_type, data),
        _ => find_file_uri(file)?
            .strip_prefix("data:")?
            .split_once(";base64,")?,
    };
    if !mime_type.starts_with("image/") || data.is_empty() {
        return None;
    }
    Some(acp::ContentBlock::Image(acp::ImageContent::new(
        data, mime_type,
    )))
}

fn find_file_uri(file: &Value) -> Option<&str> {
    file.get("uri")
        .or_else(|| file.get("source")?.get("uri"))
        .and_then(Value::as_str)
}

fn text_block(text: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text))
}

fn tool_text(text: &str) -> acp::ToolCallContent {
    acp::ToolCallContent::Content(Box::new(acp::Content::new(text_block(text))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(value: Value) -> UserMessage {
        serde_json::from_value(value).unwrap()
    }

    fn link(uri: MentionUri) -> acp::ContentBlock {
        acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
            uri.name(),
            uri.to_uri().to_string(),
        ))
    }

    fn file(path: &str) -> MentionUri {
        MentionUri::File {
            abs_path: PathBuf::from(path),
        }
    }

    fn skill(id: &str) -> Skill {
        Skill {
            id: id.into(),
            description: String::new(),
            path: PathBuf::from(format!("/skills/{id}/SKILL.md")),
            content: String::new(),
            scope: "global".into(),
        }
    }

    #[test]
    fn translating_a_part_leaves_the_rest_alone() {
        let message = Message::from_value(json!({
            "id": "msg_1",
            "type": "assistant",
            "agent": "build",
            "content": [
                { "type": "reasoning", "text": "Thinking" },
                { "type": "text", "text": "First" },
                { "type": "tool", "id": "call_1", "name": "read",
                  "state": { "status": "running", "input": {}, "metadata": {} } },
                { "type": "text", "text": "Second" },
            ],
            "time": { "created": 1 },
        }))
        .expect("message");
        let context = TranslateContext {
            tool_call_ids_awaiting_permission: &HashSet::default(),
            session_activity: &HashMap::default(),
            shell_terminals: &ShellTerminals::default(),
            skills: &[],
            directory: Path::new("/project"),
        };
        let upserts = translate_reply(&message, &ContentId::Text { ordinal: 1 }, &context);
        let [Upsert::Assistant(text)] = upserts.as_slice() else {
            panic!("one text upsert, got {}", upserts.len());
        };
        assert_eq!(text.message_id.0.as_ref(), "msg_1/t1");
        let upserts = translate_reply(
            &message,
            &ContentId::Tool {
                id: "call_1".into(),
            },
            &context,
        );
        assert!(matches!(upserts.as_slice(), [Upsert::Tool(_)]));
        assert_eq!(
            translate_message(&message, &context)
                .expect("upserts")
                .len(),
            4
        );
    }

    #[test]
    fn tui_mentions_become_chips_where_they_were_typed() {
        let skills = [skill("browser")];
        let message = user(json!({
            "text": "@browser wow",
            "skills": [{ "id": "browser", "name": "Browser", "mention": { "start": 0, "end": 8, "text": "@browser" } }],
        }));
        assert_eq!(
            rebuild_user_message(&message, &skills),
            vec![
                link(MentionUri::Skill {
                    name: "browser".into(),
                    source: "global".into(),
                    skill_file_path: PathBuf::from("/skills/browser/SKILL.md"),
                }),
                text_block(" wow"),
            ]
        );

        let image = |text: &str, start: u32| {
            json!({ "data": "aGk=", "mime": "image/png", "source": { "type": "inline" }, "name": "clipboard",
                    "mention": { "start": start, "end": start + 9, "text": text } })
        };
        let message = user(json!({
            "text": "before [Image 1] middle [Image 2]",
            "files": [image("[Image 2]", 24), image("[Image 1]", 7)],
        }));
        let picture = acp::ContentBlock::Image(acp::ImageContent::new("aGk=", "image/png"));
        assert_eq!(
            rebuild_user_message(&message, &[]),
            vec![
                text_block("before "),
                picture.clone(),
                text_block(" middle "),
                picture,
            ]
        );
    }

    #[test]
    fn a_longer_mention_is_not_split_by_a_shorter_one() {
        let message = user(json!({
            "text": "compare @src/a.rs#10-20 with @src/a.rs",
            "files": [
                { "data": "", "mime": "text/plain", "source": { "type": "uri", "uri": "file:///repo/src/a.rs" },
                  "mention": { "start": 29, "end": 38, "text": "@src/a.rs" } },
                { "data": "", "mime": "text/plain", "source": { "type": "uri", "uri": "file:///repo/src/a.rs?start=10&end=20" },
                  "mention": { "start": 8, "end": 23, "text": "@src/a.rs#10-20" } },
            ],
        }));
        assert_eq!(
            rebuild_user_message(&message, &[]),
            vec![
                text_block("compare "),
                link(MentionUri::Selection {
                    abs_path: Some(PathBuf::from("/repo/src/a.rs")),
                    line_range: 9..=19,
                    column: None,
                }),
                text_block(" with "),
                link(file("/repo/src/a.rs")),
            ]
        );
    }

    #[test]
    fn a_repeated_mention_goes_to_the_copy_nearest_its_position() {
        // The TUI counts terminal columns, so the wide characters put the
        // recorded position past the character count; it's still nearest.
        let message = user(json!({
            "text": "I mean @a.ts, the 文件 @a.ts",
            "files": [{ "data": "", "mime": "text/plain", "source": { "type": "uri", "uri": "file:///a.ts" },
                        "mention": { "start": 23, "end": 28, "text": "@a.ts" } }],
        }));
        assert_eq!(
            rebuild_user_message(&message, &[]),
            vec![text_block("I mean @a.ts, the 文件 "), link(file("/a.ts")),]
        );
    }

    #[test]
    fn unmatched_attachments_follow_the_text() {
        let message = user(json!({
            "text": "look at this and @unknown-skill",
            "files": [
                { "data": "", "mime": "text/plain", "source": { "type": "uri", "uri": "file:///notes/a%20b.md" },
                  "mention": { "start": 0, "end": 8, "text": "@a b.md" } },
                { "data": "", "mime": "application/x-directory", "source": { "type": "uri", "uri": "file:///notes" } },
            ],
            "skills": [{ "id": "unknown-skill", "name": "unknown-skill",
                         "mention": { "start": 17, "end": 31, "text": "@unknown-skill" } }],
        }));
        assert_eq!(
            rebuild_user_message(&message, &[]),
            vec![
                text_block("look at this and @unknown-skill"),
                link(file("/notes/a b.md")),
                link(MentionUri::Directory {
                    abs_path: PathBuf::from("/notes"),
                }),
            ]
        );
    }

    fn shell_call(state: Value) -> ToolCall {
        serde_json::from_value(json!({ "id": "call_1", "name": "shell", "state": state })).unwrap()
    }

    #[test]
    fn a_background_subagent_card_follows_its_session() {
        let call: ToolCall = serde_json::from_value(json!({
            "id": "call_1", "name": "subagent",
            "state": {
                "status": "completed", "input": { "description": "Sleep", "background": true },
                "content": [], "metadata": { "sessionID": "ses_child" },
            },
        }))
        .expect("tool call");
        let update = |child: Option<Activity>| {
            let (permissions, shells) = Default::default();
            let activity: HashMap<String, Activity> = child
                .map(|child| ("ses_child".to_string(), child))
                .into_iter()
                .collect();
            let context = TranslateContext {
                tool_call_ids_awaiting_permission: &permissions,
                session_activity: &activity,
                shell_terminals: &shells,
                skills: &[],
                directory: Path::new("/project"),
            };
            translate_tool_call(&call, &context)
        };
        let status = |update: acp::ToolCallUpdate| update.status.as_opt_ref().flatten().cloned();

        assert_eq!(
            status(update(Some(Activity::Running))),
            Some(acp::ToolCallStatus::InProgress)
        );
        assert_eq!(
            status(update(Some(Activity::Idle))),
            Some(acp::ToolCallStatus::Completed)
        );
        let stopped = update(Some(Activity::Interrupted));
        assert_eq!(
            stopped.content.as_opt_ref().flatten(),
            Some(&vec![tool_text(SUBAGENT_CANCELED_OUTPUT)])
        );
        assert_eq!(status(stopped), Some(acp::ToolCallStatus::Completed));

        let foreground: ToolCall = serde_json::from_value(json!({
            "id": "call_2", "name": "subagent",
            "state": {
                "status": "error", "input": { "description": "Sleep" },
                "error": { "message": "Subagent cancelled (sessionID: ses_child)" },
            },
        }))
        .expect("tool call");
        let (permissions, shells) = Default::default();
        let context = TranslateContext {
            tool_call_ids_awaiting_permission: &permissions,
            session_activity: &HashMap::default(),
            shell_terminals: &shells,
            skills: &[],
            directory: Path::new("/project"),
        };
        assert_eq!(
            translate_tool_call(&foreground, &context)
                .content
                .as_opt_ref()
                .flatten(),
            Some(&vec![tool_text(SUBAGENT_CANCELED_OUTPUT)])
        );
    }

    #[test]
    fn a_streamed_shell_keeps_its_output_when_it_ends() {
        let failed = shell_call(json!({
            "status": "error", "input": { "command": "make" }, "error": { "message": "Command cancelled" },
        }));
        let streamed = ShellTerminals::with_streamed("call_1");
        let context = TranslateContext {
            tool_call_ids_awaiting_permission: &HashSet::default(),
            session_activity: &HashMap::default(),
            shell_terminals: &streamed,
            skills: &[],
            directory: Path::new("/project"),
        };
        let Some(Upsert::Terminal(_, patch)) = upsert_terminal(&failed, &context) else {
            panic!("expected a terminal patch");
        };
        assert!(patch.output.is_undefined());
        assert!(!patch.exit_status.is_undefined());
        let content_len = |update: acp::ToolCallUpdate| match update.content {
            MaybeUndefined::Value(content) => content.len(),
            _ => 0,
        };
        assert_eq!(
            content_len(translate_tool_call(&failed, &context)),
            2,
            "the terminal, then the error under it"
        );

        let not_streamed = ShellTerminals::default();
        let context = TranslateContext {
            shell_terminals: &not_streamed,
            ..context
        };
        let Some(Upsert::Terminal(_, patch)) = upsert_terminal(&failed, &context) else {
            panic!("expected a terminal patch");
        };
        assert!(!patch.output.is_undefined());
        assert_eq!(content_len(translate_tool_call(&failed, &context)), 1);
    }

    #[test]
    fn user_messages_keep_the_chips_zed_sent() {
        let skill_uri = MentionUri::Skill {
            name: "deep-research".into(),
            source: "global".into(),
            skill_file_path: PathBuf::from("/skills/deep-research/SKILL.md"),
        }
        .to_uri()
        .to_string();
        let sent = vec![
            text_block("use "),
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new("skill file content", skill_uri.clone()),
                ),
            )),
            acp::ContentBlock::Image(acp::ImageContent::new("aGk=", "image/png")),
        ];
        // The stored message, as the server returns it.
        let message = Message::from_value(json!({
            "id": "msg_0123456789abABCDEFGHIJKLMN",
            "type": "user",
            "text": "use @deep-research",
            "files": [{
                "data": "aGk=",
                "mime": "image/png",
                "source": { "type": "uri", "uri": "data:image/png;base64,aGk=" },
                "name": "image",
            }],
            "metadata": crate::prompt::encode_display_metadata(&sent),
        }))
        .unwrap();
        let context = TranslateContext {
            tool_call_ids_awaiting_permission: &HashSet::default(),
            session_activity: &HashMap::default(),
            shell_terminals: &ShellTerminals::default(),
            skills: &[],
            directory: Path::new("/project"),
        };
        let upserts = translate_message(&message, &context).unwrap();
        let [Upsert::LocalUser(_, content)] = upserts.as_slice() else {
            panic!("expected one user message, got {} upserts", upserts.len());
        };
        assert_eq!(
            content,
            &vec![
                text_block("use "),
                acp::ContentBlock::ResourceLink(
                    acp::ResourceLink::new("deep-research", skill_uri,)
                ),
                acp::ContentBlock::Image(acp::ImageContent::new("aGk=", "image/png")),
            ]
        );
    }

    #[test]
    fn converts_edit_patches_to_git_patches() {
        let metadata = json!({
            "files": [
                {
                    "file": "/project/a.sh",
                    "status": "modified",
                    "patch": "Index: /project/a.sh\n===================================================================\n--- /project/a.sh\n+++ /project/a.sh\n@@ -1,2 +1,3 @@\n set -u\n+export LC_ALL=C\n umask 022\n",
                },
                {
                    "file": "empty.txt",
                    "status": "added",
                    "patch": "Index: /project/empty.txt\n===================================================================\n--- /project/empty.txt\n+++ /project/empty.txt\n",
                },
                {
                    "file": "new.md",
                    "status": "added",
                    "patch": "Index: /project/new.md\n===================================================================\n--- /project/new.md\n+++ /project/new.md\n@@ -0,0 +1 @@\n+hello\n",
                },
            ]
        });
        let diff = build_file_diff(&metadata, Path::new("/project")).expect("diff");
        assert_eq!(diff.changes.len(), 3);
        assert_eq!(
            diff.patch.expect("patch").text,
            "diff --git /project/a.sh /project/a.sh\n--- /project/a.sh\n+++ /project/a.sh\n@@ -1,2 +1,3 @@\n set -u\n+export LC_ALL=C\n umask 022\n\
             diff --git /project/new.md /project/new.md\n--- /dev/null\n+++ /project/new.md\n@@ -0,0 +1 @@\n+hello\n"
        );
    }

    #[test]
    fn a_failed_turn_shows_its_error_once() {
        let message = |id: &str, body: MessageBody| Message {
            id: id.into(),
            body,
        };
        let step = |error: Option<Value>| {
            let mut assistant: crate::types::AssistantMessage =
                serde_json::from_value(json!({})).expect("step");
            assistant.error = error;
            MessageBody::Assistant(assistant)
        };
        let idle = |error: &str| {
            MessageBody::Idle(
                serde_json::from_value(json!({"outcome": "failed", "error": {"message": error}}))
                    .expect("idle"),
            )
        };
        let error = json!({"message": "limit reached"});
        let repeated = find_repeated_notices(&[
            message("step", step(Some(error))),
            message("same", idle("limit reached")),
            message("clean-step", step(None)),
            message("own-error", idle("another failure")),
            message(
                "stopped-step",
                step(Some(json!({"message": STEP_INTERRUPTED}))),
            ),
            message("stopped", idle(STEP_INTERRUPTED)),
        ]);
        assert_eq!(
            repeated,
            HashSet::from_iter(["same/error".to_string(), "stopped/error".to_string()])
        );
    }

    #[test]
    fn system_reminders_are_hidden() {
        let synthetic = |text: &str| Message {
            id: "msg".into(),
            body: MessageBody::Synthetic(TextMessage {
                text: text.into(),
                description: None,
                metadata: None,
            }),
        };
        let skills = Vec::new();
        let (permissions, shells) = Default::default();
        let context = TranslateContext {
            tool_call_ids_awaiting_permission: &permissions,
            session_activity: &HashMap::default(),
            shell_terminals: &shells,
            skills: &skills,
            directory: Path::new("/project"),
        };
        let upserts = |text| {
            translate_message(&synthetic(text), &context)
                .expect("upserts")
                .len()
        };
        assert_eq!(
            upserts("<system-reminder>\nPlan mode is on\n</system-reminder>"),
            0
        );
        assert_eq!(upserts("Loaded instructions"), 1);
        let mut instruction = synthetic("<system-reminder>\nRules\n</system-reminder>");
        if let MessageBody::Synthetic(body) = &mut instruction.body {
            body.description = Some("AGENTS.md".into());
        }
        assert_eq!(
            translate_message(&instruction, &context)
                .expect("upserts")
                .len(),
            1
        );
    }

    #[test]
    fn stopped_and_declined_turns_show_a_notice() {
        let context = TranslateContext {
            tool_call_ids_awaiting_permission: &HashSet::default(),
            session_activity: &HashMap::default(),
            shell_terminals: &ShellTerminals::default(),
            skills: &[],
            directory: Path::new("/project"),
        };
        let notice = |error: Value, content: Value| {
            let message = Message::from_value(json!({
                "id": "msg_1",
                "type": "assistant",
                "agent": "build",
                "content": content,
                "error": error,
                "time": {"created": 1}
            }))
            .expect("message");
            translate_message(&message, &context)
                .expect("upserts")
                .into_iter()
                .filter_map(|upsert| match upsert {
                    Upsert::Assistant(message) if message.message_id.0.ends_with("/error") => {
                        message.content.as_opt_ref().flatten().cloned()
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let interrupted = json!({"type": "aborted", "message": "Step interrupted"});
        assert_eq!(
            notice(interrupted.clone(), json!([])),
            vec![vec![text_block("*Stopped*")]]
        );
        for (name, message) in [
            ("shell", "The user declined this tool call"),
            ("question", "The user dismissed this question"),
        ] {
            assert_eq!(
                notice(
                    interrupted.clone(),
                    json!([{
                        "type": "tool", "id": "call_1", "name": name,
                        "state": { "status": "error", "error": { "type": "aborted", "message": message } },
                    }])
                ),
                vec![vec![text_block("*You declined, so the agent stopped.*")]],
            );
        }
        let error = "Execution was interrupted repeatedly and will not be resumed automatically.";
        assert_eq!(
            notice(json!({ "type": "aborted", "message": error }), json!([])),
            vec![vec![text_block(&format!("Error: {error}"))]],
        );
        assert_eq!(
            notice(
                json!({ "type": "unknown", "message": "Step interrupted" }),
                json!([])
            ),
            vec![vec![text_block("Error: Step interrupted")]],
        );
    }

    #[test]
    fn interruption_notices_are_not_repeated_per_step_or_idle_message() {
        let message = |id: &str, body: Value| {
            let mut body = body;
            body["id"] = json!(id);
            Message::from_value(body).expect("message")
        };
        let step = json!({
            "type": "assistant", "error": {"type": "aborted", "message": "Step interrupted"},
        });
        let idle = json!({ "type": "idle", "outcome": "interrupted" });
        let messages = [
            message("first", step.clone()),
            message("second", step.clone()),
            message("idle", idle.clone()),
            message("user", json!({ "type": "user", "text": "again" })),
            message("next", step),
            message("next-idle", idle),
        ];
        assert_eq!(
            find_repeated_notices(&messages),
            HashSet::from_iter([
                "second/error".into(),
                "idle/error".into(),
                "next-idle/error".into()
            ]),
        );
        let context = TranslateContext {
            tool_call_ids_awaiting_permission: &HashSet::default(),
            session_activity: &HashMap::default(),
            shell_terminals: &ShellTerminals::default(),
            skills: &[],
            directory: Path::new("/project"),
        };
        let upserts = translate_message(&messages[2], &context).expect("upserts");
        let [Upsert::Assistant(notice)] = upserts.as_slice() else {
            panic!("expected one idle notice");
        };
        assert_eq!(
            notice.content.as_opt_ref().flatten(),
            Some(&vec![text_block("*Stopped*")])
        );
    }

    #[test]
    fn resolves_relative_edit_paths_against_the_session_directory() {
        // As the server reports an edit when the model passes a relative path.
        let metadata = json!({
            "files": [{
                "file": "src/main.py",
                "status": "modified",
                "patch": "Index: src/main.py\n===================================================================\n--- src/main.py\n+++ src/main.py\n@@ -1 +1 @@\n-a\n+b\n",
            }]
        });
        let diff = build_file_diff(&metadata, Path::new("/project")).expect("diff");
        assert_eq!(
            diff.patch.expect("patch").text,
            "diff --git /project/src/main.py /project/src/main.py\n--- /project/src/main.py\n+++ /project/src/main.py\n@@ -1 +1 @@\n-a\n+b\n"
        );

        // Without a directory there is nothing to resolve against.
        assert!(build_file_diff(&metadata, Path::new("")).is_none());

        let locations = |input: Value| {
            locate_tool_call("read", &input, None, Path::new("/project"))
                .into_iter()
                .map(|location| location.path.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            locations(json!({"path": "src/main.py"})),
            vec![PathBuf::from("/project/src/main.py")]
        );
        assert_eq!(
            locations(json!({"filePath": "/elsewhere/a.rs"})),
            vec![PathBuf::from("/elsewhere/a.rs")]
        );
        assert!(locate_tool_call("read", &json!({"path": "a.rs"}), None, Path::new("")).is_empty());
        let folder = std::env::temp_dir();
        assert!(locate_tool_call("read", &json!({"path": "."}), None, &folder).is_empty());
        assert!(
            locate_tool_call(
                "read",
                &json!({"path": "src"}),
                Some("Read directory /project/src, entries 1-1\nmain.py"),
                Path::new("/project"),
            )
            .is_empty()
        );
    }
}
