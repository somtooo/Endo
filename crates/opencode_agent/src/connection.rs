//! The OpenCode D backend: an `AgentConnection` that talks to the shared
//! OpenCode service directly.
//!
//! One `Hub` per Zed process owns the HTTP client and the single event
//! stream. Each open thread has a `SessionTranscript` (the server's state)
//! and a list of the messages already rendered into its `AcpThread`. Events
//! update the transcript; `sync_thread` then brings the thread in line,
//! rewinding when the server removed or reordered messages.

use crate::client::{ListSessions, OpenCodeClient, ShellOutput};
use crate::render::{self, RenderOp, client_message_id, message_id_string};
use crate::form;
use crate::mcp;
use crate::service;
use crate::session_sync::{Activity, Applied, Change, SessionTranscript, Snapshot};
use crate::types::{
    AgentInfo, AssistantContent, ToolState, Event, Message, MessageBody, ModelInfo, ModelRef, NamedItem, ProviderInfo, SessionInfo, Skill,
    new_message_id,
};
use acp_thread::{
    AcpThread, AgentConnection, AgentModelGroupName, AgentModelIcon, AgentModelId, AgentModelInfo,
    AgentModelList, AgentModelSelector, AgentSessionInfo, AgentSessionList,
    AgentSessionListRequest, AgentSessionListResponse, AgentSessionSetTitle, AgentSessionTruncate,
    AuthorizationKind, ClientUserMessageId, MentionUri, PermissionOptions, ReceiptSessionSubmissions,
    ReceiptSubmission, RequestPermissionOutcome, SelectedPermissionOutcome,
};
use action_log::{ActionLog, ExternalEditKind};
use agent_servers::{AgentServer as _, CustomAgentServer};
use agent_ui::AvailableSkill;
use agent_settings::{AgentProfileId, AvailableProfiles};
use agent_ui::ProfileProvider;
use agent_ui::agent_menu_providers::{AgentSteering, AgentThinking};
use agent_client_protocol::v2::schema as acp;
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet, IndexMap};
use fs::Fs;
use language_model::{LanguageModelCostInfo, LanguageModelEffortLevel};
use settings::AgentConfigOptionValue;
use ui::IconName;
use std::collections::VecDeque;
use futures::channel::{mpsc, oneshot};
use futures::future::Shared;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, SharedString, Subscription, Task, TaskExt as _, WeakEntity};
use project::{AgentId, Project};
use serde_json::Value;
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use util::ResultExt as _;
use util::path_list::PathList;
use util::paths::PathStyle;

pub const AGENT_ID: &str = "opencode-direct";
pub const DISPLAY_NAME: &str = "OpenCode D";

/// Events about any session that the hub needs even when no thread for it is
/// open, so they always reach the foreground.
const LIFECYCLE_EVENTS: &[&str] = &[
    "session.created",
    "session.renamed",
    "session.moved",
    "session.deleted",
    "session.archived",
    "session.unarchived",
    "session.execution.started",
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
];

/// Sent for a directory when its commands or skills change. The first request
/// for a directory the server hasn't loaded yet can return empty lists, so
/// these also mark the end of that load.
const CATALOG_EVENTS: &[&str] = &[
    "command.updated",
    "skill.updated",
    "agent.updated",
    "model.updated",
    "provider.updated",
    "integration.updated",
    "credential.updated",
];

enum PumpMessage {
    Connected,
    Event(Event),
    Disconnected,
}

struct SessionEntry {
    thread: WeakEntity<AcpThread>,
    transcript: SessionTranscript,
    /// IDs of messages rendered into the thread, in thread order.
    rendered: Vec<String>,
    /// The rendered messages that are editable user messages: the only
    /// places the thread can be rewound to.
    user_messages: HashSet<String>,
    /// While a snapshot loads or a rewind runs, events wait here and are
    /// replayed afterwards (applying an event twice is harmless).
    buffered: Option<Vec<Event>>,
    /// Set while the hub itself rewinds the thread, so the truncate hook
    /// doesn't ask the server to revert again.
    server_rewind: Rc<Cell<bool>>,
    /// Pending permission request ID to the tool call it was attached to.
    permissions: HashMap<String, String>,
    options: Rc<SessionOptions>,
    /// The folder the session works in now. It starts as the one it was
    /// opened in and follows the session when it moves.
    folder: String,
    /// Thread changes not applied yet, with the message each belongs to.
    /// Once a view shows the thread, they're applied one new entry per
    /// update: the agent panel assumes each effect flush adds at most one.
    queue: VecDeque<(String, RenderOp)>,
    draining: bool,
    /// Set once the thread is handed to the UI. Before that, changes are
    /// applied all at once.
    attached: bool,
    rewinding: bool,
    /// Subagent sessions started from this thread, to the message whose
    /// tool call shows each one.
    children: HashMap<String, String>,
    /// The context usage last sent to the thread, to skip repeats.
    usage: Option<(u64, u64, f64)>,
    /// The session's output tokens already counted. Unset until the first
    /// render, so a loaded history doesn't count towards a turn.
    output_tokens: Option<u64>,
    /// Finished edit tools already given to the action log.
    tracked_edits: HashSet<String>,
    /// Gives edits to the action log one at a time, in order.
    edit_tracking: Option<Task<()>>,
    /// Open forms to the elicitation showing each.
    forms: HashMap<String, acp_thread::ElicitationEntryId>,
    /// Running shell commands whose output is followed, by tool call ID.
    shells: HashMap<String, LiveShell>,
    _release: Option<Subscription>,
}

struct LiveShell {
    /// Output was appended to the terminal, so the final render keeps it.
    streamed: bool,
    /// The final output was rendered before any streamed, so nothing more
    /// is appended.
    closed: bool,
    _task: Task<()>,
}

pub struct Hub {
    client: OpenCodeClient,
    sessions: RefCell<HashMap<String, SessionEntry>>,
    open_session_ids: Arc<Mutex<HashSet<String>>>,
    activity: RefCell<HashMap<String, Activity>>,
    /// Sessions whose latest run was interrupted, so a stopped background
    /// subagent's card shows it was cancelled.
    interrupted: RefCell<HashSet<String>>,
    activity_listeners: RefCell<Vec<Box<dyn Fn(&str, Activity, &mut App)>>>,
    rename_listeners: RefCell<Vec<Box<dyn Fn(&str, &str, &mut App)>>>,
    move_listeners: RefCell<Vec<Box<dyn Fn(&str, &str, &mut App)>>>,
    mcp: Rc<mcp::McpSync>,
    /// Counts catalog events per directory, so a thread can tell that its
    /// commands and skills changed while it was loading them.
    catalog_versions: RefCell<HashMap<String, u64>>,
    /// The latest refetch of commands and skills per directory. Replacing one
    /// cancels the older fetch.
    catalog_refreshes: RefCell<HashMap<String, Task<()>>>,
    /// Steered prompts not yet delivered or cancelled, by message ID, with
    /// their session and who to tell which happened.
    steers: RefCell<HashMap<String, (String, oneshot::Sender<bool>)>>,
    /// Steered prompts taken back, possibly before OpenCode had them.
    cancelled_steers: RefCell<HashSet<String>>,
    _pump: RefCell<Option<Task<()>>>,
}

struct GlobalHub(Option<Rc<Hub>>, Option<Shared<Task<Result<Rc<Hub>, Arc<anyhow::Error>>>>>);

impl gpui::Global for GlobalHub {}

impl Hub {
    /// The hub for this process, connecting to (and if needed starting) the
    /// shared OpenCode service on first use.
    pub fn get(http: Arc<dyn http_client::HttpClient>, cx: &mut App) -> Task<Result<Rc<Hub>>> {
        if let Some(GlobalHub(Some(hub), _)) = cx.try_global::<GlobalHub>() {
            return Task::ready(Ok(hub.clone()));
        }
        if let Some(GlobalHub(None, Some(pending))) = cx.try_global::<GlobalHub>() {
            let pending = pending.clone();
            return cx.spawn(async move |_| pending.await.map_err(|error| anyhow!("{error:#}")));
        }
        let pending = cx
            .spawn(async move |cx| {
                let result = Self::connect(http, cx).await;
                match &result {
                    Ok(hub) => {
                        let hub = hub.clone();
                        cx.update(|cx| {
                            cx.set_global(GlobalHub(Some(hub.clone()), None));
                            crate::history::start(hub, cx);
                        });
                    }
                    Err(_) => cx.update(|cx| cx.set_global(GlobalHub(None, None))),
                }
                result.map_err(Arc::new)
            })
            .shared();
        cx.set_global(GlobalHub(None, Some(pending.clone())));
        cx.spawn(async move |_| pending.await.map_err(|error| anyhow!("{error:#}")))
    }

    async fn connect(http: Arc<dyn http_client::HttpClient>, cx: &mut AsyncApp) -> Result<Rc<Hub>> {
        let executor = cx.background_executor().clone();
        let registered = service::read_registration(&service::registration_path())?;
        let mut client = None;
        if let Some(registration) = registered {
            let candidate = OpenCodeClient::new(http.clone(), &registration)?;
            if candidate.info().await.is_ok() {
                client = Some(candidate);
            }
        }
        let client = match client {
            Some(client) => client,
            None => {
                let registration = service::start_service(executor).await?;
                let client = OpenCodeClient::new(http, &registration)?;
                client
                    .info()
                    .await
                    .context("the OpenCode service didn't answer")?;
                client
            }
        };
        let hub = Rc::new(Hub {
            mcp: mcp::McpSync::new(client.clone()),
            client,
            sessions: RefCell::default(),
            open_session_ids: Arc::default(),
            activity: RefCell::default(),
            interrupted: RefCell::default(),
            activity_listeners: RefCell::default(),
            rename_listeners: RefCell::default(),
            move_listeners: RefCell::default(),
            catalog_versions: RefCell::default(),
            catalog_refreshes: RefCell::default(),
            steers: RefCell::default(),
            cancelled_steers: RefCell::default(),
            _pump: RefCell::new(None),
        });
        let pump = cx.update(|cx| Self::start_pump(&hub, cx));
        *hub._pump.borrow_mut() = Some(pump);
        Ok(hub)
    }

    pub fn client(&self) -> &OpenCodeClient {
        &self.client
    }

    pub fn activity(&self, session_id: &str) -> Option<Activity> {
        self.activity.borrow().get(session_id).copied()
    }

    /// Calls `listener` whenever the server reports a session starting or
    /// finishing work, for every session on the server.
    pub fn observe_activity(&self, listener: impl Fn(&str, Activity, &mut App) + 'static) {
        self.activity_listeners.borrow_mut().push(Box::new(listener));
    }

    /// Calls `listener` with the new title whenever any session on the
    /// server is renamed.
    pub fn observe_renames(&self, listener: impl Fn(&str, &str, &mut App) + 'static) {
        self.rename_listeners.borrow_mut().push(Box::new(listener));
    }

    /// Called with a session's new directory when it moves.
    pub fn observe_moves(&self, listener: impl Fn(&str, &str, &mut App) + 'static) {
        self.move_listeners.borrow_mut().push(Box::new(listener));
    }

    fn start_pump(hub: &Rc<Hub>, cx: &mut App) -> Task<()> {
        let (tx, mut rx) = mpsc::unbounded::<PumpMessage>();
        let client = hub.client.clone();
        let open_session_ids = hub.open_session_ids.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            loop {
                match client.events().await {
                    Ok(mut events) => {
                        if tx.unbounded_send(PumpMessage::Connected).is_err() {
                            return;
                        }
                        while let Some(event) = events.next().await {
                            let event = match event {
                                Ok(event) => event,
                                Err(error) => {
                                    log::warn!("OpenCode event stream error: {error:#}");
                                    break;
                                }
                            };
                            let wanted = LIFECYCLE_EVENTS.contains(&event.kind.as_str())
                                || CATALOG_EVENTS.contains(&event.kind.as_str())
                                || event.session_id().is_some_and(|session_id| {
                                    open_session_ids
                                        .lock()
                                        .map(|ids| ids.contains(session_id))
                                        .unwrap_or(false)
                                });
                            if wanted && tx.unbounded_send(PumpMessage::Event(event)).is_err() {
                                return;
                            }
                        }
                        if tx.unbounded_send(PumpMessage::Disconnected).is_err() {
                            return;
                        }
                    }
                    Err(error) => log::warn!("Can't subscribe to OpenCode events: {error:#}"),
                }
                executor.timer(Duration::from_secs(1)).await;
            }
        })
        .detach();

        let hub = Rc::downgrade(hub);
        cx.spawn(async move |cx| {
            while let Some(message) = rx.next().await {
                let Some(hub) = hub.upgrade() else {
                    return;
                };
                cx.update(|cx| match message {
                    PumpMessage::Connected => {
                        hub.mcp.reconnected(cx);
                        Hub::recheck_steers(&hub, cx);
                        let session_ids: Vec<String> =
                            hub.sessions.borrow().keys().cloned().collect();
                        for session_id in session_ids {
                            Hub::resync(&hub, session_id, cx).detach_and_log_err(cx);
                        }
                        // Missed events aren't replayed.
                        for directory in hub.open_directories() {
                            Hub::refresh_catalog(&hub, directory, cx);
                        }
                    }
                    PumpMessage::Event(event) => hub.handle_event(event, cx),
                    PumpMessage::Disconnected => log::info!("OpenCode event stream ended"),
                });
            }
        })
    }

    fn handle_event(self: &Rc<Self>, event: Event, cx: &mut App) {
        if CATALOG_EVENTS.contains(&event.kind.as_str()) {
            // A change to a global config file comes tagged with the config
            // folder, which no thread is open in, but applies to them all.
            let open_directories = self.open_directories();
            let directories = match event.location {
                Some(location) if open_directories.contains(&location.directory) => {
                    vec![location.directory]
                }
                _ => open_directories,
            };
            for directory in directories {
                *self
                    .catalog_versions
                    .borrow_mut()
                    .entry(directory.clone())
                    .or_default() += 1;
                Hub::refresh_catalog(self, directory, cx);
            }
            return;
        }
        let Some(session_id) = event.session_id().map(str::to_string) else {
            return;
        };
        let activity = match event.kind.as_str() {
            "session.execution.started" => Some(Activity::Running),
            "session.execution.succeeded"
            | "session.execution.failed"
            | "session.execution.interrupted" => Some(Activity::Idle),
            _ => None,
        };
        if let Some(activity) = activity {
            if event.kind == "session.execution.interrupted" {
                self.interrupted.borrow_mut().insert(session_id.clone());
            } else if activity == Activity::Running {
                self.interrupted.borrow_mut().remove(&session_id);
            }
            self.activity.borrow_mut().insert(session_id.clone(), activity);
            for listener in self.activity_listeners.borrow().iter() {
                listener(&session_id, activity, cx);
            }
            self.refresh_subagent_cards(&session_id, cx);
        }
        if event.kind == "session.renamed"
            && let Some(title) = event.data.get("title").and_then(Value::as_str)
        {
            for listener in self.rename_listeners.borrow().iter() {
                listener(&session_id, title, cx);
            }
        }
        if event.kind == "session.moved"
            && let Some(directory) = event
                .data
                .pointer("/location/directory")
                .and_then(Value::as_str)
        {
            if let Some(entry) = self.sessions.borrow_mut().get_mut(&session_id) {
                entry.folder = directory.to_string();
            }
            for listener in self.move_listeners.borrow().iter() {
                listener(&session_id, directory, cx);
            }
        }

        if let Some(delivered) = match event.kind.as_str() {
            "session.inbox.delivered" => Some(true),
            "session.inbox.cancelled" => Some(false),
            _ => None,
        } && let Some(inbox_id) = event.data.get("inboxID").and_then(Value::as_str)
        {
            self.settle_steer(inbox_id, delivered);
        }

        match event.kind.as_str() {
            "permission.asked" => return self.permission_asked(&session_id, &event.data, cx),
            "permission.replied" => return self.permission_replied(&session_id, &event.data, cx),
            "form.created" => {
                if let Some(form) = event.data.get("form") {
                    self.form_created(&session_id, form, cx);
                }
                return;
            }
            "form.replied" | "form.cancelled" => {
                return self.form_settled(&session_id, &event.data, cx);
            }
            _ => {}
        }

        let applied = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(&session_id) else {
                return;
            };
            if let Some(buffered) = &mut entry.buffered {
                buffered.push(event);
                return;
            }
            entry.transcript.apply(&event)
        };
        match applied {
            Applied::Ignored => {}
            Applied::Changed(changes) => self.apply_changes(&session_id, changes, cx),
            Applied::NeedsResync(reason) => {
                log::info!("OpenCode session {session_id} resyncs: {reason:?}");
                Hub::resync(self, session_id, cx).detach_and_log_err(cx);
            }
        }
    }

    fn open_directories(&self) -> Vec<String> {
        let directories: HashSet<String> = self
            .sessions
            .borrow()
            .values()
            .map(|entry| entry.options.directory.clone())
            .collect();
        directories.into_iter().collect()
    }

    fn catalog_version(&self, directory: &str) -> u64 {
        self.catalog_versions
            .borrow()
            .get(directory)
            .copied()
            .unwrap_or_default()
    }

    /// Refetches the commands and skills of `directory` for the threads open
    /// in it.
    fn settle_steer(&self, message_id: &str, delivered: bool) {
        self.cancelled_steers.borrow_mut().remove(message_id);
        if let Some((_, settled)) = self.steers.borrow_mut().remove(message_id) {
            settled.send(delivered).ok();
        }
    }

    /// Missed events aren't replayed, so after a reconnect each steered
    /// prompt that left the inbox is settled by whether it reached the
    /// session.
    fn recheck_steers(self: &Rc<Self>, cx: &mut App) {
        let steers: Vec<(String, String)> = self
            .steers
            .borrow()
            .iter()
            .map(|(message_id, (session_id, _))| (message_id.clone(), session_id.clone()))
            .collect();
        for (message_id, session_id) in steers {
            let hub = Rc::downgrade(self);
            let client = self.client.clone();
            cx.spawn(async move |cx| {
                if client.inbox_ids(&session_id).await?.contains(&message_id) {
                    return Ok(());
                }
                let delivered = client
                    .all_messages(&session_id)
                    .await?
                    .iter()
                    .any(|message| message.id == message_id);
                cx.update(|_| {
                    if let Some(hub) = hub.upgrade() {
                        hub.settle_steer(&message_id, delivered);
                    }
                });
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }
    }

    fn refresh_catalog(self: &Rc<Self>, directory: String, cx: &mut App) {
        let has_sessions = self
            .sessions
            .borrow()
            .values()
            .any(|entry| entry.options.directory == directory);
        if !has_sessions {
            return;
        }
        let client = self.client.clone();
        let hub = Rc::downgrade(self);
        let task =
            cx.spawn({
                let directory = directory.clone();
                async move |cx| {
                    let (commands, skills, agents, models, providers) = futures::join!(
                        client.commands(&directory),
                        client.skills(&directory),
                        client.agents(&directory),
                        client.models(&directory),
                        client.providers(&directory)
                    );
                    // On failure the threads keep the lists they have.
                    let (
                        Some(commands),
                        Some(mut skills),
                        Some(agents),
                        Some(models),
                        Some(providers),
                    ) = (
                        commands.log_err(),
                        skills.log_err(),
                        agents.log_err(),
                        models.log_err(),
                        providers.log_err(),
                    )
                    else {
                        return;
                    };
                    let skills = cx
                        .background_spawn({
                            let directory = PathBuf::from(&directory);
                            async move {
                                prepare_skills(&mut skills, &directory);
                                skills
                            }
                        })
                        .await;
                    let Some(hub) = hub.upgrade() else {
                        return;
                    };
                    cx.update(|cx| {
                        hub.apply_catalog(
                            &directory, &commands, skills, agents, models, providers, cx,
                        )
                    });
                }
            });
        self.catalog_refreshes.borrow_mut().insert(directory, task);
    }

    fn apply_catalog(
        &self,
        directory: &str,
        commands: &[NamedItem],
        skills: Vec<Skill>,
        agents: Vec<AgentInfo>,
        models: Vec<ModelInfo>,
        providers: Vec<ProviderInfo>,
        cx: &mut App,
    ) {
        let names: HashSet<String> = commands.iter().map(|command| command.name.clone()).collect();
        let threads: Vec<Entity<AcpThread>> = self
            .sessions
            .borrow()
            .values()
            .filter(|entry| entry.options.directory == directory)
            .filter_map(|entry| {
                *entry.options.commands.borrow_mut() = names.clone();
                *entry.options.skills.borrow_mut() = skills.clone();
                *entry.options.agents.borrow_mut() = agents.clone();
                *entry.options.models.borrow_mut() = models.clone();
                *entry.options.providers.borrow_mut() = providers.clone();
                // Updates the profile and model pickers.
                entry.options.tx.borrow_mut().send(()).log_err();
                entry.thread.upgrade()
            })
            .collect();
        // The view rereads the skills when the commands change.
        for thread in threads {
            thread.update(cx, |thread, cx| {
                thread.update_available_commands(available_commands(commands), cx)
            });
        }
    }

    fn apply_changes(self: &Rc<Self>, session_id: &str, changes: Vec<Change>, cx: &mut App) {
        let mut changed_messages = Vec::new();
        for change in changes {
            match change {
                Change::Message(id) => changed_messages.push(id),
                Change::Activity(activity) => {
                    self.with_thread(session_id, cx, |thread, cx| {
                        thread
                            .update_session_state(state_update(activity), cx)
                            .log_err();
                    });
                }
                Change::Title(title) => {
                    self.with_thread(session_id, cx, |thread, cx| {
                        thread.update_session_info(acp::SessionInfoUpdate::new().title(title), cx);
                    });
                }
                Change::Revert(_) => {}
            }
        }
        self.update_options_from_messages(session_id, &changed_messages, cx);
        self.sync_thread(session_id, changed_messages, cx);
    }

    fn with_thread(
        &self,
        session_id: &str,
        cx: &mut App,
        update: impl FnOnce(&mut AcpThread, &mut Context<AcpThread>),
    ) {
        let thread = self
            .sessions
            .borrow()
            .get(session_id)
            .and_then(|entry| entry.thread.upgrade());
        if let Some(thread) = thread {
            thread.update(cx, update);
        }
    }

    /// Brings the thread in line with the transcript. Messages in `changed`
    /// that are already shown are re-rendered in place; visible messages not
    /// shown yet are appended. If the thread shows a message the server no
    /// longer has (or in another order), the thread is rewound to the last
    /// user message before it and rebuilt from there.
    fn sync_thread(self: &Rc<Self>, session_id: &str, changed: Vec<String>, cx: &mut App) {
        let plan = {
            let sessions = self.sessions.borrow();
            let Some(entry) = sessions.get(session_id) else {
                return;
            };
            let visible: Vec<&str> = entry
                .transcript
                .visible_messages()
                .iter()
                .map(|message| message.id.as_str())
                .collect();
            let mismatch = entry
                .rendered
                .iter()
                .enumerate()
                .position(|(ix, id)| visible.get(ix) != Some(&id.as_str()));
            match mismatch {
                None => SyncPlan::Append,
                Some(ix) => {
                    let rewind_to = entry.rendered[..=ix]
                        .iter()
                        .rposition(|id| entry.user_messages.contains(id));
                    match rewind_to {
                        Some(rewind_to) => SyncPlan::Rewind(rewind_to),
                        None => {
                            log::warn!(
                                "OpenCode session {session_id} changed before its first prompt; \
                                 the thread may show stale entries"
                            );
                            SyncPlan::Append
                        }
                    }
                }
            }
        };
        match plan {
            SyncPlan::Append => self.render_pending(session_id, &changed, cx),
            SyncPlan::Rewind(index) => Hub::rewind_and_rebuild(self, session_id.to_string(), index, cx),
        }
    }

    fn render_pending(self: &Rc<Self>, session_id: &str, changed: &[String], cx: &mut App) {
        let mut new_children = Vec::new();
        let mut edited_files = Vec::new();
        {
            let mut sessions = self.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(session_id) else {
                return;
            };
            let visible = entry.transcript.visible_messages();
            let finished_shells: HashMap<String, render::ShellOutcome> = visible
                .iter()
                .filter_map(render::shell_notification)
                .collect();
            let repeated_errors = render::repeated_errors(visible);
            // A background command's notice updates the call that started it.
            let mut changed: Vec<String> = changed.to_vec();
            for message in visible {
                let notice = changed.contains(&message.id)
                    || !entry.rendered.contains(&message.id);
                if !notice {
                    continue;
                }
                let Some((shell_id, _)) = render::shell_notification(message) else {
                    continue;
                };
                if let Some(starter) = visible.iter().find(|candidate| {
                    candidate.as_assistant().is_some_and(|assistant| {
                        assistant.content.iter().any(|item| {
                            matches!(item, AssistantContent::Tool(tool)
                                if render::background_shell_id(tool) == Some(shell_id.as_str()))
                        })
                    })
                }) && !changed.contains(&starter.id)
                {
                    changed.push(starter.id.clone());
                }
            }
            let mut to_render: Vec<Message> = changed
                .iter()
                .filter(|id| entry.rendered.contains(id))
                .filter_map(|id| visible.iter().find(|message| &message.id == id).cloned())
                .collect();
            let new_messages: Vec<Message> = visible
                .get(entry.rendered.len()..)
                .unwrap_or_default()
                .to_vec();
            for message in &new_messages {
                entry.rendered.push(message.id.clone());
                if matches!(message.body, MessageBody::User(_)) {
                    entry.user_messages.insert(message.id.clone());
                }
            }
            to_render.extend(new_messages);
            let pending_permissions: HashSet<String> =
                entry.permissions.values().cloned().collect();
            let running_sessions: HashSet<String> = self
                .activity
                .borrow()
                .iter()
                .filter(|(_, activity)| **activity == Activity::Running)
                .map(|(id, _)| id.clone())
                .collect();
            let interrupted_sessions = self.interrupted.borrow().clone();
            let options = entry.options.clone();
            let streamed_shell_calls: HashSet<String> = entry
                .shells
                .iter()
                .filter(|(_, shell)| shell.streamed)
                .map(|(tool_id, _)| tool_id.clone())
                .collect();
            let skills = options.skills.borrow();
            let context = render::RenderContext {
                pending_permissions: &pending_permissions,
                running_sessions: &running_sessions,
                interrupted_sessions: &interrupted_sessions,
                finished_shells: &finished_shells,
                skills: &skills,
                streamed_shell_calls: &streamed_shell_calls,
                directory: Path::new(&options.directory),
            };
            let mut new_shells = Vec::new();
            // Edits in a loaded history were reviewed, or not, back then.
            let is_history = entry.output_tokens.is_none();
            for message in &to_render {
                let rendered = if repeated_errors.contains(&message.id) {
                    Ok(Vec::new())
                } else {
                    render::render_message(message, &context)
                };
                match rendered {
                    Ok(ops) => entry
                        .queue
                        .extend(ops.into_iter().map(|op| (message.id.clone(), op))),
                    Err(error) => log::warn!("Can't render OpenCode message: {error:#}"),
                }
                let tools = message.as_assistant().into_iter().flat_map(|assistant| {
                    assistant.content.iter().filter_map(|item| match item {
                        AssistantContent::Tool(tool) => Some(tool),
                        _ => None,
                    })
                });
                for tool in tools {
                    let edits = render::edited_files(tool, context.directory);
                    if !edits.is_empty() && entry.tracked_edits.insert(tool.id.clone()) && !is_history {
                        edited_files.extend(edits);
                    }
                    // A finished call may no longer name its shell, so this
                    // can't depend on the call still having a shell ID.
                    let live_shell =
                        render::live_shell(tool, &finished_shells).filter(|(_, running)| *running);
                    match (entry.shells.get_mut(&tool.id), live_shell) {
                        // The render above showed the final output, so
                        // streaming it as well would repeat it.
                        (Some(shell), None) if !shell.streamed => shell.closed = true,
                        (None, Some((shell_id, _))) => new_shells.push((
                            tool.id.clone(),
                            shell_id.to_string(),
                            render::terminal_id(tool),
                        )),
                        _ => {}
                    }
                }
                for child_id in subagent_children(message) {
                    if !entry.children.contains_key(child_id) {
                        new_children.push(child_id.to_string());
                    }
                    entry
                        .children
                        .insert(child_id.to_string(), message.id.clone());
                }
            }
            for (tool_id, shell_id, terminal_id) in new_shells {
                let task = cx.spawn({
                    let hub = Rc::downgrade(self);
                    let session_id = session_id.to_string();
                    let tool_id = tool_id.clone();
                    let directory = options.directory.clone();
                    async move |cx| {
                        Hub::follow_shell(hub, session_id, tool_id, shell_id, directory, terminal_id, cx)
                            .await
                    }
                });
                entry.shells.insert(
                    tool_id,
                    LiveShell {
                        streamed: false,
                        closed: false,
                        _task: task,
                    },
                );
            }
        }
        self.track_edited_files(session_id, edited_files, cx);
        // Before draining: the turn's stats are final once the thread goes idle.
        self.report_output_tokens(session_id, cx);
        self.drain(session_id, cx);
        self.update_usage(session_id, cx);
        for child_id in new_children {
            Hub::open_subagent(self, session_id, child_id, cx);
        }
    }

    /// Streams a running shell command's output into its terminal, like the
    /// TUI's shell output view, until the command ends or the thread closes.
    async fn follow_shell(
        hub: Weak<Hub>,
        session_id: String,
        tool_id: String,
        shell_id: String,
        directory: String,
        terminal_id: acp::TerminalId,
        cx: &mut AsyncApp,
    ) {
        const POLL_INTERVAL: Duration = Duration::from_secs(1);
        // Long logs (a dev server, say) replay only their end, and a poll
        // reads at most this much so fast output can't keep it busy.
        const MAX_READ: u64 = 1024 * 1024;
        const MAX_FAILURES: u32 = 5;

        let mut cursor: Option<u64> = None;
        let mut failures = 0;
        loop {
            let Some(client) = hub.upgrade().map(|hub| hub.client.clone()) else {
                return;
            };
            let poll = async {
                let Some(status) = client.shell_status(&directory, &shell_id).await? else {
                    return anyhow::Ok(false);
                };
                let mut replay_start = false;
                let mut position = match cursor {
                    Some(position) => position,
                    None => {
                        // Reading at the end returns only the size.
                        let Some(end) = client
                            .shell_output(&directory, &shell_id, JS_MAX_SAFE_INTEGER)
                            .await?
                        else {
                            return Ok(false);
                        };
                        let start = end.size.saturating_sub(MAX_READ);
                        replay_start = start > 0;
                        start
                    }
                };
                let read_until = position + MAX_READ;
                loop {
                    let Some(page) = client.shell_output(&directory, &shell_id, position).await?
                    else {
                        return Ok(false);
                    };
                    let (text, next) = whole_characters(&page, position, replay_start);
                    replay_start = false;
                    if !text.is_empty() {
                        let appended = cx.update(|cx| {
                            Hub::append_shell_output(&hub, &session_id, &tool_id, &terminal_id, text, cx)
                        });
                        if !appended {
                            return Ok(false);
                        }
                    }
                    let advanced = next > position;
                    position = next;
                    cursor = Some(position);
                    if !advanced || page.cursor >= page.size || position >= read_until {
                        break;
                    }
                }
                Ok(status == "running")
            };
            match poll.await {
                Ok(true) => failures = 0,
                Ok(false) => return,
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

    /// Returns false once the output shouldn't be shown anymore.
    fn append_shell_output(
        hub: &Weak<Hub>,
        session_id: &str,
        tool_id: &str,
        terminal_id: &acp::TerminalId,
        text: &str,
        cx: &mut App,
    ) -> bool {
        let Some(hub) = hub.upgrade() else {
            return false;
        };
        let thread = {
            let mut sessions = hub.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(session_id) else {
                return false;
            };
            let Some(shell) = entry.shells.get_mut(tool_id) else {
                return false;
            };
            if shell.closed {
                return false;
            }
            shell.streamed = true;
            entry.thread.clone()
        };
        // Released before updating the thread, whose events can reach back
        // into the hub.
        let bytes = render::terminal_bytes(text);
        match thread.update(cx, |thread, cx| {
            thread.append_display_terminal_output(terminal_id.clone(), &bytes, cx)
        }) {
            Ok(result) => result.log_err().is_some(),
            Err(_) => false,
        }
    }

    /// OpenCode edits files from its own process, so tell the action log,
    /// which is what Zed's diff review reads.
    fn track_edited_files(
        self: &Rc<Self>,
        session_id: &str,
        edited_files: Vec<render::EditedFile>,
        cx: &mut App,
    ) {
        if edited_files.is_empty() {
            return;
        }
        let mut sessions = self.sessions.borrow_mut();
        let Some(entry) = sessions.get_mut(session_id) else {
            return;
        };
        let Some(thread) = entry.thread.upgrade() else {
            return;
        };
        let project = thread.read(cx).project().clone();
        let action_log = thread.read(cx).action_log().clone();
        let previous = entry.edit_tracking.take();
        entry.edit_tracking = Some(cx.spawn(async move |cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            // Edits this close together are all on disk by now.
            for edits in edited_files.chunk_by(|a, b| a.path == b.path) {
                track_edited_file(&project, &action_log, edits, cx)
                    .await
                    .log_err();
            }
        }));
    }

    /// Feeds the turn stats: the output tokens of steps that just finished.
    /// OpenCode reports tokens per step, not while a step streams.
    fn report_output_tokens(&self, session_id: &str, cx: &mut App) {
        let (thread, added) = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(session_id) else {
                return;
            };
            let total = session_output_tokens(entry.transcript.visible_messages());
            let added = output_tokens_added(entry.output_tokens.replace(total), total);
            if added == 0 {
                return;
            }
            let Some(thread) = entry.thread.upgrade() else {
                return;
            };
            (thread, added)
        };
        thread.update(cx, |thread, cx| thread.add_turn_output_tokens(added, cx));
    }

    /// Feeds the context-usage circle: the last step's tokens against the
    /// model's context window, plus the session's cost. This matches what
    /// OpenCode's ACP bridge reports after each turn.
    fn update_usage(&self, session_id: &str, cx: &mut App) {
        let (thread, usage) = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(session_id) else {
                return;
            };
            let messages = entry.transcript.visible_messages();
            let last_step = messages.iter().rev().find_map(|message| {
                let assistant = message.as_assistant()?;
                let used = assistant.tokens.as_ref()?.total();
                (used > 0).then_some((used, assistant.model.clone()))
            });
            let Some((used, model)) = last_step else {
                return;
            };
            let model = model.unwrap_or_else(|| entry.options.model.borrow().clone());
            let models = entry.options.models.borrow();
            let Some(size) = models
                .iter()
                .find(|info| info.provider_id == model.provider_id && info.id == model.id)
                .or_else(|| models.iter().find(|info| info.id == model.id))
                .and_then(|info| info.limit.context)
            else {
                return;
            };
            let cost: f64 = messages
                .iter()
                .filter_map(|message| message.as_assistant()?.cost)
                .sum();
            let usage = (used, size, cost);
            if entry.usage == Some(usage) {
                return;
            }
            entry.usage = Some(usage);
            let Some(thread) = entry.thread.upgrade() else {
                return;
            };
            (thread, usage)
        };
        let (used, size, cost) = usage;
        thread.update(cx, |thread, cx| {
            thread.update_usage(
                acp::UsageUpdate::new(used, size).cost(acp::Cost::new(cost, "USD")),
                cx,
            )
        });
    }

    /// Re-renders the tool calls that show `child_id`, whose running state
    /// just changed.
    fn refresh_subagent_cards(self: &Rc<Self>, child_id: &str, cx: &mut App) {
        let parents: Vec<(String, String)> = self
            .sessions
            .borrow()
            .iter()
            .filter_map(|(session_id, entry)| {
                let message_id = entry.children.get(child_id)?;
                Some((session_id.clone(), message_id.clone()))
            })
            .collect();
        for (session_id, message_id) in parents {
            self.render_pending(&session_id, &[message_id], cx);
        }
    }

    /// Opens a subagent's session as a child of its parent's thread, so the
    /// subagent card can show its work.
    fn open_subagent(hub: &Rc<Hub>, parent_id: &str, child_id: String, cx: &mut App) {
        if hub.sessions.borrow().contains_key(&child_id) {
            return;
        }
        let (parent, options) = {
            let sessions = hub.sessions.borrow();
            let Some(entry) = sessions.get(parent_id) else {
                return;
            };
            let Some(parent) = entry.thread.upgrade() else {
                return;
            };
            (parent, entry.options.clone())
        };
        let (connection, project, parent_action_log, parent_session_id) = {
            let parent = parent.read(cx);
            (
                parent.connection().clone(),
                parent.project().clone(),
                parent.action_log().clone(),
                parent.session_id().clone(),
            )
        };
        let action_log = cx.new(|_| {
            ActionLog::new(project.clone()).with_linked_action_log(parent_action_log)
        });
        let child = cx.new(|cx| {
            let mut child = AcpThread::new(
                Some(parent_session_id),
                None,
                None,
                connection,
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
        hub.register_session(&child, &child_id, options, cx);
        let resync = Hub::resync(hub, child_id.clone(), cx);
        let hub = hub.clone();
        cx.spawn(async move |cx| {
            let result = resync.await;
            cx.update(|cx| {
                if let Some(entry) = hub.sessions.borrow_mut().get_mut(&child_id) {
                    entry.attached = true;
                }
                // Registering creates the subagent's view, so the snapshot
                // above went in before anything was watching.
                parent.update(cx, |parent, cx| parent.register_subagent(child, cx));
                hub.drain(&child_id, cx);
            });
            result
        })
        .detach_and_log_err(cx);
    }

    fn drain(self: &Rc<Self>, session_id: &str, cx: &mut App) {
        let (thread, ops) = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(session_id) else {
                return;
            };
            if entry.rewinding || entry.draining || entry.queue.is_empty() {
                return;
            }
            if entry.attached {
                entry.draining = true;
                (None, Vec::new())
            } else {
                (entry.thread.upgrade(), entry.queue.drain(..).collect())
            }
        };
        if let Some(thread) = thread {
            thread.update(cx, |thread, cx| {
                for (_, op) in ops {
                    op.apply(thread, cx).log_err();
                }
            });
            return;
        }
        let hub = self.clone();
        let session_id = session_id.to_string();
        cx.spawn(async move |cx| {
            while cx.update(|cx| hub.drain_step(&session_id, cx)) {}
        })
        .detach();
    }

    /// Applies queued changes until one adds an entry. Returns whether more
    /// remain.
    fn drain_step(&self, session_id: &str, cx: &mut App) -> bool {
        loop {
            let (thread, op) = {
                let mut sessions = self.sessions.borrow_mut();
                let Some(entry) = sessions.get_mut(session_id) else {
                    return false;
                };
                let next = if entry.rewinding {
                    None
                } else {
                    entry.thread.upgrade().zip(entry.queue.pop_front())
                };
                match next {
                    Some(next) => next,
                    None => {
                        entry.draining = false;
                        return false;
                    }
                }
            };
            let added = thread.update(cx, |thread, cx| {
                let entries = thread.entries().len();
                op.1.apply(thread, cx).log_err();
                thread.entries().len() > entries
            });
            if added {
                return true;
            }
        }
    }

    fn rewind_and_rebuild(hub: &Rc<Hub>, session_id: String, index: usize, cx: &mut App) {
        let (thread, target, flag) = {
            let mut sessions = hub.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(&session_id) else {
                return;
            };
            let Some(thread) = entry.thread.upgrade() else {
                return;
            };
            let target = entry.rendered[index].clone();
            entry.forget_rendered_from(index);
            entry.rewinding = true;
            entry.buffered.get_or_insert_with(Vec::new);
            (thread, target, entry.server_rewind.clone())
        };
        let hub = hub.clone();
        cx.spawn(async move |cx| {
            if let Ok(client_id) = client_message_id(&target) {
                flag.set(true);
                let rewind = thread.update(cx, |thread, cx| thread.rewind(client_id, cx));
                rewind.await.log_err();
                flag.set(false);
            }
            cx.update(|cx| {
                if let Some(entry) = hub.sessions.borrow_mut().get_mut(&session_id) {
                    entry.rewinding = false;
                }
                hub.render_pending(&session_id, &[], cx);
                hub.replay_buffered(&session_id, cx);
            });
        })
        .detach();
    }

    fn replay_buffered(self: &Rc<Self>, session_id: &str, cx: &mut App) {
        let buffered = self
            .sessions
            .borrow_mut()
            .get_mut(session_id)
            .and_then(|entry| entry.buffered.take())
            .unwrap_or_default();
        for event in buffered {
            self.handle_event(event, cx);
        }
    }

    /// Replaces the session's copy with a fresh snapshot from the server.
    /// Events that arrive meanwhile are buffered and replayed on top.
    fn resync(hub: &Rc<Hub>, session_id: String, cx: &mut App) -> Task<Result<()>> {
        {
            let mut sessions = hub.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(&session_id) else {
                return Task::ready(Ok(()));
            };
            entry.buffered.get_or_insert_with(Vec::new);
        }
        let hub = hub.clone();
        let client = hub.client.clone();
        cx.spawn(async move |cx| {
            let result = async {
                let running = client.active_sessions().await?.contains_key(&session_id);
                let info = client.session(&session_id).await?;
                let messages = client.all_messages(&session_id).await?;
                let permissions = client.pending_permissions(&session_id).await?;
                let forms = client.pending_forms(&session_id).await.unwrap_or_else(|error| {
                    log::warn!("Can't list OpenCode forms: {error:#}");
                    Vec::new()
                });
                anyhow::Ok((running, info, messages, permissions, forms))
            }
            .await;
            let prompts = cx.update(|cx| {
                let (running, info, messages, permissions, forms) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        if let Some(entry) = hub.sessions.borrow_mut().get_mut(&session_id) {
                            entry.buffered = None;
                        }
                        return Err(error);
                    }
                };
                let changed = {
                    let mut sessions = hub.sessions.borrow_mut();
                    let Some(entry) = sessions.get_mut(&session_id) else {
                        return Ok((Vec::new(), Vec::new()));
                    };
                    let diff = entry.transcript.replace_snapshot(Snapshot {
                        messages,
                        running,
                        revert: info.revert,
                    });
                    diff.changed
                };
                hub.activity.borrow_mut().insert(
                    session_id.clone(),
                    if running { Activity::Running } else { Activity::Idle },
                );
                hub.refresh_subagent_cards(&session_id, cx);
                hub.with_thread(&session_id, cx, |thread, cx| {
                    thread
                        .update_session_state(
                            state_update(if running { Activity::Running } else { Activity::Idle }),
                            cx,
                        )
                        .log_err();
                    // The server skips a sequence number before
                    // `session.renamed`, so a generated title usually arrives
                    // through a resync rather than as an event.
                    if let Some(title) = info.title.clone()
                        && thread.title().as_deref() != Some(title.as_str())
                    {
                        thread.update_session_info(acp::SessionInfoUpdate::new().title(title), cx);
                    }
                });
                // Rendering may rewind asynchronously; keep buffering until
                // it finishes, then replay.
                let needs_rewind = {
                    let sessions = hub.sessions.borrow();
                    sessions.get(&session_id).is_some_and(|entry| {
                        let visible = entry.transcript.visible_messages();
                        entry
                            .rendered
                            .iter()
                            .enumerate()
                            .any(|(ix, id)| visible.get(ix).map(|m| &m.id) != Some(id))
                    })
                };
                hub.sync_thread(&session_id, changed, cx);
                if !needs_rewind {
                    hub.replay_buffered(&session_id, cx);
                }
                anyhow::Ok((permissions, forms))
            })?;
            // Each prompt adds a thread entry, and the agent panel expects at
            // most one new entry per update.
            let (permissions, forms) = prompts;
            for permission in permissions {
                cx.update(|cx| hub.permission_asked(&session_id, &permission, cx));
            }
            for form in forms {
                cx.update(|cx| hub.form_created(&session_id, &form, cx));
            }
            Ok(())
        })
    }

    /// Shows an OpenCode form as an elicitation and sends back the answer.
    /// Forms Zed can't show are left for another client to answer.
    fn form_created(self: &Rc<Self>, session_id: &str, form: &Value, cx: &mut App) {
        let Some(form_id) = form.get("id").and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        let thread = {
            let sessions = self.sessions.borrow();
            let Some(entry) = sessions.get(session_id) else {
                return;
            };
            if entry.forms.contains_key(&form_id) {
                return;
            }
            entry.thread.upgrade()
        };
        let Some(thread) = thread else {
            return;
        };
        let tool_call_id = form
            .get("metadata")
            .and_then(|metadata| metadata.get("tool"))
            .and_then(|tool| tool.get("id"))
            .and_then(Value::as_str);
        let Some(request) = form::elicitation_request(form, session_id, tool_call_id) else {
            log::info!("OpenCode form {form_id} can't be shown in Zed; leaving it to other clients");
            return;
        };
        let shown = thread.update(cx, |thread, cx| thread.request_elicitation_with_id(request, cx));
        let (elicitation_id, response) = match shown {
            Ok(shown) => shown,
            Err(error) => {
                log::warn!("Can't show OpenCode form {form_id}: {error:?}");
                return;
            }
        };
        if let Some(entry) = self.sessions.borrow_mut().get_mut(session_id) {
            entry.forms.insert(form_id.clone(), elicitation_id);
        }
        let hub = self.clone();
        let session_id = session_id.to_string();
        let form = form.clone();
        cx.spawn(async move |cx| {
            let response = response.await;
            // Answered in another client already: the server settled it.
            let still_pending = cx.update(|_| {
                hub.sessions
                    .borrow_mut()
                    .get_mut(&session_id)
                    .and_then(|entry| entry.forms.remove(&form_id))
                    .is_some()
            });
            if !still_pending {
                return anyhow::Ok(());
            }
            match form::answer(&form, &response) {
                Some(answer) => hub.client.reply_form(&session_id, &form_id, &answer).await,
                None => hub.client.cancel_form(&session_id, &form_id, None).await,
            }
        })
        .detach_and_log_err(cx);
    }

    /// Another client answered or cancelled a form: close it here too.
    fn form_settled(&self, session_id: &str, data: &Value, cx: &mut App) {
        let Some(form_id) = data.get("id").and_then(Value::as_str) else {
            return;
        };
        let (thread, elicitation_id) = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(session_id) else {
                return;
            };
            let Some(elicitation_id) = entry.forms.remove(form_id) else {
                return;
            };
            (entry.thread.upgrade(), elicitation_id)
        };
        if let Some(thread) = thread {
            thread.update(cx, |thread, cx| thread.cancel_elicitation(&elicitation_id, cx));
        }
    }

    fn update_options_from_messages(&self, session_id: &str, changed: &[String], cx: &mut App) {
        let (options, thread, switches) = {
            let sessions = self.sessions.borrow();
            let Some(entry) = sessions.get(session_id) else {
                return;
            };
            let switches: Vec<MessageBody> = changed
                .iter()
                .filter_map(|id| {
                    entry
                        .transcript
                        .messages()
                        .iter()
                        .find(|message| &message.id == id)
                })
                .filter(|message| {
                    matches!(
                        message.body,
                        MessageBody::AgentSwitched { .. } | MessageBody::ModelSwitched { .. }
                    )
                })
                .map(|message| message.body.clone())
                .collect();
            (entry.options.clone(), entry.thread.clone(), switches)
        };
        if switches.is_empty() {
            return;
        }
        for switch in switches {
            match switch {
                MessageBody::AgentSwitched { agent } => *options.agent.borrow_mut() = agent,
                MessageBody::ModelSwitched { model } => *options.model.borrow_mut() = model,
                _ => {}
            }
        }
        options.publish(&thread, cx);
    }

    fn permission_asked(self: &Rc<Self>, session_id: &str, data: &Value, cx: &mut App) {
        let Some(request_id) = data.get("id").and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        let (thread, tool_id) = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(session_id) else {
                return;
            };
            if entry.permissions.contains_key(&request_id) {
                return;
            }
            let Some(thread) = entry.thread.upgrade() else {
                return;
            };
            let tool_id = data
                .get("source")
                .and_then(|source| source.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| request_id.clone());
            entry.permissions.insert(request_id.clone(), tool_id.clone());
            (thread, tool_id)
        };
        let action = data.get("action").and_then(Value::as_str).unwrap_or("tool");
        let resources: Vec<&str> = data
            .get("resources")
            .and_then(Value::as_array)
            .map(|resources| resources.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut tool_call = acp::ToolCallUpdate::new(tool_id.as_str());
        if tool_id == request_id {
            // Not tied to a tool call the thread shows; give it its own card.
            tool_call = tool_call
                .name(action.to_string())
                .title(format!("{action}: {}", resources.join(", ")))
                .kind(acp::ToolKind::Other);
        }
        let options = PermissionOptions::Flat(vec![
            acp::PermissionOption::new("always", "Always allow", acp::PermissionOptionKind::AllowAlways),
            acp::PermissionOption::new("once", "Allow", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new("reject", "Reject", acp::PermissionOptionKind::RejectOnce),
        ]);
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
                return;
            }
        };
        let hub = self.clone();
        let session_id = session_id.to_string();
        cx.spawn(async move |cx| {
            let outcome = outcome.await;
            let decision = match outcome {
                RequestPermissionOutcome::Selected(selected) => selected.option_id.0.to_string(),
                RequestPermissionOutcome::Cancelled
                | RequestPermissionOutcome::InterruptedByFollowUp => "reject".to_string(),
            };
            // Answered by another client already: the server settled it.
            let still_pending = cx.update(|_| {
                hub.sessions
                    .borrow_mut()
                    .get_mut(&session_id)
                    .and_then(|entry| entry.permissions.remove(&request_id))
                    .is_some()
            });
            if still_pending {
                hub.client
                    .reply_permission(&session_id, &request_id, &decision)
                    .await?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn permission_replied(&self, session_id: &str, data: &Value, cx: &mut App) {
        let Some(request_id) = data.get("requestID").and_then(Value::as_str) else {
            return;
        };
        let reply = data.get("reply").and_then(Value::as_str).unwrap_or("reject");
        let (thread, tool_id) = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(session_id) else {
                return;
            };
            let Some(tool_id) = entry.permissions.remove(request_id) else {
                return;
            };
            (entry.thread.upgrade(), tool_id)
        };
        let Some(thread) = thread else {
            return;
        };
        let kind = match reply {
            "always" => acp::PermissionOptionKind::AllowAlways,
            "once" => acp::PermissionOptionKind::AllowOnce,
            _ => acp::PermissionOptionKind::RejectOnce,
        };
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(
                acp::ToolCallId::new(tool_id.as_str()),
                SelectedPermissionOutcome::new(acp::PermissionOptionId::new(reply), kind),
                cx,
            );
        });
    }

    fn register_session(
        self: &Rc<Self>,
        thread: &Entity<AcpThread>,
        session_id: &str,
        options: Rc<SessionOptions>,
        cx: &mut App,
    ) {
        let release = cx.observe_release(thread, {
            let hub = Rc::downgrade(self);
            let session_id = session_id.to_string();
            move |_, _| {
                if let Some(hub) = hub.upgrade() {
                    hub.sessions.borrow_mut().remove(&session_id);
                    if let Ok(mut ids) = hub.open_session_ids.lock() {
                        ids.remove(&session_id);
                    }
                }
            }
        });
        self.sessions.borrow_mut().insert(
            session_id.to_string(),
            SessionEntry {
                thread: thread.downgrade(),
                transcript: SessionTranscript::new(session_id),
                rendered: Vec::new(),
                user_messages: HashSet::default(),
                buffered: Some(Vec::new()),
                server_rewind: Rc::default(),
                permissions: HashMap::default(),
                folder: options.directory.clone(),
                options,
                queue: VecDeque::new(),
                draining: false,
                attached: false,
                rewinding: false,
                children: HashMap::default(),
                usage: None,
                output_tokens: None,
                tracked_edits: HashSet::default(),
                edit_tracking: None,
                forms: HashMap::default(),
                shells: HashMap::default(),
                _release: Some(release),
            },
        );
        if let Ok(mut ids) = self.open_session_ids.lock() {
            ids.insert(session_id.to_string());
        }
    }
}

async fn track_edited_file(
    project: &Entity<Project>,
    action_log: &Entity<ActionLog>,
    edits: &[render::EditedFile],
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
            log::info!("Not tracking an edit to {:?}: it has unsaved changes", first.path);
            return Ok(());
        }
        // The file watcher may not have noticed the edit yet.
        buffer.update(cx, |buffer, cx| buffer.reload(cx)).await.ok();
    }
    action_log.update(cx, |action_log, cx| {
        action_log.buffer_edited_externally(
            buffer,
            kind,
            &|text| render::revert_edits(edits, text),
            cx,
        );
    });
    Ok(())
}

fn session_output_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .filter_map(|message| message.as_assistant()?.tokens.as_ref())
        .map(|tokens| tokens.output + tokens.reasoning)
        .sum()
}

/// A rewind can lower the total, which adds nothing to the turn.
fn output_tokens_added(reported: Option<u64>, total: u64) -> u64 {
    reported.map_or(0, |reported| total.saturating_sub(reported))
}

fn subagent_children(message: &Message) -> impl Iterator<Item = &str> {
    message
        .as_assistant()
        .into_iter()
        .flat_map(|assistant| assistant.content.iter())
        .filter_map(|item| match item {
            AssistantContent::Tool(tool) => {
                let (input, metadata) = match &tool.state {
                    ToolState::Running { input, metadata }
                    | ToolState::Completed {
                        input, metadata, ..
                    } => (input, metadata),
                    _ => return None,
                };
                render::subagent_session_id(&tool.name, input, metadata)
            }
            _ => None,
        })
}

impl SessionEntry {
    /// Drops rendered messages from `index` on, with their queued changes.
    fn forget_rendered_from(&mut self, index: usize) {
        let removed: HashSet<String> = self.rendered.drain(index..).collect();
        self.user_messages.retain(|id| !removed.contains(id));
        self.queue.retain(|(id, _)| !removed.contains(id));
    }
}

enum SyncPlan {
    Append,
    Rewind(usize),
}

fn state_update(activity: Activity) -> acp::StateUpdate {
    match activity {
        Activity::Running => acp::StateUpdate::Running(acp::RunningStateUpdate::new()),
        Activity::Idle => acp::StateUpdate::Idle(
            acp::IdleStateUpdate::new().stop_reason(acp::StopReason::EndTurn),
        ),
    }
}

/// A session's model, effort and agent. The model and effort use Zed's native
/// model picker and thinking control; the agent is a config option. IDs and
/// values match OpenCode's own ACP bridge (`cli/src/acp/config-option.ts`), so
/// saved defaults and favorites carry over.
struct SessionOptions {
    session_id: String,
    client: OpenCodeClient,
    models: RefCell<Vec<ModelInfo>>,
    providers: RefCell<Vec<ProviderInfo>>,
    agents: RefCell<Vec<AgentInfo>>,
    model: RefCell<ModelRef>,
    agent: RefCell<String>,
    /// The effort to return to when thinking is turned back on.
    effort_before_off: RefCell<Option<String>>,
    /// The lightbulb is off for a model that can't stop thinking, which
    /// only hides the effort selector.
    effort_hidden: Cell<bool>,
    skills: RefCell<Vec<Skill>>,
    /// Names of the server's commands in the session's directory: a prompt
    /// starting with `/` and one of these runs it.
    commands: RefCell<HashSet<String>>,
    tx: RefCell<watch::Sender<()>>,
    rx: watch::Receiver<()>,
    /// The session's directory, which locates its shells.
    directory: String,
    /// Saves favorites and defaults to the settings file.
    fs: Arc<dyn Fs>,
}

/// Stands for "no variant": the model's own default effort.
const DEFAULT_VARIANT: &str = "default";
/// The variant OpenCode models use for thinking off.
const NO_THINKING_VARIANT: &str = "none";
const MODEL_OPTION: &str = "model";
const EFFORT_OPTION: &str = "effort";
const MODE_OPTION: &str = "mode";

impl SessionOptions {
    fn current_model(&self) -> Option<ModelInfo> {
        let model = self.model.borrow();
        find_model(&self.models.borrow(), &model.provider_id, &model.id).cloned()
    }

    /// Grouped by provider like the native picker, providers and models in
    /// name order.
    fn model_list(&self) -> AgentModelList {
        let all_models = self.models.borrow();
        let mut models: Vec<&ModelInfo> = all_models.iter().collect();
        models.sort_by(|left, right| {
            self.provider_name(&left.provider_id)
                .cmp(&self.provider_name(&right.provider_id))
                .then_with(|| left.provider_id.cmp(&right.provider_id))
                .then_with(|| left.name.cmp(&right.name))
        });
        let mut groups: IndexMap<AgentModelGroupName, Vec<AgentModelInfo>> = IndexMap::default();
        for info in models {
            groups
                .entry(AgentModelGroupName(
                    self.provider_name(&info.provider_id).into(),
                ))
                .or_default()
                .push(model_info(info));
        }
        AgentModelList::Grouped(groups)
    }

    /// The picker needs a model even when the session's is gone from the
    /// list, say after its provider was disconnected.
    fn selected_model_info(&self) -> AgentModelInfo {
        if let Some(info) = self.current_model() {
            return model_info(&info);
        }
        let model = self.model.borrow();
        AgentModelInfo {
            id: AgentModelId::new(model_value(&model.provider_id, &model.id)),
            name: model.id.clone().into(),
            description: None,
            icon: provider_icon(&model.provider_id),
            is_latest: false,
            cost: None,
            disabled: None,
        }
    }

    fn provider_name(&self, provider_id: &str) -> String {
        self.providers
            .borrow()
            .iter()
            .find(|provider| provider.id == provider_id)
            .map_or(provider_id, |provider| provider.name.as_str())
            .to_string()
    }

    /// Tells the pickers the session's options changed. The thread's view
    /// redraws on `ConfigOptionsUpdated`; OpenCode D has no ACP config
    /// options, so the list is empty.
    fn publish(&self, thread: &WeakEntity<AcpThread>, cx: &mut App) {
        self.tx.borrow_mut().send(()).log_err();
        thread
            .update(cx, |_, cx| {
                cx.emit(acp_thread::AcpThreadEvent::ConfigOptionsUpdated(Vec::new()))
            })
            .log_err();
    }

    /// Sets the session's model and variant, then saves `default` as the
    /// default for new threads, as the native agent does.
    fn change_model(
        self: &Rc<Self>,
        model: ModelRef,
        default: (&'static str, String),
        thread: &WeakEntity<AcpThread>,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let options = self.clone();
        let thread = thread.clone();
        cx.spawn(async move |cx| {
            options.client.set_model(&options.session_id, &model).await?;
            *options.model.borrow_mut() = model;
            cx.update(|cx| {
                options.publish(&thread, cx);
                let (config_id, value) = default;
                agent_server().set_default_config_option(
                    config_id,
                    Some(AgentConfigOptionValue::ValueId(value)),
                    options.fs.clone(),
                    cx,
                );
            });
            Ok(())
        })
    }

    /// The model with `variant`, which is dropped if the model lacks it.
    fn model_with_variant(&self, variant: Option<String>) -> ModelRef {
        let mut model = self.model.borrow().clone();
        model.variant = variant.filter(|variant| {
            self.current_model().is_some_and(|info| {
                info.variants.iter().any(|candidate| &candidate.id == variant)
            })
        });
        model
    }
}

/// OpenCode names a custom agent by its ID, like `code-review`, which reads
/// better as "Code Review".
pub(crate) fn agent_display_name(agent: &AgentInfo) -> String {
    if agent.name == agent.id {
        title_case(&agent.id)
    } else {
        agent.name.clone()
    }
}

pub(crate) fn agent_display_name_for_id(agent_id: &str) -> String {
    title_case(agent_id)
}

fn is_selectable_agent(agent: &AgentInfo) -> bool {
    !agent.hidden && agent.mode.as_deref() != Some("subagent")
}

fn find_model<'a>(models: &'a [ModelInfo], provider_id: &str, id: &str) -> Option<&'a ModelInfo> {
    models
        .iter()
        .find(|info| info.provider_id == provider_id && info.id == id)
}

/// The `provider/id` form favorites and defaults are saved with.
fn model_value(provider_id: &str, id: &str) -> String {
    format!("{provider_id}/{id}")
}

fn model_info(info: &ModelInfo) -> AgentModelInfo {
    AgentModelInfo {
        id: AgentModelId::new(model_value(&info.provider_id, &info.id)),
        name: info.name.clone().into(),
        description: None,
        icon: provider_icon(&info.provider_id),
        is_latest: false,
        cost: cost_label(info),
        disabled: None,
    }
}

/// Priced like the native picker's token-priced models. Free models show
/// nothing, as local models do there.
fn cost_label(info: &ModelInfo) -> Option<SharedString> {
    let cost = info.cost.first()?;
    if cost.input <= 0.0 && cost.output <= 0.0 {
        return None;
    }
    Some(
        LanguageModelCostInfo::TokenCost {
            input_token_cost_per_1m: cost.input,
            output_token_cost_per_1m: cost.output,
        }
        .to_shared_string(),
    )
}

/// Zed's icon for an OpenCode provider ID, where Zed has one.
fn provider_icon(provider_id: &str) -> Option<AgentModelIcon> {
    let icon = match provider_id {
        "anthropic" => IconName::AiAnthropic,
        "claude-agent" => IconName::AiClaude,
        "openai" => IconName::AiOpenAi,
        "opencode" => IconName::AiOpenCode,
        "github-copilot" => IconName::Copilot,
        "google" | "google-vertex" => IconName::AiGoogle,
        "openrouter" => IconName::AiOpenRouter,
        "xai" => IconName::AiXAi,
        "deepseek" => IconName::AiDeepSeek,
        "mistral" => IconName::AiMistral,
        "ollama" => IconName::AiOllama,
        "lmstudio" => IconName::AiLmStudio,
        "amazon-bedrock" => IconName::AiBedrock,
        "vercel" => IconName::AiVercel,
        _ => return None,
    };
    Some(AgentModelIcon::Named(icon))
}

/// Zed's settings entry for OpenCode D, which holds its favorites and
/// defaults.
fn agent_server() -> CustomAgentServer {
    CustomAgentServer::new(AgentId::new(AGENT_ID))
}

fn title_case(value: &str) -> String {
    value
        .split(['_', '-'])
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Agents OpenCode ships with, listed ahead of the user's own.
const BUILT_IN_AGENTS: &[&str] = &["build", "plan"];

/// Feeds Zed's native profile picker with OpenCode's primary agents.
struct AgentProfiles {
    options: Rc<SessionOptions>,
    thread: WeakEntity<AcpThread>,
}

impl ProfileProvider for AgentProfiles {
    fn profile_id(&self, _cx: &App) -> AgentProfileId {
        AgentProfileId(self.options.agent.borrow().clone().into())
    }

    /// Shows the pick right away, and goes back if the server refuses it.
    fn set_profile(&self, profile_id: AgentProfileId, cx: &mut App) {
        let agent = profile_id.0.to_string();
        let previous = self.options.agent.replace(agent.clone());
        if previous == agent {
            return;
        }
        self.options.publish(&self.thread, cx);
        let options = self.options.clone();
        let thread = self.thread.clone();
        cx.spawn(async move |cx| {
            let result = options.client.set_agent(&options.session_id, &agent).await;
            if result.is_err() {
                cx.update(|cx| {
                    // A later pick stands.
                    if *options.agent.borrow() == agent {
                        *options.agent.borrow_mut() = previous;
                        options.publish(&thread, cx);
                    }
                });
            }
            result
        })
        .detach_and_log_err(cx);
    }

    fn profiles_supported(&self, _cx: &App) -> bool {
        true
    }

    fn model_selected(&self, _cx: &App) -> bool {
        true
    }

    /// The session's agent is listed even when it can't be picked, say after
    /// it was deleted, so the picker can still name it.
    fn available_profiles(&self, _cx: &App) -> AvailableProfiles {
        let mut profiles: AvailableProfiles = self
            .options
            .agents
            .borrow()
            .iter()
            .filter(|agent| is_selectable_agent(agent))
            .map(|agent| {
                (
                    AgentProfileId(agent.id.clone().into()),
                    SharedString::from(agent_display_name(agent)),
                )
            })
            .collect();
        let current = self.options.agent.borrow().clone();
        let current_id = AgentProfileId(current.clone().into());
        if !profiles.contains_key(&current_id) {
            let name = self
                .options
                .agents
                .borrow()
                .iter()
                .find(|agent| agent.id == current)
                .map_or_else(|| title_case(&current), agent_display_name);
            profiles.insert(current_id, name.into());
        }
        // Built-in agents first, like Zed's own profiles.
        profiles.sort_by_cached_key(|id, _| !BUILT_IN_AGENTS.contains(&id.as_str()));
        profiles
    }

    fn is_builtin_profile(&self, profile_id: &AgentProfileId) -> bool {
        BUILT_IN_AGENTS.contains(&profile_id.as_str())
    }

    fn profile_description(&self, profile_id: &AgentProfileId, _cx: &App) -> Option<SharedString> {
        self.options
            .agents
            .borrow()
            .iter()
            .find(|agent| agent.id == profile_id.as_str())
            .and_then(|agent| agent.description.clone())
            .map(SharedString::from)
    }

    fn save_default_profile(&self, profile_id: AgentProfileId, fs: Arc<dyn Fs>, cx: &mut App) {
        agent_server().set_default_config_option(
            MODE_OPTION,
            Some(AgentConfigOptionValue::ValueId(profile_id.0.to_string())),
            fs,
            cx,
        );
    }

    fn configure_action(&self) -> Box<dyn gpui::Action> {
        Box::new(crate::agents_modal::ManageAgents)
    }

    fn profiles_changed(&self) -> Option<watch::Receiver<()>> {
        Some(self.options.rx.clone())
    }
}

/// The model, effort and agent last picked in Zed, from its settings.
struct SavedDefaults {
    model: Option<String>,
    effort: Option<String>,
    agent: Option<String>,
}

impl SavedDefaults {
    /// The model and agent with each saved pick applied where it still
    /// exists. An effort the model lacks keeps the model's own.
    fn resolve(
        &self,
        models: &[ModelInfo],
        agents: &[AgentInfo],
        model: &ModelRef,
        agent: &str,
    ) -> (ModelRef, String) {
        let mut resolved = model.clone();
        let saved_model = self.model.as_deref().and_then(|value| {
            models
                .iter()
                .find(|info| model_value(&info.provider_id, &info.id) == value)
        });
        if let Some(info) = saved_model
            && (info.provider_id != resolved.provider_id || info.id != resolved.id)
        {
            resolved = ModelRef {
                provider_id: info.provider_id.clone(),
                id: info.id.clone(),
                variant: None,
            };
        }
        if let Some(effort) = &self.effort {
            let has_effort = find_model(models, &resolved.provider_id, &resolved.id)
                .is_some_and(|info| info.variants.iter().any(|variant| &variant.id == effort));
            if effort == DEFAULT_VARIANT {
                resolved.variant = None;
            } else if has_effort {
                resolved.variant = Some(effort.clone());
            }
        }
        let agent = self
            .agent
            .as_ref()
            .filter(|saved| {
                agents
                    .iter()
                    .any(|candidate| &candidate.id == *saved && is_selectable_agent(candidate))
            })
            .cloned()
            .unwrap_or_else(|| agent.to_string());
        (resolved, agent)
    }
}

/// Feeds Zed's native model picker.
struct ModelPicker {
    options: Rc<SessionOptions>,
    thread: WeakEntity<AcpThread>,
}

impl AgentModelSelector for ModelPicker {
    fn list_models(&self, _cx: &mut App) -> Task<Result<AgentModelList>> {
        Task::ready(Ok(self.options.model_list()))
    }

    fn select_model(&self, model_id: AgentModelId, cx: &mut App) -> Task<Result<()>> {
        let value = model_id.to_string();
        let models = self.options.models.borrow();
        let Some(info) = models
            .iter()
            .find(|info| model_value(&info.provider_id, &info.id) == value)
        else {
            return Task::ready(Err(anyhow!("unknown model {value}")));
        };
        // The effort carries over when the new model has it.
        let variant = self.options.model.borrow().variant.clone().filter(|variant| {
            info.variants.iter().any(|candidate| &candidate.id == variant)
        });
        let model = ModelRef {
            provider_id: info.provider_id.clone(),
            id: info.id.clone(),
            variant,
        };
        self.options
            .change_model(model, (MODEL_OPTION, value), &self.thread, cx)
    }

    fn selected_model(&self, _cx: &mut App) -> Task<Result<AgentModelInfo>> {
        Task::ready(Ok(self.options.selected_model_info()))
    }

    fn favorite_model_ids(&self, cx: &mut App) -> HashSet<AgentModelId> {
        agent_server()
            .favorite_config_option_value_ids(&acp::SessionConfigId::new(MODEL_OPTION), cx)
            .into_iter()
            .map(|value| AgentModelId::new(value.0.to_string()))
            .collect()
    }

    fn toggle_favorite_model(&self, model_id: AgentModelId, should_be_favorite: bool, cx: &App) {
        agent_server().toggle_favorite_config_option_value(
            acp::SessionConfigId::new(MODEL_OPTION),
            acp::SessionConfigValueId::new(model_id.to_string()),
            should_be_favorite,
            self.options.fs.clone(),
            cx,
        );
    }

    fn should_render_footer(&self) -> bool {
        true
    }

    fn footer_action(&self) -> Option<Box<dyn gpui::Action>> {
        Some(Box::new(zed_actions::OpenSettingsAt {
            path: zed_actions::OPENCODE_PROVIDERS_SETTINGS_PATH.to_string(),
            target: None,
        }))
    }

    fn watch(&self, _cx: &mut App) -> Option<watch::Receiver<()>> {
        Some(self.options.rx.clone())
    }
}

/// Feeds Zed's native thinking control from the model's variants. Thinking
/// off is the `none` variant. A model without one can't stop thinking, so its
/// lightbulb only hides the effort selector.
struct Thinking {
    options: Rc<SessionOptions>,
    thread: WeakEntity<AcpThread>,
}

impl Thinking {
    fn variants(&self) -> Vec<String> {
        self.options
            .current_model()
            .map(|info| info.variants.iter().map(|variant| variant.id.clone()).collect())
            .unwrap_or_default()
    }

    fn has_no_thinking_variant(&self) -> bool {
        self.variants().iter().any(|variant| variant == NO_THINKING_VARIANT)
    }

    fn levels(&self) -> Vec<String> {
        self.variants()
            .into_iter()
            .filter(|variant| variant != NO_THINKING_VARIANT && variant != DEFAULT_VARIANT)
            .collect()
    }

    fn set_variant(&self, variant: Option<String>, cx: &mut App) {
        let model = self.options.model_with_variant(variant);
        let saved = model.variant.clone().unwrap_or_else(|| DEFAULT_VARIANT.to_string());
        self.options
            .change_model(model, (EFFORT_OPTION, saved), &self.thread, cx)
            .detach_and_log_err(cx);
    }
}

impl AgentThinking for Thinking {
    fn effort_levels(&self) -> Vec<LanguageModelEffortLevel> {
        let levels = self.levels();
        if levels.is_empty() {
            return Vec::new();
        }
        let level = |value: &str, is_default: bool| LanguageModelEffortLevel {
            name: title_case(value).into(),
            value: value.to_string().into(),
            is_default,
            supports_disabling_thinking: None,
        };
        std::iter::once(level(DEFAULT_VARIANT, true))
            .chain(levels.iter().map(|value| level(value, false)))
            .collect()
    }

    fn selected_effort(&self) -> Option<String> {
        let variant = self.options.model.borrow().variant.clone();
        Some(
            variant
                .filter(|variant| self.levels().contains(variant))
                .unwrap_or_else(|| DEFAULT_VARIANT.to_string()),
        )
    }

    fn can_disable_thinking(&self) -> bool {
        true
    }

    fn thinking_enabled(&self) -> bool {
        if self.has_no_thinking_variant() {
            self.options.model.borrow().variant.as_deref() != Some(NO_THINKING_VARIANT)
        } else {
            !self.options.effort_hidden.get()
        }
    }

    fn set_thinking_enabled(&self, enabled: bool, cx: &mut App) {
        if enabled == self.thinking_enabled() {
            return;
        }
        if !self.has_no_thinking_variant() {
            self.options.effort_hidden.set(!enabled);
            self.options.publish(&self.thread, cx);
            return;
        }
        if enabled {
            let effort = self.options.effort_before_off.borrow_mut().take();
            self.set_variant(effort, cx);
        } else {
            *self.options.effort_before_off.borrow_mut() =
                self.options.model.borrow().variant.clone();
            self.set_variant(Some(NO_THINKING_VARIANT.to_string()), cx);
        }
    }

    fn set_effort(&self, effort: String, cx: &mut App) {
        let variant = (effort != DEFAULT_VARIANT).then_some(effort);
        self.set_variant(variant, cx);
    }
}

pub struct OpenCodeConnection {
    hub: Rc<Hub>,
}

impl OpenCodeConnection {
    pub fn new(hub: Rc<Hub>) -> Self {
        Self { hub }
    }

    /// The session's thinking settings, if its model has any.
    pub fn thinking(&self, session_id: &str) -> Option<Rc<dyn AgentThinking>> {
        let sessions = self.hub.sessions.borrow();
        let entry = sessions.get(session_id)?;
        let has_variants = entry
            .options
            .current_model()
            .is_some_and(|info| !info.variants.is_empty());
        has_variants.then(|| {
            Rc::new(Thinking {
                options: entry.options.clone(),
                thread: entry.thread.clone(),
            }) as Rc<dyn AgentThinking>
        })
    }

    pub(crate) fn modal_source(&self, session_id: &str) -> Option<crate::agents_modal::ModalSource> {
        let sessions = self.hub.sessions.borrow();
        let options = &sessions.get(session_id)?.options;
        let mut models: Vec<crate::agents_modal::ModelChoice> = options
            .models
            .borrow()
            .iter()
            .map(|info| crate::agents_modal::ModelChoice {
                value: model_value(&info.provider_id, &info.id),
                name: info.name.clone().into(),
                provider: options.provider_name(&info.provider_id).into(),
            })
            .collect();
        models.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then_with(|| left.name.cmp(&right.name))
        });
        Some(crate::agents_modal::ModalSource {
            client: options.client.clone(),
            directory: options.directory.clone(),
            models,
        })
    }

    pub fn session_folders(&self, session_id: &str) -> Option<PathList> {
        let sessions = self.hub.sessions.borrow();
        let folder = &sessions.get(session_id)?.folder;
        (!folder.is_empty()).then(|| PathList::new(&[PathBuf::from(folder)]))
    }

    pub fn steering(&self, session_id: &str) -> Option<Rc<dyn AgentSteering>> {
        self.hub.sessions.borrow().get(session_id)?;
        Some(Rc::new(Steering {
            hub: self.hub.clone(),
            session_id: session_id.to_string(),
        }))
    }

    pub fn profiles(&self, session_id: &str) -> Option<Arc<dyn ProfileProvider>> {
        let sessions = self.hub.sessions.borrow();
        let entry = sessions.get(session_id)?;
        // The picker's trait takes an `Arc`, though it only ever runs on the
        // main thread.
        #[allow(clippy::arc_with_non_send_sync)]
        Some(Arc::new(AgentProfiles {
            options: entry.options.clone(),
            thread: entry.thread.clone(),
        }))
    }

    /// The skills Zed shows in its `/` and `@` menus for a session.
    pub fn session_skills(&self, session_id: &str) -> Vec<AvailableSkill> {
        let sessions = self.hub.sessions.borrow();
        let Some(entry) = sessions.get(session_id) else {
            return Vec::new();
        };
        entry
            .options
            .skills
            .borrow()
            .iter()
            .map(|skill| AvailableSkill {
                name: skill.id.as_str().into(),
                description: skill.description.as_str().into(),
                source: skill.scope.clone().into(),
                skill_file_path: skill.path.clone(),
                warning: None,
            })
            .collect()
    }

    /// Opens a thread for `session`. A new session takes the model, effort
    /// and agent last picked in Zed, like a new native thread.
    fn open_thread(
        self: Rc<Self>,
        session: SessionInfo,
        is_new: bool,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let client = self.hub.client.clone();
        let fs = project.read(cx).fs().clone();
        let defaults = is_new.then(|| {
            let server = agent_server();
            let value = |config_id: &str| {
                server
                    .default_config_option(config_id, cx)
                    .and_then(|value| value.as_value_id().map(str::to_string))
            };
            SavedDefaults {
                model: value(MODEL_OPTION),
                effort: value(EFFORT_OPTION),
                agent: value(MODE_OPTION),
            }
        });
        let directory = session
            .location
            .as_ref()
            .map(|location| location.directory.clone())
            .or_else(|| work_dirs.paths().first().map(|path| path.display().to_string()))
            .unwrap_or_default();
        let register_mcp = self.hub.mcp.watch(&project, &directory, cx);
        let catalog_version = self.hub.catalog_version(&directory);
        cx.spawn(async move |cx| {
            register_mcp.await;
            let (models, agents, default_model, commands, skills, providers) = futures::join!(
                client.models(&directory),
                client.agents(&directory),
                client.default_model(&directory),
                client.commands(&directory),
                client.skills(&directory),
                client.providers(&directory),
            );
            let mut skills = skills.log_err().unwrap_or_default();
            let skills = cx
                .background_spawn({
                    let directory = PathBuf::from(&directory);
                    async move {
                        prepare_skills(&mut skills, &directory);
                        skills
                    }
                })
                .await;
            let models = models?;
            let agents = agents.unwrap_or_default();
            let mut model = match session.model.clone() {
                Some(model) => model,
                None => {
                    let default_model = default_model?;
                    ModelRef {
                        provider_id: default_model.provider_id,
                        id: default_model.id,
                        variant: None,
                    }
                }
            };
            let mut agent = session.agent.clone().unwrap_or_else(|| "build".into());
            if let Some(defaults) = defaults {
                let (saved_model, saved_agent) = defaults.resolve(&models, &agents, &model, &agent);
                // A saved pick the server refuses is dropped; the thread
                // still opens with OpenCode's own choice.
                if saved_model != model
                    && client.set_model(&session.id, &saved_model).await.log_err().is_some()
                {
                    model = saved_model;
                }
                if saved_agent != agent
                    && client.set_agent(&session.id, &saved_agent).await.log_err().is_some()
                {
                    agent = saved_agent;
                }
            }
            let commands = commands.log_err().unwrap_or_default();
            let (tx, rx) = watch::channel(());
            let options = Rc::new(SessionOptions {
                session_id: session.id.clone(),
                client: client.clone(),
                models: RefCell::new(models),
                providers: RefCell::new(providers.log_err().unwrap_or_default()),
                agents: RefCell::new(agents),
                model: RefCell::new(model),
                agent: RefCell::new(agent),
                effort_before_off: RefCell::new(None),
                effort_hidden: Cell::new(false),
                skills: RefCell::new(skills),
                commands: RefCell::new(commands.iter().map(|command| command.name.clone()).collect()),
                tx: RefCell::new(tx),
                rx,
                directory: directory.clone(),
                fs,
            });

            let work_dirs = if work_dirs.paths().is_empty() {
                PathList::new(&[PathBuf::from(&directory)])
            } else {
                work_dirs
            };
            let thread = cx.update(|cx| {
                let action_log = cx.new(|_| ActionLog::new(project.clone()));
                let thread = cx.new(|cx| {
                    AcpThread::new(
                        None,
                        session.title.clone().map(SharedString::from),
                        Some(work_dirs),
                        self.clone(),
                        project,
                        action_log,
                        acp::SessionId::new(session.id.as_str()),
                        watch::Receiver::constant(prompt_capabilities()),
                        cx,
                    )
                });
                self.hub
                    .register_session(&thread, &session.id, options, cx);
                thread.update(cx, |thread, cx| {
                    thread.update_available_commands(available_commands(&commands), cx);
                });
                // The server may have finished loading them after answering.
                if self.hub.catalog_version(&directory) != catalog_version {
                    Hub::refresh_catalog(&self.hub, directory.clone(), cx);
                }
                thread
            });
            let resync = cx.update(|cx| Hub::resync(&self.hub, session.id.clone(), cx));
            resync.await?;
            cx.update(|cx| {
                if let Some(entry) = self.hub.sessions.borrow_mut().get_mut(&session.id) {
                    entry.attached = true;
                }
                // Anything that arrived while loading goes out one entry at a time.
                self.hub.drain(&session.id, cx);
            });
            Ok(thread)
        })
    }
}

fn prompt_capabilities() -> acp::PromptCapabilities {
    acp::PromptCapabilities::new()
        .image(acp::PromptImageCapabilities::new())
        .embedded_context(acp::PromptEmbeddedContextCapabilities::new())
}

/// Sets each skill's scope. Built-in skills have no file, but Zed reads a
/// skill's file when it's mentioned, so their content is written under Zed's
/// data directory. Skills left without a readable file are dropped.
fn prepare_skills(skills: &mut Vec<Skill>, project_directory: &Path) {
    let directory = paths::data_dir().join("opencode_agent").join("skills");
    skills.retain_mut(|skill| {
        if skill.path.is_file() {
            skill.scope = match project_directory.file_name() {
                Some(name) if skill.path.starts_with(project_directory) => {
                    name.to_string_lossy().into_owned()
                }
                _ => "global".to_string(),
            };
            return true;
        }
        if skill.id.is_empty() || skill.id.starts_with('.') || skill.id.contains(['/', '\\']) {
            return false;
        }
        let path = directory.join(&skill.id).join("SKILL.md");
        if std::fs::read_to_string(&path).ok().as_deref() != Some(skill.content.as_str()) {
            let written = path
                .parent()
                .context("skill path has no parent")
                .and_then(|parent| Ok(std::fs::create_dir_all(parent)?))
                .and_then(|()| Ok(std::fs::write(&path, &skill.content)?));
            if written.log_err().is_none() {
                return false;
            }
        }
        skill.path = path;
        skill.scope = "built-in".to_string();
        true
    });
}

/// The text sent when a thread is mentioned with `@`: a summary written by
/// the thread's own model from its context, using the native agent's prompt.
/// OpenCode prompts can't reference another session, and generating leaves
/// the mentioned thread's history unchanged.
pub(crate) async fn thread_summary(hub: &Hub, session_id: &str) -> Result<String> {
    hub.client
        .generate(session_id, agent_settings::SUMMARIZE_THREAD_DETAILED_PROMPT)
        .await
        .context("Failed to generate summary")
}

fn available_commands(commands: &[NamedItem]) -> Vec<acp::AvailableCommand> {
    // Marked native so Zed shows its compact icon and sends it bare, queueing
    // anything typed after it as the next message.
    std::iter::once(acp::AvailableCommand::Command(
        acp::Command::new(
            "compact",
            "Summarize the conversation so far to free up context",
        )
        .meta(acp_thread::meta_with_command_category(
            acp_thread::CommandCategory::Native,
        )),
    ))
    .chain(commands.iter().map(|command| {
        acp::AvailableCommand::Command(acp::Command::new(
            command.name.clone(),
            command.description.clone().unwrap_or_default(),
        ))
    }))
    .collect()
}

struct PromptParts {
    text: String,
    files: Vec<Value>,
    /// IDs of mentioned skills. OpenCode loads a skill itself, so Zed sends
    /// only its ID, like the TUI does.
    skills: Vec<String>,
}

fn skill_mention_id(uri: &str) -> Option<String> {
    match MentionUri::parse(uri, PathStyle::local()).ok()? {
        MentionUri::Skill { name, .. } => Some(name),
        _ => None,
    }
}

/// Splits Zed's prompt into OpenCode's text and file attachments. Whole files
/// and images go as attachments the server reads itself; selections, symbols
/// and other resolved context go inline.
fn prompt_parts(content: &[acp::ContentBlock]) -> PromptParts {
    let mut text = String::new();
    let mut files = Vec::new();
    let mut skills = Vec::new();
    for block in content {
        match block {
            acp::ContentBlock::Text(content) => text.push_str(&content.text),
            acp::ContentBlock::ResourceLink(link) => {
                if let Some(id) = skill_mention_id(&link.uri) {
                    text.push_str(&format!("@{id}"));
                    skills.push(id);
                    continue;
                }
                if link.uri.starts_with("file://") && !link.uri.contains('#') {
                    files.push(serde_json::json!({ "uri": link.uri, "name": link.name }));
                }
                text.push_str(&format!("@{}", link.name));
            }
            acp::ContentBlock::Resource(resource) => match &resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(contents) => {
                    if let Some(id) = skill_mention_id(&contents.uri) {
                        text.push_str(&format!("@{id}"));
                        skills.push(id);
                    } else if contents.uri.starts_with("file://") && !contents.uri.contains('#') {
                        // The URI escapes characters such as spaces; the name
                        // the model reads shouldn't.
                        let name = MentionUri::parse(&contents.uri, PathStyle::local())
                            .map(|uri| uri.name())
                            .unwrap_or_else(|_| contents.uri.clone());
                        files.push(serde_json::json!({ "uri": contents.uri, "name": name }));
                        text.push_str(&format!("@{name}"));
                    } else {
                        text.push_str(&format!(
                            "\n<context uri=\"{}\">\n{}\n</context>\n",
                            contents.uri, contents.text
                        ));
                    }
                }
                _ => {}
            },
            acp::ContentBlock::Image(image) => {
                files.push(serde_json::json!({
                    "uri": format!("data:{};base64,{}", image.mime_type.0, image.data),
                    "name": "image",
                }));
            }
            _ => {}
        }
    }
    let mut seen = HashSet::default();
    skills.retain(|id| seen.insert(id.clone()));
    PromptParts {
        text,
        files,
        skills,
    }
}

/// Zed keeps a sent message pending until a user message with its receipt ID
/// appears, but a command adds none of its own. Its receipt is therefore the
/// latest user message, waiting briefly for the one a command's template
/// creates when nothing has been sent yet.
async fn command_receipt(
    hub: &Hub,
    session_id: &str,
    max_wait: Duration,
    cx: &mut AsyncApp,
) -> Option<String> {
    const POLL_INTERVAL: Duration = Duration::from_millis(100);
    let latest_user_message = || {
        let sessions = hub.sessions.borrow();
        let entry = sessions.get(session_id)?;
        entry
            .rendered
            .iter()
            .rev()
            .find(|id| entry.user_messages.contains(*id))
            .cloned()
    };
    let mut waited = Duration::ZERO;
    loop {
        if let Some(id) = latest_user_message() {
            return Some(id);
        }
        if waited >= max_wait {
            return None;
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
        waited += POLL_INTERVAL;
    }
}

/// Where reading from the end of a shell's output starts: the largest
/// position the server, which counts in JavaScript numbers, accepts.
const JS_MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

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

struct Submissions {
    hub: Rc<Hub>,
    session_id: String,
}

impl ReceiptSessionSubmissions for Submissions {
    fn submit(
        &self,
        content: Vec<acp::ContentBlock>,
        cx: &mut Context<AcpThread>,
    ) -> ReceiptSubmission {
        let client = self.hub.client.clone();
        let session_id = self.session_id.clone();
        let message_id = new_message_id();
        let PromptParts {
            mut text,
            files,
            mut skills,
        } = prompt_parts(&content);
        let display_metadata = render::display_metadata(&content);

        let command = text.strip_prefix('/').and_then(|rest| {
            let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let is_command = name == "compact"
                || self
                    .hub
                    .sessions
                    .borrow()
                    .get(&session_id)
                    .is_some_and(|entry| entry.options.commands.borrow().contains(name));
            is_command.then(|| (name.to_string(), arguments.trim().to_string()))
        });

        // Zed accepts a skill typed as `/name` without picking it from the
        // menu. OpenCode has no such syntax, so it's sent as a mention.
        if command.is_none()
            && let Some(rest) = text.strip_prefix('/')
        {
            let name = rest.split(char::is_whitespace).next().unwrap_or_default();
            let is_skill = self
                .hub
                .sessions
                .borrow()
                .get(&session_id)
                .is_some_and(|entry| {
                    entry.options.skills.borrow().iter().any(|skill| skill.id == name)
                });
            if is_skill {
                if !skills.iter().any(|id| id == name) {
                    skills.insert(0, name.to_string());
                }
                text.replace_range(..1, "@");
            }
        }

        let local_message = if command.is_none() {
            match client_message_id(&message_id) {
                Ok(client_id) => {
                    if let Some(entry) = self.hub.sessions.borrow_mut().get_mut(&session_id) {
                        entry.rendered.push(message_id.clone());
                        entry.user_messages.insert(message_id.clone());
                    }
                    Some((client_id, content))
                }
                Err(error) => return Task::ready(Err(error)).into(),
            }
        } else {
            None
        };

        let hub = self.hub.clone();
        // `submit` runs inside the thread's own update, so the user message
        // is added from a task rather than here.
        cx.spawn(async move |thread, cx| {
            if let Some((client_id, content)) = local_message {
                thread.update(cx, |thread, cx| {
                    thread.upsert_local_user_message(client_id, content, cx)
                })??;
            }
            let is_command = command.is_some();
            // Compacting adds no user message, so there's nothing to wait for.
            let is_compact = command.as_ref().is_some_and(|(name, _)| name == "compact");
            match command {
                Some((name, _)) if name == "compact" => client.compact(&session_id).await?,
                Some((name, arguments)) => {
                    client
                        .command(&session_id, &name, &arguments, &skills)
                        .await?
                }
                None => {
                    client
                        .prompt(
                            &session_id,
                            &message_id,
                            &text,
                            &files,
                            &skills,
                            display_metadata,
                            None,
                        )
                        .await?
                }
            }
            if is_command {
                let max_wait = if is_compact {
                    Duration::ZERO
                } else {
                    Duration::from_secs(30)
                };
                let response = match command_receipt(&hub, &session_id, max_wait, cx).await {
                    Some(receipt) => acp::PromptResponse::new(receipt.as_str()),
                    None => acp::PromptResponse::new(message_id.as_str())
                        .meta(acp_thread::meta_with_no_echo()),
                };
                return Ok(response);
            }
            Ok(acp::PromptResponse::new(message_id.as_str()))
        })
        .into()
    }
}

/// Sends a queued message into the running turn. OpenCode adds it at the
/// next step, and until then it can be taken back.
struct Steering {
    hub: Rc<Hub>,
    session_id: String,
}

impl AgentSteering for Steering {
    /// Commands run as their own turn, so they wait.
    fn can_steer(&self, content: &[acp::ContentBlock]) -> bool {
        !prompt_parts(content).text.starts_with('/')
    }

    fn steer(
        &self,
        content: Vec<acp::ContentBlock>,
        cx: &mut App,
    ) -> (SharedString, Task<Result<bool>>) {
        let message_id = new_message_id();
        let PromptParts {
            text,
            files,
            skills,
        } = prompt_parts(&content);
        let metadata = render::display_metadata(&content);
        let (settled_tx, settled_rx) = oneshot::channel();
        // Registered first: the delivery event can beat the response.
        self.hub
            .steers
            .borrow_mut()
            .insert(message_id.clone(), (self.session_id.clone(), settled_tx));
        let hub = self.hub.clone();
        let session_id = self.session_id.clone();
        let task = cx.spawn({
            let message_id = message_id.clone();
            async move |cx| {
                let sent = hub
                    .client
                    .prompt(
                        &session_id,
                        &message_id,
                        &text,
                        &files,
                        &skills,
                        metadata,
                        Some("steer"),
                    )
                    .await;
                if let Err(error) = sent {
                    cx.update(|_| {
                        hub.steers.borrow_mut().remove(&message_id);
                        hub.cancelled_steers.borrow_mut().remove(&message_id);
                    });
                    return Err(error);
                }
                // A cancel sent before OpenCode had the prompt did nothing.
                let cancelled = cx.update(|_| hub.cancelled_steers.borrow().contains(&message_id));
                if cancelled {
                    hub.client.cancel_inbox(&session_id, &message_id).await?;
                }
                settled_rx
                    .await
                    .map_err(|_| anyhow!("OpenCode D closed before the message was delivered"))
            }
        });
        (message_id.into(), task)
    }

    fn cancel_steer(&self, id: SharedString, cx: &mut App) {
        self.hub
            .cancelled_steers
            .borrow_mut()
            .insert(id.to_string());
        let client = self.hub.client.clone();
        let session_id = self.session_id.clone();
        // The outcome comes from OpenCode's events: `cancelled`, or
        // `delivered` when the cancel came too late.
        cx.background_spawn(async move { client.cancel_inbox(&session_id, &id).await })
            .detach_and_log_err(cx);
    }
}

struct Truncate {
    hub: Rc<Hub>,
    session_id: String,
}

impl AgentSessionTruncate for Truncate {
    fn restores_files(&self) -> bool {
        true
    }

    fn run(&self, client_user_message_id: ClientUserMessageId, cx: &mut App) -> Task<Result<()>> {
        let message_id = message_id_string(&client_user_message_id);
        let server_rewind = {
            let mut sessions = self.hub.sessions.borrow_mut();
            let Some(entry) = sessions.get_mut(&self.session_id) else {
                return Task::ready(Err(anyhow!("session is not open")));
            };
            if entry.server_rewind.get() {
                true
            } else {
                if let Some(ix) = entry.rendered.iter().position(|id| *id == message_id) {
                    entry.forget_rendered_from(ix);
                }
                false
            }
        };
        if server_rewind {
            return Task::ready(Ok(()));
        }
        let client = self.hub.client.clone();
        let session_id = self.session_id.clone();
        let running = self.hub.activity(&session_id) == Some(Activity::Running);
        cx.background_spawn(async move {
            if running {
                client.interrupt(&session_id).await?;
                client.wait_until_idle(&session_id).await.log_err();
            }
            client.stage_revert(&session_id, &message_id).await
        })
    }
}

struct SetTitle {
    client: OpenCodeClient,
    session_id: String,
}

impl AgentSessionSetTitle for SetTitle {
    fn run(&self, title: SharedString, cx: &mut App) -> Task<Result<()>> {
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        cx.background_spawn(async move { client.set_title(&session_id, &title).await })
    }
}

pub(crate) fn session_list_info(session: &SessionInfo) -> AgentSessionInfo {
    let mut info = AgentSessionInfo::new(acp::SessionId::new(session.id.as_str()));
    info.title = session.title.clone().map(SharedString::from);
    info.work_dirs = session
        .location
        .as_ref()
        .map(|location| PathList::new(&[PathBuf::from(&location.directory)]));
    info.created_at = session.time.created.and_then(chrono::DateTime::from_timestamp_millis);
    info.updated_at = session
        .time
        .updated
        .or(session.time.created)
        .and_then(chrono::DateTime::from_timestamp_millis);
    info
}

struct SessionList {
    client: OpenCodeClient,
}

impl AgentSessionList for SessionList {
    fn list_sessions(
        &self,
        request: AgentSessionListRequest,
        cx: &mut App,
    ) -> Task<Result<AgentSessionListResponse>> {
        let client = self.client.clone();
        cx.background_spawn(async move {
            let directory = request.cwd.map(|cwd| cwd.display().to_string());
            let page = client
                .list_sessions(ListSessions {
                    directory: directory.as_deref(),
                    roots_only: true,
                    cursor: request.cursor.as_deref(),
                    limit: Some(100),
                })
                .await?;
            let mut response = AgentSessionListResponse::new(
                page.data.iter().map(session_list_info).collect(),
            );
            response.next_cursor = page.cursor.next.filter(|_| page.data.len() == 100);
            Ok(response)
        })
    }

    fn supports_delete(&self) -> bool {
        true
    }

    fn delete_session(&self, session_id: &acp::SessionId, cx: &mut App) -> Task<Result<()>> {
        let client = self.client.clone();
        let session_id = session_id.0.to_string();
        cx.background_spawn(async move { client.delete_session(&session_id).await })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

impl AgentConnection for OpenCodeConnection {
    fn agent_id(&self) -> AgentId {
        AgentId::new(AGENT_ID)
    }

    fn telemetry_id(&self) -> SharedString {
        "opencode-direct".into()
    }

    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let client = self.hub.client.clone();
        let Some(directory) = work_dirs.paths().first().map(|path| path.display().to_string())
        else {
            return Task::ready(Err(anyhow!("OpenCode D needs a project folder")));
        };
        cx.spawn(async move |cx| {
            let session = client.create_session(&directory).await?;
            cx.update(|cx| self.open_thread(session, true, project, work_dirs, cx))
                .await
        })
    }

    fn supports_load_session(&self) -> bool {
        true
    }

    fn load_session(
        self: Rc<Self>,
        session_id: acp::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        _title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let client = self.hub.client.clone();
        cx.spawn(async move |cx| {
            let session = client.session(&session_id.0).await?;
            cx.update(|cx| self.open_thread(session, false, project, work_dirs, cx))
                .await
        })
    }

    fn should_close_sessions_on_release(&self) -> bool {
        false
    }

    fn auth_methods(&self) -> &[acp::AuthMethod] {
        &[]
    }

    fn authenticate(&self, _method: acp::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn receipt_submissions(
        &self,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn ReceiptSessionSubmissions>> {
        Some(Rc::new(Submissions {
            hub: self.hub.clone(),
            session_id: session_id.0.to_string(),
        }))
    }

    fn cancel(&self, session_id: &acp::SessionId, cx: &mut App) -> Result<()> {
        let hub = self.hub.clone();
        let session_id = session_id.0.to_string();
        cx.spawn(async move |cx| {
            let interrupt = hub.client.interrupt(&session_id).await;
            // Reload either way, so a thread Zed still thinks is running
            // stops even when the server already finished the turn.
            let resync = cx.update(|cx| Hub::resync(&hub, session_id, cx)).await;
            interrupt.and(resync)
        })
        .detach_and_log_err(cx);
        Ok(())
    }

    fn truncate(
        &self,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn AgentSessionTruncate>> {
        Some(Rc::new(Truncate {
            hub: self.hub.clone(),
            session_id: session_id.0.to_string(),
        }))
    }

    fn set_title(
        &self,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn AgentSessionSetTitle>> {
        Some(Rc::new(SetTitle {
            client: self.hub.client.clone(),
            session_id: session_id.0.to_string(),
        }))
    }

    fn model_selector(&self, session_id: &acp::SessionId) -> Option<Rc<dyn AgentModelSelector>> {
        let sessions = self.hub.sessions.borrow();
        let entry = sessions.get(session_id.0.as_ref())?;
        Some(Rc::new(ModelPicker {
            options: entry.options.clone(),
            thread: entry.thread.clone(),
        }))
    }

    fn session_list(&self, _cx: &mut App) -> Option<Rc<dyn AgentSessionList>> {
        Some(Rc::new(SessionList {
            client: self.hub.client.clone(),
        }))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_resource(name: &str) -> acp::ContentBlock {
        let uri = MentionUri::Skill {
            name: name.to_string(),
            source: String::new(),
            skill_file_path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
        };
        acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents::new(
                "skill file content",
                uri.to_uri().to_string(),
            )),
        ))
    }

    fn page(output: &str, cursor: u64) -> ShellOutput {
        ShellOutput {
            output: output.into(),
            cursor,
            size: cursor,
        }
    }

    #[test]
    fn only_new_output_tokens_count_towards_the_turn() {
        // The first render may hold a loaded history, which isn't this turn's.
        assert_eq!(output_tokens_added(None, 3000), 0);
        assert_eq!(output_tokens_added(Some(3000), 3000), 0);
        assert_eq!(output_tokens_added(Some(3000), 3250), 250);
        // After a rewind the total is lower.
        assert_eq!(output_tokens_added(Some(3250), 1000), 0);
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
        assert_eq!(whole_characters(&page("\u{FFFD}llo", 7), 2, true), ("llo", 7));
        assert_eq!(whole_characters(&page("plain\n", 6), 0, false), ("plain\n", 6));
    }

    fn model(provider_id: &str, id: &str, variants: &[&str], cost: Option<(f64, f64)>) -> ModelInfo {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "providerID": provider_id,
            "name": id,
            "variants": variants.iter().map(|id| serde_json::json!({ "id": id })).collect::<Vec<_>>(),
            "cost": cost.map(|(input, output)| vec![serde_json::json!({ "input": input, "output": output })]).unwrap_or_default(),
        }))
        .unwrap()
    }

    fn agent(id: &str, mode: Option<&str>) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            name: id.into(),
            mode: mode.map(str::to_string),
            description: None,
            hidden: false,
            model: None,
            system: None,
            permissions: Vec::new(),
        }
    }

    fn model_ref(provider_id: &str, id: &str, variant: Option<&str>) -> ModelRef {
        ModelRef {
            provider_id: provider_id.into(),
            id: id.into(),
            variant: variant.map(str::to_string),
        }
    }

    #[test]
    fn saved_defaults_apply_where_they_still_exist() {
        let models = [
            model("opencode", "glm", &["low", "high"], None),
            model("openai", "gpt", &["none", "low", "medium"], None),
        ];
        let agents = [agent("build", None), agent("plan", None), agent("explore", Some("subagent"))];
        let current = model_ref("opencode", "glm", None);
        let defaults = |model: Option<&str>, effort: Option<&str>, agent: Option<&str>| SavedDefaults {
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            agent: agent.map(str::to_string),
        };

        assert_eq!(
            defaults(Some("openai/gpt"), Some("none"), Some("plan"))
                .resolve(&models, &agents, &current, "build"),
            (model_ref("openai", "gpt", Some("none")), "plan".to_string())
        );
        // A model that's gone, an effort the model lacks and a subagent are
        // all ignored.
        assert_eq!(
            defaults(Some("anthropic/claude"), Some("none"), Some("explore"))
                .resolve(&models, &agents, &current, "build"),
            (current.clone(), "build".to_string())
        );
        // "default" clears a variant the session already had.
        assert_eq!(
            defaults(None, Some("default"), None).resolve(
                &models,
                &agents,
                &model_ref("opencode", "glm", Some("high")),
                "build"
            ),
            (current, "build".to_string())
        );
        // The same model keeps its variant when no effort was saved.
        let with_variant = model_ref("opencode", "glm", Some("low"));
        assert_eq!(
            defaults(Some("opencode/glm"), None, None).resolve(&models, &agents, &with_variant, "build"),
            (with_variant.clone(), "build".to_string())
        );
    }

    #[test]
    fn free_models_show_no_cost() {
        assert_eq!(cost_label(&model("opencode", "free", &[], Some((0.0, 0.0)))), None);
        assert_eq!(cost_label(&model("openai", "unpriced", &[], None)), None);
        assert_eq!(
            cost_label(&model("github-copilot", "sonnet", &[], Some((2.0, 10.0)))),
            Some("2$/10$".into())
        );
    }

    #[test]
    fn file_mentions_use_the_unescaped_name() {
        let uri = MentionUri::File {
            abs_path: PathBuf::from("/notes/AI ML overview.md"),
        }
        .to_uri()
        .to_string();
        let parts = prompt_parts(&[
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new("file content", uri.clone()),
                ),
            )),
            acp::ContentBlock::Text(acp::TextContent::new(" is this it")),
        ]);
        assert_eq!(parts.text, "@AI ML overview.md is this it");
        assert_eq!(
            parts.files,
            vec![serde_json::json!({ "uri": uri, "name": "AI ML overview.md" })]
        );
    }

    #[test]
    fn skill_mentions_are_sent_by_id() {
        let parts = prompt_parts(&[
            skill_resource("deep-research"),
            acp::ContentBlock::Text(acp::TextContent::new(" look into this ")),
            skill_resource("deep-research"),
        ]);
        assert_eq!(parts.text, "@deep-research look into this @deep-research");
        assert_eq!(parts.skills, vec!["deep-research".to_string()]);
        assert!(parts.files.is_empty());
    }
}
