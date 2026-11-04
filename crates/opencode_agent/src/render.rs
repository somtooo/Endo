//! Renders OpenCode messages into an `AcpThread`.
//!
//! Every entry is keyed, so rendering a message again updates it in place.
//! One OpenCode assistant message holds reasoning, text and tool calls in
//! order; each becomes its own keyed chunk or tool call entry, which keeps
//! Zed's layout (a tool call splits the surrounding assistant text) the same
//! whether the message streams in or loads from a snapshot.

use crate::types::{
    AssistantContent, Message, MessageBody, Skill, TextMessage, ToolCall, ToolState, UserMessage,
};
use acp_thread::{
    AcpThread, ClientUserMessageId, DisplayTerminalOutput, DisplayTerminalPatch, MentionUri,
    SUBAGENT_SESSION_INFO_META_KEY,
};
use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::v2::schema as acp;
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use gpui::Context;
use serde_json::Value;
use http_client::Url;
use std::ops::Range;
use std::path::{Path, PathBuf};
use util::ResultExt as _;
use util::paths::PathStyle;

pub(crate) fn client_message_id(message_id: &str) -> Result<ClientUserMessageId> {
    serde_json::from_value(Value::String(message_id.to_string()))
        .context("building a client message ID")
}

pub(crate) fn message_id_string(client_id: &ClientUserMessageId) -> String {
    client_id.message_id().0.to_string()
}

/// One change to the thread. The agent panel expects at most one new entry
/// per effect flush, so the hub applies these one entry at a time.
pub(crate) enum RenderOp {
    LocalUser(ClientUserMessageId, Vec<acp::ContentBlock>),
    Assistant(acp::AgentMessage),
    Thought(acp::AgentThought),
    Tool(acp::ToolCallUpdate),
    /// Creates or updates the display-only terminal a shell tool call shows.
    /// Must come before the tool call that references it.
    Terminal(acp::TerminalId, DisplayTerminalPatch),
    Plan(acp::PlanItems),
    /// Zed's "Context Compacted" divider, with the summary inside.
    Compaction(acp::CompactionUpdate),
}

impl RenderOp {
    pub(crate) fn apply(self, thread: &mut AcpThread, cx: &mut Context<AcpThread>) -> Result<()> {
        match self {
            RenderOp::LocalUser(id, content) => thread.upsert_local_user_message(id, content, cx),
            RenderOp::Assistant(message) => thread.upsert_assistant_message(message, cx),
            RenderOp::Thought(thought) => thread.upsert_thought(thought, cx),
            RenderOp::Tool(tool_call) => thread.upsert_wire_tool_call(tool_call, cx),
            RenderOp::Terminal(terminal_id, patch) => {
                thread.upsert_display_terminal(terminal_id, patch, cx)
            }
            RenderOp::Plan(plan) => {
                thread.upsert_plan_items(plan, None, cx);
                Ok(())
            }
            RenderOp::Compaction(update) => {
                thread.upsert_context_compaction_update(update, cx);
                Ok(())
            }
        }
    }
}

/// What rendering needs besides the message itself.
pub(crate) struct RenderContext<'a> {
    /// Tool calls waiting for the user's approval. Their status is left
    /// alone so a re-render doesn't clear the approval prompt.
    pub pending_permissions: &'a HashSet<String>,
    /// Sessions the server is running, so a background subagent's card
    /// keeps spinning after its tool call returns.
    pub running_sessions: &'a HashSet<String>,
    /// Background shell commands that finished, by shell ID.
    pub finished_shells: &'a HashMap<String, ShellOutcome>,
    /// The session's skills, to turn skill mentions into chips.
    pub skills: &'a [Skill],
    /// Shell tool calls whose output was streamed into their terminal while
    /// they ran. Their final render keeps that output: the result OpenCode
    /// reports is capped, so it could be shorter than what was shown.
    pub streamed_shell_calls: &'a HashSet<String>,
    /// The session's directory, which relative paths in tool results are
    /// relative to.
    pub directory: &'a Path,
}

/// How a background shell command ended, from the notification OpenCode
/// posts into the session when it finishes.
#[derive(Clone, Debug)]
pub(crate) struct ShellOutcome {
    pub state: String,
    pub output: String,
    pub exit: Option<i64>,
}

/// The shell ID and outcome in a background shell's completion notice.
pub(crate) fn shell_notification(message: &Message) -> Option<(String, ShellOutcome)> {
    let MessageBody::Synthetic(synthetic) = &message.body else {
        return None;
    };
    let metadata = synthetic.metadata.as_ref()?;
    if metadata.get("source").and_then(Value::as_str) != Some("shell") {
        return None;
    }
    let shell_id = metadata.get("shellID").and_then(Value::as_str)?;
    // The text wraps the output in `<shell ...>` and `</shell>` lines.
    let output = synthetic
        .text
        .split_once('\n')
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    let output = output.strip_suffix("</shell>").unwrap_or(output);
    let output = output.strip_suffix('\n').unwrap_or(output);
    Some((
        shell_id.to_string(),
        ShellOutcome {
            state: metadata
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("completed")
                .to_string(),
            output: output.to_string(),
            exit: metadata.get("exit").and_then(Value::as_i64),
        },
    ))
}

/// The background shell a shell tool call started, if it's still marked
/// as running on the call itself.
/// The shell a shell tool call runs, and whether it's still running: a
/// foreground call reports its shell while running, a background call in its
/// result until the completion notice arrives.
pub(crate) fn live_shell<'a>(
    tool: &'a ToolCall,
    finished_shells: &HashMap<String, ShellOutcome>,
) -> Option<(&'a str, bool)> {
    if !is_shell(&tool.name) {
        return None;
    }
    let (metadata, running) = match &tool.state {
        ToolState::Running { metadata, .. } => (metadata, true),
        ToolState::Completed { metadata, .. } => (metadata, false),
        ToolState::Streaming { .. } | ToolState::Error { .. } => return None,
    };
    let shell_id = metadata.get("shellID").and_then(Value::as_str)?;
    let running = running
        || background_shell_id(tool).is_some() && !finished_shells.contains_key(shell_id);
    Some((shell_id, running))
}

pub(crate) fn background_shell_id(tool: &ToolCall) -> Option<&str> {
    if !is_shell(&tool.name) {
        return None;
    }
    let ToolState::Completed { metadata, .. } = &tool.state else {
        return None;
    };
    if metadata.get("status").and_then(Value::as_str) != Some("running") {
        return None;
    }
    metadata.get("shellID").and_then(Value::as_str)
}

/// The thread changes that render `message`.
pub(crate) fn render_message(message: &Message, context: &RenderContext) -> Result<Vec<RenderOp>> {
    let ops = match &message.body {
        MessageBody::User(user) => {
            let content = match displayed_content(user.metadata.as_ref()) {
                Some(mut content) => {
                    content.extend(user.files.iter().filter_map(image_block));
                    content
                }
                None => mentioned_content(user, context.skills),
            };
            vec![RenderOp::LocalUser(client_message_id(&message.id)?, content)]
        }
        MessageBody::Synthetic(synthetic) if is_model_only(synthetic) => Vec::new(),
        MessageBody::Synthetic(synthetic) => {
            // Server-injected input (loaded instructions, subagent results).
            // Shown as a note in the agent's reply, like the TUI does, not
            // as a user message.
            vec![RenderOp::Assistant(
                acp::AgentMessage::new(message.id.as_str())
                    .content(vec![text_block(&synthetic_note(synthetic))]),
            )]
        }
        MessageBody::Assistant(assistant) => {
            let mut ops = Vec::new();
            let mut text_ordinal = 0;
            let mut reasoning_ordinal = 0;
            for item in &assistant.content {
                match item {
                    AssistantContent::Text { text } => {
                        let id = format!("{}/t{text_ordinal}", message.id);
                        text_ordinal += 1;
                        if !text.is_empty() {
                            ops.push(RenderOp::Assistant(
                                acp::AgentMessage::new(id).content(vec![text_block(text)]),
                            ));
                        }
                    }
                    AssistantContent::Reasoning { text, .. } => {
                        let id = format!("{}/r{reasoning_ordinal}", message.id);
                        reasoning_ordinal += 1;
                        if !text.is_empty() {
                            ops.push(RenderOp::Thought(
                                acp::AgentThought::new(id).content(vec![text_block(text)]),
                            ));
                        }
                    }
                    AssistantContent::Tool(tool) => {
                        if let Some(terminal) = shell_terminal(tool, context) {
                            ops.push(terminal);
                        }
                        if let Some(plan) = todo_plan(tool) {
                            ops.push(plan);
                        }
                        ops.push(RenderOp::Tool(tool_call_update(tool, context)));
                    }
                }
            }
            // A step the user stopped isn't an error, as in the TUI. Other
            // aborts, like a turn that can't be resumed, still show.
            if let Some(error) = assistant
                .error
                .as_ref()
                .filter(|error| error_text(error) != STEP_INTERRUPTED)
            {
                let id = format!("{}/error", message.id);
                ops.push(RenderOp::Assistant(
                    acp::AgentMessage::new(id)
                        .content(vec![text_block(&format!("Error: {}", error_text(error)))]),
                ));
            }
            ops
        }
        MessageBody::Idle(idle) if idle.outcome == "failed" => {
            let id = format!("{}/error", message.id);
            let error = idle.error.as_ref().map(error_text).unwrap_or_default();
            vec![RenderOp::Assistant(
                acp::AgentMessage::new(id)
                    .content(vec![text_block(&format!("The agent stopped: {error}"))]),
            )]
        }
        MessageBody::Compaction(compaction) => {
            let status = match compaction.status.as_str() {
                "running" => acp::CompactionStatus::InProgress,
                "completed" => acp::CompactionStatus::Completed,
                _ => acp::CompactionStatus::Failed,
            };
            let mut update = acp::CompactionUpdate::new(message.id.as_str(), status);
            if !compaction.summary.is_empty() {
                update = update.summary(vec![text_block(&compaction.summary)]);
            }
            if let Some(error) = &compaction.error {
                update = update.error(error_text(error));
            }
            vec![RenderOp::Compaction(update)]
        }
        MessageBody::Shell(shell) => {
            let output = shell
                .output
                .as_ref()
                .map(error_text)
                .unwrap_or_default();
            vec![RenderOp::Tool(
                acp::ToolCallUpdate::new(message.id.as_str())
                    .name("shell")
                    .title(shell.command.clone())
                    .kind(acp::ToolKind::Execute)
                    .status(match shell.status.as_deref() {
                        Some("running") => acp::ToolCallStatus::InProgress,
                        Some("failed") => acp::ToolCallStatus::Failed,
                        _ => acp::ToolCallStatus::Completed,
                    })
                    .content(vec![tool_text(&output)]),
            )]
        }
        MessageBody::Idle(_)
        | MessageBody::System(_)
        | MessageBody::AgentSwitched { .. }
        | MessageBody::ModelSwitched { .. }
        | MessageBody::Other { .. } => Vec::new(),
    };
    Ok(ops)
}

fn synthetic_note(synthetic: &TextMessage) -> String {
    let metadata = synthetic.metadata.as_ref();
    let field = |name: &str| {
        metadata
            .and_then(|metadata| metadata.get(name))
            .and_then(Value::as_str)
    };
    let description = synthetic.description.as_deref().unwrap_or_default();
    let note = if field("source") == Some("shell") {
        let state = field("state").unwrap_or("finished");
        format!("↳ Background command {state} · {description}")
    } else if field("source") == Some("subagent") {
        let agent = field("agent").unwrap_or("Subagent");
        let state = field("state").unwrap_or("finished");
        format!("↳ {agent} {state} · {description}")
    } else if metadata.is_some_and(|metadata| metadata.get("instruction").is_some()) {
        format!("Instructions: {description}")
    } else if !description.is_empty() {
        description.to_string()
    } else {
        synthetic.text.lines().next().unwrap_or_default().to_string()
    };
    format!("*{}*", note.trim())
}

pub(crate) fn tool_call_update(tool: &ToolCall, context: &RenderContext) -> acp::ToolCallUpdate {
    let empty = Value::Null;
    let (input, mut status, output, metadata) = match &tool.state {
        ToolState::Streaming { input } => (
            serde_json::from_str(input).unwrap_or(Value::Null),
            acp::ToolCallStatus::Pending,
            None,
            &empty,
        ),
        ToolState::Running { input, metadata } => {
            (input.clone(), acp::ToolCallStatus::InProgress, None, metadata)
        }
        ToolState::Completed {
            input,
            content,
            metadata,
        } => (
            input.clone(),
            acp::ToolCallStatus::Completed,
            Some(tool_output_text(content)),
            metadata,
        ),
        ToolState::Error { input, error } => (
            input.clone(),
            acp::ToolCallStatus::Failed,
            Some(error_text(error)),
            &empty,
        ),
    };
    let mut update = acp::ToolCallUpdate::new(tool.id.as_str())
        .name(tool.name.clone())
        .title(tool_title(&tool.name, &input))
        .kind(tool_kind(&tool.name))
        .raw_input(input.clone())
        .locations(tool_locations(&tool.name, &input, output.as_deref(), context.directory));

    let mut content = Vec::new();
    if continues_subagent(&tool.name, &input) {
        // Sends more work to a subagent another call already shows, so it
        // isn't a new card, like the TUI's "Continue subagent" line.
        let description = input.get("description").and_then(Value::as_str).unwrap_or_default();
        update = update
            .title(format!("Continue subagent: {description}"))
            .kind(acp::ToolKind::Other);
    } else if let Some(child_id) = subagent_session_id(&tool.name, &input, metadata) {
        // Zed's subagent card. A background subagent's call returns at once,
        // so the card follows the subagent session itself.
        if status == acp::ToolCallStatus::Completed && context.running_sessions.contains(child_id)
        {
            status = acp::ToolCallStatus::InProgress;
        }
        update = update.meta(subagent_meta(child_id));
    } else if is_shell(&tool.name) {
        if let Some(shell_id) = background_shell_id(tool) {
            status = match context.finished_shells.get(shell_id) {
                None => acp::ToolCallStatus::InProgress,
                Some(outcome) if outcome.state == "completed" => acp::ToolCallStatus::Completed,
                Some(_) => acp::ToolCallStatus::Failed,
            };
        }
        content.push(acp::ToolCallContent::Terminal(acp::Terminal::new(
            terminal_id(tool).0.to_string(),
        )));
        // A streamed terminal keeps its output, so the error goes below it.
        if let ToolState::Error { error, .. } = &tool.state
            && context.streamed_shell_calls.contains(&tool.id)
        {
            content.push(tool_text(&error_text(error)));
        }
    } else if let Some(diff) = file_diff(metadata, context.directory) {
        content.push(acp::ToolCallContent::Diff(diff));
    } else if let Some(output) = output {
        content.push(tool_text(&output));
    }
    if !content.is_empty() {
        update = update.content(content);
    }
    if !context.pending_permissions.contains(&tool.id) {
        update = update.status(status);
    }
    update
}

fn continues_subagent(tool_name: &str, input: &Value) -> bool {
    matches!(tool_name, "subagent" | "task")
        && input.get("sessionID").and_then(Value::as_str).is_some()
}

/// The child session a subagent tool call starts, once the server reports
/// it. Calls that continue an existing subagent don't count.
pub(crate) fn subagent_session_id<'a>(
    tool_name: &str,
    input: &Value,
    metadata: &'a Value,
) -> Option<&'a str> {
    if !matches!(tool_name, "subagent" | "task") || continues_subagent(tool_name, input) {
        return None;
    }
    metadata.get("sessionID").and_then(Value::as_str)
}

fn subagent_meta(child_id: &str) -> acp::Meta {
    let mut meta = acp::Meta::new();
    meta.insert(
        SUBAGENT_SESSION_INFO_META_KEY.into(),
        serde_json::json!({ "session_id": child_id, "message_start_index": 0 }),
    );
    meta
}

fn is_shell(tool_name: &str) -> bool {
    matches!(tool_name, "shell" | "bash")
}

pub(crate) fn terminal_id(tool: &ToolCall) -> acp::TerminalId {
    acp::TerminalId::new(format!("{}/terminal", tool.id))
}

/// OpenCode runs shell commands itself, so Zed shows them in a display-only
/// terminal: the command, its output once it finishes, and the exit code.
fn shell_terminal(tool: &ToolCall, context: &RenderContext) -> Option<RenderOp> {
    if !is_shell(&tool.name) {
        return None;
    }
    // A background command's call returns at once; the terminal stays
    // running until its completion notice arrives.
    let background = background_shell_id(tool)
        .map(|shell_id| context.finished_shells.get(shell_id).cloned());
    let (input, output, exit_code) = match &tool.state {
        ToolState::Completed { input, .. } if background.is_some() => match background.flatten() {
            Some(outcome) => (input, Some(outcome.output), outcome.exit),
            None => (input, None, None),
        },
        ToolState::Streaming { .. } => return None,
        ToolState::Running { input, .. } => (input, None, None),
        ToolState::Completed {
            input,
            content,
            metadata,
        } => (
            input,
            Some(tool_output_text(content)),
            metadata.get("exit").and_then(Value::as_i64),
        ),
        ToolState::Error { input, error } => (input, Some(error_text(error)), None),
    };
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let mut patch = DisplayTerminalPatch::default();
    if let Some(command) = field("command").or(field("cmd")) {
        patch.command = MaybeUndefined::Value(command.to_string());
    }
    if let Some(cwd) = field("workdir").or(field("cwd")).filter(|cwd| cwd.starts_with('/')) {
        patch.cwd = MaybeUndefined::Value(PathBuf::from(cwd).into());
    }
    if let Some(output) = output {
        if !context.streamed_shell_calls.contains(&tool.id) {
            patch.output = MaybeUndefined::Value(DisplayTerminalOutput {
                data: terminal_bytes(&output),
                meta: None,
            });
        }
        patch.exit_status =
            MaybeUndefined::Value(acp::TerminalExitStatus::new().exit_code(exit_code));
    }
    Some(RenderOp::Terminal(terminal_id(tool), patch))
}

/// The todo tool's list, shown in Zed's plan view. Each call sends the
/// whole list, so the latest call replaces it.
fn todo_plan(tool: &ToolCall) -> Option<RenderOp> {
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
            Some(acp::PlanEntry::new(content, acp::PlanEntryPriority::Medium, status))
        })
        .collect();
    Some(RenderOp::Plan(acp::PlanItems::new("todo", entries)))
}

/// Edit tools report each changed file as a unified diff. Zed renders git
/// patches natively, so convert the headers and keep the hunks. A file with
/// no hunks (such as a new empty file) is still listed as a change.
fn file_diff(metadata: &Value, directory: &Path) -> Option<acp::Diff> {
    let files = metadata.get("files")?.as_array()?;
    let mut text = String::new();
    let mut changes = Vec::new();
    for file in files {
        let patch = file.get("patch").and_then(Value::as_str).unwrap_or_default();
        let Some(path) = changed_file_path(file, directory) else {
            continue;
        };
        let path = path.as_str();
        let (old_path, new_path, change) = match file.get("status").and_then(Value::as_str) {
            Some("added") => ("/dev/null", path, acp::DiffChange::add(PathBuf::from(path))),
            Some("deleted") => (path, "/dev/null", acp::DiffChange::delete(PathBuf::from(path))),
            _ => (path, path, acp::DiffChange::modify(PathBuf::from(path))),
        };
        changes.push(change);
        let Some(hunks) = patch_hunks(patch) else {
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

/// Both the patch header and `file` follow the path the model passed, so
/// either can be relative to the session's directory.
fn changed_file_path(file: &Value, directory: &Path) -> Option<String> {
    let patch = file.get("patch").and_then(Value::as_str).unwrap_or_default();
    let header_path = patch_header_path(patch);
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

fn patch_hunks(patch: &str) -> Option<&str> {
    let start = patch
        .find("\n@@")
        .map(|ix| ix + 1)
        .or_else(|| patch.starts_with("@@").then_some(0))?;
    Some(&patch[start..])
}

/// Reminders addressed to the model, such as the plan mode instructions. The
/// notes shown for instructions and finished work carry a description or a
/// source, so they aren't these.
fn is_model_only(synthetic: &TextMessage) -> bool {
    let has_note = |name| {
        synthetic
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.get(name).is_some())
    };
    synthetic.text.trim_start().starts_with("<system-reminder>")
        && synthetic.description.as_deref().unwrap_or_default().is_empty()
        && !has_note("source")
        && !has_note("instruction")
}

/// The IDs of turn-ending messages that only repeat the error of the failed
/// step before them, which is already shown.
pub(crate) fn repeated_errors(messages: &[Message]) -> HashSet<String> {
    let mut repeated = HashSet::default();
    let mut last_step_error = None;
    for message in messages {
        match &message.body {
            MessageBody::Assistant(assistant) => {
                // An interrupted step shows no error, so nothing would be repeated.
                last_step_error = assistant
                    .error
                    .as_ref()
                    .map(error_text)
                    .filter(|error| error != STEP_INTERRUPTED);
            }
            MessageBody::Idle(idle) if idle.outcome == "failed" => {
                if idle.error.as_ref().map(error_text) == last_step_error && last_step_error.is_some() {
                    repeated.insert(message.id.clone());
                }
            }
            _ => {}
        }
    }
    repeated
}

/// A file changed by a finished edit tool, for Zed's diff review.
pub(crate) struct EditedFile {
    pub path: PathBuf,
    pub kind: action_log::ExternalEditKind,
    hunks: String,
}

/// Undoes consecutive edits to one file in `text`, newest first, or returns
/// `None` if they don't apply.
pub(crate) fn revert_edits(edits: &[EditedFile], text: &str) -> Option<String> {
    edits
        .iter()
        .rev()
        .try_fold(text.to_string(), |text, edit| edit.revert(&text))
}

impl EditedFile {
    fn revert(&self, text: &str) -> Option<String> {
        if self.hunks.is_empty() {
            return Some(text.to_string());
        }
        let patch = format!("--- a\n+++ b\n{}", self.hunks);
        let patch = diffy::Patch::from_str(&patch).ok()?;
        diffy::apply(text, &patch.reverse()).ok()
    }
}

pub(crate) fn edited_files(tool: &ToolCall, directory: &Path) -> Vec<EditedFile> {
    let ToolState::Completed { metadata, .. } = &tool.state else {
        return Vec::new();
    };
    let Some(files) = metadata.get("files").and_then(Value::as_array) else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(|file| {
            let patch = file.get("patch").and_then(Value::as_str).unwrap_or_default();
            let kind = match file.get("status").and_then(Value::as_str) {
                Some("added") => action_log::ExternalEditKind::Created,
                Some("deleted") => action_log::ExternalEditKind::Deleted,
                _ => action_log::ExternalEditKind::Modified,
            };
            // Buffers normalize line endings, and the revert runs on their text.
            let mut hunks = patch_hunks(patch)
                .unwrap_or_default()
                .replace("\r\n", "\n");
            if hunks.is_empty() && kind == action_log::ExternalEditKind::Modified {
                return None;
            }
            if !hunks.is_empty() && !hunks.ends_with('\n') {
                hunks.push('\n');
            }
            Some(EditedFile {
                path: PathBuf::from(changed_file_path(file, directory)?),
                kind,
                hunks,
            })
        })
        .collect()
}

fn patch_header_path(patch: &str) -> Option<&str> {
    patch.lines().find_map(|line| {
        line.strip_prefix("Index: ")
            .or_else(|| line.strip_prefix("+++ "))
            .filter(|path| *path != "/dev/null")
    })
}

fn tool_kind(name: &str) -> acp::ToolKind {
    match name {
        "read" | "list" | "ls" => acp::ToolKind::Read,
        "edit" | "write" | "patch" | "multiedit" | "apply_patch" => acp::ToolKind::Edit,
        "shell" | "bash" => acp::ToolKind::Execute,
        "grep" | "glob" | "find" | "codesearch" => acp::ToolKind::Search,
        "webfetch" | "websearch" => acp::ToolKind::Fetch,
        "subagent" | "task" | "todowrite" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

fn tool_title(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    // The subagent card shows the title as its label.
    if matches!(name, "subagent" | "task")
        && let Some(description) = field("description")
    {
        return description.to_string();
    }
    let detail = match name {
        "shell" | "bash" => field("command").or(field("description")),
        "subagent" | "task" => field("description"),
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

/// The file a tool works on, which Zed opens from the card and follows.
/// Models may pass a path relative to the session's directory. Directories
/// aren't locations: Zed would try to open them as files. The read tool also
/// reads directories, which its output names (`core/src/tool/plugin/read.ts`).
fn tool_locations(
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
        .filter(|path| path.is_absolute())
        .take(1)
        .map(acp::ToolCallLocation::new)
        .collect()
}

fn tool_output_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn error_text(error: &Value) -> String {
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

/// A chip for an attached file. OpenCode's clients give a line range as
/// `?start=&end=` (1-based), which Zed shows as a selection.
fn file_link(file: &Value) -> Option<acp::ContentBlock> {
    let uri = file_uri(file)?;
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

const DISPLAY_METADATA_KEY: &str = "zed";

/// The error OpenCode records on a step the user stopped
/// (`core/src/session/runner/step.ts`).
const STEP_INTERRUPTED: &str = "Step interrupted";

/// The prompt as Zed showed it, saved in the message's metadata. Images are
/// left out because the message's files already carry them. Skills keep only
/// their link: OpenCode loads a skill itself, so its file isn't needed again.
pub(crate) fn display_metadata(content: &[acp::ContentBlock]) -> Value {
    let blocks: Vec<acp::ContentBlock> = content
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Image(_) => None,
            acp::ContentBlock::Resource(resource) => {
                if let acp::EmbeddedResourceResource::TextResourceContents(contents) =
                    &resource.resource
                    && let Ok(uri @ MentionUri::Skill { .. }) =
                        MentionUri::parse(&contents.uri, PathStyle::local())
                {
                    return Some(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                        uri.name(),
                        contents.uri.clone(),
                    )));
                }
                Some(block.clone())
            }
            _ => Some(block.clone()),
        })
        .collect();
    serde_json::json!({ DISPLAY_METADATA_KEY: { "content": blocks } })
}

fn displayed_content(metadata: Option<&Value>) -> Option<Vec<acp::ContentBlock>> {
    let content = metadata?.get(DISPLAY_METADATA_KEY)?.get("content")?;
    serde_json::from_value(content.clone()).log_err()
}

/// Rebuilds chips for a prompt Zed didn't send, from the mentions OpenCode's
/// own clients record. Like the TUI, a mention is found by its text: its
/// position is in the sending client's units (the TUI counts terminal
/// columns), so it only decides which copy of a repeated text is meant.
/// Attachments without a mention found in the text follow it.
fn mentioned_content(user: &UserMessage, skills: &[Skill]) -> Vec<acp::ContentBlock> {
    let mut attachments: Vec<(Option<(String, f64)>, acp::ContentBlock)> = Vec::new();
    for file in &user.files {
        if let Some(block) = image_block(file).or_else(|| file_link(file)) {
            attachments.push((mention(file), block));
        }
    }
    for skill in &user.skills {
        if let Some(block) = skill_link(skill, skills) {
            attachments.push((mention(skill), block));
        }
    }
    place_mentions(&user.text, attachments)
}

fn mention(attachment: &Value) -> Option<(String, f64)> {
    let mention = attachment.get("mention")?;
    let text = mention.get("text")?.as_str()?;
    if text.is_empty() {
        return None;
    }
    let start = mention.get("start").and_then(Value::as_f64).unwrap_or(0.0);
    Some((text.to_string(), start))
}

fn place_mentions(
    text: &str,
    attachments: Vec<(Option<(String, f64)>, acp::ContentBlock)>,
) -> Vec<acp::ContentBlock> {
    // Trailing chips keep the message's attachment order.
    let mut groups: HashMap<String, Vec<(f64, usize, acp::ContentBlock)>> = HashMap::default();
    let mut trailing = Vec::new();
    for (order, (mention, block)) in attachments.into_iter().enumerate() {
        match mention {
            Some((mention_text, start)) => groups
                .entry(mention_text)
                .or_default()
                .push((start, order, block)),
            None => trailing.push((order, block)),
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
        items.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
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
        for (index, (start, order, block)) in items.into_iter().enumerate() {
            if index >= placeable {
                trailing.push((order, block));
                continue;
            }
            let last = available.len() - (placeable - index);
            let nearest = (next..=last)
                .min_by(|left, right| {
                    let distance = |candidate: usize| {
                        let offset = text[..available[candidate].start].chars().count();
                        (offset as f64 - start).abs()
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

fn skill_link(attachment: &Value, skills: &[Skill]) -> Option<acp::ContentBlock> {
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
fn image_block(file: &Value) -> Option<acp::ContentBlock> {
    let (mime_type, data) = match (
        file.get("mime").and_then(Value::as_str),
        file.get("data").and_then(Value::as_str),
    ) {
        (Some(mime_type), Some(data)) => (mime_type, data),
        _ => file_uri(file)?
            .strip_prefix("data:")?
            .split_once(";base64,")?,
    };
    if !mime_type.starts_with("image/") || data.is_empty() {
        return None;
    }
    Some(acp::ContentBlock::Image(acp::ImageContent::new(data, mime_type)))
}

fn file_uri(file: &Value) -> Option<&str> {
    file.get("uri")
        .or_else(|| file.get("source")?.get("uri"))
        .and_then(Value::as_str)
}

/// A terminal needs carriage returns to start each line at column 0.
pub(crate) fn terminal_bytes(output: &str) -> Vec<u8> {
    output.replace("\r\n", "\n").replace('\n', "\r\n").into_bytes()
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
        acp::ContentBlock::ResourceLink(acp::ResourceLink::new(uri.name(), uri.to_uri().to_string()))
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
    fn tui_mentions_become_chips_where_they_were_typed() {
        let skills = [skill("browser")];
        let message = user(json!({
            "text": "@browser wow",
            "skills": [{ "id": "browser", "name": "Browser", "mention": { "start": 0, "end": 8, "text": "@browser" } }],
        }));
        assert_eq!(
            mentioned_content(&message, &skills),
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
            mentioned_content(&message, &[]),
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
            mentioned_content(&message, &[]),
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
            mentioned_content(&message, &[]),
            vec![
                text_block("I mean @a.ts, the 文件 "),
                link(file("/a.ts")),
            ]
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
            mentioned_content(&message, &[]),
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
    fn finds_the_shell_a_call_runs() {
        let finished = HashMap::default();
        let foreground = shell_call(json!({
            "status": "running", "input": { "command": "make" }, "metadata": { "shellID": "sh_1" },
        }));
        assert_eq!(live_shell(&foreground, &finished), Some(("sh_1", true)));

        let background = shell_call(json!({
            "status": "completed", "input": { "command": "make", "background": true }, "content": [],
            "metadata": { "status": "running", "shellID": "sh_2" },
        }));
        assert_eq!(live_shell(&background, &finished), Some(("sh_2", true)));
        let finished: HashMap<String, ShellOutcome> = [(
            "sh_2".to_string(),
            ShellOutcome { state: "completed".into(), output: String::new(), exit: Some(0) },
        )]
        .into_iter()
        .collect();
        assert_eq!(live_shell(&background, &finished), Some(("sh_2", false)));

        let done = shell_call(json!({
            "status": "completed", "input": { "command": "make" }, "content": [],
            "metadata": { "status": "completed", "shellID": "sh_3" },
        }));
        assert_eq!(live_shell(&done, &finished), Some(("sh_3", false)));
    }

    #[test]
    fn a_streamed_shell_keeps_its_output_when_it_ends() {
        let failed = shell_call(json!({
            "status": "error", "input": { "command": "make" }, "error": { "message": "Command cancelled" },
        }));
        let streamed: HashSet<String> = ["call_1".to_string()].into_iter().collect();
        let context = RenderContext {
            pending_permissions: &HashSet::default(),
            running_sessions: &HashSet::default(),
            finished_shells: &HashMap::default(),
            skills: &[],
            streamed_shell_calls: &streamed,
            directory: Path::new("/project"),
        };
        let Some(RenderOp::Terminal(_, patch)) = shell_terminal(&failed, &context) else {
            panic!("expected a terminal patch");
        };
        assert!(patch.output.is_undefined());
        assert!(!patch.exit_status.is_undefined());
        let content_len = |update: acp::ToolCallUpdate| match update.content {
            MaybeUndefined::Value(content) => content.len(),
            _ => 0,
        };
        assert_eq!(
            content_len(tool_call_update(&failed, &context)),
            2,
            "the terminal, then the error under it"
        );

        let context = RenderContext {
            streamed_shell_calls: &HashSet::default(),
            ..context
        };
        let Some(RenderOp::Terminal(_, patch)) = shell_terminal(&failed, &context) else {
            panic!("expected a terminal patch");
        };
        assert!(!patch.output.is_undefined());
        assert_eq!(content_len(tool_call_update(&failed, &context)), 1);
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
            "metadata": display_metadata(&sent),
        }))
        .unwrap();
        let context = RenderContext {
            pending_permissions: &HashSet::default(),
            running_sessions: &HashSet::default(),
            finished_shells: &HashMap::default(),
            skills: &[],
            streamed_shell_calls: &HashSet::default(),
            directory: Path::new("/project"),
        };
        let ops = render_message(&message, &context).unwrap();
        let [RenderOp::LocalUser(_, content)] = ops.as_slice() else {
            panic!("expected one user message, got {} ops", ops.len());
        };
        assert_eq!(
            content,
            &vec![
                text_block("use "),
                acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                    "deep-research",
                    skill_uri,
                )),
                acp::ContentBlock::Image(acp::ImageContent::new("aGk=", "image/png")),
            ]
        );
    }

    #[test]
    fn reads_background_shell_notices() {
        let message = Message {
            id: "msg_1".into(),
            body: MessageBody::Synthetic(TextMessage {
                text: "<shell id=\"job_1\" state=\"completed\" command=\"make\">\nbuilt\nok\n</shell>"
                    .into(),
                description: Some("make".into()),
                metadata: Some(json!({
                    "source": "shell",
                    "shellID": "shl_1",
                    "state": "completed",
                    "exit": 0,
                })),
            }),
        };
        let (shell_id, outcome) = shell_notification(&message).expect("notice");
        assert_eq!(shell_id, "shl_1");
        assert_eq!(outcome.state, "completed");
        assert_eq!(outcome.output, "built\nok");
        assert_eq!(outcome.exit, Some(0));
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
        let diff = file_diff(&metadata, Path::new("/project")).expect("diff");
        assert_eq!(diff.changes.len(), 3);
        assert_eq!(
            diff.patch.expect("patch").text,
            "diff --git /project/a.sh /project/a.sh\n--- /project/a.sh\n+++ /project/a.sh\n@@ -1,2 +1,3 @@\n set -u\n+export LC_ALL=C\n umask 022\n\
             diff --git /project/new.md /project/new.md\n--- /dev/null\n+++ /project/new.md\n@@ -0,0 +1 @@\n+hello\n"
        );
    }

    #[test]
    fn reverting_an_edit_restores_the_old_text() {
        let tool = |status: &str, patch: &str| ToolCall {
            id: "tool".into(),
            name: "edit".into(),
            state: ToolState::Completed {
                input: Value::Null,
                content: Vec::new(),
                metadata: json!({"files": [{"file": "a.txt", "status": status, "patch": patch}]}),
            },
        };
        let edited = |status: &str, patch: &str| {
            edited_files(&tool(status, patch), Path::new("/project"))
                .pop()
                .expect("file")
        };
        let header = "Index: a.txt\n===================================================================\n--- a.txt\n+++ a.txt\n";

        let modified = edited("modified", &format!("{header}@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n"));
        assert_eq!(modified.path, PathBuf::from("/project/a.txt"));
        assert_eq!(modified.kind, action_log::ExternalEditKind::Modified);
        assert_eq!(modified.revert("one\nTWO\n").as_deref(), Some("one\ntwo\n"));
        let second = edited("modified", &format!("{header}@@ -1,2 +1,2 @@\n-one\n+ONE\n TWO\n"));
        assert_eq!(
            revert_edits(&[modified, second], "ONE\nTWO\n").as_deref(),
            Some("one\ntwo\n")
        );
        let modified = edited("modified", &format!("{header}@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n"));
        // Files with Windows line endings: buffer text has already normalized them.
        let crlf = edited("modified", &format!("{header}@@ -1,2 +1,2 @@\r\n one\r\n-two\r\n+TWO\r\n"));
        assert_eq!(crlf.revert("one\nTWO\n").as_deref(), Some("one\ntwo\n"));
        // Nothing to review, such as a tool that lists files it only looked at.
        assert!(edited_files(&tool("modified", ""), Path::new("/project")).is_empty());
        // Text the edit doesn't apply to.
        assert_eq!(modified.revert("something\nelse\n"), None);

        let created = edited("added", &format!("{header}@@ -0,0 +1 @@\n+hello\n"));
        assert_eq!(created.kind, action_log::ExternalEditKind::Created);
        assert_eq!(created.revert("hello\n").as_deref(), Some(""));

        let deleted = edited("deleted", &format!("{header}@@ -1 +0,0 @@\n-hello\n"));
        assert_eq!(deleted.kind, action_log::ExternalEditKind::Deleted);
        assert_eq!(deleted.revert("").as_deref(), Some("hello\n"));

        // A new empty file has no hunks.
        assert_eq!(edited("added", header).revert("").as_deref(), Some(""));
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
        let idle = |error: &str| MessageBody::Idle(serde_json::from_value(json!({"outcome": "failed", "error": {"message": error}})).expect("idle"));
        let error = json!({"message": "limit reached"});
        let repeated = repeated_errors(&[
            message("step", step(Some(error))),
            message("same", idle("limit reached")),
            message("clean-step", step(None)),
            message("own-error", idle("another failure")),
            message("stopped-step", step(Some(json!({"message": STEP_INTERRUPTED})))),
            message("stopped", idle(STEP_INTERRUPTED)),
        ]);
        assert_eq!(repeated, HashSet::from_iter(["same".to_string()]));
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
        let (permissions, running, shells, streamed) = Default::default();
        let context = RenderContext {
            pending_permissions: &permissions,
            running_sessions: &running,
            finished_shells: &shells,
            skills: &skills,
            streamed_shell_calls: &streamed,
            directory: Path::new("/project"),
        };
        let ops = |text| render_message(&synthetic(text), &context).expect("ops").len();
        assert_eq!(ops("<system-reminder>\nPlan mode is on\n</system-reminder>"), 0);
        assert_eq!(ops("Loaded instructions"), 1);
        let mut instruction = synthetic("<system-reminder>\nRules\n</system-reminder>");
        if let MessageBody::Synthetic(body) = &mut instruction.body {
            body.description = Some("AGENTS.md".into());
        }
        assert_eq!(render_message(&instruction, &context).expect("ops").len(), 1);
    }

    #[test]
    fn a_stopped_step_shows_no_error() {
        let context = RenderContext {
            pending_permissions: &HashSet::default(),
            running_sessions: &HashSet::default(),
            finished_shells: &HashMap::default(),
            skills: &[],
            streamed_shell_calls: &HashSet::default(),
            directory: Path::new("/project"),
        };
        let error_ops = |error: Value| {
            let message = Message::from_value(json!({
                "id": "msg_1",
                "type": "assistant",
                "agent": "build",
                "content": [{"type": "text", "text": "working"}],
                "error": error,
                "time": {"created": 1}
            }))
            .unwrap();
            render_message(&message, &context)
                .unwrap()
                .into_iter()
                .filter(|op| matches!(op, RenderOp::Assistant(message) if message.message_id.0.ends_with("/error")))
                .count()
        };
        assert_eq!(error_ops(json!({"type": "aborted", "message": "Step interrupted"})), 0);
        assert_eq!(
            error_ops(json!({
                "type": "aborted",
                "message": "Execution was interrupted repeatedly and will not be resumed automatically.",
            })),
            1
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
        let diff = file_diff(&metadata, Path::new("/project")).expect("diff");
        assert_eq!(
            diff.patch.expect("patch").text,
            "diff --git /project/src/main.py /project/src/main.py\n--- /project/src/main.py\n+++ /project/src/main.py\n@@ -1 +1 @@\n-a\n+b\n"
        );

        // Without a directory there is nothing to resolve against.
        assert!(file_diff(&metadata, Path::new("")).is_none());

        let locations = |input: Value| {
            tool_locations("read", &input, None, Path::new("/project"))
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
        assert!(tool_locations("read", &json!({"path": "a.rs"}), None, Path::new("")).is_empty());
        assert!(
            tool_locations(
                "read",
                &json!({"path": "src"}),
                Some("Read directory /project/src, entries 1-1\nmain.py"),
                Path::new("/project"),
            )
            .is_empty()
        );
    }
}
