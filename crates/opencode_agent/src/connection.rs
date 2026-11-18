//! The OpenCode D backend: an `AgentConnection` that talks to the shared
//! OpenCode service directly.
//!
//! One `OpenCodeConnection` per Zed process owns the HTTP client and the one
//! event stream. It routes each event to the open session it's about, and
//! keeps what spans sessions: which ones run, and each working directory's
//! project setup.

use crate::client::{ListSessions, OpenCodeClient};
use crate::history::{self, session_list_info};
use crate::mcp;
use crate::opencode_session::{OpenCodeSession, SessionConfig, Steering, Submissions, Truncate};
use crate::project_setup::{ProjectSetup, fetch_project_setup};
use crate::service;
use crate::session_selectors::{SavedChoices, SessionSelectors};
use crate::types::{EventFrame, EventPayload, ModelRef, SessionInfo};
use acp_thread::{
    AcpThread, AgentConnection, AgentModelSelector, AgentSessionList, AgentSessionListRequest,
    AgentSessionListResponse, AgentSessionSetTitle, AgentSessionTruncate,
    ReceiptSessionSubmissions,
};
use action_log::ActionLog;
use agent_client_protocol::v2::schema as acp;
use agent_ui::AvailableSkill;
use agent_ui::ProfileProvider;
use agent_ui::agent_ui_hooks::{AgentSteering, AgentThinking};
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::channel::{mpsc, oneshot};
use futures::future::Shared;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, AsyncApp, Entity, SharedString, Subscription, Task, TaskExt as _,
    WeakEntity,
};
use project::{AgentId, Project};
use std::any::Any;
use std::cell::{Cell, Ref, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use util::ResultExt as _;
use util::path_list::PathList;

pub const AGENT_ID: &str = "opencode-direct";
pub const DISPLAY_NAME: &str = "OpenCode D";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Activity {
    Running,
    Idle,
    /// Idle because the run was stopped. A stopped background subagent's card
    /// shows it was cancelled.
    Interrupted,
}

impl Activity {
    pub(crate) fn is_running(self) -> bool {
        self == Activity::Running
    }
}

/// The server sends a heartbeat every 15 seconds, so a stream silent this
/// long is dead even if the connection looks open. OpenCode's own client uses
/// the same limit.
const EVENT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

enum StreamMessage {
    /// Missed events aren't replayed, so what they'd have changed is fetched
    /// again.
    Reconnected,
    Event(EventFrame),
}

/// The one connection to the OpenCode service, shared by every OpenCode D
/// thread in this Zed process.
pub struct OpenCodeConnection {
    client: OpenCodeClient,
    sessions: RefCell<HashMap<String, SessionHandle>>,
    /// Every session the server ran since Zed connected, open or not.
    activity: RefCell<HashMap<String, Activity>>,
    /// Counts every activity change, so a reply can tell which sessions
    /// changed after it was asked for.
    activity_changes: Cell<u64>,
    /// The count when each session's activity last changed.
    last_activity_change: RefCell<HashMap<String, u64>>,
    mcp: Rc<mcp::McpForwarder>,
    /// By working directory, for the directories threads are open in.
    project_setups: RefCell<HashMap<String, ProjectSharedSetup>>,
    /// For the trait methods that get `&self` but hand out adapters holding
    /// the connection.
    this: Weak<OpenCodeConnection>,
    _event_reader: RefCell<Option<Task<()>>>,
}

impl OpenCodeConnection {
    pub fn shared(cx: &mut App) -> Task<Result<Rc<OpenCodeConnection>>> {
        let connecting = match cx.try_global::<SharedConnection>() {
            Some(SharedConnection(connecting)) => connecting.clone(),
            None => {
                let Some(http) = cx
                    .try_global::<crate::ServiceHttpClient>()
                    .map(|http| http.0.clone())
                else {
                    return Task::ready(Err(anyhow!("OpenCode D isn't available here")));
                };
                let connecting = cx
                    .spawn(async move |cx| {
                        let result = Self::connect(http, cx).await;
                        cx.update(|cx| {
                            let installed = result.as_ref().err().is_none_or(|error| {
                                error.downcast_ref::<service::NotInstalled>().is_none()
                            });
                            cx.set_global(crate::Installed(installed));
                            match &result {
                                Ok(connection) => history::start(connection.client.clone(), cx),
                                Err(_) if cx.has_global::<SharedConnection>() => {
                                    cx.remove_global::<SharedConnection>();
                                }
                                Err(_) => {}
                            }
                        });
                        result.map_err(Arc::new)
                    })
                    .shared();
                cx.set_global(SharedConnection(connecting.clone()));
                connecting
            }
        };
        if let Some(Ok(connection)) = connecting.peek() {
            return Task::ready(Ok(connection.clone()));
        }
        cx.spawn(async move |_| connecting.await.map_err(|error| anyhow!("{error:#}")))
    }

    async fn connect(
        http: Arc<dyn http_client::HttpClient>,
        cx: &mut AsyncApp,
    ) -> Result<Rc<OpenCodeConnection>> {
        let client = service::connect(http, cx.background_executor().clone()).await?;
        Ok(cx.update(|cx| {
            let connection = Self::new(client);
            connection.start_event_reader(cx);
            connection
        }))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn use_test_client(client: OpenCodeClient, cx: &mut App) {
        let connection: Result<_, Arc<anyhow::Error>> = Ok(Self::new(client));
        cx.set_global(SharedConnection(Task::ready(connection).shared()));
    }

    fn new(client: OpenCodeClient) -> Rc<Self> {
        Rc::new_cyclic(|this| Self {
            mcp: mcp::McpForwarder::new(client.clone()),
            client,
            sessions: RefCell::default(),
            activity: RefCell::default(),
            activity_changes: Cell::default(),
            last_activity_change: RefCell::default(),
            project_setups: RefCell::default(),
            this: this.clone(),
            _event_reader: RefCell::default(),
        })
    }

    pub(crate) fn activity_by_session(&self) -> Ref<'_, HashMap<String, Activity>> {
        self.activity.borrow()
    }

    fn start_event_reader(self: &Rc<Self>, cx: &mut App) {
        let (tx, mut rx) = mpsc::unbounded::<StreamMessage>();
        let client = self.client.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            loop {
                match client.subscribe_to_events().await {
                    Ok(mut events) => {
                        if tx.unbounded_send(StreamMessage::Reconnected).is_err() {
                            return;
                        }
                        loop {
                            let next = futures::select_biased! {
                                next = events.next().fuse() => next,
                                _ = executor.timer(EVENT_STREAM_IDLE_TIMEOUT).fuse() => {
                                    log::warn!("OpenCode event stream went quiet; reconnecting");
                                    break;
                                }
                            };
                            let frame = match next {
                                None => break,
                                Some(Ok(Some(frame))) => frame,
                                Some(Ok(None)) => continue,
                                Some(Err(error)) => {
                                    log::warn!("OpenCode event stream error: {error:#}");
                                    break;
                                }
                            };
                            if tx.unbounded_send(StreamMessage::Event(frame)).is_err() {
                                return;
                            }
                        }
                        log::info!("OpenCode event stream ended");
                    }
                    Err(error) => {
                        log::warn!("Can't subscribe to OpenCode events: {error:#}");
                        // The service may have restarted at another address or
                        // with another password.
                        if let Some(registration) = service::read_registration().log_err().flatten()
                        {
                            client.set_endpoint(&registration).log_err();
                        }
                    }
                }
                executor.timer(Duration::from_secs(1)).await;
            }
        })
        .detach();

        let connection = Rc::downgrade(self);
        let reader = cx.spawn(async move |cx| {
            while let Some(message) = rx.next().await {
                let Some(connection) = connection.upgrade() else {
                    return;
                };
                cx.update(|cx| match message {
                    StreamMessage::Reconnected => connection.catch_up_after_reconnect(cx),
                    StreamMessage::Event(frame) => connection.handle_event(frame, cx),
                });
            }
        });
        *self._event_reader.borrow_mut() = Some(reader);
    }

    pub fn client(&self) -> &OpenCodeClient {
        &self.client
    }

    fn catch_up_after_reconnect(self: &Rc<Self>, cx: &mut App) {
        self.client.notify_config_reloaded();
        self.mcp.resend_all(cx);
        self.recheck_activity(cx);
        for session in self.open_sessions() {
            session.update(cx, |session, cx| session.catch_up_after_reconnect(cx));
        }
        for directory in self.forget_unheld_project_setups() {
            self.refresh_project_setup(directory, cx);
        }
    }

    /// Missed events aren't replayed, so after a reconnect sessions that
    /// started or finished meanwhile are found by asking which ones run.
    fn recheck_activity(self: &Rc<Self>, cx: &mut App) {
        let connection = Rc::downgrade(self);
        let client = self.client.clone();
        let changes_before = self.activity_changes.get();
        cx.spawn(async move |cx| {
            let running = client.list_running_sessions().await?;
            cx.update(|cx| {
                let Some(connection) = connection.upgrade() else {
                    return;
                };
                // An execution event that arrived meanwhile is newer than the list.
                let (finished, listed_running): (Vec<String>, Vec<String>) = {
                    let last_change = connection.last_activity_change.borrow();
                    let is_unchanged = |session_id: &String| {
                        last_change
                            .get(session_id)
                            .is_none_or(|change| *change <= changes_before)
                    };
                    let finished = connection
                        .activity
                        .borrow()
                        .iter()
                        .filter(|(session_id, activity)| {
                            activity.is_running() && !running.contains(*session_id)
                        })
                        .map(|(session_id, _)| session_id.clone())
                        .filter(is_unchanged)
                        .collect();
                    let listed_running = running
                        .iter()
                        .filter(|id| is_unchanged(id))
                        .cloned()
                        .collect();
                    (finished, listed_running)
                };
                for session_id in finished {
                    connection.set_activity(&session_id, Activity::Idle, cx);
                }
                for session_id in listed_running {
                    connection.set_activity(&session_id, Activity::Running, cx);
                }
            });
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn is_running(&self, session_id: &str) -> bool {
        self.activity
            .borrow()
            .get(session_id)
            .is_some_and(|activity| activity.is_running())
    }

    /// Must not run inside a session's update: it redraws other sessions'
    /// subagent cards.
    pub(crate) fn set_activity(
        self: &Rc<Self>,
        session_id: &str,
        activity: Activity,
        cx: &mut App,
    ) {
        let previous = self
            .activity
            .borrow_mut()
            .insert(session_id.to_string(), activity);
        let change = self.activity_changes.get() + 1;
        self.activity_changes.set(change);
        self.last_activity_change
            .borrow_mut()
            .insert(session_id.to_string(), change);
        if previous.map(Activity::is_running) != Some(activity.is_running()) {
            history::mark_session_running(&self.client, session_id, activity.is_running(), cx);
        }
        // Also when nothing changed: a thread that just opened starts idle.
        if let Some(thread) = self.thread(session_id, cx) {
            thread.update(cx, |thread, cx| {
                thread
                    .update_session_state(state_update(activity), cx)
                    .log_err();
            });
        }
        self.refresh_subagent_cards(session_id, cx);
    }

    fn handle_event(self: &Rc<Self>, frame: EventFrame, cx: &mut App) {
        let durable = frame.durable.is_some();
        let session_id = frame.session_id().map(str::to_string);
        let event = match frame.decode() {
            Ok(event) => event,
            Err(error) => {
                log::warn!("{error:#}");
                // A durable event changed what the server stores, so the copy
                // now misses a change. A live one is followed by a durable
                // event that carries the whole value.
                if durable && let Some(session) = session_id.and_then(|id| self.session(&id)) {
                    session.update(cx, |session, cx| session.resync(cx).detach_and_log_err(cx));
                }
                return;
            }
        };
        if let EventPayload::ProjectSetupChanged {} = event.payload {
            self.client.notify_config_reloaded();
            // A change to a global config file comes tagged with the config
            // folder, which no project setup is for, but applies to them all.
            let held_directories = self.forget_unheld_project_setups();
            let directories = match event.location {
                Some(location) if held_directories.contains(&location.directory) => {
                    vec![location.directory]
                }
                _ => held_directories,
            };
            for directory in directories {
                self.refresh_project_setup(directory, cx);
            }
            return;
        }
        let Some(session_id) = event.session_id.clone() else {
            return;
        };
        // About any session, open or not.
        match &event.payload {
            EventPayload::ExecutionStarted {} => {
                self.set_activity(&session_id, Activity::Running, cx)
            }
            EventPayload::ExecutionSucceeded {} | EventPayload::ExecutionFailed { .. } => {
                self.set_activity(&session_id, Activity::Idle, cx)
            }
            EventPayload::ExecutionInterrupted { .. } => {
                self.set_activity(&session_id, Activity::Interrupted, cx)
            }
            EventPayload::Renamed { data } => {
                history::rename_history_row(&session_id, &data.title, cx);
                if let Some(thread) = self.thread(&session_id, cx) {
                    let title = data.title.clone();
                    thread.update(cx, |thread, cx| {
                        thread.update_session_info(acp::SessionInfoUpdate::new().title(title), cx)
                    });
                }
            }
            EventPayload::Archived {} => history::archive_history_row(&session_id, cx),
            EventPayload::Unarchived {} => history::unarchive_history_row(&session_id, cx),
            EventPayload::Deleted {} => history::delete_history_row(&session_id, cx),
            EventPayload::Moved { data } => {
                self.follow_session_move(&session_id, &data.location.directory, cx)
            }
            _ => {}
        }
        if let Some(session) = self.session(&session_id) {
            session.update(cx, |session, cx| session.handle_event(event, cx));
        }
    }

    pub(crate) fn follow_session_move(
        self: &Rc<Self>,
        session_id: &str,
        directory: &str,
        cx: &mut App,
    ) {
        history::move_history_row(session_id, directory, cx);
        let Some(session) = self.session(session_id) else {
            return;
        };
        session.update(cx, |session, _| {
            session.set_directory(directory.to_string())
        });
        // Commands and skills come from the folder the session works in.
        let project_setup = self.load_project_setup(directory, cx);
        let directory = directory.to_string();
        cx.spawn(async move |cx| {
            let project_setup = project_setup.await?;
            session.update(cx, |session, cx| {
                session.use_project_setup(&directory, project_setup, cx)
            });
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn session(&self, session_id: &str) -> Option<Entity<OpenCodeSession>> {
        Some(self.sessions.borrow().get(session_id)?.session.clone())
    }

    fn open_sessions(&self) -> Vec<Entity<OpenCodeSession>> {
        self.sessions
            .borrow()
            .values()
            .map(|handle| handle.session.clone())
            .collect()
    }

    /// Returns the directories whose project setup someone still holds.
    fn forget_unheld_project_setups(&self) -> Vec<String> {
        let mut project_setups = self.project_setups.borrow_mut();
        project_setups.retain(|_, project_setup| {
            project_setup.project_setup.upgrade().is_some() || !project_setup.waiting.is_empty()
        });
        project_setups.keys().cloned().collect()
    }

    fn thread(&self, session_id: &str, cx: &App) -> Option<Entity<AcpThread>> {
        self.session(session_id)?.read(cx).thread().upgrade()
    }

    pub(crate) fn refresh_project_setup(self: &Rc<Self>, directory: String, cx: &mut App) {
        let previous = {
            let mut project_setups = self.project_setups.borrow_mut();
            let project_setup = project_setups.entry(directory.clone()).or_default();
            project_setup
                .project_setup
                .upgrade()
                .map(|project_setup| project_setup.read(cx).clone())
                .unwrap_or_default()
        };
        let client = self.client.clone();
        let connection = Rc::downgrade(self);
        let load = cx.spawn({
            let directory = directory.clone();
            async move |cx| {
                let fetched = fetch_project_setup(&client, &directory, &previous, cx).await;
                if let Some(connection) = connection.upgrade() {
                    cx.update(|cx| connection.finish_project_setup_load(&directory, fetched, cx));
                }
            }
        });
        if let Some(project_setup) = self.project_setups.borrow_mut().get_mut(&directory) {
            project_setup._load = Some(load);
        }
    }

    /// Holders of the project setup hear of the change from the project setup itself.
    fn finish_project_setup_load(
        &self,
        directory: &str,
        fetched: Result<ProjectSetup>,
        cx: &mut App,
    ) {
        let (held, waiting) = {
            let mut project_setups = self.project_setups.borrow_mut();
            let Some(project_setup) = project_setups.get_mut(directory) else {
                return;
            };
            (
                project_setup.project_setup.upgrade(),
                std::mem::take(&mut project_setup.waiting),
            )
        };
        let result = match (fetched, held) {
            (Ok(fetched), Some(held)) => {
                held.update(cx, |project_setup, cx| project_setup.replace(fetched, cx));
                Ok(held)
            }
            (Ok(fetched), None) => {
                let project_setup = cx.new(|_| fetched);
                if let Some(entry) = self.project_setups.borrow_mut().get_mut(directory) {
                    entry.project_setup = project_setup.downgrade();
                }
                Ok(project_setup)
            }
            (Err(error), held) => {
                log::warn!("Can't load the OpenCode project setup for {directory}: {error:#}");
                if held.is_none() {
                    // The next thread opened there tries again.
                    self.project_setups.borrow_mut().remove(directory);
                }
                Err(Arc::new(error))
            }
        };
        for waiter in waiting {
            waiter.send(result.clone()).ok();
        }
    }

    fn refresh_subagent_cards(&self, child_id: &str, cx: &mut App) {
        let parents: Vec<(Entity<OpenCodeSession>, String)> = self
            .open_sessions()
            .into_iter()
            .filter_map(|session| {
                let message_id = session
                    .read(cx)
                    .subagent_card_message_id(child_id)?
                    .to_string();
                Some((session, message_id))
            })
            .collect();
        for (session, message_id) in parents {
            session.update(cx, |session, cx| session.show_messages(&[message_id], cx));
        }
    }

    pub fn thinking(&self, session_id: &str, cx: &App) -> Option<Rc<dyn AgentThinking>> {
        let sessions = self.sessions.borrow();
        let selectors = &sessions.get(session_id)?.selectors;
        let has_variants = selectors
            .find_current_model(cx)
            .is_some_and(|info| !info.variants.is_empty());
        has_variants.then(|| Rc::new(selectors.clone()) as Rc<dyn AgentThinking>)
    }

    pub(crate) fn modal_source(
        &self,
        session_id: &str,
        cx: &App,
    ) -> Option<crate::agents_modal::ModalSource> {
        let session = self.session(session_id)?;
        let session = session.read(cx);
        Some(crate::agents_modal::ModalSource {
            client: self.client.clone(),
            project_setup: session.project_setup().clone(),
            directory: session.directory().to_string(),
        })
    }

    pub fn session_folders(&self, session_id: &str, cx: &App) -> Option<PathList> {
        let session = self.session(session_id)?;
        let directory = session.read(cx).directory();
        (!directory.is_empty()).then(|| PathList::new(&[PathBuf::from(directory)]))
    }

    pub fn steering(&self, session_id: &str) -> Option<Rc<dyn AgentSteering>> {
        let sessions = self.sessions.borrow();
        let handle = sessions.get(session_id)?;
        Some(Rc::new(Steering {
            session: handle.session.downgrade(),
        }))
    }

    pub fn profiles(&self, session_id: &str) -> Option<Arc<dyn ProfileProvider>> {
        let sessions = self.sessions.borrow();
        let selectors = &sessions.get(session_id)?.selectors;
        // The picker's trait takes an `Arc`, though it only ever runs on the
        // main thread.
        #[allow(clippy::arc_with_non_send_sync)]
        Some(Arc::new(selectors.clone()))
    }

    pub fn session_skills(&self, session_id: &str, cx: &App) -> Vec<AvailableSkill> {
        let Some(session) = self.session(session_id) else {
            return Vec::new();
        };
        session.read(cx).project_setup().read(cx).available_skills()
    }

    fn open_thread(
        self: Rc<Self>,
        session: SessionInfo,
        is_new: bool,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let client = self.client.clone();
        let defaults = is_new.then(|| SavedChoices::read(cx));
        let directory = session
            .location
            .as_ref()
            .map(|location| location.directory.clone())
            .or_else(|| {
                work_dirs
                    .paths()
                    .first()
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_default();
        let register_mcp = self.mcp.forward_servers(&project, &directory, cx);
        let project_setup = self.load_project_setup(&directory, cx);
        cx.spawn(async move |cx| {
            register_mcp.await;
            let project_setup = project_setup.await?;
            let mut model = match session.model.clone() {
                Some(model) => model,
                None => {
                    let default_model = client.fetch_default_model(&directory).await?;
                    ModelRef {
                        provider_id: default_model.provider_id,
                        id: default_model.id,
                        variant: None,
                    }
                }
            };
            let mut agent = session.agent.clone().unwrap_or_else(|| {
                cx.update(|cx| project_setup.read(cx).find_default_agent_id().to_string())
            });
            if let Some(defaults) = defaults {
                let resolved = cx.update(|cx| {
                    defaults.choose_starting_config(project_setup.read(cx), &session, &model)
                });
                // A default the server refuses is dropped; the thread
                // still opens with OpenCode's own choice.
                if resolved.model != model
                    && client
                        .set_model(&session.id, &resolved.model)
                        .await
                        .log_err()
                        .is_some()
                {
                    model = resolved.model;
                }
                if resolved.agent != agent
                    && client
                        .set_agent(&session.id, &resolved.agent)
                        .await
                        .log_err()
                        .is_some()
                {
                    agent = resolved.agent;
                }
            }
            let config = SessionConfig { agent, model };

            // Another window may have opened the same session while this one
            // loaded. Both show that thread: a second one would take over
            // the session's events.
            if let Some(thread) = cx.update(|cx| self.thread(&session.id, cx)) {
                return Ok(thread);
            }
            let work_dirs = if work_dirs.paths().is_empty() {
                PathList::new(&[PathBuf::from(&directory)])
            } else {
                work_dirs
            };
            let (thread, session) = cx.update(|cx| {
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
                        watch::Receiver::constant(
                            acp::PromptCapabilities::new()
                                .image(acp::PromptImageCapabilities::new())
                                .embedded_context(acp::PromptEmbeddedContextCapabilities::new()),
                        ),
                        cx,
                    )
                });
                let commands = project_setup.read(cx).available_commands();
                let open_session = self.register_session(
                    &thread,
                    &session.id,
                    config,
                    directory,
                    project_setup,
                    cx,
                );
                thread.update(cx, |thread, cx| {
                    thread.update_available_commands(commands, cx)
                });
                (thread, open_session)
            });
            let resync = session.update(cx, |session, cx| session.resync(cx));
            resync.await?;
            session.update(cx, |session, cx| session.start_live_updates(cx));
            Ok(thread)
        })
    }

    /// `directory`'s project setup, once it has loaded.
    pub(crate) fn load_project_setup(
        self: &Rc<Self>,
        directory: &str,
        cx: &mut App,
    ) -> Task<Result<Entity<ProjectSetup>>> {
        let (loaded, start_load) = {
            let mut project_setups = self.project_setups.borrow_mut();
            let project_setup = project_setups.entry(directory.to_string()).or_default();
            if let Some(held) = project_setup.project_setup.upgrade() {
                return Task::ready(Ok(held));
            }
            // Otherwise the first load is already running.
            let start_load = project_setup.waiting.is_empty();
            let (loaded_tx, loaded_rx) = oneshot::channel();
            project_setup.waiting.push(loaded_tx);
            (loaded_rx, start_load)
        };
        if start_load {
            self.refresh_project_setup(directory.to_string(), cx);
        }
        cx.spawn(async move |_| match loaded.await {
            Ok(Ok(project_setup)) => Ok(project_setup),
            Ok(Err(error)) => Err(anyhow!("{error:#}")),
            Err(_) => Err(anyhow!("The OpenCode project setup stopped loading")),
        })
    }

    /// The session lives as long as its thread.
    pub(crate) fn register_session(
        self: &Rc<Self>,
        thread: &Entity<AcpThread>,
        session_id: &str,
        config: SessionConfig,
        directory: String,
        project_setup: Entity<ProjectSetup>,
        cx: &mut App,
    ) -> Entity<OpenCodeSession> {
        let release = cx.observe_release(thread, {
            let connection = self.this.clone();
            let session_id = session_id.to_string();
            move |_, _| {
                if let Some(connection) = connection.upgrade() {
                    connection.sessions.borrow_mut().remove(&session_id);
                }
            }
        });
        let session = cx.new(|cx| {
            OpenCodeSession::new(
                self,
                session_id.to_string(),
                thread,
                config,
                directory,
                project_setup,
                cx,
            )
        });
        let selectors = session.read(cx).selectors().clone();
        self.sessions.borrow_mut().insert(
            session_id.to_string(),
            SessionHandle {
                session: session.clone(),
                selectors,
                _release: release,
            },
        );
        session
    }
}

impl AgentConnection for OpenCodeConnection {
    fn agent_id(&self) -> AgentId {
        AgentId::new(AGENT_ID)
    }

    fn telemetry_id(&self) -> SharedString {
        AGENT_ID.into()
    }

    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let client = self.client.clone();
        let Some(directory) = work_dirs
            .paths()
            .first()
            .map(|path| path.display().to_string())
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
        if let Some(thread) = self.thread(&session_id.0, cx) {
            return Task::ready(Ok(thread));
        }
        let client = self.client.clone();
        cx.spawn(async move |cx| {
            let session = client.fetch_session(&session_id.0).await?;
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

    /// Asked while the thread is built, before its session is open, so the
    /// submissions find the session when something is sent.
    fn receipt_submissions(
        &self,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn ReceiptSessionSubmissions>> {
        Some(Rc::new(Submissions {
            connection: self.this.clone(),
            client: self.client.clone(),
            session_id: session_id.0.to_string(),
        }))
    }

    fn cancel(&self, session_id: &acp::SessionId, cx: &mut App) -> Result<()> {
        let client = self.client.clone();
        let session = self
            .session(&session_id.0)
            .context("the session isn't open")?;
        let session_id = session_id.0.to_string();
        cx.spawn(async move |cx| {
            let interrupt = client.interrupt(&session_id).await;
            // Reload either way, so a thread Zed still thinks is running
            // stops even when the server already finished the turn.
            let resync = session.update(cx, |session, cx| session.resync(cx)).await;
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
            connection: self.this.clone(),
            session_id: session_id.0.to_string(),
        }))
    }

    fn set_title(
        &self,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn AgentSessionSetTitle>> {
        Some(Rc::new(SetTitle {
            client: self.client.clone(),
            session_id: session_id.0.to_string(),
        }))
    }

    fn model_selector(&self, session_id: &acp::SessionId) -> Option<Rc<dyn AgentModelSelector>> {
        let sessions = self.sessions.borrow();
        let selectors = &sessions.get(session_id.0.as_ref())?.selectors;
        Some(Rc::new(selectors.clone()))
    }

    fn session_list(&self, _cx: &mut App) -> Option<Rc<dyn AgentSessionList>> {
        Some(Rc::new(SessionList {
            client: self.client.clone(),
        }))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

struct SessionHandle {
    session: Entity<OpenCodeSession>,
    /// Kept beside the entity: Zed asks for the model picker without an `App`
    /// to read the entity with.
    selectors: SessionSelectors,
    /// Forgets the session when its thread closes.
    _release: Subscription,
}

struct ProjectSharedSetup {
    /// Held by whoever reads it, once the first load is done.
    project_setup: WeakEntity<ProjectSetup>,
    /// Readers waiting for the first load.
    waiting: Vec<oneshot::Sender<Result<Entity<ProjectSetup>, Arc<anyhow::Error>>>>,
    /// The latest load. Starting another cancels it, so a project setup event that
    /// arrives during a load gives its waiters the newer lists.
    _load: Option<Task<()>>,
}

impl Default for ProjectSharedSetup {
    fn default() -> Self {
        Self {
            project_setup: WeakEntity::new_invalid(),
            waiting: Vec::new(),
            _load: None,
        }
    }
}

/// The connection, or the attempt to make it. A failed attempt is forgotten,
/// so the next caller tries again.
struct SharedConnection(Shared<Task<Result<Rc<OpenCodeConnection>, Arc<anyhow::Error>>>>);

impl gpui::Global for SharedConnection {}

fn state_update(activity: Activity) -> acp::StateUpdate {
    match activity {
        Activity::Running => acp::StateUpdate::Running(acp::RunningStateUpdate::new()),
        Activity::Idle | Activity::Interrupted => acp::StateUpdate::Idle(
            acp::IdleStateUpdate::new().stop_reason(acp::StopReason::EndTurn),
        ),
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
                .list_sessions_page(ListSessions {
                    directory: directory.as_deref(),
                    cursor: request.cursor.as_deref(),
                    limit: 100,
                })
                .await?;
            let mut response =
                AgentSessionListResponse::new(page.data.iter().map(session_list_info).collect());
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

/// Drives open threads end to end: a fake OpenCode service answers the HTTP
/// routes, and events go straight to the connection, as the event stream
/// would send them.
#[cfg(test)]
pub(crate) mod thread_sync_tests {
    use super::*;
    use crate::service::ServiceRegistration;
    use crate::types::Durable;
    use acp_thread::{ForegroundActivity, ToolCallStatus};
    use agent_ui::thread_metadata_store::{ThreadMetadata, ThreadMetadataStore};
    use collections::HashSet;
    use futures::AsyncReadExt as _;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::{Value, json};
    use std::cell::Cell;
    use std::path::Path;
    use std::sync::Mutex;

    pub(crate) const SESSION: &str = "ses_parent";
    const DIRECTORY: &str = "/project";
    const OTHER_DIRECTORY: &str = "/elsewhere";

    #[derive(Default)]
    pub(crate) struct FakeService {
        sessions: HashMap<String, FakeSession>,
        commands: Vec<Value>,
        config: Vec<Value>,
        pub(crate) models: Vec<Value>,
        /// Agents besides `build`.
        pub(crate) agents: Vec<Value>,
        default_agent: Option<String>,
        /// The folder `/api/config` names as the global config folder.
        pub(crate) config_directory: Option<String>,
        /// Paths that answer with a server error.
        failing: HashSet<String>,
        requests: Vec<String>,
        /// The `delivery` of each prompt sent.
        prompt_deliveries: Vec<String>,
        shells: HashMap<String, FakeShell>,
    }

    struct FakeShell {
        output: String,
        running: bool,
    }

    #[derive(Default)]
    pub(crate) struct FakeSession {
        parent_id: Option<String>,
        messages: Vec<Value>,
        running: bool,
        pub(crate) archived: Option<i64>,
        pub(crate) unused: bool,
        agent: Option<String>,
        model: Option<ModelRef>,
        directory: Option<String>,
    }

    impl FakeSession {
        fn directory(&self) -> &str {
            self.directory.as_deref().unwrap_or(DIRECTORY)
        }
    }

    impl FakeService {
        fn respond(&mut self, method: &str, path: &str, query: &str, body: &[u8]) -> Option<Value> {
            self.requests.push(format!("{method} {path}"));
            if self.failing.contains(path) {
                return None;
            }
            let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
            Some(match (method, parts.as_slice()) {
                ("GET", ["api", "session"]) => {
                    let sessions: Vec<Value> = self
                        .sessions
                        .iter()
                        .filter(|(_, session)| session.parent_id.is_none())
                        .map(|(id, session)| {
                            json!({
                                "id": id,
                                "title": (!session.unused).then_some("Test session"),
                                "location": { "directory": session.directory() },
                                "time": { "archived": session.archived },
                            })
                        })
                        .collect();
                    json!({ "data": sessions, "cursor": {} })
                }
                ("PATCH", ["api", "session", id]) => {
                    let body: Value = serde_json::from_slice(body).expect("valid request body");
                    if let Some(archived) = body.get("archived").and_then(Value::as_bool) {
                        self.sessions.entry((*id).to_string()).or_default().archived =
                            archived.then_some(1);
                    }
                    json!({})
                }
                ("POST", ["api", "session"]) => {
                    self.sessions.insert(SESSION.into(), FakeSession::default());
                    json!({ "data": { "id": SESSION, "location": { "directory": DIRECTORY } } })
                }
                ("POST", ["api", "session", id, "agent"]) => {
                    let body: Value = serde_json::from_slice(body).expect("agent request");
                    self.sessions.entry(id.to_string()).or_default().agent =
                        Some(body["agent"].as_str().expect("agent").to_string());
                    json!({})
                }
                ("POST", ["api", "session", _, "prompt"]) => {
                    let body: Value = serde_json::from_slice(body).expect("prompt request");
                    self.prompt_deliveries
                        .push(body["delivery"].as_str().unwrap_or_default().to_string());
                    json!({})
                }
                ("POST", ["api", "session", id, "model"]) => {
                    let body: Value = serde_json::from_slice(body).expect("model request");
                    self.sessions.entry(id.to_string()).or_default().model =
                        Some(serde_json::from_value(body["model"].clone()).expect("model"));
                    json!({})
                }
                ("GET", ["api", "session", "active"]) => {
                    let running: serde_json::Map<String, Value> = self
                        .sessions
                        .iter()
                        .filter(|(_, session)| session.running)
                        .map(|(id, _)| (id.clone(), json!({})))
                        .collect();
                    json!({ "data": running })
                }
                ("GET", ["api", "session", id]) => {
                    let parent_id = self
                        .sessions
                        .get(*id)
                        .and_then(|session| session.parent_id.clone());
                    json!({ "data": {
                        "id": id,
                        "parentID": parent_id,
                        "title": "Test session",
                        "agent": self.sessions.get(*id).and_then(|session| session.agent.as_deref()).unwrap_or("build"),
                        "model": self.sessions.get(*id).and_then(|session| session.model.as_ref()).map(|model| json!({ "providerID": model.provider_id, "id": model.id, "variant": model.variant })).unwrap_or_else(|| json!({ "providerID": "test", "id": "model" })),
                        "location": { "directory": self.sessions.get(*id).map_or(DIRECTORY, FakeSession::directory) },
                        "time": {},
                    }})
                }
                ("GET", ["api", "session", id, "message"]) => {
                    let messages = self
                        .sessions
                        .get(*id)
                        .map(|session| session.messages.clone())
                        .unwrap_or_default();
                    json!({ "data": messages, "cursor": {} })
                }
                ("GET", ["api", "session", _, "permission" | "form" | "inbox"]) => {
                    json!({ "data": [] })
                }
                ("GET", ["api", "shell", id]) => {
                    let shell = self.shells.get(*id)?;
                    let status = if shell.running { "running" } else { "exited" };
                    json!({ "data": { "status": status } })
                }
                ("GET", ["api", "shell", id, "output"]) => {
                    let shell = self.shells.get(*id)?;
                    let cursor: usize = query
                        .split('&')
                        .find_map(|pair| pair.strip_prefix("cursor="))
                        .and_then(|cursor| cursor.parse().ok())?;
                    let size = shell.output.len();
                    let output = shell.output.get(cursor.min(size)..).unwrap_or_default();
                    json!({ "data": { "output": output, "cursor": size, "size": size } })
                }
                ("GET", ["api", "model"]) => {
                    let models: Vec<Value> = std::iter::once(model())
                        .chain(self.models.clone())
                        .collect();
                    json!({ "data": models })
                }
                ("GET", ["api", "model", "default"]) => json!({ "data": model() }),
                ("GET", ["api", "provider"]) => {
                    json!({ "data": [{ "id": "test", "name": "Test" }] })
                }
                ("GET", ["api", "agent"]) => {
                    let build = primary_agent("build");
                    let mut agents: Vec<Value> =
                        std::iter::once(build).chain(self.agents.clone()).collect();
                    if let Some(default) = &self.default_agent
                        && let Some(index) = agents.iter().position(|agent| agent["id"] == *default)
                    {
                        let default = agents.remove(index);
                        agents.insert(0, default);
                    }
                    json!({ "data": agents })
                }
                ("GET", ["api", "command"]) => json!({ "data": self.commands }),
                ("GET", ["api", "skill"]) => json!({ "data": [] }),
                ("GET", ["api", "config"]) => {
                    let mut entries = self.config.clone();
                    if let Some(directory) = &self.config_directory {
                        entries.push(json!({ "type": "directory", "path": directory }));
                    }
                    json!(entries)
                }
                _ => json!({}),
            })
        }
    }

    pub(crate) fn primary_agent(id: &str) -> Value {
        json!({ "id": id, "name": id, "mode": "primary" })
    }

    fn model() -> Value {
        json!({ "id": "model", "providerID": "test", "name": "Model" })
    }

    fn user(id: &str, text: &str) -> Value {
        json!({ "id": id, "type": "user", "text": text })
    }

    fn assistant(id: &str, content: Value) -> Value {
        json!({
            "id": id,
            "type": "assistant",
            "agent": "build",
            "content": content,
            "time": { "created": 1, "completed": 2 },
        })
    }

    fn text(text: &str) -> Value {
        json!([{ "type": "text", "text": text }])
    }

    fn running_tool(id: &str, name: &str, metadata: Value) -> Value {
        json!({
            "type": "tool",
            "id": id,
            "name": name,
            "state": { "status": "running", "input": { "description": "explore" }, "metadata": metadata },
        })
    }

    pub(crate) struct Harness {
        pub(crate) service: Arc<Mutex<FakeService>>,
        pub(crate) connection: Rc<OpenCodeConnection>,
        pub(crate) project: Entity<Project>,
        events_sent: Cell<usize>,
        last_seq: Cell<u64>,
    }

    impl Harness {
        pub(crate) async fn new(cx: &mut TestAppContext) -> Self {
            cx.update(|cx| {
                let settings = settings::SettingsStore::test(cx);
                cx.set_global(settings);
                cx.set_global(db::AppDatabase::test_new());
                agent_ui::thread_metadata_store::ThreadMetadataStore::init_global(cx);
            });
            let service = Arc::new(Mutex::new(FakeService::default()));
            let http = FakeHttpClient::create({
                let service = service.clone();
                move |request| {
                    let service = service.clone();
                    async move {
                        let method = request.method().to_string();
                        let path = request.uri().path().to_string();
                        let query = request.uri().query().unwrap_or_default().to_string();
                        let mut body = Vec::new();
                        request.into_body().read_to_end(&mut body).await?;
                        let reply = service
                            .lock()
                            .expect("fake service lock")
                            .respond(&method, &path, &query, &body);
                        Ok(match reply {
                            Some(reply) => Response::builder()
                                .status(200)
                                .body(AsyncBody::from(reply.to_string()))?,
                            None => Response::builder()
                                .status(500)
                                .body(AsyncBody::from("{}"))?,
                        })
                    }
                }
            });
            let registration = ServiceRegistration {
                id: "test".into(),
                version: "2.2.5".into(),
                url: "http://127.0.0.1:4096".into(),
                pid: 1,
                password: "secret".into(),
            };
            let client = OpenCodeClient::new(http, &registration).expect("client");
            let connection = OpenCodeConnection::new(client);
            cx.update(|cx| history::start(connection.client().clone(), cx));
            let fs = fs::FakeFs::new(cx.executor());
            fs.insert_tree(DIRECTORY, json!({})).await;
            let project = Project::test(fs, [Path::new(DIRECTORY)], cx).await;
            Self {
                service,
                connection,
                project,
                events_sent: Cell::new(0),
                last_seq: Cell::new(0),
            }
        }

        pub(crate) fn serve(&self, session_id: &str, update: impl FnOnce(&mut FakeSession)) {
            let mut service = self.service.lock().expect("fake service lock");
            update(service.sessions.entry(session_id.to_string()).or_default());
        }

        fn message_fetches(&self, session_id: &str) -> usize {
            self.requests(&format!("GET /api/session/{session_id}/message"))
        }

        fn requests(&self, request: &str) -> usize {
            let service = self.service.lock().expect("fake service lock");
            service
                .requests
                .iter()
                .filter(|sent| *sent == request)
                .count()
        }

        pub(crate) async fn open(
            &self,
            session_id: &str,
            cx: &mut TestAppContext,
        ) -> Entity<AcpThread> {
            let task = cx.update(|cx| {
                self.connection.clone().load_session(
                    acp::SessionId::new(session_id),
                    self.project.clone(),
                    PathList::new(&[PathBuf::from(DIRECTORY)]),
                    None,
                    cx,
                )
            });
            let thread = task.await.expect("the thread opens");
            cx.run_until_parked();
            thread
        }

        pub(crate) fn send(
            &self,
            kind: &str,
            data: Value,
            durability: Durability,
            cx: &mut TestAppContext,
        ) {
            self.send_for(SESSION, kind, data, durability, cx);
        }

        fn send_for(
            &self,
            session_id: &str,
            kind: &str,
            mut data: Value,
            durability: Durability,
            cx: &mut TestAppContext,
        ) {
            if let Some(object) = data.as_object_mut() {
                object.insert("sessionID".into(), json!(session_id));
            }
            let index = self.events_sent.get() + 1;
            self.events_sent.set(index);
            let seq = match durability {
                Durability::Live => None,
                Durability::Durable => Some(self.last_seq.get() + 1),
                Durability::DurableAt(seq) => Some(seq),
            };
            if let Some(seq) = seq {
                self.last_seq.set(seq);
            }
            let frame = EventFrame {
                id: format!("evt_{index:012x}"),
                kind: kind.to_string(),
                created: Some(index as i64),
                location: None,
                data,
                durable: seq.map(|seq| Durable {
                    aggregate_id: session_id.to_string(),
                    seq,
                }),
            };
            cx.update(|cx| self.connection.handle_event(frame, cx));
        }
    }

    #[derive(Clone, Copy)]
    pub(crate) enum Durability {
        Live,
        Durable,
        DurableAt(u64),
    }

    fn session_config(
        harness: &Harness,
        session_id: &str,
        cx: &mut TestAppContext,
    ) -> SessionConfig {
        let session = harness
            .connection
            .session(session_id)
            .expect("open session");
        session.read_with(cx, |session, _| session.choices().config())
    }

    fn markdown(thread: &Entity<AcpThread>, cx: &mut TestAppContext) -> String {
        thread.read_with(cx, |thread, cx| thread.to_markdown(cx))
    }

    fn streamed_step(harness: &Harness, message_id: &str, words: &[&str], cx: &mut TestAppContext) {
        let message = json!(message_id);
        harness.send(
            "session.step.started",
            json!({ "assistantMessageID": message, "agent": "build" }),
            Durability::Durable,
            cx,
        );
        harness.send(
            "session.text.started",
            json!({ "assistantMessageID": message, "ordinal": 0 }),
            Durability::Durable,
            cx,
        );
        for word in words {
            harness.send(
                "session.text.delta",
                json!({ "assistantMessageID": message, "ordinal": 0, "delta": word }),
                Durability::Live,
                cx,
            );
        }
        harness.send(
            "session.text.ended",
            json!({ "assistantMessageID": message, "ordinal": 0, "text": words.concat() }),
            Durability::Durable,
            cx,
        );
        harness.send(
            "session.step.ended",
            json!({ "assistantMessageID": message, "finish": "stop" }),
            Durability::Durable,
            cx,
        );
    }

    #[gpui::test]
    async fn new_threads_use_configured_defaults_and_loaded_threads_keep_their_choices(
        cx: &mut TestAppContext,
    ) {
        let harness = Harness::new(cx).await;
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.agents = vec![primary_agent("review")];
            service.models =
                vec![json!({ "id": "configured", "providerID": "test", "name": "Configured" })];
            service.config = vec![json!({ "type": "document", "info": {
                "default_agent": "review", "model": { "providerID": "test", "model": "configured" }
            } })];
        }
        let thread = cx
            .update(|cx| {
                harness.connection.clone().new_session(
                    harness.project.clone(),
                    PathList::new(&[PathBuf::from(DIRECTORY)]),
                    cx,
                )
            })
            .await
            .expect("new thread");
        let config = session_config(&harness, SESSION, cx);
        assert_eq!(config.agent, "review");
        assert_eq!(config.model.id, "configured");
        {
            let service = harness.service.lock().expect("fake service lock");
            assert_eq!(
                service
                    .sessions
                    .get(SESSION)
                    .expect("session")
                    .agent
                    .as_deref(),
                Some("review")
            );
            assert_eq!(
                service
                    .sessions
                    .get(SESSION)
                    .expect("session")
                    .model
                    .as_ref()
                    .map(|model| model.id.as_str()),
                Some("configured")
            );
        }
        drop(thread);
        harness.serve("existing", |_| {});
        let _thread = harness.open("existing", cx).await;
        let config = session_config(&harness, "existing", cx);
        assert_eq!(config.agent, "build");
        assert_eq!(config.model.id, "model");
        assert_eq!(harness.requests("POST /api/session/existing/agent"), 0);
        assert_eq!(harness.requests("POST /api/session/existing/model"), 0);
    }

    #[gpui::test]
    async fn switching_agents_applies_their_available_models_and_preserves_supported_effort(
        cx: &mut TestAppContext,
    ) {
        use agent_servers::{AgentServer as _, CustomAgentServer};

        let harness = Harness::new(cx).await;
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.models = vec![
                json!({ "providerID": "test", "id": "source", "name": "Source", "variants": [{ "id": "low" }] }),
                json!({ "providerID": "test", "id": "shared", "name": "Shared", "variants": [{ "id": "low" }, { "id": "high" }] }),
                json!({ "providerID": "test", "id": "different", "name": "Different", "variants": [{ "id": "high" }] }),
            ];
            service.agents = vec![
                json!({ "id": "review", "name": "Review", "model": { "providerID": "test", "id": "shared", "variant": "high" } }),
                json!({ "id": "fast", "name": "Fast", "model": { "providerID": "test", "id": "different" } }),
                primary_agent("plain"),
                json!({ "id": "missing", "name": "Missing", "model": { "providerID": "test", "id": "unavailable" } }),
            ];
        }
        harness.serve(SESSION, |session| {
            session.model = Some(ModelRef {
                provider_id: "test".into(),
                id: "source".into(),
                variant: Some("low".into()),
            });
        });
        let _thread = harness.open(SESSION, cx).await;
        harness
            .project
            .read_with(cx, |project, _| project.fs().clone())
            .create_dir(paths::config_dir())
            .await
            .expect("settings directory");
        let profiles = harness.connection.profiles(SESSION).expect("profiles");
        let selector = harness
            .connection
            .model_selector(&acp::SessionId::new(SESSION))
            .expect("model picker");
        for (agent, model, variant, writes) in [
            ("review", "shared", Some("low"), 1),
            ("fast", "different", None, 2),
            ("plain", "different", None, 2),
            ("missing", "different", None, 2),
        ] {
            cx.update(|cx| profiles.set_profile(agent_settings::AgentProfileId(agent.into()), cx));
            cx.run_until_parked();
            let expected = ModelRef {
                provider_id: "test".into(),
                id: model.into(),
                variant: variant.map(str::to_string),
            };
            let config = session_config(&harness, SESSION, cx);
            assert_eq!(config.agent, agent);
            assert_eq!(config.model, expected);
            {
                let service = harness.service.lock().expect("fake service lock");
                assert_eq!(
                    service
                        .sessions
                        .get(SESSION)
                        .expect("session")
                        .model
                        .as_ref(),
                    Some(&expected)
                );
            }
            let selected = cx
                .update(|cx| selector.selected_model(cx))
                .await
                .expect("selected model");
            assert_eq!(selected.id.to_string(), expected.reference());
            cx.update(|cx| {
                assert_eq!(
                    CustomAgentServer::new(AgentId::new(AGENT_ID))
                        .default_config_option("model", cx),
                    Some(settings::AgentConfigOptionValue::ValueId(
                        expected.reference()
                    )),
                );
            });
            assert_eq!(
                harness.requests("POST /api/session/ses_parent/model"),
                writes
            );
        }

        cx.update(|cx| selector.select_model(acp_thread::AgentModelId::new("test/source"), cx))
            .await
            .expect("a later model pick");
        let config = session_config(&harness, SESSION, cx);
        assert_eq!(config.model.id, "source");
        assert_eq!(config.agent, "missing");
    }

    #[gpui::test]
    async fn shell_output_streams_into_its_terminal_until_its_notice_finishes_it(
        cx: &mut TestAppContext,
    ) {
        let harness = Harness::new(cx).await;
        let set_shell = |output: &str, running: bool| {
            harness
                .service
                .lock()
                .expect("fake service lock")
                .shells
                .insert(
                    "sh_1".into(),
                    FakeShell {
                        output: output.into(),
                        running,
                    },
                );
        };
        set_shell("hello\n", true);
        // A command run in the background: the call returns while it runs.
        harness.serve(SESSION, |session| {
            session.messages = vec![
                user("msg_user", "build it"),
                assistant(
                    "msg_assistant",
                    json!([{
                        "type": "tool",
                        "id": "call_1",
                        "name": "shell",
                        "state": {
                            "status": "completed",
                            "input": { "command": "make", "background": true },
                            "content": [],
                            "metadata": { "status": "running", "shellID": "sh_1" },
                        },
                    }]),
                ),
            ];
        });
        let thread = harness.open(SESSION, cx).await;
        cx.run_until_parked();
        let terminal = thread.read_with(cx, |thread, _| {
            thread
                .terminal(acp::TerminalId::new("call_1/terminal"))
                .expect("the call's terminal")
        });
        let content = |cx: &mut TestAppContext| {
            terminal.read_with(cx, |terminal, cx| terminal.inner().read(cx).get_content())
        };
        assert!(content(cx).contains("hello"));
        assert!(terminal.read_with(cx, |terminal, _| terminal.output().is_none()));

        set_shell("hello\nworld\n", true);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(content(cx).contains("world"));

        set_shell("hello\nworld\n", false);
        harness.send(
            "session.synthetic",
            json!({
                "text": "<shell id=\"sh_1\" state=\"completed\" command=\"make\">\nhello\nworld\n</shell>",
                "metadata": { "source": "shell", "shellID": "sh_1", "state": "completed", "exit": 0 },
            }),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        let exit_code = terminal.read_with(cx, |terminal, _| {
            terminal
                .output()
                .and_then(|output| output.exit_status.exit_code)
        });
        assert_eq!(exit_code, Some(0));
        let shown = content(cx);
        assert_eq!(shown.matches("world").count(), 1, "{shown}");
    }

    #[gpui::test]
    async fn forms_and_permissions_are_shown_answered_and_closed(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![
                user("msg_user", "set it up"),
                assistant(
                    "msg_assistant",
                    json!([running_tool("call_shell", "shell", json!({}))]),
                ),
            ];
        });
        let thread = harness.open(SESSION, cx).await;
        let question = |id: &str| {
            json!({
                "form": {
                    "id": id,
                    "title": "Which database?",
                    "metadata": { "kind": "question" },
                    "fields": [{
                        "key": "db",
                        "type": "string",
                        "required": true,
                        "options": [{ "value": "pg", "label": "Postgres" }],
                    }],
                },
            })
        };
        let elicitation = |cx: &mut TestAppContext, index: usize| {
            thread.read_with(cx, |thread, _| {
                let ids: Vec<_> = thread
                    .entries()
                    .iter()
                    .filter_map(|entry| match entry {
                        acp_thread::AgentThreadEntry::Elicitation(id) => Some(id.clone()),
                        _ => None,
                    })
                    .collect();
                ids.get(index).cloned().expect("an elicitation")
            })
        };

        harness.send("form.created", question("frm_1"), Durability::Live, cx);
        cx.run_until_parked();
        let answered = elicitation(cx, 0);
        let response =
            serde_json::from_value(json!({ "action": "accept", "content": { "db": "pg" } }))
                .expect("response");
        thread.update(cx, |thread, cx| {
            thread.respond_to_elicitation(&answered, response, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            harness.requests(&format!("POST /api/session/{SESSION}/form/frm_1/reply")),
            1
        );

        // Answered in another client: Zed's copy closes without replying.
        harness.send("form.created", question("frm_2"), Durability::Live, cx);
        cx.run_until_parked();
        let elsewhere = elicitation(cx, 1);
        harness.send(
            "form.replied",
            json!({ "id": "frm_2" }),
            Durability::Live,
            cx,
        );
        cx.run_until_parked();
        thread.read_with(cx, |thread, _| {
            let (_, elicitation) = thread.elicitation(&elsewhere).expect("the elicitation");
            assert!(matches!(
                elicitation.status,
                acp_thread::ElicitationStatus::Canceled
            ));
        });
        assert_eq!(
            harness.requests(&format!("POST /api/session/{SESSION}/form/frm_2/reply")),
            0
        );

        harness.send(
            "permission.asked",
            json!({ "id": "per_1", "action": "bash", "resources": ["ls"], "source": { "id": "call_shell" } }),
            Durability::Live,
            cx,
        );
        cx.run_until_parked();
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(
                acp::ToolCallId::new("call_shell"),
                acp_thread::SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new("once"),
                    acp::PermissionOptionKind::AllowOnce,
                ),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            harness.requests(&format!(
                "POST /api/session/{SESSION}/permission/per_1/reply"
            )),
            1
        );
    }

    #[gpui::test]
    async fn a_steer_taken_back_early_is_cancelled_on_the_server(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        let _thread = harness.open(SESSION, cx).await;
        let steering = harness.connection.steering(SESSION).expect("steering");
        let content = vec![acp::ContentBlock::Text(acp::TextContent::new(
            "also say banana",
        ))];

        // Taken back before the server answered the send.
        let (message_id, delivered) = cx.update(|cx| steering.steer(content, cx));
        cx.update(|cx| steering.cancel_steer(message_id.clone(), cx));
        cx.run_until_parked();
        let requests = harness
            .service
            .lock()
            .expect("fake service lock")
            .requests
            .clone();
        let sent = requests
            .iter()
            .position(|request| *request == format!("POST /api/session/{SESSION}/prompt"))
            .expect("the steer was sent");
        let cancel = format!("DELETE /api/session/{SESSION}/inbox/{message_id}");
        assert!(
            requests[sent..].contains(&cancel),
            "no cancel after the send: {requests:?}"
        );

        harness.send(
            "session.inbox.cancelled",
            json!({ "inboxID": message_id.to_string() }),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert!(!delivered.await.expect("the steer finished"));
    }

    #[gpui::test]
    async fn each_new_entry_reaches_the_thread_in_its_own_update(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_1", "hi"), assistant("msg_2", text("hello"))];
        });
        let thread = harness.open(SESSION, cx).await;
        cx.run_until_parked();
        // Like the agent panel's `NewEntry` handler, which takes the last
        // entry as the new one.
        let new_entries = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let new_entries = new_entries.clone();
            cx.subscribe(&thread, move |thread, event, cx| {
                if let acp_thread::AcpThreadEvent::NewEntry = event {
                    let entries = thread.read(cx).entries();
                    let is_user = matches!(
                        entries.last(),
                        Some(acp_thread::AgentThreadEntry::UserMessage(_))
                    );
                    new_entries.borrow_mut().push((entries.len() - 1, is_user));
                }
            })
        });

        {
            let mut service = harness.service.lock().expect("fake service lock");
            let messages = &mut service.sessions.get_mut(SESSION).expect("session").messages;
            messages.push(user("msg_3", "again"));
            messages.push(assistant("msg_4", text("hello again")));
        }
        let session = harness.connection.session(SESSION).expect("open session");
        session
            .update(cx, |session, cx| session.resync(cx))
            .await
            .expect("resync");
        cx.run_until_parked();
        assert_eq!(*new_entries.borrow(), [(2, true), (3, false)]);
    }

    #[gpui::test]
    async fn new_output_tokens_and_edits_reach_the_thread_once(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        let fs = harness
            .project
            .read_with(cx, |project, _| project.fs().clone());
        let file = Path::new(DIRECTORY).join("a.txt");
        fs.write(&file, b"one\ntwo\n").await.expect("written");
        let step = |id: &str, call_id: &str, from: &str, to: &str, output: u64| {
            let mut message = assistant(
                id,
                json!([{
                    "type": "tool",
                    "id": call_id,
                    "name": "edit",
                    "state": {
                        "status": "completed",
                        "input": {},
                        "content": [],
                        "metadata": { "files": [{
                            "file": "a.txt",
                            "status": "modified",
                            "patch": format!("Index: a.txt\n===================================================================\n--- a.txt\n+++ a.txt\n@@ -1,2 +1,2 @@\n one\n-{from}\n+{to}\n"),
                        }] },
                    },
                }]),
            );
            message["tokens"] = json!({ "input": 100, "output": output, "reasoning": 2 });
            message
        };
        harness.serve(SESSION, |session| {
            session.messages = vec![
                user("msg_1", "edit it"),
                step("msg_2", "call_old", "zero", "two", 10),
            ];
        });
        let thread = harness.open(SESSION, cx).await;
        let added = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let added = added.clone();
            cx.subscribe(&thread, move |_, event, _| {
                if let acp_thread::AcpThreadEvent::TokenUsageUpdated(
                    acp_thread::OutputTokensUpdate::LegacyAdd { output_tokens, .. },
                ) = event
                {
                    added.borrow_mut().push(*output_tokens);
                }
            })
        });
        let changed_buffers = |cx: &mut TestAppContext| {
            thread.read_with(cx, |thread, cx| {
                thread.action_log().read(cx).changed_buffers(cx).count()
            })
        };
        cx.run_until_parked();
        // A loaded history was counted, and reviewed, back then.
        assert_eq!(changed_buffers(cx), 0);

        fs.write(&file, b"one\nthree\n").await.expect("written");
        {
            let mut service = harness.service.lock().expect("fake service lock");
            let messages = &mut service.sessions.get_mut(SESSION).expect("session").messages;
            messages.push(user("msg_3", "again"));
            messages.push(step("msg_4", "call_new", "two", "three", 5));
        }
        let session = harness.connection.session(SESSION).expect("open session");
        let resync = |cx: &mut TestAppContext| session.update(cx, |session, cx| session.resync(cx));
        resync(cx).await.expect("resync");
        cx.run_until_parked();
        assert_eq!(*added.borrow(), [7]);
        assert_eq!(changed_buffers(cx), 1);

        resync(cx).await.expect("resync");
        cx.run_until_parked();
        assert_eq!(*added.borrow(), [7]);
        assert_eq!(changed_buffers(cx), 1);
    }

    #[gpui::test]
    async fn turning_thinking_back_on_restores_the_effort(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.models = vec![
                json!({ "providerID": "test", "id": "switchable", "name": "Switchable", "variants": [{ "id": "none" }, { "id": "low" }, { "id": "high" }] }),
                json!({ "providerID": "test", "id": "always", "name": "Always", "variants": [{ "id": "low" }, { "id": "high" }] }),
            ];
        }
        harness.serve(SESSION, |session| {
            session.model = Some(ModelRef {
                provider_id: "test".into(),
                id: "switchable".into(),
                variant: Some("high".into()),
            });
        });
        let _thread = harness.open(SESSION, cx).await;
        let thinking = |cx: &App| harness.connection.thinking(SESSION, cx).expect("thinking");
        let server_variant = || {
            harness.service.lock().expect("fake service lock").sessions[SESSION]
                .model
                .as_ref()
                .and_then(|model| model.variant.clone())
        };

        cx.update(|cx| thinking(cx).set_thinking_enabled(false, cx));
        cx.run_until_parked();
        assert_eq!(server_variant().as_deref(), Some("none"));
        assert!(!cx.update(|cx| thinking(cx).thinking_enabled(cx)));
        cx.update(|cx| thinking(cx).set_thinking_enabled(true, cx));
        cx.run_until_parked();
        assert_eq!(server_variant().as_deref(), Some("high"));
        assert!(cx.update(|cx| thinking(cx).thinking_enabled(cx)));

        // A model that can't stop thinking only hides its effort.
        let selector = harness
            .connection
            .model_selector(&acp::SessionId::new(SESSION))
            .expect("model picker");
        cx.update(|cx| selector.select_model(acp_thread::AgentModelId::new("test/always"), cx))
            .await
            .expect("model pick");
        cx.update(|cx| thinking(cx).set_thinking_enabled(false, cx));
        cx.run_until_parked();
        assert!(!cx.update(|cx| thinking(cx).thinking_enabled(cx)));
        assert_eq!(server_variant().as_deref(), Some("high"));
        cx.update(|cx| thinking(cx).set_thinking_enabled(true, cx));
        assert!(cx.update(|cx| thinking(cx).thinking_enabled(cx)));
    }

    #[gpui::test]
    async fn a_model_pick_shows_at_once_and_reverts_when_refused(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.models = vec![json!({ "providerID": "test", "id": "other", "name": "Other" })];
            service
                .failing
                .insert(format!("/api/session/{SESSION}/model"));
        }
        let _thread = harness.open(SESSION, cx).await;
        let selector = harness
            .connection
            .model_selector(&acp::SessionId::new(SESSION))
            .expect("model picker");
        let selected = |cx: &mut TestAppContext| {
            cx.update(|cx| selector.selected_model(cx))
                .now_or_never()
                .expect("the selected model is known at once")
                .expect("selected model")
                .id
                .to_string()
        };

        let pick =
            cx.update(|cx| selector.select_model(acp_thread::AgentModelId::new("test/other"), cx));
        assert_eq!(selected(cx), "test/other");
        assert!(pick.await.is_err());
        assert_eq!(selected(cx), "test/model");
    }

    #[gpui::test]
    async fn a_refused_agent_switch_leaves_the_model_alone(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.models =
                vec![json!({ "providerID": "test", "id": "review", "name": "Review" })];
            service.agents = vec![
                json!({ "id": "review", "name": "Review", "model": { "providerID": "test", "id": "review" } }),
            ];
            service
                .failing
                .insert(format!("/api/session/{SESSION}/agent"));
        }
        let _thread = harness.open(SESSION, cx).await;
        let profiles = harness.connection.profiles(SESSION).expect("profiles");
        cx.update(|cx| profiles.set_profile(agent_settings::AgentProfileId("review".into()), cx));
        cx.run_until_parked();
        let config = session_config(&harness, SESSION, cx);
        assert_eq!(config.agent, "build");
        assert_eq!(config.model.id, "model");
        assert_eq!(cx.update(|cx| profiles.profile_id(cx)).as_str(), "build");
        assert_eq!(harness.requests("POST /api/session/ses_parent/model"), 0);
    }

    #[gpui::test]
    async fn events_during_a_reload_are_replayed_without_duplicates(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_001user", "hello")];
        });
        let thread = harness.open(SESSION, cx).await;

        // The snapshot holds the first buffered step but not the second.
        harness.serve(SESSION, |session| {
            session
                .messages
                .push(assistant("msg_002assistant", text("Good day")));
        });
        let session = harness
            .connection
            .session(SESSION)
            .expect("the session is open");
        let reload = cx.update(|cx| session.update(cx, |session, cx| session.resync(cx)));
        streamed_step(&harness, "msg_002assistant", &["Good", " day"], cx);
        streamed_step(&harness, "msg_003assistant", &["See", " you"], cx);
        reload.await.expect("the reload succeeds");
        cx.run_until_parked();

        let shown = markdown(&thread, cx);
        assert!(shown.contains("hello"), "{shown}");
        assert_eq!(shown.matches("Good day").count(), 1, "{shown}");
        assert_eq!(shown.matches("See you").count(), 1, "{shown}");
    }

    #[gpui::test]
    async fn a_skipped_sequence_number_doesnt_reload_the_session(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_001user", "hello")];
        });
        let thread = harness.open(SESSION, cx).await;

        harness.send(
            "session.execution.started",
            json!({}),
            Durability::Durable,
            cx,
        );
        // OpenCode numbers internal events too, and doesn't stream them.
        harness.send(
            "session.step.started",
            json!({ "assistantMessageID": "msg_002assistant", "agent": "build" }),
            Durability::DurableAt(5),
            cx,
        );
        streamed_step(&harness, "msg_002assistant", &["Still", " here"], cx);
        cx.run_until_parked();

        assert_eq!(harness.message_fetches(SESSION), 1);
        let shown = markdown(&thread, cx);
        assert!(shown.contains("Still here"), "{shown}");
    }

    #[gpui::test]
    async fn an_unreadable_durable_event_reloads_the_session(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_001user", "hello")];
        });
        harness.open(SESSION, cx).await;

        harness.send(
            "session.text.ended",
            json!({ "assistantMessageID": 7, "ordinal": 0, "text": "x" }),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(harness.message_fetches(SESSION), 2);

        harness.send(
            "session.text.delta",
            json!({ "assistantMessageID": 7, "ordinal": 0, "delta": "x" }),
            Durability::Live,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(harness.message_fetches(SESSION), 2);
    }

    #[gpui::test]
    async fn text_after_compact_is_queued_behind_the_compaction(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |_| {});
        let thread = harness.open(SESSION, cx).await;

        // Not awaited: the turn ends when the server reports the message.
        let _turn = thread.update(cx, |thread, cx| {
            thread.send(vec!["/compact keep the API notes".into()], cx)
        });
        cx.run_until_parked();

        let service = harness.service.lock().expect("fake service lock");
        let posts: Vec<&str> = service
            .requests
            .iter()
            .filter(|request| request.starts_with("POST"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            posts,
            [
                format!("POST /api/session/{SESSION}/compact"),
                format!("POST /api/session/{SESSION}/prompt"),
            ]
        );
        assert_eq!(service.prompt_deliveries, ["queue"]);
    }

    #[gpui::test]
    async fn a_reconnect_shows_switches_made_while_disconnected(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.agents = vec![primary_agent("plan")];
            service.models = vec![json!({ "providerID": "test", "id": "other", "name": "Other" })];
        }
        harness.serve(SESSION, |session| {
            session.agent = Some("build".to_string());
        });
        let _thread = harness.open(SESSION, cx).await;

        let other = ModelRef {
            provider_id: "test".to_string(),
            id: "other".to_string(),
            variant: None,
        };
        harness.serve(SESSION, |session| {
            session.agent = Some("plan".to_string());
            session.model = Some(other.clone());
        });
        cx.update(|cx| harness.connection.catch_up_after_reconnect(cx));
        cx.run_until_parked();

        let config = session_config(&harness, SESSION, cx);
        assert_eq!(config.agent, "plan");
        assert_eq!(config.model, other);

        let selector = harness
            .connection
            .model_selector(&acp::SessionId::new(SESSION))
            .expect("model picker");
        cx.update(|cx| selector.select_model(acp_thread::AgentModelId::new("test/model"), cx))
            .await
            .expect("switching back to the earlier model");
        assert_eq!(
            harness.requests(&format!("POST /api/session/{SESSION}/model")),
            1
        );
    }

    #[gpui::test]
    async fn a_resync_that_cant_list_forms_keeps_them_open(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_001user", "hello")];
        });
        let thread = harness.open(SESSION, cx).await;
        harness.send(
            "form.created",
            json!({ "form": {
                "id": "frm_1",
                "title": "Which database?",
                "metadata": { "kind": "question" },
                "fields": [{ "key": "db", "type": "string", "required": true }],
            }}),
            Durability::Live,
            cx,
        );
        cx.run_until_parked();
        harness
            .service
            .lock()
            .expect("fake service lock")
            .failing
            .insert(format!("/api/session/{SESSION}/form"));

        harness.send(
            "session.text.ended",
            json!({ "assistantMessageID": 7, "ordinal": 0, "text": "x" }),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(harness.message_fetches(SESSION), 2);
        thread.read_with(cx, |thread, _| {
            let statuses: Vec<_> = thread
                .entries()
                .iter()
                .filter_map(|entry| match entry {
                    acp_thread::AgentThreadEntry::Elicitation(id) => thread.elicitation(id),
                    _ => None,
                })
                .map(|(_, elicitation)| {
                    matches!(elicitation.status, acp_thread::ElicitationStatus::Canceled)
                })
                .collect();
            assert_eq!(statuses, [false]);
        });
    }

    #[gpui::test]
    async fn a_message_the_server_no_longer_has_rewinds_the_thread(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![
                user("msg_001user", "first question"),
                assistant("msg_002assistant", text("first answer")),
                user("msg_003user", "second question"),
                assistant("msg_004assistant", text("second answer")),
            ];
        });
        let thread = harness.open(SESSION, cx).await;
        assert!(markdown(&thread, cx).contains("second answer"));

        harness.serve(SESSION, |session| session.messages.truncate(2));
        harness.send(
            "session.revert.committed",
            json!({ "to": "msg_003user" }),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();

        let shown = markdown(&thread, cx);
        assert!(shown.contains("first answer"), "{shown}");
        assert!(!shown.contains("second question"), "{shown}");
        assert!(!shown.contains("second answer"), "{shown}");
    }

    #[gpui::test]
    async fn a_permission_answered_elsewhere_closes_its_prompt(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.running = true;
            session.messages = vec![
                user("msg_001user", "read it"),
                assistant(
                    "msg_002assistant",
                    json!([running_tool("tool_read", "read", json!({}))]),
                ),
            ];
        });
        let thread = harness.open(SESSION, cx).await;
        let status = |cx: &mut TestAppContext| {
            thread.read_with(cx, |thread, _| {
                thread
                    .tool_call(&acp::ToolCallId::new("tool_read"))
                    .map(|(_, call)| call.status())
            })
        };

        harness.send(
            "permission.asked",
            json!({ "id": "per_1", "source": { "id": "tool_read" }, "action": "read", "resources": ["a.rs"] }),
            Durability::Live,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(status(cx), Some(ToolCallStatus::WaitingForConfirmation));

        harness.send(
            "permission.replied",
            json!({ "requestID": "per_1", "reply": "once" }),
            Durability::Live,
            cx,
        );
        cx.run_until_parked();
        assert_ne!(status(cx), Some(ToolCallStatus::WaitingForConfirmation));
    }

    #[gpui::test]
    async fn a_steered_message_finishes_when_the_server_delivers_it(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.running = true;
            session.messages = vec![user("msg_001user", "start")];
        });
        let _thread = harness.open(SESSION, cx).await;
        let steering = harness
            .connection
            .steering(SESSION)
            .expect("an open session can be steered");

        let (message_id, delivered) = cx.update(|cx| {
            steering.steer(
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "one more thing",
                ))],
                cx,
            )
        });
        cx.run_until_parked();
        harness.send(
            "session.inbox.delivered",
            json!({ "inboxID": message_id.as_ref() }),
            Durability::Durable,
            cx,
        );

        assert!(delivered.await.expect("the steer finishes"));
    }

    #[gpui::test]
    async fn a_subagent_opens_as_a_child_thread(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve("ses_child", |session| {
            session.parent_id = Some(SESSION.to_string());
            session.agent = Some("explore".to_string());
            session.messages = vec![assistant("msg_101assistant", text("child says hi"))];
        });
        harness.serve(SESSION, |session| {
            session.agent = Some("build".to_string());
            session.running = true;
            session.messages = vec![
                user("msg_001user", "explore"),
                assistant(
                    "msg_002assistant",
                    json!([running_tool(
                        "tool_task",
                        "task",
                        json!({ "sessionID": "ses_child" })
                    )]),
                ),
            ];
        });
        let thread = harness.open(SESSION, cx).await;

        let children: Vec<Entity<AcpThread>> =
            thread.read_with(cx, |thread, _| thread.subagents().cloned().collect());
        assert_eq!(children.len(), 1);
        let shown = markdown(&children[0], cx);
        assert!(shown.contains("child says hi"), "{shown}");

        // The subagent's own agent, kept apart from the parent's choices.
        assert_eq!(session_config(&harness, "ses_child", cx).agent, "explore");
        assert_eq!(session_config(&harness, SESSION, cx).agent, "build");
    }

    #[gpui::test]
    async fn opening_an_open_session_again_shows_the_same_thread(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_001user", "hello")];
        });
        // Both loads start before either finishes, as from two windows.
        let load = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                harness.connection.clone().load_session(
                    acp::SessionId::new(SESSION),
                    harness.project.clone(),
                    PathList::new(&[PathBuf::from(DIRECTORY)]),
                    None,
                    cx,
                )
            })
        };
        let (first, second) = (load(cx), load(cx));
        let first = first.await.expect("the thread opens");
        let second = second.await.expect("the thread opens");
        cx.run_until_parked();
        assert_eq!(first.entity_id(), second.entity_id());
        let third = harness.open(SESSION, cx).await;
        assert_eq!(first.entity_id(), third.entity_id());

        harness.send(
            "session.execution.started",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(
            first.read_with(cx, |thread, _| thread.foreground_activity()),
            ForegroundActivity::Running
        );
    }

    #[gpui::test]
    async fn a_stopped_turn_leaves_the_thread_idle(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_001user", "hello")];
        });
        let thread = harness.open(SESSION, cx).await;
        let message = json!("msg_002assistant");
        for (kind, data) in [
            ("session.execution.started", json!({})),
            (
                "session.step.started",
                json!({ "assistantMessageID": message, "agent": "build" }),
            ),
            (
                "session.reasoning.started",
                json!({ "assistantMessageID": message, "ordinal": 0 }),
            ),
            (
                "session.step.failed",
                json!({
                    "assistantMessageID": message,
                    "error": { "type": "aborted", "message": "Step interrupted" },
                }),
            ),
        ] {
            harness.send(kind, data, Durability::Durable, cx);
            cx.run_until_parked();
        }
        // A stop reloads the session, and the server can still list the turn
        // as running when the reload asks, just before it ends.
        harness.serve(SESSION, |session| session.running = true);
        let session = harness.connection.session(SESSION).expect("open session");
        session.update(cx, |session, cx| session.resync(cx).detach());
        harness.send(
            "session.execution.interrupted",
            json!({ "reason": "user" }),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();

        let shown = markdown(&thread, cx);
        assert_eq!(shown.matches("Stopped").count(), 1, "{shown}");
        assert_eq!(
            thread.read_with(cx, |thread, _| thread.foreground_activity()),
            ForegroundActivity::Idle
        );
    }

    #[gpui::test]
    async fn the_running_state_follows_the_server(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_001user", "hello")];
        });
        let thread = harness.open(SESSION, cx).await;
        let activity = |cx: &mut TestAppContext| {
            thread.read_with(cx, |thread, _| thread.foreground_activity())
        };
        assert_eq!(activity(cx), ForegroundActivity::Idle);

        harness.send(
            "session.execution.started",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(activity(cx), ForegroundActivity::Running);

        harness.send(
            "session.execution.succeeded",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(activity(cx), ForegroundActivity::Idle);
    }

    #[gpui::test]
    async fn threads_in_one_directory_share_one_project_setup(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |_| {});
        harness.serve("ses_other", |_| {});
        let _first = harness.open(SESSION, cx).await;
        let _second = harness.open("ses_other", cx).await;
        assert_eq!(harness.requests("GET /api/model"), 1);
    }

    #[gpui::test]
    async fn config_changes_reach_clients_without_open_threads(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        let client = harness.connection.client().clone();
        let mut changes = client.watch_config_changes();
        assert!(changes.changed().now_or_never().is_none());

        for kind in ["agent.updated", "config.updated"] {
            harness.send(kind, json!({}), Durability::Live, cx);
            assert_eq!(changes.changed().now_or_never(), Some(Ok(())));
        }

        cx.update(|cx| harness.connection.catch_up_after_reconnect(cx));
        assert_eq!(changes.changed().now_or_never(), Some(Ok(())));
    }

    #[gpui::test]
    async fn a_failed_refresh_keeps_the_lists(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |_| {});
        harness.service.lock().expect("fake service lock").commands =
            vec![json!({ "name": "review" })];
        let _thread = harness.open(SESSION, cx).await;

        harness
            .service
            .lock()
            .expect("fake service lock")
            .failing
            .insert("/api/command".into());
        harness.send("command.updated", json!({}), Durability::Live, cx);
        cx.run_until_parked();

        assert_eq!(harness.requests("GET /api/command"), 2);
        let session = harness.connection.session(SESSION).expect("open session");
        session.read_with(cx, |session, cx| {
            assert!(session.project_setup().read(cx).has_command("review"))
        });
    }

    #[gpui::test]
    async fn a_deleted_agent_switches_to_the_server_default(cx: &mut TestAppContext) {
        for default in ["build", "plan"] {
            let harness = Harness::new(cx).await;
            harness.serve(SESSION, |_| {});
            {
                let mut service = harness.service.lock().expect("fake service lock");
                let mut custom = primary_agent("pickertest");
                custom["name"] = json!("Picker Test");
                service.agents = vec![custom, primary_agent("plan")];
                service.default_agent = Some(default.into());
            }
            let thread = harness.open(SESSION, cx).await;
            let profiles = harness.connection.profiles(SESSION).expect("profiles");
            cx.update(|cx| {
                profiles.set_profile(agent_settings::AgentProfileId("pickertest".into()), cx)
            });
            cx.run_until_parked();
            assert_eq!(
                cx.update(|cx| profiles.profile_id(cx)).as_str(),
                "pickertest"
            );

            harness.service.lock().expect("fake service lock").agents = vec![primary_agent("plan")];
            harness.send("agent.updated", json!({}), Durability::Live, cx);
            // Starting a refresh does not establish that the agent was deleted.
            assert_eq!(
                cx.update(|cx| profiles.profile_id(cx)).as_str(),
                "pickertest"
            );
            cx.run_until_parked();

            assert_eq!(cx.update(|cx| profiles.profile_id(cx)).as_str(), default);
            assert_eq!(harness.requests("POST /api/session/ses_parent/agent"), 2);
            thread.read_with(cx, |thread, _| {
                let notices = thread.notices();
                assert_eq!(notices.len(), 1);
                assert_eq!(
                    notices[0].1.title,
                    format!(
                        "The agent \"Picker Test\" was deleted, so this thread now uses {}.",
                        crate::project_setup::to_title_case(default),
                    )
                );
            });
            harness.send("agent.updated", json!({}), Durability::Live, cx);
            cx.run_until_parked();
            assert_eq!(harness.requests("POST /api/session/ses_parent/agent"), 2);
            thread.read_with(cx, |thread, _| assert_eq!(thread.notices().len(), 1));
        }
    }

    #[gpui::test]
    async fn an_empty_or_failed_project_setup_does_not_delete_the_selected_agent(
        cx: &mut TestAppContext,
    ) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |_| {});
        harness.service.lock().expect("fake service lock").agents =
            vec![primary_agent("pickertest")];
        let thread = harness.open(SESSION, cx).await;
        let profiles = harness.connection.profiles(SESSION).expect("profiles");
        cx.update(|cx| {
            profiles.set_profile(agent_settings::AgentProfileId("pickertest".into()), cx)
        });
        cx.run_until_parked();
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.agents.clear();
            service.failing.insert("/api/agent".into());
        }
        harness.send("agent.updated", json!({}), Durability::Live, cx);
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| profiles.profile_id(cx)).as_str(),
            "pickertest"
        );
        cx.update(|cx| {
            harness
                .connection
                .finish_project_setup_load(DIRECTORY, Ok(ProjectSetup::default()), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| profiles.profile_id(cx)).as_str(),
            "pickertest"
        );
        assert_eq!(harness.requests("POST /api/session/ses_parent/agent"), 1);
        thread.read_with(cx, |thread, _| assert!(thread.notices().is_empty()));
    }

    #[gpui::test]
    async fn a_session_started_elsewhere_appears_in_history(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve("ses_tui", |_| {});
        harness.send_for(
            "ses_tui",
            "session.execution.started",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(
            history_row(cx, "ses_tui").map(|row| row.archived),
            Some(false)
        );
    }

    fn history_row(cx: &mut TestAppContext, session_id: &str) -> Option<ThreadMetadata> {
        cx.update(|cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry_by_session(&acp::SessionId::new(session_id))
                .cloned()
        })
    }

    /// A session another client ran, which history shows unarchived.
    fn session_run_elsewhere(harness: &Harness, cx: &mut TestAppContext) {
        harness.serve("ses_tui", |_| {});
        harness.send_for(
            "ses_tui",
            "session.execution.started",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn a_late_running_list_keeps_a_newer_finish(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        session_run_elsewhere(&harness, cx);
        // The list is answered after the finish below, but says what the
        // server saw before it.
        harness.serve("ses_tui", |session| session.running = true);

        cx.update(|cx| harness.connection.catch_up_after_reconnect(cx));
        harness.send_for(
            "ses_tui",
            "session.execution.succeeded",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();

        assert!(!harness.connection.is_running("ses_tui"));
    }

    #[gpui::test]
    async fn a_session_moved_elsewhere_moves_in_the_sidebar(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        session_run_elsewhere(&harness, cx);
        harness.send_for(
            "ses_tui",
            "session.moved",
            json!({ "location": { "directory": OTHER_DIRECTORY } }),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(
            history_row(cx, "ses_tui").map(|row| row.folder_paths().clone()),
            Some(PathList::new(&[PathBuf::from(OTHER_DIRECTORY)]))
        );
    }

    #[gpui::test]
    async fn an_open_thread_follows_its_session_when_it_moves(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |_| {});
        let thread = harness.open(SESSION, cx).await;
        harness.send(
            "session.moved",
            json!({ "location": { "directory": OTHER_DIRECTORY } }),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        let session = harness
            .connection
            .session(SESSION)
            .expect("the session is open");
        session.read_with(cx, |session, _| {
            assert_eq!(session.directory(), OTHER_DIRECTORY)
        });
        cx.update(|cx| {
            // The agent panel saves an open thread's sidebar row with these
            // folders, so the row follows the move.
            assert_eq!(
                harness.connection.session_folders(SESSION, cx),
                Some(PathList::new(&[PathBuf::from(OTHER_DIRECTORY)]))
            );
            // Nothing holds the old folder's project setup any more.
            assert_eq!(
                harness.connection.forget_unheld_project_setups(),
                vec![OTHER_DIRECTORY.to_string()]
            )
        });
        drop(thread);
    }

    #[gpui::test]
    async fn a_reconnect_follows_a_move_made_while_disconnected(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |_| {});
        let thread = harness.open(SESSION, cx).await;

        harness.serve(SESSION, |session| {
            session.directory = Some(OTHER_DIRECTORY.to_string());
        });
        cx.update(|cx| harness.connection.catch_up_after_reconnect(cx));
        cx.run_until_parked();

        let session = harness
            .connection
            .session(SESSION)
            .expect("the session is open");
        session.read_with(cx, |session, _| {
            assert_eq!(session.directory(), OTHER_DIRECTORY)
        });
        cx.update(|cx| {
            assert_eq!(
                harness.connection.session_folders(SESSION, cx),
                Some(PathList::new(&[PathBuf::from(OTHER_DIRECTORY)]))
            );
            assert_eq!(
                harness.connection.forget_unheld_project_setups(),
                vec![OTHER_DIRECTORY.to_string()]
            )
        });
        drop(thread);
    }

    #[gpui::test]
    async fn a_moved_session_reopens_in_its_new_folder(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.directory = Some(OTHER_DIRECTORY.into())
        });
        let thread = harness.open(SESSION, cx).await;
        let session = harness
            .connection
            .session(SESSION)
            .expect("the session is open");
        session.read_with(cx, |session, _| {
            assert_eq!(session.directory(), OTHER_DIRECTORY)
        });
        drop(thread);
    }

    #[gpui::test]
    async fn a_session_archived_elsewhere_leaves_the_sidebar(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        session_run_elsewhere(&harness, cx);
        assert_eq!(
            history_row(cx, "ses_tui").map(|row| row.archived),
            Some(false)
        );

        harness.send_for(
            "ses_tui",
            "session.archived",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(
            history_row(cx, "ses_tui").map(|row| row.archived),
            Some(true)
        );
    }

    #[gpui::test]
    async fn archiving_a_thread_in_zed_archives_its_session(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        session_run_elsewhere(&harness, cx);
        let thread_id = history_row(cx, "ses_tui").expect("a row").thread_id;
        cx.update(|cx| {
            ThreadMetadataStore::global(cx)
                .update(cx, |store, cx| store.archive(thread_id, None, cx))
        });
        cx.run_until_parked();
        assert_eq!(harness.requests("PATCH /api/session/ses_tui"), 1);
        assert_eq!(
            harness.service.lock().expect("fake service lock").sessions["ses_tui"].archived,
            Some(1)
        );
    }

    #[gpui::test]
    async fn a_session_unarchived_elsewhere_returns_to_the_sidebar(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        session_run_elsewhere(&harness, cx);
        harness.send_for(
            "ses_tui",
            "session.archived",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert!(history_row(cx, "ses_tui").expect("a row").archived);

        harness.send_for(
            "ses_tui",
            "session.unarchived",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert!(!history_row(cx, "ses_tui").expect("a row").archived);
        assert_eq!(harness.requests("PATCH /api/session/ses_tui"), 2);
    }

    #[gpui::test]
    async fn unarchiving_a_thread_in_zed_unarchives_its_session(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        session_run_elsewhere(&harness, cx);
        let thread_id = history_row(cx, "ses_tui").expect("a row").thread_id;
        cx.update(|cx| {
            ThreadMetadataStore::global(cx)
                .update(cx, |store, cx| store.archive(thread_id, None, cx))
        });
        cx.run_until_parked();
        assert_eq!(
            harness.service.lock().expect("fake service lock").sessions["ses_tui"].archived,
            Some(1)
        );

        cx.update(|cx| {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| store.unarchive(thread_id, cx))
        });
        cx.run_until_parked();
        assert_eq!(harness.requests("PATCH /api/session/ses_tui"), 2);
        assert_eq!(
            harness.service.lock().expect("fake service lock").sessions["ses_tui"].archived,
            None
        );
    }

    #[gpui::test]
    async fn running_an_archived_session_unarchives_it_in_both_apps(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        session_run_elsewhere(&harness, cx);
        harness.send_for(
            "ses_tui",
            "session.execution.succeeded",
            json!({}),
            Durability::Durable,
            cx,
        );
        harness.send_for(
            "ses_tui",
            "session.archived",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();

        harness.send_for(
            "ses_tui",
            "session.execution.started",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert!(!history_row(cx, "ses_tui").expect("a row").archived);
        cx.update(|cx| {
            assert!(
                ThreadMetadataStore::global(cx)
                    .read(cx)
                    .is_externally_running(&acp::SessionId::new("ses_tui"))
            )
        });
        assert_eq!(
            harness.service.lock().expect("fake service lock").sessions["ses_tui"].archived,
            None
        );
    }

    #[gpui::test]
    async fn a_session_deleted_elsewhere_leaves_history(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        session_run_elsewhere(&harness, cx);
        harness.send_for(
            "ses_tui",
            "session.deleted",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();
        assert!(history_row(cx, "ses_tui").is_none());
    }

    #[gpui::test]
    async fn streamed_text_shows_before_its_block_ends(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_001user", "hello")];
        });
        let thread = harness.open(SESSION, cx).await;
        let message = json!("msg_002assistant");
        for (kind, data) in [
            (
                "session.step.started",
                json!({ "assistantMessageID": message, "agent": "build" }),
            ),
            (
                "session.text.started",
                json!({ "assistantMessageID": message, "ordinal": 0 }),
            ),
        ] {
            harness.send(kind, data, Durability::Durable, cx);
        }
        for word in ["Good", " day"] {
            harness.send(
                "session.text.delta",
                json!({ "assistantMessageID": message, "ordinal": 0, "delta": word }),
                Durability::Live,
                cx,
            );
            cx.run_until_parked();
        }
        let shown = markdown(&thread, cx);
        assert_eq!(shown.matches("Good day").count(), 1, "{shown}");
    }

    #[gpui::test]
    async fn a_live_turn_reaches_the_thread(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve(SESSION, |session| {
            session.messages = vec![user("msg_001user", "hello")];
        });
        let thread = harness.open(SESSION, cx).await;

        harness.send(
            "session.execution.started",
            json!({}),
            Durability::Durable,
            cx,
        );
        streamed_step(&harness, "msg_002assistant", &["Hi", " there"], cx);
        streamed_step(&harness, "msg_003assistant", &["And", " more"], cx);
        harness.send(
            "session.execution.succeeded",
            json!({}),
            Durability::Durable,
            cx,
        );
        cx.run_until_parked();

        let shown = markdown(&thread, cx);
        assert_eq!(shown.matches("Hi there").count(), 1, "{shown}");
        assert_eq!(shown.matches("And more").count(), 1, "{shown}");
    }
}
