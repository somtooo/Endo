//! Shell commands OpenCode runs, as Zed shows them. OpenCode runs them itself,
//! so each call gets a display-only terminal: its output streams in while the
//! command runs, then the call's result finishes it, or, for a command run in
//! the background, the notice OpenCode posts into the session when it ends.

use crate::client::OpenCodeClient;
use crate::translate::{EntryKey, ToolFamily};
use crate::types::{Message, MessageBody, ShellOutput, ShellStatus, ToolCall, ToolState};
use acp_thread::AcpThread;
use agent_client_protocol::v2::schema as acp;
use anyhow::Result;
use collections::HashMap;
use gpui::{App, AsyncApp, Task, WeakEntity};
use serde_json::Value;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use util::ResultExt as _;

/// One session's shell calls.
#[derive(Default)]
pub(crate) struct ShellTerminals {
    /// How background commands ended, by shell ID.
    finished: HashMap<String, ShellOutcome>,
    /// The message showing the call that started each background command,
    /// by shell ID, so the command's notice can redraw it.
    started_in: HashMap<String, String>,
    /// Calls whose output is followed, by tool call ID.
    followed: HashMap<String, FollowedShell>,
}

impl ShellTerminals {
    /// Records the commands that `messages` report ended, and returns the
    /// messages whose calls started them, which now render differently.
    pub(crate) fn record_notices(&mut self, messages: &[Message]) -> Vec<String> {
        let mut starters = Vec::new();
        for message in messages {
            let Some((shell_id, outcome)) = notice(message) else {
                continue;
            };
            if let Some(starter) = self.started_in.get(&shell_id) {
                starters.push(starter.clone());
            }
            self.finished.insert(shell_id, outcome);
        }
        starters
    }

    pub(crate) fn outcome(&self, shell_id: &str) -> Option<&ShellOutcome> {
        self.finished.get(shell_id)
    }

    pub(crate) fn has_streamed(&self, tool_id: &str) -> bool {
        self.followed
            .get(tool_id)
            .is_some_and(|followed| followed.output.streamed.get())
    }

    /// Call after rendering `tool`, which `message_id` shows. A call that's
    /// no longer running stops being followed, and one that just started
    /// running is returned, to be followed.
    pub(crate) fn track(&mut self, message_id: &str, tool: &ToolCall) -> Option<ShellToFollow> {
        if let Some(shell_id) = background_shell_id(tool) {
            self.started_in
                .insert(shell_id.to_string(), message_id.to_string());
        }
        // A finished call may no longer name its shell, so this can't depend
        // on the call still having a shell ID.
        let running = self.running_shell(tool);
        match (self.followed.get_mut(&tool.id), running) {
            // The render showed the final output, so streaming it as well
            // would repeat it.
            (Some(followed), None) if !followed.output.streamed.get() => {
                followed.output.closed.set(true);
                None
            }
            (None, Some(shell_id)) => Some(ShellToFollow {
                tool_id: tool.id.clone(),
                shell_id: shell_id.to_string(),
                terminal_id: terminal_id(tool),
            }),
            _ => None,
        }
    }

    /// Streams the command's output into the call's terminal until the
    /// command ends, or until the call's final output is shown first.
    pub(crate) fn follow(
        &mut self,
        shell: ShellToFollow,
        client: OpenCodeClient,
        directory: String,
        thread: WeakEntity<AcpThread>,
        cx: &mut App,
    ) {
        let ShellToFollow {
            tool_id,
            shell_id,
            terminal_id,
        } = shell;
        let output = Rc::<StreamedOutput>::default();
        let append = {
            let output = output.clone();
            move |text: &str, cx: &mut AsyncApp| {
                if let Follow::Stop = output.begin_append() {
                    return Follow::Stop;
                }
                let bytes = terminal_bytes(text);
                let appended = thread
                    .update(cx, |thread, cx| {
                        thread.append_display_terminal_output(terminal_id.clone(), &bytes, cx)
                    })
                    .is_ok_and(|result| result.log_err().is_some());
                if appended {
                    Follow::Continue
                } else {
                    Follow::Stop
                }
            }
        };
        let task =
            cx.spawn(async move |cx| follow_output(client, directory, shell_id, append, cx).await);
        self.followed.insert(
            tool_id,
            FollowedShell {
                output,
                _task: task,
            },
        );
    }

    /// Shells where `tool_id`'s output has been streamed.
    #[cfg(test)]
    pub(crate) fn with_streamed(tool_id: &str) -> Self {
        let output = Rc::<StreamedOutput>::default();
        output.streamed.set(true);
        let mut shells = Self::default();
        shells.followed.insert(
            tool_id.to_string(),
            FollowedShell {
                output,
                _task: Task::ready(()),
            },
        );
        shells
    }

    /// A foreground call reports its shell while running, a background call
    /// in its result until the completion notice arrives.
    fn running_shell<'a>(&self, tool: &'a ToolCall) -> Option<&'a str> {
        if ToolFamily::from_tool_name(&tool.name) != ToolFamily::Shell {
            return None;
        }
        let (metadata, running) = match &tool.state {
            ToolState::Running { metadata, .. } => (metadata, true),
            ToolState::Completed { metadata, .. } => (metadata, false),
            ToolState::Streaming { .. } | ToolState::Error { .. } | ToolState::Other => {
                return None;
            }
        };
        let shell_id = metadata.get("shellID").and_then(Value::as_str)?;
        let running =
            running || background_shell_id(tool).is_some() && !self.finished.contains_key(shell_id);
        running.then_some(shell_id)
    }
}

/// How a background command ended, from the notice OpenCode posts when it
/// finishes.
#[derive(Clone, Debug)]
pub(crate) struct ShellOutcome {
    pub completed: bool,
    pub output: String,
    pub exit: Option<i64>,
}

struct FollowedShell {
    output: Rc<StreamedOutput>,
    _task: Task<()>,
}

/// What a followed call's terminal was given, shared with the task that
/// streams into it.
#[derive(Default)]
struct StreamedOutput {
    /// Output was appended, so the final render keeps it: the result OpenCode
    /// reports is capped, so it could be shorter than what was shown.
    streamed: Cell<bool>,
    /// The final output was rendered before any streamed, so nothing more is
    /// appended.
    closed: Cell<bool>,
}

impl StreamedOutput {
    /// Whether more output may be appended. Asking marks it streamed.
    fn begin_append(&self) -> Follow {
        if self.closed.get() {
            return Follow::Stop;
        }
        self.streamed.set(true);
        Follow::Continue
    }
}

/// A running command whose output nobody follows yet.
pub(crate) struct ShellToFollow {
    tool_id: String,
    shell_id: String,
    terminal_id: acp::TerminalId,
}

enum Follow {
    Continue,
    Stop,
}

/// The shell a call started in the background, which keeps running after
/// the call returns.
pub(crate) fn background_shell_id(tool: &ToolCall) -> Option<&str> {
    if ToolFamily::from_tool_name(&tool.name) != ToolFamily::Shell {
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

pub(crate) fn terminal_id(tool: &ToolCall) -> acp::TerminalId {
    acp::TerminalId::new(EntryKey::Terminal.to_entry_id(&tool.id))
}

fn notice(message: &Message) -> Option<(String, ShellOutcome)> {
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
    let state = metadata.get("state").and_then(Value::as_str);
    Some((
        shell_id.to_string(),
        ShellOutcome {
            completed: state.is_none_or(|state| state == "completed"),
            output: output.to_string(),
            exit: metadata.get("exit").and_then(Value::as_i64),
        },
    ))
}

/// Streams a running command's output to `append`, like the TUI's shell
/// output view, until the command ends or `append` stops it.
async fn follow_output(
    client: OpenCodeClient,
    directory: String,
    shell_id: String,
    mut append: impl FnMut(&str, &mut AsyncApp) -> Follow,
    cx: &mut AsyncApp,
) {
    const POLL_INTERVAL: Duration = Duration::from_secs(1);
    const MAX_FAILURES: u32 = 5;

    let mut reader = OutputReader::default();
    let mut failures = 0;
    loop {
        match reader
            .read(&client, &directory, &shell_id, &mut append, cx)
            .await
        {
            Ok(Follow::Continue) => failures = 0,
            Ok(Follow::Stop) => return,
            Err(error) => {
                failures += 1;
                if failures >= MAX_FAILURES {
                    log::warn!("Stopped following OpenCode shell {shell_id}: {error:#}");
                    return;
                }
            }
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// Where reading from the end of a shell's output starts: the largest
/// position the server, which counts in JavaScript numbers, accepts.
const JS_MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Long logs (a dev server, say) replay only their end, and a poll reads at
/// most this much so fast output can't keep it busy.
const MAX_READ: u64 = 1024 * 1024;

#[derive(Default)]
struct OutputReader {
    /// Unset until the first read, which starts near the end.
    position: Option<u64>,
}

impl OutputReader {
    /// One poll: the output written since the last, then whether the command
    /// still runs.
    async fn read(
        &mut self,
        client: &OpenCodeClient,
        directory: &str,
        shell_id: &str,
        append: &mut impl FnMut(&str, &mut AsyncApp) -> Follow,
        cx: &mut AsyncApp,
    ) -> Result<Follow> {
        let Some(status) = client.fetch_shell_status(directory, shell_id).await? else {
            return Ok(Follow::Stop);
        };
        let mut replay_start = false;
        let mut position = match self.position {
            Some(position) => position,
            None => {
                // Reading at the end returns only the size.
                let Some(end) = client
                    .fetch_shell_output(directory, shell_id, JS_MAX_SAFE_INTEGER)
                    .await?
                else {
                    return Ok(Follow::Stop);
                };
                let start = end.size.saturating_sub(MAX_READ);
                replay_start = start > 0;
                start
            }
        };
        let read_until = position + MAX_READ;
        loop {
            let Some(page) = client
                .fetch_shell_output(directory, shell_id, position)
                .await?
            else {
                return Ok(Follow::Stop);
            };
            let (text, next) = whole_characters(&page, position, replay_start);
            replay_start = false;
            if !text.is_empty()
                && let Follow::Stop = append(text, cx)
            {
                return Ok(Follow::Stop);
            }
            let advanced = next > position;
            position = next;
            self.position = Some(position);
            if !advanced || page.cursor >= page.size || position >= read_until {
                break;
            }
        }
        Ok(if status == ShellStatus::Running {
            Follow::Continue
        } else {
            Follow::Stop
        })
    }
}

/// The server cuts output by bytes and decodes each page on its own, so a
/// character split across pages arrives as replacement characters. One cut at
/// the end is left for the next read, which starts at its first byte. One cut
/// at the start, from replaying only the end of a long log, is dropped.
/// Only a replay's first read can start mid-character: every later read
/// starts where a whole character does.
fn whole_characters(page: &ShellOutput, position: u64, replay_start: bool) -> (&str, u64) {
    const REPLACEMENT: char = '\u{FFFD}';
    let mut text = page.output.as_str();
    let cut_at_start = replay_start && text.starts_with(REPLACEMENT);
    if cut_at_start {
        text = text.trim_start_matches(REPLACEMENT);
    }
    let trimmed = text.trim_end_matches(REPLACEMENT);
    // With nothing else invalid, the kept text's length is its byte count.
    // After a cut start that count is unknown, so the read just moves on.
    if trimmed.len() < text.len() && !trimmed.contains(REPLACEMENT) && !cut_at_start {
        return (trimmed, position + trimmed.len() as u64);
    }
    (text, page.cursor)
}

/// A terminal needs carriage returns to start each line at column 0.
pub(crate) fn terminal_bytes(output: &str) -> Vec<u8> {
    output
        .replace("\r\n", "\n")
        .replace('\n', "\r\n")
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::ServiceRegistration;
    use crate::types::TextMessage;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn page(output: &str, cursor: u64) -> ShellOutput {
        ShellOutput {
            output: output.into(),
            cursor,
            size: cursor,
        }
    }

    fn shell_call(id: &str, state: Value) -> ToolCall {
        serde_json::from_value(json!({ "id": id, "name": "shell", "state": state }))
            .expect("tool call")
    }

    fn notice(shell_id: &str, state: &str) -> Message {
        Message {
            id: format!("msg_notice_{shell_id}"),
            body: MessageBody::Synthetic(TextMessage {
                text: format!(
                    "<shell id=\"job_1\" state=\"{state}\" command=\"make\">\nbuilt\nok\n</shell>"
                ),
                description: Some("make".into()),
                metadata: Some(json!({
                    "source": "shell",
                    "shellID": shell_id,
                    "state": state,
                    "exit": 0,
                })),
            }),
        }
    }

    fn followed(shells: &mut ShellTerminals, tool: &ToolCall) -> Option<String> {
        shells
            .track("msg_1", tool)
            .map(|to_follow| to_follow.shell_id)
    }

    #[test]
    fn follows_running_calls_only() {
        let mut shells = ShellTerminals::default();
        let foreground = shell_call(
            "call_1",
            json!({ "status": "running", "input": { "command": "make" }, "metadata": { "shellID": "sh_1" } }),
        );
        assert_eq!(followed(&mut shells, &foreground).as_deref(), Some("sh_1"));

        // How OpenCode reports a finished foreground command: no shell ID.
        let finished_foreground = shell_call(
            "call_2",
            json!({ "status": "completed", "input": { "command": "make" }, "content": [],
                    "metadata": { "status": "completed", "exit": 0 } }),
        );
        assert_eq!(followed(&mut shells, &finished_foreground), None);

        let background = shell_call(
            "call_3",
            json!({ "status": "completed", "input": { "command": "make", "background": true },
                    "content": [], "metadata": { "status": "running", "shellID": "sh_2" } }),
        );
        assert_eq!(followed(&mut shells, &background).as_deref(), Some("sh_2"));
        shells.record_notices(&[notice("sh_2", "completed")]);
        assert_eq!(followed(&mut shells, &background), None);
    }

    #[test]
    fn a_notice_finishes_its_command_and_redraws_the_call_that_started_it() {
        let mut shells = ShellTerminals::default();
        let background = shell_call(
            "call_1",
            json!({ "status": "completed", "input": { "command": "make", "background": true },
                    "content": [], "metadata": { "status": "running", "shellID": "sh_1" } }),
        );
        shells.track("msg_starter", &background);

        assert_eq!(
            shells.record_notices(&[notice("sh_1", "failed")]),
            vec!["msg_starter".to_string()]
        );
        let outcome = shells.outcome("sh_1").expect("an outcome");
        assert!(!outcome.completed);
        assert_eq!(outcome.output, "built\nok");
        assert_eq!(outcome.exit, Some(0));
    }

    #[test]
    fn streaming_stops_once_the_final_output_is_shown() {
        let mut shells = ShellTerminals::default();
        let running = shell_call(
            "call_1",
            json!({ "status": "running", "input": { "command": "make" }, "metadata": { "shellID": "sh_1" } }),
        );
        shells.track("msg_1", &running);
        let output = Rc::<StreamedOutput>::default();
        shells.followed.insert(
            "call_1".into(),
            FollowedShell {
                output: output.clone(),
                _task: Task::ready(()),
            },
        );
        let finished = shell_call(
            "call_1",
            json!({ "status": "completed", "input": { "command": "make" }, "content": [],
                    "metadata": { "status": "completed", "exit": 0 } }),
        );
        shells.track("msg_1", &finished);
        assert!(matches!(output.begin_append(), Follow::Stop));
        assert!(!shells.has_streamed("call_1"));
    }

    #[gpui::test]
    async fn follows_a_commands_output_until_it_ends(cx: &mut TestAppContext) {
        let polls = Arc::new(Mutex::new(0));
        let http = FakeHttpClient::create({
            let polls = polls.clone();
            move |request| {
                let polls = polls.clone();
                async move {
                    let path = request.uri().path().to_string();
                    let query = request.uri().query().unwrap_or_default().to_string();
                    let reply = if path == "/api/shell/sh_1" {
                        let mut polls = polls.lock().expect("polls lock");
                        *polls += 1;
                        let status = if *polls == 1 { "running" } else { "exited" };
                        json!({ "data": { "status": status } })
                    } else if query.ends_with("cursor=0") {
                        json!({ "data": { "output": "hello world", "cursor": 11, "size": 11 } })
                    } else {
                        json!({ "data": { "output": "", "cursor": 11, "size": 11 } })
                    };
                    Ok(Response::builder()
                        .status(200)
                        .body(AsyncBody::from(reply.to_string()))?)
                }
            }
        });
        let registration = ServiceRegistration {
            id: "a".into(),
            version: "2.2.5".into(),
            url: "http://127.0.0.1:4096".into(),
            pid: 1,
            password: String::new(),
        };
        let client = OpenCodeClient::new(http, &registration).expect("client");
        let appended = Arc::new(Mutex::new(Vec::<String>::new()));
        let task = cx.spawn({
            let appended = appended.clone();
            async move |mut cx| {
                follow_output(
                    client,
                    "/project".into(),
                    "sh_1".into(),
                    move |text: &str, _: &mut AsyncApp| {
                        appended.lock().expect("appended lock").push(text.into());
                        Follow::Continue
                    },
                    &mut cx,
                )
                .await
            }
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        task.await;
        assert_eq!(*appended.lock().expect("appended lock"), ["hello world"]);
        assert_eq!(*polls.lock().expect("polls lock"), 2);
    }

    #[test]
    fn shell_output_keeps_characters_whole() {
        // "héllo" with the read ending after the first byte of "é".
        assert_eq!(whole_characters(&page("h\u{FFFD}", 2), 0, false), ("h", 1));
        // Reading the held-back character again, once the rest arrived.
        assert_eq!(whole_characters(&page("éllo", 7), 1, false), ("éllo", 7));
        // ...or before it did: nothing yet, and the position stays put.
        assert_eq!(whole_characters(&page("\u{FFFD}", 2), 1, false), ("", 1));
        // A replay starting inside "é" drops the broken byte.
        assert_eq!(
            whole_characters(&page("\u{FFFD}llo", 7), 2, true),
            ("llo", 7)
        );
        assert_eq!(
            whole_characters(&page("plain\n", 6), 0, false),
            ("plain\n", 6)
        );
    }
}
