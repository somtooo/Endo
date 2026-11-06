//! OpenCode D's take on Zed's Manage Profiles modal. OpenCode's primary agents
//! play the part of profiles, and their tool permissions and default model
//! live in OpenCode's global config file, so editing them here changes
//! OpenCode itself rather than any Zed setting.

use crate::client::OpenCodeClient;
use crate::connection::OpenCodeConnection;
use crate::opencode_config::{self as config, ALL_TOOLS, Scope, TOOLS, Tool};
use crate::types::{AgentInfo, PermissionEffect};
use agent_ui::AgentPanel;
use anyhow::{Result, anyhow};
use editor::Editor;
use fs::Fs;
use gpui::{
    App, Context, DismissEvent, Div, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Stateful, Task, WeakEntity, Window, actions, prelude::*,
};
use picker::{Picker, PickerDelegate};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use ui::{
    KeyBinding, ListItem, ListItemSpacing, ListSeparator, Navigable, NavigableEntry, Tooltip,
    prelude::*,
};
use util::ResultExt as _;
use workspace::notifications::NotificationId;
use workspace::{ModalView, Toast, Workspace};

actions!(
    opencode_d,
    [
        /// Opens OpenCode's agents, to change their tool permissions and
        /// default model or add new ones.
        ManageAgents
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ManageAgents, window, cx| {
            AgentsModal::toggle(workspace, window, cx);
        });
    })
    .detach();
}

/// A model the agents can default to.
#[derive(Clone)]
pub(crate) struct ModelChoice {
    /// `provider/model`, as the config file names it.
    pub value: String,
    pub name: SharedString,
    pub provider: SharedString,
}

/// What the modal needs from the thread it was opened from. The thread's
/// directory decides which project config files apply.
pub(crate) struct ModalSource {
    pub client: OpenCodeClient,
    pub directory: String,
    pub models: Vec<ModelChoice>,
}

/// OpenCode lists these first, so the modal does too.
const BUILT_IN_AGENTS: &[&str] = &["build", "plan"];

#[derive(Clone)]
struct AgentEntry {
    id: String,
    navigation: NavigableEntry,
}

#[derive(Clone)]
struct ChooseMode {
    agents: Vec<AgentEntry>,
    all_agents_permissions: NavigableEntry,
    add_new_agent: NavigableEntry,
}

#[derive(Clone)]
struct ViewMode {
    agent_id: String,
    fork: NavigableEntry,
    default_model: NavigableEntry,
    tools: NavigableEntry,
    delete: NavigableEntry,
    back: NavigableEntry,
}

enum Mode {
    Choose(ChooseMode),
    New {
        name_editor: Entity<Editor>,
        base: Option<String>,
    },
    View(ViewMode),
    Tools {
        scope: Scope,
        picker: Entity<Picker<ToolsDelegate>>,
    },
    Model {
        agent_id: String,
        picker: Entity<Picker<ModelDelegate>>,
    },
}

pub struct AgentsModal {
    source: Arc<ModalSource>,
    fs: Arc<dyn Fs>,
    config_path: Option<PathBuf>,
    config_text: String,
    agents: Vec<AgentInfo>,
    loading: bool,
    error: Option<SharedString>,
    mode: Mode,
    focus_handle: FocusHandle,
    /// Writes run one after another, each on the file as the last left it.
    last_write: Option<Task<()>>,
    _load: Task<()>,
}

impl AgentsModal {
    fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let source = workspace
            .panel::<AgentPanel>(cx)
            .and_then(|panel| panel.read(cx).active_agent_thread(cx))
            .and_then(|thread| {
                let thread = thread.read(cx);
                thread
                    .connection()
                    .clone()
                    .downcast::<OpenCodeConnection>()?
                    .modal_source(&thread.session_id().0)
            });
        let Some(source) = source else {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<AgentsModal>(),
                    "Open an OpenCode D thread to configure OpenCode's agents.",
                ),
                cx,
            );
            return;
        };
        let fs = workspace.app_state().fs.clone();
        workspace.toggle_modal(window, cx, |window, cx| Self::new(source, fs, window, cx));
    }

    fn new(
        source: ModalSource,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = Arc::new(source);
        let load = cx.spawn_in(window, {
            let source = source.clone();
            let fs = fs.clone();
            async move |this, cx| {
                let (entries, agents) = futures::join!(
                    source.client.config_entries(&source.directory),
                    source.client.agents(&source.directory)
                );
                let config_path = entries
                    .ok()
                    .and_then(|entries| config::global_config_path(&entries));
                let config_text = match &config_path {
                    Some(path) => read_config(fs.as_ref(), path).await,
                    None => Err(anyhow!("OpenCode didn't report its config directory")),
                };
                this.update_in(cx, |this, window, cx| {
                    this.loading = false;
                    this.config_path = config_path;
                    match config_text {
                        Ok(text) => this.config_text = text,
                        Err(error) => this.error = Some(format!("{error:#}").into()),
                    }
                    match agents {
                        Ok(agents) => this.agents = agents,
                        Err(error) => this.error = Some(format!("{error:#}").into()),
                    }
                    this.refresh_mode(window, cx);
                })
                .log_err();
            }
        });
        Self {
            source,
            fs,
            config_path: None,
            config_text: String::new(),
            agents: Vec::new(),
            loading: true,
            error: None,
            mode: Mode::Choose(ChooseMode {
                agents: Vec::new(),
                all_agents_permissions: NavigableEntry::focusable(cx),
                add_new_agent: NavigableEntry::focusable(cx),
            }),
            focus_handle: cx.focus_handle(),
            last_write: None,
            _load: load,
        }
    }

    fn agent(&self, agent_id: &str) -> Option<&AgentInfo> {
        self.agents.iter().find(|agent| agent.id == agent_id)
    }

    fn agent_name(&self, agent_id: &str) -> SharedString {
        self.agent(agent_id)
            .map_or_else(
                || crate::connection::agent_display_name_for_id(agent_id),
                crate::connection::agent_display_name,
            )
            .into()
    }

    /// Primary agents, built-in ones first.
    fn listed_agents(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .agents
            .iter()
            .filter(|agent| !agent.hidden && agent.mode.as_deref() != Some("subagent"))
            .map(|agent| agent.id.clone())
            .collect();
        ids.sort_by_key(|id| !BUILT_IN_AGENTS.contains(&id.as_str()));
        ids
    }

    fn choose_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::Choose(ChooseMode {
            agents: self
                .listed_agents()
                .into_iter()
                .map(|id| AgentEntry {
                    id,
                    navigation: NavigableEntry::focusable(cx),
                })
                .collect(),
            all_agents_permissions: NavigableEntry::focusable(cx),
            add_new_agent: NavigableEntry::focusable(cx),
        });
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn view_agent(&mut self, agent_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::View(ViewMode {
            agent_id,
            fork: NavigableEntry::focusable(cx),
            default_model: NavigableEntry::focusable(cx),
            tools: NavigableEntry::focusable(cx),
            delete: NavigableEntry::focusable(cx),
            back: NavigableEntry::focusable(cx),
        });
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn new_agent(&mut self, base: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let name_editor = cx.new(|cx| Editor::single_line(window, cx));
        name_editor.update(cx, |editor, cx| {
            editor.set_placeholder_text("Agent name", window, cx);
        });
        self.mode = Mode::New { name_editor, base };
        self.error = None;
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn tool_rows(&self, scope: &Scope) -> Vec<ToolRow> {
        let rules = match scope {
            Scope::Global => config::file_rules(&self.config_text, scope).unwrap_or_default(),
            Scope::Agent(agent_id) => self
                .agent(agent_id)
                .map(|agent| agent.permissions.clone())
                .unwrap_or_default(),
        };
        std::iter::once(&ALL_TOOLS)
            .chain(TOOLS)
            .map(|tool| ToolRow {
                tool,
                effect: config::effective(&rules, tool.action),
                has_patterns: config::has_pattern_rules(&rules, tool.action),
            })
            .collect()
    }

    fn configure_tools(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        let delegate = ToolsDelegate {
            modal: cx.weak_entity(),
            rows: self.tool_rows(&scope),
            scope: scope.clone(),
            filtered: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            let mut picker = Picker::uniform_list(delegate, window, cx).embedded();
            picker.refresh(window, cx);
            picker
        });
        self.mode = Mode::Tools { scope, picker };
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn configure_model(&mut self, agent_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .agent(&agent_id)
            .and_then(|agent| agent.model.as_ref())
            .map(|model| format!("{}/{}", model.provider_id, model.id));
        let delegate = ModelDelegate {
            modal: cx.weak_entity(),
            agent_id: agent_id.clone(),
            models: self.source.models.clone(),
            current,
            filtered: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            let mut picker = Picker::list(delegate, window, cx).embedded();
            picker.refresh(window, cx);
            picker
        });
        self.mode = Mode::Model { agent_id, picker };
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Rebuilds what the current mode shows after the agents or the file
    /// changed.
    fn refresh_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Choose(_) => self.choose_agent(window, cx),
            Mode::Tools { scope, picker } => {
                let rows = self.tool_rows(scope);
                picker.update(cx, |picker, cx| {
                    picker.delegate.rows = rows;
                    cx.notify();
                });
            }
            Mode::Model { agent_id, picker } => {
                let current = self
                    .agent(agent_id)
                    .and_then(|agent| agent.model.as_ref())
                    .map(|model| format!("{}/{}", model.provider_id, model.id));
                picker.update(cx, |picker, cx| {
                    picker.delegate.current = current;
                    cx.notify();
                });
            }
            Mode::New { .. } | Mode::View(_) => {}
        }
        cx.notify();
    }

    /// Applies `edit` to the config file as it is on disk, then waits for
    /// the service to pick the change up.
    fn edit_config(
        &mut self,
        edit: impl FnOnce(&str) -> Result<String> + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.config_path.clone() else {
            self.error = Some("OpenCode's config file couldn't be found.".into());
            cx.notify();
            return;
        };
        let previous = self.last_write.take();
        let fs = self.fs.clone();
        let source = self.source.clone();
        let agents_before = self.agents.clone();
        self.last_write = Some(cx.spawn_in(window, async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let written = async {
                let text = read_config(fs.as_ref(), &path).await?;
                let updated = edit(&text)?;
                if updated != text {
                    if let Some(directory) = path.parent() {
                        fs.create_dir(directory).await?;
                    }
                    fs.atomic_write(path.clone(), updated.clone()).await?;
                }
                anyhow::Ok(updated)
            }
            .await;
            let text = match written {
                Ok(text) => text,
                Err(error) => {
                    log::error!("Failed to update OpenCode's config: {error:#}");
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };
            this.update(cx, |this, cx| {
                this.config_text = text;
                this.error = None;
                cx.notify();
            })
            .log_err();
            // The service reloads a moment after the file changes.
            let mut agents = None;
            for _ in 0..20 {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                match source.client.agents(&source.directory).await {
                    Ok(fetched) if fetched != agents_before => {
                        agents = Some(fetched);
                        break;
                    }
                    Ok(fetched) => agents = Some(fetched),
                    Err(error) => log::warn!("Failed to reload OpenCode's agents: {error:#}"),
                }
            }
            this.update_in(cx, |this, window, cx| {
                if let Some(agents) = agents {
                    this.agents = agents;
                }
                this.refresh_mode(window, cx);
            })
            .log_err();
        }));
    }

    fn set_permission(
        &mut self,
        scope: Scope,
        tool: &'static Tool,
        effect: PermissionEffect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_config(
            move |text| config::set_permission(text, &scope, tool, effect),
            window,
            cx,
        );
    }

    fn set_model(
        &mut self,
        agent_id: String,
        model: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_config(
            move |text| config::set_agent_model(text, &agent_id, model.as_deref()),
            window,
            cx,
        );
    }

    fn delete_agent(&mut self, agent_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_delete(&agent_id) {
            return;
        }
        self.edit_config(
            {
                let agent_id = agent_id.clone();
                move |text| config::delete_agent(text, &agent_id)
            },
            window,
            cx,
        );
        // After the write starts, so its wait for the reload compares
        // against the list that still has the agent.
        self.agents.retain(|agent| agent.id != agent_id);
        self.choose_agent(window, cx);
    }

    /// Only agents the global file defines can be removed from it.
    fn can_delete(&self, agent_id: &str) -> bool {
        !BUILT_IN_AGENTS.contains(&agent_id) && config::defines_agent(&self.config_text, agent_id)
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::New { name_editor, base } = &self.mode else {
            return;
        };
        let name = name_editor.read(cx).text(cx);
        let agent_id = config::agent_id_for_name(&name);
        if agent_id.is_empty() {
            self.error = Some("Give the agent a name.".into());
            cx.notify();
            return;
        }
        if self.agent(&agent_id).is_some() || config::defines_agent(&self.config_text, &agent_id) {
            self.error = Some(format!("An agent named \"{agent_id}\" already exists.").into());
            cx.notify();
            return;
        }
        let agent = config::forked_agent(base.as_deref().and_then(|base| self.agent(base)));
        self.edit_config(
            {
                let agent_id = agent_id.clone();
                move |text| config::create_agent(text, &agent_id, &agent)
            },
            window,
            cx,
        );
        self.view_agent(agent_id, window, cx);
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Choose(_) => cx.emit(DismissEvent),
            Mode::New {
                base: Some(base), ..
            } => self.view_agent(base.clone(), window, cx),
            Mode::New { base: None, .. } | Mode::View(_) => {
                self.error = None;
                self.choose_agent(window, cx);
            }
            Mode::Tools {
                scope: Scope::Agent(agent_id),
                ..
            }
            | Mode::Model { agent_id, .. } => self.view_agent(agent_id.clone(), window, cx),
            Mode::Tools {
                scope: Scope::Global,
                ..
            } => self.choose_agent(window, cx),
        }
    }
}

async fn read_config(fs: &dyn Fs, path: &Path) -> Result<String> {
    if fs.is_file(path).await {
        fs.load(path).await
    } else {
        Ok(String::new())
    }
}

impl ModalView for AgentsModal {}

impl EventEmitter<DismissEvent> for AgentsModal {}

impl Focusable for AgentsModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.mode {
            Mode::New { name_editor, .. } => name_editor.focus_handle(cx),
            Mode::Tools { picker, .. } => picker.focus_handle(cx),
            Mode::Model { picker, .. } => picker.focus_handle(cx),
            Mode::Choose(_) | Mode::View(_) => self.focus_handle.clone(),
        }
    }
}

fn header(label: impl Into<SharedString>, icon: Option<IconName>, cx: &App) -> impl IntoElement {
    h_flex()
        .w_full()
        .px(DynamicSpacing::Base12.rems(cx))
        .pt(DynamicSpacing::Base08.rems(cx))
        .pb(DynamicSpacing::Base04.rems(cx))
        .gap_1p5()
        .when_some(icon, |this, icon| {
            this.child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        })
        .child(
            div()
                .max_w_96()
                .overflow_x_hidden()
                .text_ellipsis()
                .child(Headline::new(label.into()).size(HeadlineSize::XSmall)),
        )
}

impl AgentsModal {
    /// A row that runs `activate` when clicked or confirmed with the
    /// keyboard.
    fn item(
        &self,
        id: impl Into<SharedString>,
        entry: &NavigableEntry,
        start_icon: Option<(IconName, Color)>,
        label: impl Into<SharedString>,
        label_color: Color,
        end_slot: Option<AnyElement>,
        activate: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id: SharedString = id.into();
        let is_focused = entry.focus_handle.contains_focused(window, cx);
        div()
            .id(id.clone())
            .track_focus(&entry.focus_handle)
            .on_action({
                let activate = activate.clone();
                cx.listener(move |this, _: &menu::Confirm, window, cx| activate(this, window, cx))
            })
            .child(
                ListItem::new(id)
                    .toggle_state(is_focused)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .when_some(start_icon, |this, (icon, color)| {
                        this.start_slot(Icon::new(icon).size(IconSize::Small).color(color))
                    })
                    .child(Label::new(label.into()).color(label_color))
                    .when_some(end_slot, |this, end_slot| this.end_slot(end_slot))
                    .on_click(cx.listener(move |this, _, window, cx| activate(this, window, cx))),
            )
    }

    fn render_choose(
        &self,
        mode: &ChooseMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (built_in, custom): (Vec<&AgentEntry>, Vec<&AgentEntry>) = mode
            .agents
            .iter()
            .partition(|agent| BUILT_IN_AGENTS.contains(&agent.id.as_str()));
        let render_agent =
            |this: &Self, agent: &AgentEntry, window: &mut Window, cx: &mut Context<Self>| {
                let is_focused = agent.navigation.focus_handle.contains_focused(window, cx);
                let agent_id = agent.id.clone();
                this.item(
                    format!("agent-{}", agent.id),
                    &agent.navigation,
                    None,
                    this.agent_name(&agent.id),
                    Color::Default,
                    is_focused.then(|| {
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new("Customize")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(KeyBinding::for_action_in(
                                &menu::Confirm,
                                &this.focus_handle,
                                cx,
                            ))
                            .into_any_element()
                    }),
                    move |this, window, cx| this.view_agent(agent_id.clone(), window, cx),
                    window,
                    cx,
                )
            };
        let mut list = v_flex().pb_1().child(ListSeparator);
        if self.loading {
            list = list.child(
                div()
                    .px_3()
                    .py_1()
                    .child(Label::new("Loading agents…").color(Color::Muted)),
            );
        }
        for agent in &built_in {
            list = list.child(render_agent(self, agent, window, cx));
        }
        if !custom.is_empty() {
            list = list.child(ListSeparator).child(
                div().pl_2().pb_1().child(
                    Label::new("Custom Agents")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
            for agent in &custom {
                list = list.child(render_agent(self, agent, window, cx));
            }
        }
        list = list
            .child(ListSeparator)
            .child(self.item(
                "all-agents-permissions",
                &mode.all_agents_permissions,
                Some((IconName::Lock, Color::Muted)),
                "Tool Permissions for All Agents",
                Color::Default,
                None,
                |this, window, cx| this.configure_tools(Scope::Global, window, cx),
                window,
                cx,
            ))
            .child(self.item(
                "new-agent",
                &mode.add_new_agent,
                Some((IconName::Plus, Color::Default)),
                "Add New Agent",
                Color::Default,
                None,
                |this, window, cx| this.new_agent(None, window, cx),
                window,
                cx,
            ));

        let mut navigable = Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(header("OpenCode Agents", None, cx))
                .child(list)
                .into_any_element(),
        );
        for agent in built_in.into_iter().chain(custom) {
            navigable = navigable.entry(agent.navigation.clone());
        }
        navigable
            .entry(mode.all_agents_permissions.clone())
            .entry(mode.add_new_agent.clone())
    }

    fn render_view(
        &self,
        mode: &ViewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let agent_id = mode.agent_id.clone();
        let can_delete = self.can_delete(&agent_id);
        let icon = match agent_id.as_str() {
            "build" => IconName::Pencil,
            "plan" => IconName::ListTodo,
            _ => IconName::UserRoundPen,
        };
        let delete_tooltip = if BUILT_IN_AGENTS.contains(&agent_id.as_str()) {
            Some("OpenCode's built-in agents can't be deleted.")
        } else if !can_delete {
            Some("This agent is defined in a file other than OpenCode's global config.")
        } else {
            None
        };
        let delete = self
            .item(
                "delete-agent",
                &mode.delete,
                Some((
                    IconName::Trash,
                    if can_delete {
                        Color::Error
                    } else {
                        Color::Disabled
                    },
                )),
                "Delete Agent",
                if can_delete {
                    Color::Error
                } else {
                    Color::Disabled
                },
                None,
                {
                    let agent_id = agent_id.clone();
                    move |this, window, cx| this.delete_agent(agent_id.clone(), window, cx)
                },
                window,
                cx,
            )
            .when_some(delete_tooltip, |this, tooltip| {
                this.tooltip(Tooltip::text(tooltip))
            });
        Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(header(self.agent_name(&agent_id), Some(icon), cx))
                .child(
                    v_flex()
                        .pb_1()
                        .child(ListSeparator)
                        .child(self.item(
                            "fork-agent",
                            &mode.fork,
                            Some((IconName::Scissors, Color::Muted)),
                            "Fork Agent",
                            Color::Default,
                            None,
                            {
                                let agent_id = agent_id.clone();
                                move |this, window, cx| {
                                    this.new_agent(Some(agent_id.clone()), window, cx)
                                }
                            },
                            window,
                            cx,
                        ))
                        .child(self.item(
                            "configure-default-model",
                            &mode.default_model,
                            Some((IconName::ZedAssistant, Color::Muted)),
                            "Configure Default Model",
                            Color::Default,
                            None,
                            {
                                let agent_id = agent_id.clone();
                                move |this, window, cx| {
                                    this.configure_model(agent_id.clone(), window, cx)
                                }
                            },
                            window,
                            cx,
                        ))
                        .child(self.item(
                            "configure-tools",
                            &mode.tools,
                            Some((IconName::Settings, Color::Muted)),
                            "Configure Tool Permissions",
                            Color::Default,
                            None,
                            {
                                let agent_id = agent_id.clone();
                                move |this, window, cx| {
                                    this.configure_tools(Scope::Agent(agent_id.clone()), window, cx)
                                }
                            },
                            window,
                            cx,
                        ))
                        .child(delete)
                        .child(ListSeparator)
                        .child(self.go_back(&mode.back, window, cx)),
                )
                .into_any_element(),
        )
        .entry(mode.fork.clone())
        .entry(mode.default_model.clone())
        .entry(mode.tools.clone())
        .entry(mode.delete.clone())
        .entry(mode.back.clone())
    }

    fn go_back(
        &self,
        entry: &NavigableEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_focused = entry.focus_handle.contains_focused(window, cx);
        div()
            .id("go-back")
            .track_focus(&entry.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.cancel(window, cx)))
            .child(
                ListItem::new("go-back")
                    .toggle_state(is_focused)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .start_slot(
                        Icon::new(IconName::ArrowLeft)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("Go Back"))
                    .end_slot(
                        div().child(
                            KeyBinding::for_action_in(&menu::Cancel, &self.focus_handle, cx)
                                .size(rems_from_px(12_f32)),
                        ),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
            )
    }

    /// Where changes go, and errors from the last one.
    fn render_footer(&self, cx: &App) -> impl IntoElement {
        let location = self.config_path.as_ref().map(|path| {
            let path = util::paths::home_dir()
                .as_path()
                .to_str()
                .and_then(|home| {
                    path.to_str()?
                        .strip_prefix(home)
                        .map(|rest| format!("~{rest}"))
                })
                .unwrap_or_else(|| path.display().to_string());
            format!("Saved to {path}. Project config files can override it.")
        });
        v_flex()
            .px_3()
            .pb_2()
            .gap_1()
            .when_some(location, |this, location| {
                this.child(
                    Label::new(location)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .pt_2()
    }
}

impl Render for AgentsModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let back_entry = NavigableEntry {
            focus_handle: self.focus_handle.clone(),
            scroll_anchor: None,
        };
        let content = match &self.mode {
            Mode::Choose(mode) => self.render_choose(mode, window, cx).into_any_element(),
            Mode::View(mode) => self.render_view(mode, window, cx).into_any_element(),
            Mode::New { name_editor, base } => {
                let title = match base {
                    Some(base) => format!("Fork {}", self.agent_name(base)),
                    None => "New Agent".into(),
                };
                let icon = if base.is_some() {
                    IconName::Scissors
                } else {
                    IconName::Plus
                };
                v_flex()
                    .child(header(title, Some(icon), cx))
                    .child(ListSeparator)
                    .child(h_flex().p_2().child(name_editor.clone()))
                    .into_any_element()
            }
            Mode::Tools { scope, picker } => {
                let title = match scope {
                    Scope::Global => "All Agents — Configure Tool Permissions".to_string(),
                    Scope::Agent(agent_id) => {
                        format!("{} — Configure Tool Permissions", self.agent_name(agent_id))
                    }
                };
                v_flex()
                    .pb_1()
                    .child(header(title, Some(IconName::Settings), cx))
                    .child(ListSeparator)
                    .child(picker.clone())
                    .child(ListSeparator)
                    .child(self.go_back(&back_entry, window, cx))
                    .into_any_element()
            }
            Mode::Model { agent_id, picker } => v_flex()
                .pb_1()
                .child(header(
                    format!("{} — Configure Default Model", self.agent_name(agent_id)),
                    Some(IconName::ZedAgent),
                    cx,
                ))
                .child(ListSeparator)
                .child(picker.clone())
                .child(ListSeparator)
                .child(self.go_back(&back_entry, window, cx))
                .into_any_element(),
        };
        v_flex()
            .elevation_3(cx)
            .w(rems(34.))
            .key_context("ManageProfilesModal")
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| this.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .child(content)
            .child(self.render_footer(cx))
    }
}

struct ToolRow {
    tool: &'static Tool,
    effect: Option<PermissionEffect>,
    has_patterns: bool,
}

/// Lists each tool with what OpenCode does when the agent uses it.
/// Confirming a row moves it to the next of allow, ask and deny.
struct ToolsDelegate {
    modal: WeakEntity<AgentsModal>,
    scope: Scope,
    rows: Vec<ToolRow>,
    filtered: Vec<usize>,
    selected_index: usize,
}

fn next_effect(effect: Option<PermissionEffect>) -> PermissionEffect {
    match effect {
        Some(PermissionEffect::Allow) => PermissionEffect::Ask,
        Some(PermissionEffect::Ask) => PermissionEffect::Deny,
        Some(PermissionEffect::Deny) | None => PermissionEffect::Allow,
    }
}

fn matching(label: &str, query: &str) -> bool {
    label.to_lowercase().contains(&query.trim().to_lowercase())
}

impl PickerDelegate for ToolsDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "opencode tool permissions"
    }

    fn match_count(&self) -> usize {
        self.filtered.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search tools…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.filtered = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                matching(row.tool.label, &query) || matching(row.tool.action, &query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.filtered.len().saturating_sub(1));
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(row) = self
            .filtered
            .get(self.selected_index)
            .and_then(|index| self.rows.get_mut(*index))
        else {
            return;
        };
        let effect = next_effect(row.effect);
        // Shown at once; the reload after the write confirms it.
        row.effect = Some(effect);
        let tool = row.tool;
        let scope = self.scope.clone();
        self.modal
            .update(cx, |modal, cx| {
                modal.set_permission(scope, tool, effect, window, cx)
            })
            .log_err();
        cx.notify();
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let modal = self.modal.clone();
        window.defer(cx, move |window, cx| {
            modal
                .update(cx, |modal, cx| modal.cancel(window, cx))
                .log_err();
        });
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = self.rows.get(*self.filtered.get(ix)?)?;
        let (label, color) = match row.effect {
            Some(PermissionEffect::Allow) => ("Allow", Color::Success),
            Some(PermissionEffect::Ask) => ("Ask", Color::Warning),
            Some(PermissionEffect::Deny) => ("Deny", Color::Error),
            None => ("Not Set", Color::Muted),
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(Label::new(row.tool.label))
                        .when(row.has_patterns, |this| {
                            this.child(
                                Label::new("· some differ")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
                .end_slot(Label::new(label).size(LabelSize::Small).color(color))
                .tooltip(Tooltip::text(if row.has_patterns {
                    "Click to change. Rules for particular files, commands or URLs stay as they are."
                } else {
                    "Click to change."
                }))
                .into_any_element(),
        )
    }
}

/// Picks the model an agent uses instead of the thread's.
struct ModelDelegate {
    modal: WeakEntity<AgentsModal>,
    agent_id: String,
    models: Vec<ModelChoice>,
    current: Option<String>,
    /// `None` stands for the thread's model.
    filtered: Vec<Option<usize>>,
    selected_index: usize,
}

impl PickerDelegate for ModelDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "opencode agent model"
    }

    fn match_count(&self) -> usize {
        self.filtered.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search models…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let models = self.models.iter().enumerate().filter(|(_, model)| {
            matching(&model.name, &query)
                || matching(&model.provider, &query)
                || matching(&model.value, &query)
        });
        self.filtered = query
            .trim()
            .is_empty()
            .then_some(None)
            .into_iter()
            .chain(models.map(|(index, _)| Some(index)))
            .collect();
        self.selected_index = self
            .filtered
            .iter()
            .position(|choice| {
                choice
                    .and_then(|index| self.models.get(index))
                    .map(|model| &model.value)
                    == self.current.as_ref()
            })
            .unwrap_or(0);
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(choice) = self.filtered.get(self.selected_index) else {
            return;
        };
        let model = choice
            .and_then(|index| self.models.get(index))
            .map(|model| model.value.clone());
        self.current = model.clone();
        let agent_id = self.agent_id.clone();
        let modal = self.modal.clone();
        window.defer(cx, move |window, cx| {
            modal
                .update(cx, |modal, cx| {
                    modal.set_model(agent_id.clone(), model, window, cx);
                    modal.view_agent(agent_id, window, cx);
                })
                .log_err();
        });
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let modal = self.modal.clone();
        window.defer(cx, move |window, cx| {
            modal
                .update(cx, |modal, cx| modal.cancel(window, cx))
                .log_err();
        });
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let choice = *self.filtered.get(ix)?;
        let (label, detail, value) = match choice {
            None => (SharedString::from("Thread's Model"), None, None),
            Some(index) => {
                let model = self.models.get(index)?;
                (
                    model.name.clone(),
                    Some(model.provider.clone()),
                    Some(&model.value),
                )
            }
        };
        let is_current = value == self.current.as_ref();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(h_flex().gap_1p5().child(Label::new(label)).when_some(
                    detail,
                    |this, detail| {
                        this.child(
                            Label::new(detail)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    },
                ))
                .end_slot::<Icon>(is_current.then(|| {
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Accent)
                }))
                .into_any_element(),
        )
    }
}
