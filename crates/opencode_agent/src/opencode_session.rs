//! One open OpenCode D thread: Zed's copy of its session, kept in step with
//! the server, and what the thread shows of it.
//!
//! Nothing calls into the thread from inside this entity's update when the
//! thread can call back into the session. `AcpThread::rewind` runs the
//! truncate hook, which updates the session, so rewinds run from a task.

use crate::client::{Delivery, OpenCodeClient, PromptBody};
use crate::connection::{Activity, OpenCodeConnection};
use crate::form::{FormElicitation, FormReply};
use crate::project_setup::{ProjectSetup, ProjectSetupReplaced};
use crate::prompt::{Prompt, PromptKind};
use crate::session_messages::{ContentId, MessageStatus, SessionMessages, Snapshot};
use crate::session_selectors::{Choice, SessionChoices, SessionSelectors};
use crate::shell_terminals::ShellTerminals;
use crate::translate::{self, Upsert, to_client_message_id, to_message_id};
use crate::types::{
    Event, EventPayload, Form, Message, MessageBody, ModelRef, PermissionReply, PermissionRequest,
    ToolCall, ToolState, new_message_id,
};
use acp_thread::{
    AcpThread, AgentSessionTruncate, AuthorizationKind, ClientUserMessageId, ElicitationEntryId,
    PermissionOptions, ReceiptSessionSubmissions, ReceiptSubmission, RequestPermissionOutcome,
    SelectedPermissionOutcome,
};
use action_log::{ActionLog, ExternalEditKind};
use agent_client_protocol::v2::schema as acp;
use agent_ui::agent_ui_hooks::AgentSteering;
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, SharedString, Subscription, Task,
    TaskExt as _, WeakEntity,
};
use project::Project;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;
use util::ResultExt as _;

pub(crate) struct OpenCodeSession {
    id: String,
    connection: Weak<OpenCodeConnection>,
    client: OpenCodeClient,
    thread: WeakEntity<AcpThread>,
    /// Where the session works now. It starts as the folder it opened in and
    /// follows the session when it moves.
    directory: String,
    choices: SessionChoices,
    /// The directory's project setup, shared with the other sessions there.
    project_setup: Entity<ProjectSetup>,
    _project_setup_changes: Subscription,
    selectors: SessionSelectors,
    messages: SessionMessages,
    sync: ThreadSync,
    shown: ShownMessages,
    upserts: UpsertQueue,
    open_requests: OpenRequests,
    steers: Steers,
    shells: ShellTerminals,
    edits: ExternalEdits,
    usage: UsageReport,
    subagents: SubagentCards,
}

/// The session's agent and model, which ACP calls its config options. The
/// effort is the model's variant, as OpenCode stores it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SessionConfig {
    pub agent: String,
    pub model: ModelRef,
}

impl OpenCodeSession {
    pub(crate) fn subagent_card_message_id(&self, child_id: &str) -> Option<&str> {
        self.subagents
            .card_messages
            .get(child_id)
            .map(String::as_str)
    }

    pub(crate) fn set_directory(&mut self, directory: String) {
        self.directory = directory;
    }

    pub(crate) fn directory(&self) -> &str {
        &self.directory
    }

    #[cfg(test)]
    pub(crate) fn choices(&self) -> &SessionChoices {
        &self.choices
    }

    pub(crate) fn selectors(&self) -> &SessionSelectors {
        &self.selectors
    }

    pub(crate) fn project_setup(&self) -> &Entity<ProjectSetup> {
        &self.project_setup
    }

    /// A session that moved takes the project setup of the folder it moved to.
    pub(crate) fn use_project_setup(
        &mut self,
        directory: &str,
        project_setup: Entity<ProjectSetup>,
        cx: &mut Context<Self>,
    ) {
        // A later move already set another folder.
        if self.directory != directory || self.project_setup == project_setup {
            return;
        }
        let previous = self.project_setup.read(cx).clone();
        self._project_setup_changes =
            cx.subscribe(&project_setup, Self::follow_project_setup_change);
        self.project_setup = project_setup;
        self.apply_project_setup_change(&previous, cx);
    }

    fn follow_project_setup_change(
        &mut self,
        _: Entity<ProjectSetup>,
        event: &ProjectSetupReplaced,
        cx: &mut Context<Self>,
    ) {
        self.apply_project_setup_change(&event.previous, cx);
    }

    /// Shows the project setup's commands, and moves the thread off an agent that
    /// was deleted.
    fn apply_project_setup_change(&mut self, previous: &ProjectSetup, cx: &mut Context<Self>) {
        let project_setup = self.project_setup.read(cx);
        let current = self.choices.config().agent;
        // With no agent to switch to, the thread keeps its own.
        let replacement = (project_setup.find_agent(&current).is_none()
            && !project_setup.list_selectable_agents().is_empty())
        .then(|| project_setup.find_default_agent_id().to_string());
        let commands = project_setup.available_commands();
        self.choices.notify_changed(cx);
        self.thread
            .update(cx, |thread, cx| {
                thread.update_available_commands(commands, cx)
            })
            .log_err();
        let Some(replacement) = replacement else {
            return;
        };
        let notice = format!(
            "The agent \"{}\" was deleted, so this thread now uses {}.",
            previous.format_agent_name(&current),
            self.project_setup.read(cx).format_agent_name(&replacement),
        );
        let pick = self.choices.pick(Choice::Agent(replacement.clone()), cx);
        let thread = self.thread.clone();
        cx.spawn(async move |this, cx| {
            pick.await?;
            if this.read_with(cx, |this, _| this.choices.config().agent == replacement)? {
                thread.update(cx, |thread, cx| {
                    thread.push_notice(acp::Notice::new(acp::NoticeSeverity::Info, notice), cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn thread(&self) -> &WeakEntity<AcpThread> {
        &self.thread
    }

    pub(crate) fn handle_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match &event.payload {
            EventPayload::PermissionAsked { data } => {
                return self.open_requests.show_permission(data, &self.thread, cx);
            }
            EventPayload::PermissionReplied { data } => {
                return self.open_requests.close_permission(
                    &data.request_id,
                    data.reply,
                    &self.thread,
                    cx,
                );
            }
            EventPayload::FormCreated { data } => {
                return self.open_requests.show_form(&data.form, &self.thread, cx);
            }
            EventPayload::FormClosed { data } => {
                return self.open_requests.close_form(&data.id, &self.thread, cx);
            }
            EventPayload::InboxDelivered { data } => self.steers.finish(&data.inbox_id, true),
            EventPayload::InboxCancelled { data } => self.steers.finish(&data.inbox_id, false),
            _ => {}
        }
        if !self.sync.is_live() {
            return self.sync.buffer(event);
        }
        match self.messages.apply(event) {
            MessageStatus::Unchanged => {}
            MessageStatus::Changed(changed) => {
                self.follow_agent_and_model_switches(&changed, cx);
                self.sync_thread(changed, cx);
            }
            MessageStatus::Streamed {
                message_id,
                content,
            } => self.show_streamed_content(message_id, &content, cx),
            MessageStatus::NeedsResync(reason) => {
                log::info!("OpenCode session {} resyncs: {reason:?}", self.id);
                self.resync(cx).detach_and_log_err(cx);
            }
        }
    }

    fn sync_thread(&mut self, changed: Vec<String>, cx: &mut Context<Self>) {
        match self.shown.compare_with(&self.messages) {
            ThreadStatus::Matches => {}
            ThreadStatus::DiffersBeforeFirstUserMessage => {
                log::warn!(
                    "OpenCode session {} changed before its first prompt; \
                     the thread may show stale entries",
                    self.id
                );
            }
            ThreadStatus::DiffersFromUserMessage(index) => {
                return self.rewind_and_rebuild(index, cx);
            }
        }
        self.show_messages(&changed, cx);
        if self.sync.is_loading() {
            self.go_live(cx);
        }
    }

    /// Missed events aren't replayed, so everything they could have changed
    /// is fetched again.
    pub(crate) fn catch_up_after_reconnect(&mut self, cx: &mut Context<Self>) {
        self.steers.recheck(cx);
        self.resync(cx).detach_and_log_err(cx);
    }

    pub(crate) fn resync(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let generation = self.sync.start_resync();
        let client = self.client.clone();
        let session_id = self.id.clone();
        let connection = self.connection.clone();
        cx.spawn(async move |this, cx| {
            let snapshot = fetch_snapshot(&client, &session_id).await;
            let snapshot = match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    // The copy is as it was, so the events still apply. A gap
                    // in them starts another resync.
                    this.update(cx, |this, cx| {
                        if this.sync.is_current(generation) && this.sync.is_loading() {
                            this.go_live(cx);
                        }
                    })?;
                    return Err(error);
                }
            };
            // Before the snapshot is shown, so subagent cards render with
            // the activity it reports. Outside this session's update, since
            // it redraws other sessions' cards. An execution event that
            // arrived meanwhile already set a newer activity: a stop's
            // reload can see the turn still running just before it ends.
            let activity = if snapshot.running {
                Activity::Running
            } else {
                Activity::Idle
            };
            // A later resync started meanwhile, and its snapshot is newer.
            let (current, newer_activity) = this.read_with(cx, |this, _| {
                (
                    this.sync.is_current(generation),
                    this.sync.holds_execution_event(),
                )
            })?;
            if !current {
                return Ok(());
            }
            if !newer_activity {
                cx.update(|cx| {
                    if let Some(connection) = connection.upgrade() {
                        connection.set_activity(&session_id, activity, cx);
                    }
                });
            }
            // A move missed while the stream was down. Before the snapshot is
            // shown, since its file paths resolve against the session's folder.
            let moved_to = this.read_with(cx, |this, _| {
                snapshot
                    .directory
                    .clone()
                    .filter(|directory| *directory != this.directory)
            })?;
            if let Some(directory) = moved_to {
                cx.update(|cx| {
                    if let Some(connection) = connection.upgrade() {
                        connection.follow_session_move(&session_id, &directory, cx);
                    }
                });
            }
            let (permissions, forms) =
                this.update(cx, |this, cx| this.apply_snapshot(snapshot, cx))?;
            // Each prompt adds a thread entry, and the agent panel expects at
            // most one new entry per update.
            for permission in permissions {
                this.update(cx, |this, cx| {
                    this.open_requests
                        .show_permission(&permission, &this.thread, cx)
                })?;
            }
            for form in forms {
                this.update(cx, |this, cx| {
                    this.open_requests.show_form(&form, &this.thread, cx)
                })?;
            }
            Ok(())
        })
    }

    fn go_live(&mut self, cx: &mut Context<Self>) {
        for event in self.sync.release_held_events() {
            self.handle_event(event, cx);
        }
    }

    /// Returns the asks the server still has open, to show one at a time.
    fn apply_snapshot(
        &mut self,
        snapshot: FetchedSnapshot,
        cx: &mut Context<Self>,
    ) -> (Vec<PermissionRequest>, Vec<Form>) {
        let changed = self.messages.replace_snapshot(snapshot.messages);
        // A title generated while the stream was down arrives this way.
        if let Some(title) = snapshot.title
            && let Some(thread) = self.thread.upgrade()
            && thread.read(cx).title().as_deref() != Some(title.as_str())
        {
            thread.update(cx, |thread, cx| {
                thread.update_session_info(acp::SessionInfoUpdate::new().title(title), cx)
            });
        }
        // Another client may have switched them while the stream was down.
        if let Some(agent) = snapshot.agent {
            self.choices.confirm(Choice::Agent(agent), cx);
        }
        if let Some(model) = snapshot.model {
            self.choices.confirm(Choice::Model(model), cx);
        }
        // A rewind already in progress shows the new snapshot when it's done.
        if !self.sync.is_rewinding() {
            self.sync_thread(changed, cx);
        }
        self.open_requests.close_answered(
            &snapshot.permissions,
            snapshot.forms.as_deref(),
            &self.messages,
            &self.thread,
            cx,
        );
        (snapshot.permissions, snapshot.forms.unwrap_or_default())
    }

    fn rewind_and_rebuild(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(thread) = self.thread.upgrade() else {
            return;
        };
        let Some(target) = self.forget_shown_from(index).into_iter().next() else {
            return;
        };
        self.sync.start_rewind();
        // Rewinding cancels the prompts it removes. Forgetting them first
        // keeps that from rejecting them on the server; they're asked again
        // below.
        let withdrew_permissions = self.open_requests.withdraw_permissions();
        let client = self.client.clone();
        let session_id = self.id.clone();
        cx.spawn(async move |this, cx| {
            // The server already cut its history, so only the thread's
            // entries go.
            if let Ok(client_id) = to_client_message_id(&target) {
                thread.update(cx, |thread, cx| thread.remove_entries_from(&client_id, cx));
            }
            this.update(cx, |this, cx| {
                this.show_messages(&[], cx);
                this.go_live(cx);
            })?;
            if withdrew_permissions {
                let permissions = client.list_pending_permissions(&session_id).await?;
                for permission in permissions {
                    this.update(cx, |this, cx| {
                        this.open_requests
                            .show_permission(&permission, &this.thread, cx)
                    })?;
                }
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Forgets the shown messages from `index` on, with their changes not
    /// applied yet. Returns their IDs, in thread order.
    fn forget_shown_from(&mut self, index: usize) -> Vec<String> {
        let removed = self.shown.forget_from(index);
        self.upserts.forget(&removed);
        removed
    }

    /// Shows the messages the thread doesn't have yet, and redraws `changed`.
    pub(crate) fn show_messages(&mut self, changed: &[String], cx: &mut Context<Self>) {
        let to_show = self.collect_messages_to_show(changed);
        self.queue_messages(&to_show, cx);
        let new_subagents = self.track_tool_calls(&to_show, cx);
        // Before the queue is applied: the turn's stats are final once the
        // thread goes idle.
        self.usage
            .report_output_tokens(self.messages.visible_messages(), &self.thread, cx);
        self.apply_queued(cx);
        self.usage.report_context_usage(
            self.messages.visible_messages(),
            &self.choices.config().model,
            &self.project_setup,
            &self.thread,
            cx,
        );
        for child_id in new_subagents {
            self.open_subagent(child_id, cx);
        }
    }

    fn collect_messages_to_show(&mut self, changed: &[String]) -> Vec<Message> {
        self.shown.forget_delivered(&self.messages);
        let visible = self.messages.visible_messages();
        let new_messages = self.shown.not_shown(visible).to_vec();
        let find_shown = |id: &String| {
            self.shown
                .contains(id)
                .then(|| visible.iter().find(|message| &message.id == id))
                .flatten()
        };
        let mut to_show: Vec<Message> = changed.iter().filter_map(find_shown).cloned().collect();
        // A background command's notice updates the call that started it.
        let starters: Vec<String> = self
            .shells
            .record_notices(&to_show)
            .into_iter()
            .chain(self.shells.record_notices(&new_messages))
            .collect();
        for starter in &starters {
            if !to_show.iter().any(|message| &message.id == starter)
                && let Some(message) = find_shown(starter)
            {
                to_show.push(message.clone());
            }
        }
        for message in &new_messages {
            self.shown.push(message);
        }
        to_show.extend(new_messages);
        to_show
    }

    fn queue_messages(&mut self, messages: &[Message], cx: &mut Context<Self>) {
        let repeated_notices = translate::find_repeated_notices(self.messages.visible_messages());
        let queued = self.with_translate_context(cx, |context| {
            let mut queued = Vec::new();
            for message in messages {
                match translate::translate_message(message, context) {
                    Ok(upserts) => queued.extend(
                        upserts
                            .into_iter()
                            .filter(|upsert| match upsert {
                                Upsert::Assistant(message) => {
                                    !repeated_notices.contains(message.message_id.0.as_ref())
                                }
                                _ => true,
                            })
                            .map(|upsert| (message.id.clone(), upsert)),
                    ),
                    Err(error) => log::warn!("Can't translate OpenCode message: {error:#}"),
                }
            }
            queued
        });
        self.upserts.extend(queued.into_iter().flatten());
        cx.notify();
    }

    /// A delta changes one item of a message, so only that item is shown
    /// again: what else a change can affect (the order of messages, tool
    /// calls starting, usage) stays as it was.
    fn show_streamed_content(
        &mut self,
        message_id: String,
        content: &ContentId,
        cx: &mut Context<Self>,
    ) {
        if !self.shown.contains(&message_id) {
            return self.sync_thread(vec![message_id], cx);
        }
        let upserts = self.with_translate_context(cx, |context| {
            let message = self.messages.message(&message_id)?;
            Some(translate::translate_reply(message, content, context))
        });
        let upserts = upserts.flatten().unwrap_or_default();
        self.upserts.extend(
            upserts
                .into_iter()
                .map(|upsert| (message_id.clone(), upsert)),
        );
        self.apply_queued(cx);
    }

    fn with_translate_context<R>(
        &self,
        cx: &App,
        translate: impl FnOnce(&translate::TranslateContext) -> R,
    ) -> Option<R> {
        let connection = self.connection.upgrade()?;
        let pending_permissions = self.open_requests.tool_ids_awaiting_permission();
        let activity = connection.activity_by_session();
        let project_setup = self.project_setup.read(cx);
        let context = translate::TranslateContext {
            tool_call_ids_awaiting_permission: &pending_permissions,
            session_activity: &activity,
            shell_terminals: &self.shells,
            skills: &project_setup.skills,
            directory: Path::new(&self.directory),
        };
        Some(translate(&context))
    }

    /// Reports finished edits, follows running shells, and returns the
    /// subagent sessions `messages` start.
    fn track_tool_calls(&mut self, messages: &[Message], cx: &mut Context<Self>) -> Vec<String> {
        let mut edited_files = Vec::new();
        let mut new_subagents = Vec::new();
        for message in messages {
            for tool in message.tool_calls() {
                edited_files.extend(self.edits.track(tool, Path::new(&self.directory)));
                if let Some(shell) = self.shells.track(&message.id, tool) {
                    self.shells.follow(
                        shell,
                        self.client.clone(),
                        self.directory.clone(),
                        self.thread.clone(),
                        cx,
                    );
                }
            }
            new_subagents.extend(self.subagents.track(message));
        }
        self.edits.report(edited_files, &self.thread, cx);
        new_subagents
    }

    /// Opens a subagent's session as a child of this thread, so the subagent
    /// card can show its work.
    fn open_subagent(&mut self, child_id: String, cx: &mut Context<Self>) {
        if self
            .connection
            .upgrade()
            .is_none_or(|connection| connection.session(&child_id).is_some())
        {
            return;
        }
        let client = self.client.clone();
        let parent_config = self.choices.config();
        cx.spawn(async move |this, cx| {
            // The subagent runs its own agent and model, which the parent's
            // config doesn't describe.
            let info = client.fetch_session(&child_id).await?;
            let config = SessionConfig {
                agent: info.agent.unwrap_or(parent_config.agent),
                model: info.model.unwrap_or(parent_config.model),
            };
            this.update(cx, |this, cx| {
                this.open_subagent_thread(child_id, config, cx)
            })?
            .await
        })
        .detach_and_log_err(cx);
    }

    fn open_subagent_thread(
        &mut self,
        child_id: String,
        config: SessionConfig,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let (Some(connection), Some(parent)) = (self.connection.upgrade(), self.thread.upgrade())
        else {
            return Task::ready(Ok(()));
        };
        // Asked again while the session's details loaded.
        if connection.session(&child_id).is_some() {
            return Task::ready(Ok(()));
        }
        let (agent_connection, project, parent_action_log, parent_session_id) = {
            let parent = parent.read(cx);
            (
                parent.connection().clone(),
                parent.project().clone(),
                parent.action_log().clone(),
                parent.session_id().clone(),
            )
        };
        let action_log =
            cx.new(|_| ActionLog::new(project.clone()).with_linked_action_log(parent_action_log));
        let child = cx.new(|cx| {
            let mut child = AcpThread::new(
                Some(parent_session_id),
                None,
                None,
                agent_connection,
                project,
                action_log,
                acp::SessionId::new(child_id.as_str()),
                watch::Receiver::constant(acp::PromptCapabilities::default()),
                cx,
            )
            .agent_owned();
            // OpenCode can interrupt any session, so Zed's Stop Subagent
            // button may cancel it.
            child.set_subagent_association(acp_thread::subagents::SubagentAssociation {
                can_cancel: true,
                ..Default::default()
            });
            child
        });
        let session = connection.register_session(
            &child,
            &child_id,
            config,
            self.directory.clone(),
            self.project_setup.clone(),
            cx,
        );
        let resync = session.update(cx, |session, cx| session.resync(cx));
        cx.spawn(async move |_, cx| {
            let result = resync.await;
            // Registering creates the subagent's view, so the snapshot above
            // went in before anything was watching.
            parent.update(cx, |parent, cx| parent.register_subagent(child, cx));
            session.update(cx, |session, cx| session.start_live_updates(cx));
            result
        })
    }

    pub(crate) fn new(
        connection: &Rc<OpenCodeConnection>,
        id: String,
        thread: &Entity<AcpThread>,
        config: SessionConfig,
        directory: String,
        project_setup: Entity<ProjectSetup>,
        cx: &mut Context<Self>,
    ) -> Self {
        let client = connection.client().clone();
        let choices = SessionChoices::new(id.clone(), client.clone(), thread, config);
        Self {
            selectors: SessionSelectors::new(choices.clone(), thread, cx),
            choices,
            open_requests: OpenRequests::new(id.clone(), client.clone()),
            steers: Steers::new(id.clone(), client.clone()),
            _project_setup_changes: cx.subscribe(&project_setup, Self::follow_project_setup_change),
            project_setup,
            messages: SessionMessages::new(id.as_str()),
            id,
            connection: Rc::downgrade(connection),
            client,
            thread: thread.downgrade(),
            directory,
            sync: ThreadSync::default(),
            shown: ShownMessages::default(),
            upserts: UpsertQueue::default(),
            shells: ShellTerminals::default(),
            edits: ExternalEdits::default(),
            usage: UsageReport::default(),
            subagents: SubagentCards::default(),
        }
    }

    /// Hands the thread to the UI: from now on, changes go in one new entry
    /// per update, and whatever arrived while loading goes out that way.
    pub(crate) fn start_live_updates(&mut self, cx: &mut Context<Self>) {
        self.upserts.mode = ApplyMode::OneEntryPerUpdate;
        self.apply_queued(cx);
    }

    /// A rewind leaves the queue alone; it applies the queue once it's done.
    fn apply_queued(&mut self, cx: &mut Context<Self>) {
        if self.sync.is_rewinding() {
            return;
        }
        let Some(thread) = self.thread.upgrade() else {
            return;
        };
        match self.upserts.mode {
            ApplyMode::AllAtOnce => {
                self.upserts.consume(&thread, cx);
            }
            // The caller's update may already have added an entry, so the
            // queue is consumed in updates of its own.
            ApplyMode::OneEntryPerUpdate => {
                if self.upserts.consumer.is_some() || self.upserts.is_empty() {
                    return;
                }
                self.upserts.consumer = Some(cx.spawn(async move |this, cx| {
                    while let Ok(Some(QueueProgress::MoreLeft)) = this.update(cx, |this, cx| {
                        let thread = this
                            .thread
                            .upgrade()
                            .filter(|_| !this.sync.is_rewinding())?;
                        Some(this.upserts.consume(&thread, cx))
                    }) {}
                    this.update(cx, |this, _| this.upserts.consumer = None)
                        .log_err();
                }));
            }
        }
    }

    fn follow_agent_and_model_switches(&mut self, changed: &[String], cx: &mut Context<Self>) {
        for message in changed.iter().filter_map(|id| self.messages.message(id)) {
            match &message.body {
                MessageBody::AgentSwitched { agent } => {
                    self.choices.confirm(Choice::Agent(agent.clone()), cx);
                }
                MessageBody::ModelSwitched { model } => {
                    self.choices.confirm(Choice::Model(model.clone()), cx);
                }
                _ => {}
            }
        }
    }

    /// The latest user message the thread shows, waiting up to `max_wait`
    /// for one.
    async fn wait_for_latest_user_message(
        this: &WeakEntity<Self>,
        max_wait: Duration,
        cx: &mut AsyncApp,
    ) -> Option<String> {
        const POLL_INTERVAL: Duration = Duration::from_millis(100);
        let mut waited = Duration::ZERO;
        loop {
            let latest = this
                .read_with(cx, |this, _| this.shown.latest_user_message_id())
                .ok()
                .flatten();
            if latest.is_some() || waited >= max_wait {
                return latest;
            }
            cx.background_executor().timer(POLL_INTERVAL).await;
            waited += POLL_INTERVAL;
        }
    }
}

/// While a snapshot loads or the thread is rebuilt, events wait and are
/// replayed afterwards. Applying an event twice is harmless.
struct ThreadSync {
    state: SyncState,
    /// Counts resyncs, so only the latest one's snapshot is applied.
    generation: u64,
}

impl Default for ThreadSync {
    fn default() -> Self {
        Self {
            state: SyncState::Loading { buffer: Vec::new() },
            generation: 0,
        }
    }
}

impl ThreadSync {
    fn is_live(&self) -> bool {
        matches!(self.state, SyncState::Live)
    }

    /// Keeps `event` until the thread is live again.
    fn buffer(&mut self, event: Event) {
        match &mut self.state {
            SyncState::Loading { buffer } | SyncState::Rewinding { buffer } => buffer.push(event),
            SyncState::Live => {
                log::error!("an OpenCode event was buffered while the thread is live")
            }
        }
    }

    /// Returns the resync's generation. A rewind in progress keeps going.
    fn start_resync(&mut self) -> u64 {
        if let SyncState::Live = self.state {
            self.state = SyncState::Loading { buffer: Vec::new() };
        }
        self.generation += 1;
        self.generation
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn is_loading(&self) -> bool {
        matches!(self.state, SyncState::Loading { .. })
    }

    fn is_rewinding(&self) -> bool {
        matches!(self.state, SyncState::Rewinding { .. })
    }

    /// Events held back while loading stay held until the rewind is done.
    fn start_rewind(&mut self) {
        let buffer = match std::mem::replace(&mut self.state, SyncState::Live) {
            SyncState::Loading { buffer } | SyncState::Rewinding { buffer } => buffer,
            SyncState::Live => Vec::new(),
        };
        self.state = SyncState::Rewinding { buffer };
    }

    /// Makes the thread live, returning the events held back to replay.
    fn release_held_events(&mut self) -> Vec<Event> {
        match std::mem::replace(&mut self.state, SyncState::Live) {
            SyncState::Loading { buffer } | SyncState::Rewinding { buffer } => buffer,
            SyncState::Live => Vec::new(),
        }
    }

    fn holds_execution_event(&self) -> bool {
        let (SyncState::Loading { buffer } | SyncState::Rewinding { buffer }) = &self.state else {
            return false;
        };
        buffer.iter().any(|event| {
            matches!(
                event.payload,
                EventPayload::ExecutionStarted {}
                    | EventPayload::ExecutionSucceeded {}
                    | EventPayload::ExecutionFailed { .. }
                    | EventPayload::ExecutionInterrupted { .. }
            )
        })
    }
}

enum SyncState {
    Loading { buffer: Vec<Event> },
    Rewinding { buffer: Vec<Event> },
    Live,
}

/// What the thread shows of the session.
#[derive(Default)]
struct ShownMessages {
    /// In thread order.
    ids: Vec<String>,
    /// The same IDs, to look one up.
    id_set: HashSet<String>,
    /// The shown messages that are editable user messages: the only places
    /// the thread can be rewound to.
    user_message_ids: HashSet<String>,
    /// Messages Zed sent that the server hasn't delivered yet. The server
    /// starts the turn before it delivers the prompt, so until then the
    /// thread shows a message the session's messages don't have.
    awaiting_delivery: HashSet<String>,
}

impl ShownMessages {
    /// A prompt Zed shows before the server has it.
    fn push_sent(&mut self, message_id: &str) {
        self.ids.push(message_id.to_string());
        self.id_set.insert(message_id.to_string());
        self.user_message_ids.insert(message_id.to_string());
        self.awaiting_delivery.insert(message_id.to_string());
    }

    fn push(&mut self, message: &Message) {
        self.ids.push(message.id.clone());
        self.id_set.insert(message.id.clone());
        if matches!(message.body, MessageBody::User(_)) {
            self.user_message_ids.insert(message.id.clone());
        }
    }

    /// Returns the IDs forgotten, in thread order.
    fn forget_from(&mut self, index: usize) -> Vec<String> {
        let Some(removed) = self.ids.get(index..).map(<[String]>::to_vec) else {
            return Vec::new();
        };
        self.ids.truncate(index);
        for id in &removed {
            self.id_set.remove(id);
            self.user_message_ids.remove(id);
            self.awaiting_delivery.remove(id);
        }
        removed
    }

    fn forget_delivered(&mut self, messages: &SessionMessages) {
        self.awaiting_delivery.retain(|id| !messages.contains(id));
    }

    /// The end of `visible` the thread doesn't show yet.
    fn not_shown<'a>(&self, visible: &'a [Message]) -> &'a [Message] {
        visible.get(self.ids.len()..).unwrap_or_default()
    }

    fn contains(&self, message_id: &str) -> bool {
        self.id_set.contains(message_id)
    }

    fn position(&self, message_id: &str) -> Option<usize> {
        self.ids.iter().position(|id| id == message_id)
    }

    fn latest_user_message_id(&self) -> Option<String> {
        self.ids
            .iter()
            .rev()
            .find(|id| self.user_message_ids.contains(*id))
            .cloned()
    }

    /// Messages at the end that Zed sent and the server hasn't delivered yet
    /// still match.
    fn compare_with(&self, messages: &SessionMessages) -> ThreadStatus {
        let visible: Vec<&str> = messages
            .visible_messages()
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        let awaiting = self
            .ids
            .iter()
            .rev()
            .take_while(|id| self.awaiting_delivery.contains(*id) && !messages.contains(id))
            .count();
        let mismatch = self.ids[..self.ids.len() - awaiting]
            .iter()
            .enumerate()
            .position(|(ix, id)| visible.get(ix) != Some(&id.as_str()));
        let Some(ix) = mismatch else {
            return ThreadStatus::Matches;
        };
        match self.ids[..=ix]
            .iter()
            .rposition(|id| self.user_message_ids.contains(id))
        {
            Some(rewind_to) => ThreadStatus::DiffersFromUserMessage(rewind_to),
            None => ThreadStatus::DiffersBeforeFirstUserMessage,
        }
    }
}

#[derive(Debug, PartialEq)]
enum ThreadStatus {
    Matches,
    /// The index of the user message to rewind to.
    DiffersFromUserMessage(usize),
    DiffersBeforeFirstUserMessage,
}

/// Thread changes not applied yet, with the message each belongs to.
///
/// The agent panel's `NewEntry` handler takes the thread's last entry as the
/// new one (`ConversationView`'s `AcpThreadEvent::NewEntry` arm), and GPUI
/// delivers events after the whole update. So each `thread.update` may add at
/// most one entry, or the panel builds views for the wrong entries. ACP agents
/// get this for free, as their updates are small. OpenCode sends whole
/// messages, so they are split here. Effects flush only when the outermost
/// update ends, which is why the split runs in its own task.
#[derive(Default)]
struct UpsertQueue {
    upserts: VecDeque<(String, Upsert)>,
    mode: ApplyMode,
    /// The task consuming the queue one entry per update.
    consumer: Option<Task<()>>,
}

impl UpsertQueue {
    fn extend(&mut self, upserts: impl IntoIterator<Item = (String, Upsert)>) {
        self.upserts.extend(upserts);
    }

    fn forget(&mut self, message_ids: &[String]) {
        self.upserts.retain(|(id, _)| !message_ids.contains(id));
    }

    fn is_empty(&self) -> bool {
        self.upserts.is_empty()
    }

    /// Applies what fits in one update of the thread, at the mode's pace.
    fn consume(&mut self, thread: &Entity<AcpThread>, cx: &mut App) -> QueueProgress {
        match self.mode {
            ApplyMode::AllAtOnce if !self.upserts.is_empty() => {
                let upserts: Vec<(String, Upsert)> = self.upserts.drain(..).collect();
                thread.update(cx, |thread, cx| {
                    for (_, upsert) in upserts {
                        upsert.apply(thread, cx).log_err();
                    }
                });
            }
            ApplyMode::AllAtOnce => {}
            ApplyMode::OneEntryPerUpdate => {
                while let Some((_, upsert)) = self.upserts.pop_front() {
                    let added = thread.update(cx, |thread, cx| {
                        let entries = thread.entries().len();
                        upsert.apply(thread, cx).log_err();
                        thread.entries().len() > entries
                    });
                    if added {
                        break;
                    }
                }
            }
        }
        if self.upserts.is_empty() {
            QueueProgress::Empty
        } else {
            QueueProgress::MoreLeft
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum ApplyMode {
    /// Before the thread is handed to the UI, changes go in all at once.
    #[default]
    AllAtOnce,
    OneEntryPerUpdate,
}

#[derive(PartialEq)]
enum QueueProgress {
    MoreLeft,
    Empty,
}

/// The permission requests and forms the server asked, as the thread shows
/// them, and the replies sent back.
struct OpenRequests {
    session_id: String,
    client: OpenCodeClient,
    /// By request ID.
    permissions: HashMap<String, ShownPermission>,
    /// Numbers each permission prompt, so a prompt withdrawn by a rebuild
    /// can't answer for the one that replaces it.
    next_generation: u64,
    /// Open forms, to the elicitation showing each.
    forms: HashMap<String, ElicitationEntryId>,
}

impl OpenRequests {
    fn new(session_id: String, client: OpenCodeClient) -> Self {
        Self {
            session_id,
            client,
            permissions: HashMap::default(),
            next_generation: 0,
            forms: HashMap::default(),
        }
    }

    /// The tool calls waiting for a permission reply.
    fn tool_ids_awaiting_permission(&self) -> HashSet<String> {
        self.permissions
            .values()
            .map(|permission| permission.tool_id.clone())
            .collect()
    }

    /// Forgets the permission prompts shown, so closing them doesn't reject
    /// them on the server. Returns whether there were any.
    fn withdraw_permissions(&mut self) -> bool {
        let withdrew = !self.permissions.is_empty();
        self.permissions.clear();
        withdrew
    }

    /// Forms Zed can't show are left for another client to answer.
    fn show_form(
        &mut self,
        form: &Form,
        thread: &WeakEntity<AcpThread>,
        cx: &mut Context<OpenCodeSession>,
    ) {
        let form_id = form.id.clone();
        if self.forms.contains_key(&form_id) {
            return;
        }
        let Some(elicitation) = FormElicitation::new(form, &self.session_id) else {
            log::info!(
                "OpenCode form {form_id} can't be shown in Zed; leaving it to other clients"
            );
            return;
        };
        let shown = thread.update(cx, |thread, cx| {
            thread.request_elicitation_with_id(elicitation.request.clone(), cx)
        });
        let (elicitation_id, response) = match shown {
            Ok(Ok(shown)) => shown,
            Ok(Err(error)) => {
                log::warn!("Can't show OpenCode form {form_id}: {error:?}");
                return;
            }
            Err(_) => return,
        };
        self.forms.insert(form_id.clone(), elicitation_id);
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        cx.spawn(async move |this, cx| {
            let response = response.await;
            // Answered in another client already: the server closed it.
            let still_open = this.update(cx, |this, _| {
                this.open_requests.forms.remove(&form_id).is_some()
            })?;
            if !still_open {
                return anyhow::Ok(());
            }
            match elicitation.answer(&response) {
                FormReply::Answer(answer) => {
                    client.reply_form(&session_id, &form_id, &answer).await
                }
                FormReply::Decline => client.cancel_form(&session_id, &form_id, None).await,
            }
        })
        .detach_and_log_err(cx);
    }

    fn close_form(&mut self, form_id: &str, thread: &WeakEntity<AcpThread>, cx: &mut App) {
        let Some(elicitation_id) = self.forms.remove(form_id) else {
            return;
        };
        thread
            .update(cx, |thread, cx| {
                thread.cancel_elicitation(&elicitation_id, cx)
            })
            .log_err();
    }

    fn show_permission(
        &mut self,
        request: &PermissionRequest,
        thread: &WeakEntity<AcpThread>,
        cx: &mut Context<OpenCodeSession>,
    ) {
        let request_id = request.id.clone();
        if self.permissions.contains_key(&request_id) {
            return;
        }
        let Some(thread) = thread.upgrade() else {
            return;
        };
        let tool_id = request.tool_call_id().unwrap_or(&request_id).to_string();
        let generation = self.next_generation;
        self.next_generation += 1;
        self.permissions.insert(
            request_id.clone(),
            ShownPermission {
                tool_id: tool_id.clone(),
                generation,
            },
        );
        let mut tool_call = acp::ToolCallUpdate::new(tool_id.as_str());
        if request.tool_call_id().is_none() {
            // Not tied to a tool call the thread shows; give it its own card.
            let action = &request.action;
            tool_call = tool_call
                .name(action.clone())
                .title(format!("{action}: {}", request.resources.join(", ")))
                .kind(acp::ToolKind::Other);
        }
        let options = PermissionOptions::Flat(
            PermissionReply::ALL
                .into_iter()
                .map(|reply| {
                    let (label, kind) = to_permission_option(reply);
                    acp::PermissionOption::new(reply.as_str(), label, kind)
                })
                .collect(),
        );
        let outcome = thread.update(cx, |thread, cx| {
            thread.request_tool_call_update_authorization(
                tool_call,
                options,
                AuthorizationKind::PermissionGrant,
                cx,
            )
        });
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                log::warn!("Can't show OpenCode permission request: {error:#}");
                self.permissions.remove(&request_id);
                return;
            }
        };
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        cx.spawn(async move |this, cx| {
            let reply = match outcome.await {
                RequestPermissionOutcome::Selected(selected) => PermissionReply::ALL
                    .into_iter()
                    .find(|reply| reply.as_str() == selected.option_id.0.as_ref())
                    .unwrap_or(PermissionReply::Reject),
                RequestPermissionOutcome::Cancelled
                | RequestPermissionOutcome::InterruptedByFollowUp => PermissionReply::Reject,
            };
            // Answered by another client already: the server closed it. Or
            // withdrawn by a rebuild, which shows the request again.
            let still_open = this.update(cx, |this, _| {
                let shown = this.open_requests.permissions.get(&request_id);
                let current = shown.is_some_and(|shown| shown.generation == generation);
                current && this.open_requests.permissions.remove(&request_id).is_some()
            })?;
            if still_open {
                client
                    .reply_permission(&session_id, &request_id, reply)
                    .await?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn close_permission(
        &mut self,
        request_id: &str,
        reply: PermissionReply,
        thread: &WeakEntity<AcpThread>,
        cx: &mut App,
    ) {
        let Some(permission) = self.permissions.remove(request_id) else {
            return;
        };
        let (_, kind) = to_permission_option(reply);
        thread
            .update(cx, |thread, cx| {
                thread.authorize_tool_call(
                    acp::ToolCallId::new(permission.tool_id.as_str()),
                    SelectedPermissionOutcome::new(
                        acp::PermissionOptionId::new(reply.as_str()),
                        kind,
                    ),
                    cx,
                );
            })
            .log_err();
    }

    /// Closes asks the server no longer has, which another client answered
    /// while the event stream was down. A permission counts as allowed if its
    /// tool went on to run.
    fn close_answered(
        &mut self,
        open_permissions: &[PermissionRequest],
        open_forms: Option<&[Form]>,
        messages: &SessionMessages,
        thread: &WeakEntity<AcpThread>,
        cx: &mut App,
    ) {
        let open_permission_ids: HashSet<&str> = open_permissions
            .iter()
            .map(|permission| permission.id.as_str())
            .collect();
        let open_form_ids: Option<HashSet<&str>> =
            open_forms.map(|forms| forms.iter().map(|form| form.id.as_str()).collect());
        let answered_permissions: Vec<(String, PermissionReply)> = self
            .permissions
            .iter()
            .filter(|(request_id, _)| !open_permission_ids.contains(request_id.as_str()))
            .map(|(request_id, permission)| {
                let ran = messages.tool(&permission.tool_id).is_some_and(|tool| {
                    matches!(
                        tool.state,
                        ToolState::Running { .. } | ToolState::Completed { .. }
                    )
                });
                let reply = if ran {
                    PermissionReply::Once
                } else {
                    PermissionReply::Reject
                };
                (request_id.clone(), reply)
            })
            .collect();
        let answered_forms: Vec<String> = match &open_form_ids {
            Some(open_form_ids) => self
                .forms
                .keys()
                .filter(|form_id| !open_form_ids.contains(form_id.as_str()))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        for (request_id, reply) in answered_permissions {
            self.close_permission(&request_id, reply, thread, cx);
        }
        for form_id in answered_forms {
            self.close_form(&form_id, thread, cx);
        }
    }
}

struct ShownPermission {
    tool_id: String,
    generation: u64,
}

fn to_permission_option(reply: PermissionReply) -> (&'static str, acp::PermissionOptionKind) {
    match reply {
        PermissionReply::Always => ("Always allow", acp::PermissionOptionKind::AllowAlways),
        PermissionReply::Once => ("Allow", acp::PermissionOptionKind::AllowOnce),
        PermissionReply::Reject => ("Reject", acp::PermissionOptionKind::RejectOnce),
    }
}

/// Messages steered into a running turn: who waits for each one's delivery,
/// and which were taken back.
struct Steers {
    session_id: String,
    client: OpenCodeClient,
    /// Not yet delivered or cancelled, by message ID.
    waiting: HashMap<String, oneshot::Sender<bool>>,
    /// Taken back, possibly before OpenCode had them.
    cancelled: HashSet<String>,
}

impl Steers {
    fn new(session_id: String, client: OpenCodeClient) -> Self {
        Self {
            session_id,
            client,
            waiting: HashMap::default(),
            cancelled: HashSet::default(),
        }
    }

    /// Sends a queued message into the running turn. The task finishes with
    /// whether it was delivered.
    fn send(
        &mut self,
        body: PromptBody,
        display_metadata: serde_json::Value,
        cx: &mut Context<OpenCodeSession>,
    ) -> (String, Task<Result<bool>>) {
        let message_id = new_message_id();
        let (delivered_tx, delivered_rx) = oneshot::channel();
        // Registered first: the delivery event can beat the response.
        self.waiting.insert(message_id.clone(), delivered_tx);
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        let task = cx.spawn({
            let message_id = message_id.clone();
            async move |this, cx| {
                let sent = client
                    .send_prompt(
                        &session_id,
                        &message_id,
                        &body,
                        display_metadata,
                        Delivery::Steer,
                    )
                    .await;
                if let Err(error) = sent {
                    this.update(cx, |this, _| {
                        this.steers.waiting.remove(&message_id);
                        this.steers.cancelled.remove(&message_id);
                    })
                    .log_err();
                    return Err(error);
                }
                // A cancel sent before OpenCode had the prompt did nothing.
                let cancelled =
                    this.update(cx, |this, _| this.steers.cancelled.contains(&message_id))?;
                if cancelled {
                    client.cancel_inbox_entry(&session_id, &message_id).await?;
                }
                delivered_rx
                    .await
                    .map_err(|_| anyhow!("OpenCode D closed before the message was delivered"))
            }
        });
        (message_id, task)
    }

    fn cancel(&mut self, message_id: String, cx: &mut App) {
        self.cancelled.insert(message_id.clone());
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        // The outcome comes from OpenCode's events: `cancelled`, or
        // `delivered` when the cancel came too late.
        cx.background_spawn(
            async move { client.cancel_inbox_entry(&session_id, &message_id).await },
        )
        .detach_and_log_err(cx);
    }

    fn finish(&mut self, message_id: &str, delivered: bool) {
        self.cancelled.remove(message_id);
        if let Some(delivered_tx) = self.waiting.remove(message_id) {
            delivered_tx.send(delivered).ok();
        }
    }

    /// Missed events aren't replayed, so after a reconnect each steered
    /// message that left the inbox finishes by whether it reached the session.
    fn recheck(&self, cx: &mut Context<OpenCodeSession>) {
        let steers: Vec<String> = self.waiting.keys().cloned().collect();
        if steers.is_empty() {
            return;
        }
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        cx.spawn(async move |this, cx| {
            let inbox = client.list_inbox(&session_id).await?;
            let messages = client.list_all_messages(&session_id).await?;
            this.update(cx, |this, _| {
                for message_id in steers {
                    if inbox.iter().any(|entry| entry.id == message_id) {
                        continue;
                    }
                    let delivered = messages.iter().any(|message| message.id == message_id);
                    this.steers.finish(&message_id, delivered);
                }
            })
        })
        .detach_and_log_err(cx);
    }
}

/// Subagent sessions started from this thread, to the message whose tool
/// call shows each one's card.
#[derive(Default)]
struct SubagentCards {
    card_messages: HashMap<String, String>,
}

impl SubagentCards {
    /// Returns the subagent sessions `message` starts that weren't seen
    /// before.
    fn track(&mut self, message: &Message) -> Vec<String> {
        let mut new_subagents = Vec::new();
        for tool in message.tool_calls() {
            let (input, metadata) = match &tool.state {
                ToolState::Running { input, metadata }
                | ToolState::Completed {
                    input, metadata, ..
                } => (input, metadata),
                _ => continue,
            };
            let Some(child_id) = translate::find_subagent_session_id(&tool.name, input, metadata)
            else {
                continue;
            };
            let previous = self
                .card_messages
                .insert(child_id.to_string(), message.id.clone());
            if previous.is_none() {
                new_subagents.push(child_id.to_string());
            }
        }
        new_subagents
    }
}

/// OpenCode edits files from its own process, so the action log, which is
/// what Zed's diff review reads, is told about each finished edit.
#[derive(Default)]
struct ExternalEdits {
    /// The tool calls whose edits were seen, so each is reported once.
    seen_tool_call_ids: HashSet<String>,
    /// Edits in a loaded history were reviewed, or not, back then, so the
    /// first batch only marks them as seen.
    history_shown: bool,
    /// Gives edits to the action log one at a time, in order.
    reporting: Option<Task<()>>,
}

impl ExternalEdits {
    /// The edits `tool` made, if they weren't seen before and aren't history.
    fn track(&mut self, tool: &ToolCall, directory: &Path) -> Vec<EditedFile> {
        let edits = collect_edited_files(tool, directory);
        if edits.is_empty()
            || !self.seen_tool_call_ids.insert(tool.id.clone())
            || !self.history_shown
        {
            return Vec::new();
        }
        edits
    }

    fn report(
        &mut self,
        edited_files: Vec<EditedFile>,
        thread: &WeakEntity<AcpThread>,
        cx: &mut Context<OpenCodeSession>,
    ) {
        self.history_shown = true;
        if edited_files.is_empty() {
            return;
        }
        let Some(thread) = thread.upgrade() else {
            return;
        };
        let project = thread.read(cx).project().clone();
        let action_log = thread.read(cx).action_log().clone();
        let previous = self.reporting.take();
        self.reporting = Some(cx.spawn(async move |_, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            // Edits this close together are all on disk by now.
            for edits in edited_files.chunk_by(|a, b| a.path == b.path) {
                report_file_edits(&project, &action_log, edits, cx)
                    .await
                    .log_err();
            }
        }));
    }
}

async fn report_file_edits(
    project: &Entity<Project>,
    action_log: &Entity<ActionLog>,
    edits: &[EditedFile],
    cx: &mut AsyncApp,
) -> Result<()> {
    let (Some(first), Some(last)) = (edits.first(), edits.last()) else {
        return Ok(());
    };
    let kind = match (first.kind, last.kind) {
        // Created and deleted again: nothing is left to review.
        (ExternalEditKind::Created, ExternalEditKind::Deleted) => return Ok(()),
        (ExternalEditKind::Created, _) => ExternalEditKind::Created,
        (_, kind) => kind,
    };
    let buffer = project
        .update(cx, |project, cx| project.open_local_buffer(&first.path, cx))
        .await?;
    if kind != ExternalEditKind::Deleted {
        // Reloading would lose unsaved changes, and the patch wouldn't apply.
        if buffer.read_with(cx, |buffer, _| buffer.is_dirty()) {
            log::info!(
                "Not tracking an edit to {:?}: it has unsaved changes",
                first.path
            );
            return Ok(());
        }
        // The file watcher may not have noticed the edit yet.
        buffer
            .update(cx, |buffer, cx| buffer.reload(cx))
            .await
            .log_err();
    }
    action_log.update(cx, |action_log, cx| {
        action_log.buffer_edited_externally(buffer, kind, &|text| revert_edits(edits, text), cx);
    });
    Ok(())
}

/// An edit to one file, which Zed's diff review can undo.
struct EditedFile {
    path: PathBuf,
    kind: ExternalEditKind,
    hunks: String,
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

/// Undoes consecutive edits to one file in `text`, newest first, or returns
/// `None` if they don't apply.
fn revert_edits(edits: &[EditedFile], text: &str) -> Option<String> {
    edits
        .iter()
        .rev()
        .try_fold(text.to_string(), |text, edit| edit.revert(&text))
}

fn collect_edited_files(tool: &ToolCall, directory: &Path) -> Vec<EditedFile> {
    let ToolState::Completed { metadata, .. } = &tool.state else {
        return Vec::new();
    };
    translate::parse_changed_files(metadata, directory)
        .into_iter()
        .filter_map(|file| {
            // Buffers normalize line endings, and the revert runs on their text.
            let mut hunks = translate::strip_patch_header(file.patch)
                .unwrap_or_default()
                .replace("\r\n", "\n");
            if hunks.is_empty() && file.kind == ExternalEditKind::Modified {
                return None;
            }
            if !hunks.is_empty() && !hunks.ends_with('\n') {
                hunks.push('\n');
            }
            Some(EditedFile {
                path: PathBuf::from(file.path),
                kind: file.kind,
                hunks,
            })
        })
        .collect()
}

/// OpenCode reports token totals per step, but Zed's live turn count takes
/// increments, so what was already passed on is remembered.
#[derive(Default)]
struct UsageReport {
    /// The session's output tokens already counted. Unset until the first
    /// show, so a loaded history doesn't count towards a turn.
    output_tokens: Option<u64>,
    /// The context usage last sent to the thread, to skip repeats.
    shown: Option<ContextUsage>,
}

impl UsageReport {
    /// OpenCode reports tokens per step, not while a step streams.
    fn report_output_tokens(
        &mut self,
        messages: &[Message],
        thread: &WeakEntity<AcpThread>,
        cx: &mut App,
    ) {
        let total = count_output_tokens(messages);
        let added = count_added_output_tokens(self.output_tokens.replace(total), total);
        if added == 0 {
            return;
        }
        thread
            .update(cx, |thread, cx| thread.add_turn_output_tokens(added, cx))
            .log_err();
    }

    /// The usage matches what OpenCode's ACP bridge reports after each turn.
    fn report_context_usage(
        &mut self,
        messages: &[Message],
        session_model: &ModelRef,
        project_setup: &Entity<ProjectSetup>,
        thread: &WeakEntity<AcpThread>,
        cx: &mut App,
    ) {
        let last_step = messages.iter().rev().find_map(|message| {
            let assistant = message.as_assistant()?;
            let used = assistant.tokens.as_ref()?.total();
            (used > 0).then_some((used, assistant.model.clone()))
        });
        let Some((used, model)) = last_step else {
            return;
        };
        let model = model.unwrap_or_else(|| session_model.clone());
        let project_setup = project_setup.read(cx);
        let Some(size) = project_setup
            .find_model(&model.provider_id, &model.id)
            .or_else(|| project_setup.models.iter().find(|info| info.id == model.id))
            .and_then(|info| info.limit.context)
        else {
            return;
        };
        let cost: f64 = messages
            .iter()
            .filter_map(|message| message.as_assistant()?.cost)
            .sum();
        let usage = ContextUsage { used, size, cost };
        if self.shown == Some(usage) {
            return;
        }
        self.shown = Some(usage);
        thread
            .update(cx, |thread, cx| {
                thread.update_usage(
                    acp::UsageUpdate::new(used, size).cost(acp::Cost::new(cost, "USD")),
                    cx,
                )
            })
            .log_err();
    }
}

#[derive(Clone, Copy, PartialEq)]
struct ContextUsage {
    used: u64,
    size: u64,
    cost: f64,
}

fn count_output_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .filter_map(|message| message.as_assistant()?.tokens.as_ref())
        .map(|tokens| tokens.output + tokens.reasoning)
        .sum()
}

/// A rewind can lower the total, which adds nothing to the turn.
fn count_added_output_tokens(reported: Option<u64>, total: u64) -> u64 {
    reported.map_or(0, |reported| total.saturating_sub(reported))
}

struct FetchedSnapshot {
    running: bool,
    title: Option<String>,
    agent: Option<String>,
    model: Option<ModelRef>,
    directory: Option<String>,
    messages: Snapshot,
    permissions: Vec<PermissionRequest>,
    /// `None` when listing failed: unknown, not empty.
    forms: Option<Vec<Form>>,
}

async fn fetch_snapshot(client: &OpenCodeClient, session_id: &str) -> Result<FetchedSnapshot> {
    let running = client.list_running_sessions().await?.contains(session_id);
    let info = client.fetch_session(session_id).await?;
    let messages = client.list_all_messages(session_id).await?;
    let inbox = client.list_inbox(session_id).await?;
    let permissions = client.list_pending_permissions(session_id).await?;
    let forms = client
        .list_pending_forms(session_id)
        .await
        .context("Can't list OpenCode forms")
        .log_err();
    Ok(FetchedSnapshot {
        running,
        title: info.title,
        agent: info.agent,
        model: info.model,
        directory: info.location.map(|location| location.directory),
        messages: Snapshot {
            messages,
            revert: info.revert,
            inbox,
        },
        permissions,
        forms,
    })
}

/// Sends what the user typed: a message, a command or a compaction.
pub(crate) struct Submissions {
    pub connection: Weak<OpenCodeConnection>,
    pub client: OpenCodeClient,
    pub session_id: String,
}

impl ReceiptSessionSubmissions for Submissions {
    fn submit(
        &self,
        content: Vec<acp::ContentBlock>,
        cx: &mut Context<AcpThread>,
    ) -> ReceiptSubmission {
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        let message_id = new_message_id();
        let Some(session) = self
            .connection
            .upgrade()
            .and_then(|connection| connection.session(&session_id))
        else {
            return Task::ready(Err(anyhow!("the session isn't open"))).into();
        };
        let prompt = Prompt::parse(&content, session.read(cx).project_setup.read(cx));
        let (compacts_first, prompt, content) = match prompt.kind {
            PromptKind::Compaction {
                follow_up: Some(follow_up),
            } => (
                true,
                Prompt::parse(&follow_up, session.read(cx).project_setup.read(cx)),
                follow_up,
            ),
            _ => (false, prompt, content),
        };

        let local_message = if let PromptKind::Message { .. } = prompt.kind {
            match to_client_message_id(&message_id) {
                Ok(client_id) => {
                    session.update(cx, |session, _| session.shown.push_sent(&message_id));
                    Some((client_id, content))
                }
                Err(error) => return Task::ready(Err(error)).into(),
            }
        } else {
            None
        };

        let session = session.downgrade();
        // `submit` runs inside the thread's own update, so the user message
        // is added from a task rather than here.
        cx.spawn(async move |thread, cx| {
            if let Some((client_id, content)) = local_message {
                thread.update(cx, |thread, cx| {
                    thread.upsert_local_user_message(client_id, content, cx)
                })??;
            }
            if compacts_first {
                client.compact(&session_id).await?;
            }
            let max_wait = match prompt.kind {
                PromptKind::Message { display_metadata } => {
                    // Queued, so the compaction runs before it.
                    let delivery = if compacts_first {
                        Delivery::Queue
                    } else {
                        Delivery::Steer
                    };
                    client
                        .send_prompt(
                            &session_id,
                            &message_id,
                            &prompt.body,
                            display_metadata,
                            delivery,
                        )
                        .await?;
                    return Ok(acp::PromptResponse::new(message_id.as_str()));
                }
                PromptKind::Command { name } => {
                    client.run_command(&session_id, &name, &prompt.body).await?;
                    Duration::from_secs(30)
                }
                // Compacting adds no user message, so there's nothing to wait for.
                PromptKind::Compaction { .. } => {
                    client.compact(&session_id).await?;
                    Duration::ZERO
                }
            };
            // Zed keeps a sent message pending until a user message with its
            // receipt ID appears, but a command adds none of its own. Its
            // receipt is therefore the latest user message, waiting briefly
            // for the one a command's template creates when nothing has been
            // sent yet.
            Ok(
                match OpenCodeSession::wait_for_latest_user_message(&session, max_wait, cx).await {
                    Some(receipt) => acp::PromptResponse::new(receipt.as_str()),
                    None => acp::PromptResponse::new(message_id.as_str())
                        .meta(acp_thread::meta_with_no_echo()),
                },
            )
        })
        .into()
    }
}

/// Sends a queued message into the running turn. OpenCode adds it at the
/// next step, and until then it can be taken back.
pub(crate) struct Steering {
    pub session: WeakEntity<OpenCodeSession>,
}

impl AgentSteering for Steering {
    /// Commands run as their own turn, so they wait.
    fn can_steer(&self, content: &[acp::ContentBlock], cx: &App) -> bool {
        self.session.upgrade().is_some_and(|session| {
            let prompt = Prompt::parse(content, session.read(cx).project_setup.read(cx));
            matches!(prompt.kind, PromptKind::Message { .. })
        })
    }

    fn steer(
        &self,
        content: Vec<acp::ContentBlock>,
        cx: &mut App,
    ) -> (SharedString, Task<Result<bool>>) {
        match self.session.update(cx, |session, cx| {
            let prompt = Prompt::parse(&content, session.project_setup.read(cx));
            let PromptKind::Message { display_metadata } = prompt.kind else {
                return Err(anyhow!("only a message can join a running turn"));
            };
            Ok(session.steers.send(prompt.body, display_metadata, cx))
        }) {
            Ok(Ok((message_id, task))) => (message_id.into(), task),
            Ok(Err(error)) => (SharedString::default(), Task::ready(Err(error))),
            Err(error) => (SharedString::default(), Task::ready(Err(error))),
        }
    }

    fn cancel_steer(&self, id: SharedString, cx: &mut App) {
        self.session
            .update(cx, |session, cx| session.steers.cancel(id.to_string(), cx))
            .log_err();
    }
}

pub(crate) struct Truncate {
    pub connection: Weak<OpenCodeConnection>,
    pub session_id: String,
}

impl AgentSessionTruncate for Truncate {
    fn restores_files(&self) -> bool {
        true
    }

    fn run(&self, client_user_message_id: ClientUserMessageId, cx: &mut App) -> Task<Result<()>> {
        let message_id = to_message_id(&client_user_message_id);
        let Some(connection) = self.connection.upgrade() else {
            return Task::ready(Err(anyhow!("OpenCode D's connection is closed")));
        };
        let Some(session) = connection.session(&self.session_id) else {
            return Task::ready(Err(anyhow!("the session isn't open")));
        };
        session.update(cx, |session, _| {
            if let Some(index) = session.shown.position(&message_id) {
                session.forget_shown_from(index);
            }
        });
        let client = connection.client().clone();
        let session_id = self.session_id.clone();
        let running = connection.is_running(&session_id);
        cx.background_spawn(async move {
            if running {
                client.interrupt(&session_id).await?;
                client.wait_until_idle(&session_id).await.log_err();
            }
            client.stage_revert(&session_id, &message_id).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::thread_sync_tests::{Durability, Harness, SESSION};
    use gpui::TestAppContext;
    use serde_json::json;

    #[test]
    fn reverting_an_edit_restores_the_old_text() {
        let tool = |status: &str, patch: &str| ToolCall {
            id: "tool".into(),
            name: "edit".into(),
            state: ToolState::Completed {
                input: serde_json::Value::Null,
                content: Vec::new(),
                metadata: json!({"files": [{"file": "a.txt", "status": status, "patch": patch}]}),
            },
        };
        let edited = |status: &str, patch: &str| {
            collect_edited_files(&tool(status, patch), Path::new("/project"))
                .pop()
                .expect("file")
        };
        let header = "Index: a.txt\n===================================================================\n--- a.txt\n+++ a.txt\n";

        let modified = edited(
            "modified",
            &format!("{header}@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n"),
        );
        assert_eq!(modified.path, PathBuf::from("/project/a.txt"));
        assert_eq!(modified.kind, ExternalEditKind::Modified);
        assert_eq!(modified.revert("one\nTWO\n").as_deref(), Some("one\ntwo\n"));
        let second = edited(
            "modified",
            &format!("{header}@@ -1,2 +1,2 @@\n-one\n+ONE\n TWO\n"),
        );
        assert_eq!(
            revert_edits(&[modified, second], "ONE\nTWO\n").as_deref(),
            Some("one\ntwo\n")
        );
        let modified = edited(
            "modified",
            &format!("{header}@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n"),
        );
        // Files with Windows line endings: buffer text has already normalized them.
        let crlf = edited(
            "modified",
            &format!("{header}@@ -1,2 +1,2 @@\r\n one\r\n-two\r\n+TWO\r\n"),
        );
        assert_eq!(crlf.revert("one\nTWO\n").as_deref(), Some("one\ntwo\n"));
        // Nothing to review, such as a tool that lists files it only looked at.
        assert!(collect_edited_files(&tool("modified", ""), Path::new("/project")).is_empty());
        assert_eq!(modified.revert("something\nelse\n"), None);

        let created = edited("added", &format!("{header}@@ -0,0 +1 @@\n+hello\n"));
        assert_eq!(created.kind, ExternalEditKind::Created);
        assert_eq!(created.revert("hello\n").as_deref(), Some(""));

        let deleted = edited("deleted", &format!("{header}@@ -1 +0,0 @@\n-hello\n"));
        assert_eq!(deleted.kind, ExternalEditKind::Deleted);
        assert_eq!(deleted.revert("").as_deref(), Some("hello\n"));

        // A new empty file has no hunks.
        assert_eq!(edited("added", header).revert("").as_deref(), Some(""));
    }

    #[gpui::test]
    async fn repeated_stop_notices_keep_each_steps_content(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |_| {});
        let thread = harness.open(SESSION, cx).await;
        for (id, text) in [("msg_first", "First step"), ("msg_second", "Second step")] {
            for (kind, data) in [
                (
                    "session.step.started",
                    json!({ "assistantMessageID": id, "agent": "build" }),
                ),
                (
                    "session.text.ended",
                    json!({ "assistantMessageID": id, "ordinal": 0, "text": text }),
                ),
                (
                    "session.step.failed",
                    json!({
                        "assistantMessageID": id,
                        "error": { "type": "aborted", "message": "Step interrupted" },
                    }),
                ),
            ] {
                harness.send(kind, data, Durability::Durable, cx);
                cx.run_until_parked();
            }
        }
        harness.send(
            "session.execution.interrupted",
            json!({ "reason": "user" }),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        let shown = thread.read_with(cx, |thread, cx| thread.to_markdown(cx));
        assert!(shown.contains("First step"), "{shown}");
        assert!(shown.contains("Second step"), "{shown}");
        assert_eq!(shown.matches("Stopped").count(), 1, "{shown}");
    }

    fn messages_with(ids: &[&str]) -> SessionMessages {
        let messages: Vec<Message> = ids
            .iter()
            .map(|id| {
                Message::from_value(serde_json::json!({
                    "id": id,
                    "type": "user",
                    "text": "hello",
                }))
                .expect("message")
            })
            .collect();
        let mut session = SessionMessages::new("ses_test");
        session.replace_snapshot(Snapshot {
            messages,
            revert: None,
            inbox: Vec::new(),
        });
        session
    }

    fn shown(ids: &[&str], user_message_ids: &[&str], awaiting: &[&str]) -> ShownMessages {
        ShownMessages {
            ids: ids.iter().map(|id| id.to_string()).collect(),
            id_set: ids.iter().map(|id| id.to_string()).collect(),
            user_message_ids: user_message_ids.iter().map(|id| id.to_string()).collect(),
            awaiting_delivery: awaiting.iter().map(|id| id.to_string()).collect(),
        }
    }

    #[test]
    fn a_sent_prompt_the_server_has_not_delivered_is_kept() {
        let shown = shown(
            &["msg_a", "msg_sent"],
            &["msg_a", "msg_sent"],
            &["msg_sent"],
        );
        assert_eq!(
            shown.compare_with(&messages_with(&["msg_a"])),
            ThreadStatus::Matches
        );
    }

    #[test]
    fn a_shown_message_the_server_dropped_rewinds_to_its_prompt() {
        let shown = shown(&["msg_a", "msg_gone"], &["msg_a", "msg_gone"], &[]);
        assert_eq!(
            shown.compare_with(&messages_with(&["msg_a"])),
            ThreadStatus::DiffersFromUserMessage(1)
        );
    }

    #[test]
    fn a_mismatch_with_no_prompt_before_it_is_stale() {
        let shown = shown(&["msg_a"], &[], &[]);
        assert_eq!(
            shown.compare_with(&messages_with(&["msg_b"])),
            ThreadStatus::DiffersBeforeFirstUserMessage
        );
    }

    #[test]
    fn only_new_output_tokens_count_towards_the_turn() {
        // The first show may hold a loaded history, which isn't this turn's.
        assert_eq!(count_added_output_tokens(None, 3000), 0);
        assert_eq!(count_added_output_tokens(Some(3000), 3000), 0);
        assert_eq!(count_added_output_tokens(Some(3000), 3250), 250);
        // After a rewind the total is lower.
        assert_eq!(count_added_output_tokens(Some(3250), 1000), 0);
    }
}
