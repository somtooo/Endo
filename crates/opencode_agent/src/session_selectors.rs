use crate::client::OpenCodeClient;
use crate::connection::AGENT_ID;
use crate::opencode_session::{OpenCodeSession, SessionConfig};
use crate::project_setup::{BUILT_IN_AGENTS, ProjectSetup, to_title_case};
use crate::types::{ModelInfo, ModelRef, SessionInfo};
use acp_thread::{
    AcpThread, AgentModelGroupName, AgentModelIcon, AgentModelId, AgentModelInfo, AgentModelList,
    AgentModelSelector,
};
use agent_client_protocol::v2::schema as acp;
use agent_servers::{AgentServer as _, CustomAgentServer};
use agent_settings::{AgentProfileId, AvailableProfiles};
use agent_ui::ProfileProvider;
use agent_ui::agent_ui_hooks::AgentThinking;
use anyhow::{Result, anyhow};
use collections::{HashSet, IndexMap};
use fs::Fs;
use gpui::{App, Context, Entity, SharedString, Task, TaskExt as _, WeakEntity};
use language_model::{LanguageModelCostInfo, LanguageModelEffortLevel};
use project::AgentId;
use settings::AgentConfigOptionValue;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use ui::IconName;
use util::ResultExt as _;

/// Zed's value for the model's own default effort, which has no variant.
const DEFAULT_EFFORT: &str = "default";
/// The variant OpenCode models use for thinking off.
const NO_THINKING_VARIANT: &str = "none";
const MODEL_OPTION: &str = "model";
const EFFORT_OPTION: &str = "effort";
const MODE_OPTION: &str = "mode";

/// The session's agent and model. A pick shows at once, over what the
/// server last confirmed, and goes back if the server refuses it.
#[derive(Clone)]
pub(crate) struct SessionChoices {
    session_id: String,
    client: OpenCodeClient,
    thread: WeakEntity<AcpThread>,
    state: Rc<RefCell<ChoiceState>>,
}

struct ChoiceState {
    confirmed: SessionConfig,
    /// Picks sent to the server and not answered yet.
    agent_pick: Option<String>,
    model_pick: Option<ModelRef>,
    /// Replies can arrive out of order; only the latest pick's settles.
    agent_picks_sent: u64,
    model_picks_sent: u64,
    thinking_off: Option<ThinkingOff>,
    /// Tells Zed's selectors to read again.
    refresh: watch::Sender<()>,
}

/// Thinking turned off from Zed's lightbulb. A model without a `none`
/// variant can't stop thinking, so for it this only hides the effort
/// control.
struct ThinkingOff {
    effort_to_restore: Effort,
}

#[derive(Clone)]
pub(crate) enum Choice {
    Agent(String),
    Model(ModelRef),
}

impl SessionChoices {
    pub(crate) fn new(
        session_id: String,
        client: OpenCodeClient,
        thread: &Entity<AcpThread>,
        confirmed: SessionConfig,
    ) -> Self {
        Self {
            session_id,
            client,
            thread: thread.downgrade(),
            state: Rc::new(RefCell::new(ChoiceState {
                confirmed,
                agent_pick: None,
                model_pick: None,
                agent_picks_sent: 0,
                model_picks_sent: 0,
                thinking_off: None,
                refresh: watch::channel(()).0,
            })),
        }
    }

    /// The agent and model that apply: the latest picks, else what the
    /// server confirmed.
    pub(crate) fn config(&self) -> SessionConfig {
        let state = self.state.borrow();
        SessionConfig {
            agent: state
                .agent_pick
                .clone()
                .unwrap_or_else(|| state.confirmed.agent.clone()),
            model: state
                .model_pick
                .clone()
                .unwrap_or_else(|| state.confirmed.model.clone()),
        }
    }

    /// Shows `choice` at once and sends it to the server. A refused choice
    /// goes back, and a later pick stands either way.
    pub(crate) fn pick(&self, choice: Choice, cx: &mut App) -> Task<Result<()>> {
        let config = self.config();
        let pick_number = {
            let mut state = self.state.borrow_mut();
            match &choice {
                Choice::Agent(agent) if *agent == config.agent => return Task::ready(Ok(())),
                Choice::Model(model) if *model == config.model => return Task::ready(Ok(())),
                Choice::Agent(agent) => {
                    state.agent_pick = Some(agent.clone());
                    state.agent_picks_sent += 1;
                    state.agent_picks_sent
                }
                Choice::Model(model) => {
                    state.model_pick = Some(model.clone());
                    state.model_picks_sent += 1;
                    state.model_picks_sent
                }
            }
        };
        self.notify_changed(cx);
        let choices = self.clone();
        cx.spawn(async move |cx| {
            let session_id = &choices.session_id;
            let result = match &choice {
                Choice::Agent(agent) => choices.client.set_agent(session_id, agent).await,
                Choice::Model(model) => choices.client.set_model(session_id, model).await,
            };
            {
                let mut state = choices.state.borrow_mut();
                match choice {
                    Choice::Agent(agent) if state.agent_picks_sent == pick_number => {
                        state.agent_pick = None;
                        if result.is_ok() {
                            state.confirmed.agent = agent;
                        }
                    }
                    Choice::Model(model) if state.model_picks_sent == pick_number => {
                        state.model_pick = None;
                        if result.is_ok() {
                            state.confirmed.model = model;
                        }
                    }
                    Choice::Agent(_) | Choice::Model(_) => {}
                }
            }
            cx.update(|cx| choices.notify_changed(cx));
            result
        })
    }

    /// The server switched the agent or model itself.
    pub(crate) fn confirm(&self, choice: Choice, cx: &mut App) {
        match choice {
            Choice::Agent(agent) => self.state.borrow_mut().confirmed.agent = agent,
            Choice::Model(model) => self.state.borrow_mut().confirmed.model = model,
        }
        self.notify_changed(cx);
    }

    pub(crate) fn notify_changed(&self, cx: &mut App) {
        self.state.borrow_mut().refresh.send(()).log_err();
        // The thread's view redraws on `ConfigOptionsUpdated`. OpenCode D has
        // no ACP config options, so the list is empty.
        self.thread
            .update(cx, |_, cx| {
                cx.emit(acp_thread::AcpThreadEvent::ConfigOptionsUpdated(Vec::new()))
            })
            .log_err();
    }

    fn watch_changes(&self) -> watch::Receiver<()> {
        self.state.borrow().refresh.receiver()
    }

    fn is_thinking_off(&self) -> bool {
        self.state.borrow().thinking_off.is_some()
    }

    fn turn_thinking_off(&self, effort_to_restore: Effort) {
        self.state.borrow_mut().thinking_off = Some(ThinkingOff { effort_to_restore });
    }

    /// Returns the effort thinking was turned off from.
    fn turn_thinking_on(&self) -> Effort {
        self.state
            .borrow_mut()
            .thinking_off
            .take()
            .map_or(Effort::Default, |off| off.effort_to_restore)
    }
}

/// How hard the model thinks. OpenCode stores it as the model's variant.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Effort {
    /// No variant: the model's own default.
    Default,
    Level(String),
    /// The `none` variant.
    Off,
}

impl Effort {
    fn from_variant(variant: Option<&str>) -> Self {
        variant.map_or(Self::Default, Self::parse)
    }

    fn to_variant(&self) -> Option<String> {
        match self {
            Self::Default => None,
            Self::Level(level) => Some(level.clone()),
            Self::Off => Some(NO_THINKING_VARIANT.to_string()),
        }
    }

    /// Reads the value Zed's settings and thinking control use, which names
    /// a variant or is `default`.
    fn parse(value: &str) -> Self {
        match value {
            DEFAULT_EFFORT => Self::Default,
            NO_THINKING_VARIANT => Self::Off,
            level => Self::Level(level.to_string()),
        }
    }

    fn to_setting_value(&self) -> String {
        self.to_variant()
            .unwrap_or_else(|| DEFAULT_EFFORT.to_string())
    }
}

impl ModelInfo {
    fn offers(&self, effort: &Effort) -> bool {
        effort
            .to_variant()
            .is_none_or(|variant| self.has_variant(&variant))
    }

    /// The levels the effort control lists, besides the default and off.
    fn list_effort_levels(&self) -> Vec<String> {
        self.variants
            .iter()
            .filter_map(|variant| match Effort::parse(&variant.id) {
                Effort::Level(level) => Some(level),
                Effort::Default | Effort::Off => None,
            })
            .collect()
    }

    /// The model with `effort`. An effort the model doesn't offer falls back
    /// to the model's default.
    fn to_model_ref(&self, effort: &Effort) -> ModelRef {
        ModelRef {
            provider_id: self.provider_id.clone(),
            id: self.id.clone(),
            variant: effort.to_variant().filter(|_| self.offers(effort)),
        }
    }
}

/// Feeds Zed's native model picker, thinking control and profile picker
/// from the session's choices and its project setup.
#[derive(Clone)]
pub(crate) struct SessionSelectors {
    choices: SessionChoices,
    /// Saves favorites and defaults to the settings file.
    fs: Arc<dyn Fs>,
    session: WeakEntity<OpenCodeSession>,
}

impl SessionSelectors {
    /// Saves to the file system of the thread's project.
    pub(crate) fn new(
        choices: SessionChoices,
        thread: &Entity<AcpThread>,
        cx: &mut Context<OpenCodeSession>,
    ) -> Self {
        Self {
            choices,
            fs: thread.read(cx).project().read(cx).fs().clone(),
            session: cx.weak_entity(),
        }
    }

    /// The session's project setup, unless the session has closed.
    fn project_setup<'a>(&self, cx: &'a App) -> Option<&'a ProjectSetup> {
        Some(self.session.upgrade()?.read(cx).project_setup().read(cx))
    }

    pub(crate) fn find_current_model(&self, cx: &App) -> Option<ModelInfo> {
        let model = self.choices.config().model;
        self.project_setup(cx)?
            .find_model(&model.provider_id, &model.id)
            .cloned()
    }

    fn read_current_effort(&self) -> Effort {
        Effort::from_variant(self.choices.config().model.variant.as_deref())
    }

    fn list_effort_levels(&self, cx: &App) -> Vec<String> {
        self.find_current_model(cx)
            .map(|info| info.list_effort_levels())
            .unwrap_or_default()
    }

    fn can_stop_thinking(&self, cx: &App) -> bool {
        self.find_current_model(cx)
            .is_some_and(|info| info.offers(&Effort::Off))
    }

    /// Saves the effort as the default for new threads once the server takes
    /// it, as the native agent does.
    fn pick_effort(&self, effort: Effort, cx: &mut App) {
        let Some(info) = self.find_current_model(cx) else {
            return;
        };
        let model = info.to_model_ref(&effort);
        let effort = Effort::from_variant(model.variant.as_deref());
        let pick = self.choices.pick(Choice::Model(model), cx);
        let fs = self.fs.clone();
        cx.spawn(async move |cx| {
            pick.await?;
            cx.update(|cx| SavedChoices::save_effort(&effort, fs, cx));
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

impl ProfileProvider for SessionSelectors {
    fn profile_id(&self, _cx: &App) -> AgentProfileId {
        AgentProfileId(self.choices.config().agent.into())
    }

    fn set_profile(&self, profile_id: AgentProfileId, cx: &mut App) {
        let agent = profile_id.0.to_string();
        if self.choices.config().agent == agent {
            return;
        }
        let pick = self.choices.pick(Choice::Agent(agent.clone()), cx);
        let selectors = self.clone();
        cx.spawn(async move |cx| {
            pick.await?;
            let model = cx.update(|cx| {
                if selectors.choices.config().agent != agent {
                    return None;
                }
                let project_setup = selectors.project_setup(cx)?;
                project_setup
                    .find_agent(&agent)
                    .and_then(|agent| agent.model.as_ref())
                    .filter(|model| {
                        project_setup
                            .find_model(&model.provider_id, &model.id)
                            .is_some()
                    })
                    .map(|model| AgentModelId::new(model.reference()))
            });
            match model {
                Some(model) => cx.update(|cx| selectors.select_model(model, cx)).await,
                None => Ok(()),
            }
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
    fn available_profiles(&self, cx: &App) -> AvailableProfiles {
        let Some(project_setup) = self.project_setup(cx) else {
            return AvailableProfiles::default();
        };
        let mut profiles: AvailableProfiles = project_setup
            .list_selectable_agents()
            .into_iter()
            .map(|agent| {
                (
                    AgentProfileId(agent.id.clone().into()),
                    SharedString::from(project_setup.format_agent_name(&agent.id)),
                )
            })
            .collect();
        let current = self.choices.config().agent;
        let current_id = AgentProfileId(current.clone().into());
        if !profiles.contains_key(&current_id) {
            let name = project_setup.format_agent_name(&current);
            profiles.insert(current_id, name.into());
        }
        profiles
    }

    fn is_builtin_profile(&self, profile_id: &AgentProfileId) -> bool {
        BUILT_IN_AGENTS.contains(&profile_id.as_str())
    }

    fn profile_description(&self, profile_id: &AgentProfileId, cx: &App) -> Option<SharedString> {
        self.project_setup(cx)?
            .find_agent(profile_id.as_str())
            .and_then(|agent| agent.description.clone())
            .map(SharedString::from)
    }

    fn save_default_profile(&self, profile_id: AgentProfileId, fs: Arc<dyn Fs>, cx: &mut App) {
        SavedChoices::save_agent(profile_id.0.to_string(), fs, cx);
    }

    fn configure_action(&self) -> Box<dyn gpui::Action> {
        Box::new(crate::agents_modal::ManageAgents)
    }

    fn profiles_changed(&self) -> Option<watch::Receiver<()>> {
        Some(self.choices.watch_changes())
    }
}

impl AgentModelSelector for SessionSelectors {
    /// Grouped by provider like the native picker, providers and models in
    /// name order.
    fn list_models(&self, cx: &mut App) -> Task<Result<AgentModelList>> {
        let mut groups: IndexMap<AgentModelGroupName, Vec<AgentModelInfo>> = IndexMap::default();
        if let Some(project_setup) = self.project_setup(cx) {
            for (provider_name, models) in project_setup.group_models_by_provider() {
                groups
                    .entry(AgentModelGroupName(provider_name.into()))
                    .or_default()
                    .extend(models.into_iter().map(to_agent_model_info));
            }
        }
        Task::ready(Ok(AgentModelList::Grouped(groups)))
    }

    /// The effort carries over when the new model offers it. The model is
    /// saved as the default for new threads once the server takes it.
    fn select_model(&self, model_id: AgentModelId, cx: &mut App) -> Task<Result<()>> {
        let reference = model_id.to_string();
        let Some(info) = self
            .project_setup(cx)
            .and_then(|project_setup| project_setup.find_model_by_reference(&reference))
            .cloned()
        else {
            return Task::ready(Err(anyhow!("unknown model {reference}")));
        };
        let model = info.to_model_ref(&self.read_current_effort());
        let pick = self.choices.pick(Choice::Model(model), cx);
        let fs = self.fs.clone();
        cx.spawn(async move |cx| {
            pick.await?;
            cx.update(|cx| SavedChoices::save_model(reference, fs, cx));
            Ok(())
        })
    }

    /// The picker needs a model even when the session's is gone from the
    /// list, say after its provider was disconnected.
    fn selected_model(&self, cx: &mut App) -> Task<Result<AgentModelInfo>> {
        let info = match self.find_current_model(cx) {
            Some(info) => to_agent_model_info(&info),
            None => {
                let model = self.choices.config().model;
                AgentModelInfo {
                    id: AgentModelId::new(model.reference()),
                    name: model.id.clone().into(),
                    description: None,
                    icon: find_provider_icon(&model.provider_id).map(AgentModelIcon::Named),
                    is_latest: false,
                    cost: None,
                    disabled: None,
                }
            }
        };
        Task::ready(Ok(info))
    }

    fn favorite_model_ids(&self, cx: &mut App) -> HashSet<AgentModelId> {
        SavedChoices::read_favorite_models(cx)
    }

    fn toggle_favorite_model(&self, model_id: AgentModelId, should_be_favorite: bool, cx: &App) {
        SavedChoices::toggle_favorite_model(&model_id, should_be_favorite, self.fs.clone(), cx);
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
        Some(self.choices.watch_changes())
    }
}

/// Thinking off is the `none` variant. A model without one can't stop
/// thinking, so its lightbulb only hides the effort control.
impl AgentThinking for SessionSelectors {
    fn effort_levels(&self, cx: &App) -> Vec<LanguageModelEffortLevel> {
        let levels = self.list_effort_levels(cx);
        if levels.is_empty() {
            return Vec::new();
        }
        let level = |value: &str, is_default: bool| LanguageModelEffortLevel {
            name: to_title_case(value).into(),
            value: value.to_string().into(),
            is_default,
            supports_disabling_thinking: None,
        };
        std::iter::once(level(DEFAULT_EFFORT, true))
            .chain(levels.iter().map(|value| level(value, false)))
            .collect()
    }

    fn selected_effort(&self, cx: &App) -> Option<String> {
        let effort = match self.read_current_effort() {
            Effort::Level(level) if self.list_effort_levels(cx).contains(&level) => {
                Effort::Level(level)
            }
            Effort::Default | Effort::Level(_) | Effort::Off => Effort::Default,
        };
        Some(effort.to_setting_value())
    }

    fn can_disable_thinking(&self) -> bool {
        true
    }

    fn set_thinking_enabled(&self, enabled: bool, cx: &mut App) {
        if enabled == self.thinking_enabled(cx) {
            return;
        }
        let effort = if enabled {
            self.choices.turn_thinking_on()
        } else {
            self.choices.turn_thinking_off(self.read_current_effort());
            Effort::Off
        };
        if self.can_stop_thinking(cx) {
            self.pick_effort(effort, cx);
        } else {
            self.choices.notify_changed(cx);
        }
    }

    fn thinking_enabled(&self, cx: &App) -> bool {
        if self.can_stop_thinking(cx) {
            self.read_current_effort() != Effort::Off
        } else {
            !self.choices.is_thinking_off()
        }
    }

    fn set_effort(&self, effort: String, cx: &mut App) {
        // Picking an effort means thinking is on.
        self.choices.turn_thinking_on();
        self.pick_effort(Effort::parse(&effort), cx);
    }
}

/// Zed's saved choices for OpenCode D, kept in its settings entry: the
/// model, effort and agent new threads start with, and the favorite models.
pub(crate) struct SavedChoices {
    model: Option<String>,
    effort: Option<Effort>,
    agent: Option<String>,
}

impl SavedChoices {
    pub(crate) fn read(cx: &App) -> Self {
        let entry = Self::create_settings_entry();
        let value = |option: &str| {
            entry
                .default_config_option(option, cx)
                .and_then(|value| value.as_value_id().map(str::to_string))
        };
        Self {
            model: value(MODEL_OPTION),
            effort: value(EFFORT_OPTION).as_deref().map(Effort::parse),
            agent: value(MODE_OPTION),
        }
    }

    /// Saves `reference`, as `provider/model`.
    fn save_model(reference: String, fs: Arc<dyn Fs>, cx: &mut App) {
        Self::save(MODEL_OPTION, reference, fs, cx);
    }

    fn save_effort(effort: &Effort, fs: Arc<dyn Fs>, cx: &mut App) {
        Self::save(EFFORT_OPTION, effort.to_setting_value(), fs, cx);
    }

    fn save_agent(agent: String, fs: Arc<dyn Fs>, cx: &mut App) {
        Self::save(MODE_OPTION, agent, fs, cx);
    }

    fn save(option: &str, value: String, fs: Arc<dyn Fs>, cx: &mut App) {
        Self::create_settings_entry().set_default_config_option(
            option,
            Some(AgentConfigOptionValue::ValueId(value)),
            fs,
            cx,
        );
    }

    fn read_favorite_models(cx: &mut App) -> HashSet<AgentModelId> {
        Self::create_settings_entry()
            .favorite_config_option_value_ids(&acp::SessionConfigId::new(MODEL_OPTION), cx)
            .into_iter()
            .map(|value| AgentModelId::new(value.0.to_string()))
            .collect()
    }

    fn toggle_favorite_model(
        model_id: &AgentModelId,
        should_be_favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        Self::create_settings_entry().toggle_favorite_config_option_value(
            acp::SessionConfigId::new(MODEL_OPTION),
            acp::SessionConfigValueId::new(model_id.to_string()),
            should_be_favorite,
            fs,
            cx,
        );
    }

    fn create_settings_entry() -> CustomAgentServer {
        CustomAgentServer::new(AgentId::new(AGENT_ID))
    }

    /// A session's own choices win, then OpenCode's config, then Zed's saved
    /// choices where they still exist. A saved effort the model doesn't
    /// offer leaves the model's own.
    pub(crate) fn choose_starting_config(
        &self,
        project_setup: &ProjectSetup,
        session: &SessionInfo,
        fallback_model: &ModelRef,
    ) -> SessionConfig {
        let saved_model = self
            .model
            .as_deref()
            .and_then(|reference| project_setup.find_model_by_reference(reference));
        let mut model = session
            .model
            .clone()
            .or_else(|| project_setup.defaults.model.clone())
            .unwrap_or_else(|| match saved_model {
                Some(info)
                    if info.provider_id != fallback_model.provider_id
                        || info.id != fallback_model.id =>
                {
                    info.to_model_ref(&Effort::Default)
                }
                _ => fallback_model.clone(),
            });
        if session.model.is_none()
            && project_setup
                .defaults
                .model
                .as_ref()
                .is_none_or(|model| model.variant.is_none())
            && let Some(effort) = &self.effort
            && project_setup
                .find_model(&model.provider_id, &model.id)
                .is_some_and(|info| info.offers(effort))
        {
            model.variant = effort.to_variant();
        }
        let agent = session
            .agent
            .as_ref()
            .or(project_setup.defaults.agent.as_ref())
            .or_else(|| {
                self.agent.as_ref().filter(|saved| {
                    project_setup
                        .list_selectable_agents()
                        .iter()
                        .any(|candidate| &candidate.id == *saved)
                })
            })
            .cloned()
            .unwrap_or_else(|| project_setup.find_default_agent_id().to_string());
        SessionConfig { agent, model }
    }
}

fn to_agent_model_info(info: &ModelInfo) -> AgentModelInfo {
    AgentModelInfo {
        id: AgentModelId::new(info.reference()),
        name: info.name.clone().into(),
        description: None,
        icon: find_provider_icon(&info.provider_id).map(AgentModelIcon::Named),
        is_latest: false,
        cost: format_cost(info),
        disabled: None,
    }
}

/// Priced like the native picker's token-priced models. Free models show
/// nothing, as local models do there.
fn format_cost(info: &ModelInfo) -> Option<SharedString> {
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

pub fn find_provider_icon(provider_id: &str) -> Option<IconName> {
    Some(match provider_id {
        "anthropic" => IconName::AiAnthropic,
        "claude-agent" => IconName::AiClaude,
        "openai" => IconName::AiOpenAi,
        "opencode" | "opencode-go" => IconName::AiOpenCode,
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentInfo;

    fn model(
        provider_id: &str,
        id: &str,
        variants: &[&str],
        cost: Option<(f64, f64)>,
    ) -> ModelInfo {
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
        let agents = [
            agent("build", None),
            agent("plan", None),
            agent("explore", Some("subagent")),
        ];
        let project_setup = ProjectSetup {
            models: models.to_vec(),
            agents: agents.to_vec(),
            ..ProjectSetup::default()
        };
        let current = model_ref("opencode", "glm", None);
        let session =
            serde_json::from_value(serde_json::json!({ "id": "session" })).expect("session");
        let defaults =
            |model: Option<&str>, effort: Option<&str>, agent: Option<&str>| SavedChoices {
                model: model.map(str::to_string),
                effort: effort.map(Effort::parse),
                agent: agent.map(str::to_string),
            };

        assert_eq!(
            defaults(Some("openai/gpt"), Some("none"), Some("plan")).choose_starting_config(
                &project_setup,
                &session,
                &current
            ),
            SessionConfig {
                model: model_ref("openai", "gpt", Some("none")),
                agent: "plan".to_string()
            }
        );
        // A model that's gone, an effort the model lacks and a subagent are
        // all ignored.
        assert_eq!(
            defaults(Some("anthropic/claude"), Some("none"), Some("explore"))
                .choose_starting_config(&project_setup, &session, &current),
            SessionConfig {
                model: current.clone(),
                agent: "build".to_string()
            }
        );
        // "default" clears a variant the session already had.
        assert_eq!(
            defaults(None, Some("default"), None).choose_starting_config(
                &project_setup,
                &session,
                &model_ref("opencode", "glm", Some("high")),
            ),
            SessionConfig {
                model: current,
                agent: "build".to_string()
            }
        );
        // The same model keeps its variant when no effort was saved.
        let with_variant = model_ref("opencode", "glm", Some("low"));
        assert_eq!(
            defaults(Some("opencode/glm"), None, None).choose_starting_config(
                &project_setup,
                &session,
                &with_variant
            ),
            SessionConfig {
                model: with_variant.clone(),
                agent: "build".to_string()
            }
        );
    }

    #[test]
    fn configured_defaults_win_independently_without_changing_session_choices() {
        let mut project_setup = ProjectSetup {
            models: vec![
                model("test", "fallback", &[], None),
                model("test", "saved", &["low"], None),
                model("test", "configured", &["low", "high"], None),
            ],
            agents: vec![agent("build", None), agent("saved", None)],
            ..ProjectSetup::default()
        };
        let defaults = SavedChoices {
            model: Some("test/saved".into()),
            effort: Some(Effort::Level("low".into())),
            agent: Some("saved".into()),
        };
        let mut session: SessionInfo =
            serde_json::from_value(serde_json::json!({ "id": "session" })).expect("session");
        let fallback = model_ref("test", "fallback", None);
        project_setup.defaults.agent = Some("configured".into());
        assert_eq!(
            defaults.choose_starting_config(&project_setup, &session, &fallback),
            SessionConfig {
                model: model_ref("test", "saved", Some("low")),
                agent: "configured".into(),
            }
        );

        project_setup.defaults.agent = None;
        project_setup.defaults.model = Some(model_ref("test", "configured", None));
        assert_eq!(
            defaults.choose_starting_config(&project_setup, &session, &fallback),
            SessionConfig {
                model: model_ref("test", "configured", Some("low")),
                agent: "saved".into(),
            }
        );

        project_setup.defaults.agent = Some("configured".into());
        project_setup.defaults.model = Some(model_ref("test", "configured", Some("high")));
        assert_eq!(
            defaults.choose_starting_config(&project_setup, &session, &fallback),
            SessionConfig {
                model: model_ref("test", "configured", Some("high")),
                agent: "configured".into(),
            }
        );

        session.agent = Some("existing".into());
        session.model = Some(model_ref("test", "existing", Some("existing")));
        assert_eq!(
            defaults.choose_starting_config(&project_setup, &session, &fallback),
            SessionConfig {
                model: model_ref("test", "existing", Some("existing")),
                agent: "existing".into(),
            }
        );
    }

    #[test]
    fn server_agent_order_supplies_the_fallback() {
        let mut project_setup = ProjectSetup {
            agents: vec![agent("server-default", None), agent("build", None)],
            ..ProjectSetup::default()
        };
        let defaults = SavedChoices {
            model: None,
            effort: None,
            agent: None,
        };
        let session =
            serde_json::from_value(serde_json::json!({ "id": "session" })).expect("session");
        let fallback = model_ref("test", "fallback", None);
        assert_eq!(
            defaults.choose_starting_config(&project_setup, &session, &fallback),
            SessionConfig {
                model: fallback.clone(),
                agent: "server-default".into(),
            }
        );
        project_setup.agents.clear();
        assert_eq!(
            defaults.choose_starting_config(&project_setup, &session, &fallback),
            SessionConfig {
                model: fallback,
                agent: "build".into(),
            }
        );
    }

    #[test]
    fn free_models_show_no_cost() {
        assert_eq!(
            format_cost(&model("opencode", "free", &[], Some((0.0, 0.0)))),
            None
        );
        assert_eq!(format_cost(&model("openai", "unpriced", &[], None)), None);
        assert_eq!(
            format_cost(&model("github-copilot", "sonnet", &[], Some((2.0, 10.0)))),
            Some("2$/10$".into())
        );
    }
}
