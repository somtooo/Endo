//! Lets agents built outside this crate fill the `/` and `@` menus and drive
//! the thinking control, which are otherwise native-only. Kept in its own
//! file so the native code paths only gain a fallback line each.

use crate::completion_provider::AvailableSkill;
use crate::thread_metadata_store::ThreadMetadataStore;
use acp_thread::AgentConnection;
use agent_client_protocol::v2::schema as acp;
use anyhow::Result;
use collections::HashMap;
use gpui::{App, Global, Task};
use language_model::LanguageModelEffortLevel;
use project::AgentId;
use std::rc::Rc;

/// Lists a session's skills. Read synchronously, so the agent must have them
/// ready before it reports its commands.
pub type SkillsProvider =
    Rc<dyn Fn(&Rc<dyn AgentConnection>, &acp::SessionId, &App) -> Vec<AvailableSkill>>;

/// Gives the text sent when one of the agent's threads is mentioned with `@`.
pub type ThreadTextProvider = Rc<dyn Fn(&acp::SessionId, &mut App) -> Task<Result<String>>>;

/// A session's thinking settings, shown with the native lightbulb and effort
/// selector.
pub trait AgentThinking {
    /// The effort levels for when thinking is on. May be empty.
    fn effort_levels(&self) -> Vec<LanguageModelEffortLevel>;
    fn selected_effort(&self) -> Option<String>;
    /// Whether thinking can be turned off. When it can't, only the effort
    /// selector is shown.
    fn can_disable_thinking(&self) -> bool;
    fn thinking_enabled(&self) -> bool;
    fn set_thinking_enabled(&self, enabled: bool, cx: &mut App);
    fn set_effort(&self, effort: String, cx: &mut App);
}

/// Gives a session's thinking settings, or `None` when its model has none.
pub type ThinkingProvider =
    Rc<dyn Fn(&Rc<dyn AgentConnection>, &acp::SessionId, &App) -> Option<Rc<dyn AgentThinking>>>;

#[derive(Default)]
struct AgentMenuProviders {
    skills: HashMap<AgentId, SkillsProvider>,
    thread_text: HashMap<AgentId, ThreadTextProvider>,
    thinking: HashMap<AgentId, ThinkingProvider>,
}

impl Global for AgentMenuProviders {}

pub fn register_skills_provider(agent_id: AgentId, provider: SkillsProvider, cx: &mut App) {
    cx.default_global::<AgentMenuProviders>()
        .skills
        .insert(agent_id, provider);
}

pub fn register_thread_text_provider(
    agent_id: AgentId,
    provider: ThreadTextProvider,
    cx: &mut App,
) {
    cx.default_global::<AgentMenuProviders>()
        .thread_text
        .insert(agent_id, provider);
}

pub fn register_thinking_provider(agent_id: AgentId, provider: ThinkingProvider, cx: &mut App) {
    cx.default_global::<AgentMenuProviders>()
        .thinking
        .insert(agent_id, provider);
}

pub(crate) fn provided_thinking(
    connection: &Rc<dyn AgentConnection>,
    session_id: &acp::SessionId,
    cx: &App,
) -> Option<Rc<dyn AgentThinking>> {
    let provider = cx
        .try_global::<AgentMenuProviders>()?
        .thinking
        .get(&connection.agent_id())?
        .clone();
    provider(connection, session_id, cx)
}

pub(crate) fn provided_skills(
    connection: &Rc<dyn AgentConnection>,
    session_id: &acp::SessionId,
    cx: &App,
) -> Option<Vec<AvailableSkill>> {
    let provider = cx
        .try_global::<AgentMenuProviders>()?
        .skills
        .get(&connection.agent_id())?
        .clone();
    Some(provider(connection, session_id, cx))
}

pub(crate) fn provides_thread_text(agent_id: &AgentId, cx: &App) -> bool {
    cx.try_global::<AgentMenuProviders>()
        .is_some_and(|providers| providers.thread_text.contains_key(agent_id))
}

/// Uses the provider of the agent that owns the mentioned thread, which may
/// differ from the agent the message is being written to.
pub(crate) fn provided_thread_text(
    session_id: &acp::SessionId,
    cx: &mut App,
) -> Option<Task<Result<String>>> {
    let agent_id = ThreadMetadataStore::try_global(cx)?
        .read(cx)
        .entry_by_session(session_id)?
        .agent_id
        .clone();
    let provider = cx
        .try_global::<AgentMenuProviders>()?
        .thread_text
        .get(&agent_id)?
        .clone();
    Some(provider(session_id, cx))
}
