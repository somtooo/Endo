//! Lets agents built outside this crate fill the `/` and `@` menus and drive
//! the thinking control, profile picker and queue steering, which are
//! otherwise native-only.

use crate::completion_provider::AvailableSkill;
use crate::profile_selector::ProfileProvider;
use crate::thread_metadata_store::ThreadMetadataStore;
use acp_thread::AgentConnection;
use agent_client_protocol::v2::schema as acp;
use anyhow::Result;
use collections::HashMap;
use gpui::{App, Global, SharedString, Task};
use language_model::LanguageModelEffortLevel;
use project::AgentId;
use std::{rc::Rc, sync::Arc};
use workspace::PathList;

/// The agent UI an agent built outside this crate provides for its sessions.
/// Each method's default provides nothing, which leaves the native-only UI
/// hidden.
pub trait AgentUiHooks {
    /// Lists a session's skills. Read synchronously, so the agent must have
    /// them ready before it reports its commands.
    fn skills(
        &self,
        _connection: &Rc<dyn AgentConnection>,
        _session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Vec<AvailableSkill>> {
        None
    }

    /// Whether the agent's threads can be mentioned with `@`, which needs
    /// `thread_text`.
    fn can_mention_threads(&self) -> bool {
        false
    }

    /// Gives the text sent when one of the agent's threads is mentioned with
    /// `@`.
    fn thread_text(
        &self,
        _session_id: &acp::SessionId,
        _cx: &mut App,
    ) -> Option<Task<Result<String>>> {
        None
    }

    /// Gives a session's thinking settings, or `None` when its model has none.
    fn thinking(
        &self,
        _connection: &Rc<dyn AgentConnection>,
        _session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn AgentThinking>> {
        None
    }

    /// Gives a session's profiles, shown with the native profile picker in
    /// place of the agent's mode option.
    fn profiles(
        &self,
        _connection: &Rc<dyn AgentConnection>,
        _session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Arc<dyn ProfileProvider>> {
        None
    }

    fn steering(
        &self,
        _connection: &Rc<dyn AgentConnection>,
        _session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn AgentSteering>> {
        None
    }

    /// Gives the folders a session works in, for agents whose sessions can
    /// move to another folder than the project the thread is open in.
    fn session_folders(
        &self,
        _connection: &Rc<dyn AgentConnection>,
        _session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<PathList> {
        None
    }
}

/// A session's thinking settings, shown with the native lightbulb and effort
/// selector.
pub trait AgentThinking {
    /// The effort levels for when thinking is on. May be empty.
    fn effort_levels(&self, cx: &App) -> Vec<LanguageModelEffortLevel>;
    fn selected_effort(&self, cx: &App) -> Option<String>;
    /// Whether thinking can be turned off. When it can't, only the effort
    /// selector is shown.
    fn can_disable_thinking(&self) -> bool;
    fn thinking_enabled(&self, cx: &App) -> bool;
    fn set_thinking_enabled(&self, enabled: bool, cx: &mut App);
    fn set_effort(&self, effort: String, cx: &mut App);
}

/// Adds a queued message to the agent's running turn at its next step, for
/// agents that take messages mid-turn rather than ending the turn early as
/// the native agent does.
pub trait AgentSteering {
    /// Content that can't be steered waits for the turn to end.
    fn can_steer(&self, content: &[acp::ContentBlock], cx: &App) -> bool;
    /// Hands `content` to the agent. The task resolves to true once the agent
    /// adds it to the thread, or false once `cancel_steer` takes it back.
    fn steer(
        &self,
        content: Vec<acp::ContentBlock>,
        cx: &mut App,
    ) -> (SharedString, Task<Result<bool>>);
    fn cancel_steer(&self, id: SharedString, cx: &mut App);
}

#[derive(Default)]
struct RegisteredHooks(HashMap<AgentId, Rc<dyn AgentUiHooks>>);

impl Global for RegisteredHooks {}

pub fn register_agent_ui_hooks(agent_id: AgentId, hooks: Rc<dyn AgentUiHooks>, cx: &mut App) {
    cx.default_global::<RegisteredHooks>()
        .0
        .insert(agent_id, hooks);
}

pub(crate) fn find_agent_ui_hooks(agent_id: &AgentId, cx: &App) -> Option<Rc<dyn AgentUiHooks>> {
    cx.try_global::<RegisteredHooks>()?.0.get(agent_id).cloned()
}

/// Uses the hooks of the agent that owns the mentioned thread, which may
/// differ from the agent the message is being written to.
pub(crate) fn find_thread_text(
    session_id: &acp::SessionId,
    cx: &mut App,
) -> Option<Task<Result<String>>> {
    let agent_id = ThreadMetadataStore::try_global(cx)?
        .read(cx)
        .entry_by_session(session_id)?
        .agent_id
        .clone();
    find_agent_ui_hooks(&agent_id, cx)?.thread_text(session_id, cx)
}
