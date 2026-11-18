//! OpenCode D's take on Zed's Manage Profiles modal. OpenCode's primary agents
//! play the part of profiles, and their tool permissions and default model
//! live in OpenCode's global config file, so editing them here changes
//! OpenCode itself rather than any Zed setting.
//!
//! The agents come from the thread's project setup. OpenCode rereads the file after
//! an edit and reports that its agents changed, and the connection then
//! reloads the project setup, so an edit shows here a moment after it's saved.

use crate::client::OpenCodeClient;
use crate::connection::OpenCodeConnection;
use crate::opencode_config::{
    self as config, ALL_TOOLS, ConfigFileState, LoadedConfig, Scope, TOOLS, Tool,
};
use crate::project_setup::BUILT_IN_AGENTS;
use crate::project_setup::ProjectSetup;
use crate::types::{ModelRef, PermissionEffect, PermissionRule};
use agent_ui::{AgentPanel, ProfileModalHeader};
use anyhow::Result;
use editor::Editor;
use fs::Fs;
use gpui::{
    AnyView, App, Context, DismissEvent, Div, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, Stateful, Subscription, Task, WeakEntity, Window, actions, prelude::*,
};
use picker::{Picker, PickerDelegate};
use serde_json::Value;
use std::sync::Arc;
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

/// What the modal needs from the thread it was opened from.
pub(crate) struct ModalSource {
    pub client: OpenCodeClient,
    pub project_setup: Entity<ProjectSetup>,
    /// The thread's directory, which the config file's requests name.
    pub directory: String,
}

pub struct AgentsModal {
    /// The project setup of the thread's directory.
    project_setup: Entity<ProjectSetup>,
    config: Entity<LoadedConfig>,
    /// The latest form or write error. Showing another page clears it.
    error: Option<SharedString>,
    pending_permissions: Vec<PendingPermission>,
    next_pending_permission_id: u64,
    page: Page,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

struct PendingPermission {
    id: u64,
    agent_id: String,
    action: &'static str,
    effect: Option<PermissionEffect>,
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
                    .modal_source(&thread.session_id().0, cx)
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
        workspace.toggle_modal(window, cx, |_, cx| Self::new(source, fs, cx));
    }

    fn new(source: ModalSource, fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        let ModalSource {
            client,
            project_setup,
            directory,
        } = source;
        let config = LoadedConfig::shared(client, directory, fs, cx);
        let subscriptions = vec![
            cx.observe(&config, |this, _, cx| {
                this.error = None;
                this.refresh_page(cx);
            }),
            cx.observe(&project_setup, |this, _, cx| {
                let setup = this.project_setup.read(cx);
                this.pending_permissions.retain(|pending| {
                    setup.find_agent(&pending.agent_id).is_none_or(|agent| {
                        config::find_general_effect(&agent.permissions, pending.action)
                            != pending.effect
                    })
                });
                this.refresh_page(cx);
            }),
        ];
        let page = Page::AgentList(AgentListPage {
            agents: build_agent_rows(project_setup.read(cx), &[], cx),
            all_agents_permissions: NavigableEntry::focusable(cx),
            add_new_agent: NavigableEntry::focusable(cx),
        });
        Self {
            project_setup,
            config,
            error: None,
            pending_permissions: Vec::new(),
            next_pending_permission_id: 0,
            page,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }
}

impl Render for AgentsModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.page {
            Page::AgentList(page) => self.render_agent_list(page, window, cx).into_any_element(),
            Page::Agent(page) => self.render_agent(page, window, cx).into_any_element(),
            Page::NewAgent(form) => self.render_new_agent_form(form, cx).into_any_element(),
            Page::ToolPermissions(page) => {
                let title = match &page.scope {
                    Scope::AllAgents => "All Agents — Configure Tool Permissions".to_string(),
                    Scope::Agent(agent_id) => {
                        format!(
                            "{} — Configure Tool Permissions",
                            self.project_setup.read(cx).format_agent_name(agent_id)
                        )
                    }
                };
                self.render_picker_page(
                    title,
                    IconName::Settings,
                    page.picker.clone().into(),
                    window,
                    cx,
                )
            }
            Page::DefaultModel(page) => self.render_picker_page(
                format!(
                    "{} — Configure Default Model",
                    self.project_setup
                        .read(cx)
                        .format_agent_name(&page.agent_id)
                ),
                IconName::ZedAgent,
                page.picker.clone().into(),
                window,
                cx,
            ),
        };
        v_flex()
            .elevation_3(cx)
            .w(rems(34.))
            .key_context("OpenCodeAgentsModal")
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| this.go_back(window, cx)))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .child(content)
            .child(self.render_footer(cx))
    }
}

impl AgentsModal {
    /// Writes run in the background. A failed one takes back what the page
    /// showed early, and a saved one shows once OpenCode rereads the file.
    /// The task returns whether the write was saved.
    fn edit_config(
        &mut self,
        change: impl FnOnce(&mut Value) -> Result<()> + 'static,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        let ConfigFileState::Loaded { .. } = self.config.read(cx).state() else {
            self.error = Some("OpenCode's config file isn't loaded.".into());
            // The caller may be the picker this rebuild updates.
            let modal = cx.weak_entity();
            cx.defer(move |cx| {
                modal.update(cx, Self::refresh_page).log_err();
            });
            return Task::ready(false);
        };
        let written = self.config.update(cx, |config, cx| config.edit(change, cx));
        cx.spawn(async move |this, cx| {
            let result = written.await;
            this.update(cx, |this, cx| {
                let saved = match result {
                    Ok(()) => {
                        this.error = None;
                        true
                    }
                    Err(error) => {
                        this.error = Some(LoadedConfig::report_edit_error(&error));
                        false
                    }
                };
                this.refresh_page(cx);
                saved
            })
            .unwrap_or(false)
        })
    }

    /// Shows what changed in the project setup or the config file, leaving the
    /// focus where it is.
    fn refresh_page(&mut self, cx: &mut Context<Self>) {
        match &self.page {
            Page::AgentList(page) => {
                let agents = build_agent_rows(self.project_setup.read(cx), &page.agents, cx);
                if let Page::AgentList(page) = &mut self.page {
                    page.agents = agents;
                }
            }
            Page::ToolPermissions(page) => {
                let rows = self.build_tool_rows(&page.scope, cx);
                page.picker.update(cx, |picker, cx| {
                    picker.delegate.rows = rows;
                    cx.notify();
                });
            }
            Page::DefaultModel(page) => {
                let current_reference = self.find_agent_model(&page.agent_id, cx);
                page.picker.update(cx, |picker, cx| {
                    picker.delegate.current_reference = current_reference;
                    cx.notify();
                });
            }
            Page::Agent(_) | Page::NewAgent(_) => {}
        }
        cx.notify();
    }

    fn build_tool_rows(&self, scope: &Scope, cx: &App) -> Vec<ToolRow> {
        let rules_in_file;
        let rules: &[PermissionRule] = match (scope, self.config.read(cx).state()) {
            (Scope::AllAgents, ConfigFileState::Loaded { config, .. }) => {
                rules_in_file = config::read_file_rules(config, scope)
                    .log_err()
                    .unwrap_or_default();
                &rules_in_file
            }
            (Scope::AllAgents, _) => &[],
            (Scope::Agent(agent_id), _) => self
                .project_setup
                .read(cx)
                .find_agent(agent_id)
                .map_or(&[], |agent| agent.permissions.as_slice()),
        };
        std::iter::once(&ALL_TOOLS)
            .chain(TOOLS)
            .map(|tool| ToolRow {
                tool,
                permission: match scope {
                    Scope::Agent(agent_id) => self
                        .pending_permissions
                        .iter()
                        .find(|pending| {
                            pending.agent_id == *agent_id && pending.action == tool.action
                        })
                        .map_or_else(
                            || config::find_general_effect(rules, tool.action),
                            |pending| pending.effect,
                        ),
                    Scope::AllAgents => config::find_general_effect(rules, tool.action),
                },
                has_resource_rules: config::has_resource_rules(rules, tool.action),
            })
            .collect()
    }

    fn find_agent_model(&self, agent_id: &str, cx: &App) -> Option<String> {
        self.project_setup
            .read(cx)
            .find_agent(agent_id)?
            .model
            .as_ref()
            .map(ModelRef::reference)
    }

    fn create_agent_from_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Page::NewAgent(form) = &self.page else {
            return;
        };
        let agent_id = config::to_agent_id(&form.name_editor.read(cx).text(cx));
        if agent_id.is_empty() {
            self.error = Some("Give the agent a name.".into());
            cx.notify();
            return;
        }
        // The file can define agents OpenCode doesn't list, which creating
        // the agent checks for.
        if self.project_setup.read(cx).find_agent(&agent_id).is_some() {
            self.error = Some(format!("An agent named \"{agent_id}\" already exists.").into());
            cx.notify();
            return;
        }
        let forked_from = form
            .forked_from
            .as_deref()
            .and_then(|agent_id| self.project_setup.read(cx).find_agent(agent_id))
            .cloned();
        let saved = self.edit_config(
            {
                let agent_id = agent_id.clone();
                move |json| config::create_agent(json, &agent_id, forked_from.as_ref())
            },
            cx,
        );
        self.leave_page_once_saved(
            saved,
            |page| matches!(page, Page::NewAgent(_)),
            move |this, window, cx| this.show_agent(agent_id, window, cx),
            window,
            cx,
        );
    }

    /// Leaves `page` once `saved` finishes, if the write worked and the user
    /// is still on that page; a failed one stays, with its error.
    fn leave_page_once_saved(
        &self,
        saved: Task<bool>,
        is_page: impl Fn(&Page) -> bool + 'static,
        leave: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            if !saved.await {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                if is_page(&this.page) {
                    leave(this, window, cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn show_agent(&mut self, agent_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let page = AgentPage {
            agent_id,
            fork: NavigableEntry::focusable(cx),
            default_model: NavigableEntry::focusable(cx),
            tools: NavigableEntry::focusable(cx),
            delete: NavigableEntry::focusable(cx),
            back: NavigableEntry::focusable(cx),
        };
        self.show_page(Page::Agent(page), window, cx);
    }

    fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        let return_to = match (&page, &self.page) {
            (Page::AgentList(page), Page::Agent(agent)) => page
                .agents
                .iter()
                .find(|row| row.id == agent.agent_id)
                .map(|row| &row.navigation),
            (Page::AgentList(page), Page::NewAgent(_)) => Some(&page.add_new_agent),
            (Page::AgentList(page), Page::ToolPermissions(_)) => Some(&page.all_agents_permissions),
            (Page::Agent(page), Page::NewAgent(form))
                if form.forked_from.as_ref() == Some(&page.agent_id) =>
            {
                Some(&page.fork)
            }
            (Page::Agent(page), Page::ToolPermissions(tools))
                if tools.scope == Scope::Agent(page.agent_id.clone()) =>
            {
                Some(&page.tools)
            }
            (Page::Agent(page), Page::DefaultModel(model)) if model.agent_id == page.agent_id => {
                Some(&page.default_model)
            }
            _ => None,
        }
        .map(|entry| entry.focus_handle.clone());
        self.page = page;
        self.error = None;
        return_to
            .unwrap_or_else(|| self.focus_handle(cx))
            .focus(window, cx);
        cx.notify();
    }

    fn render_agent_list(
        &self,
        page: &AgentListPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (built_in, custom): (Vec<&AgentRow>, Vec<&AgentRow>) = page
            .agents
            .iter()
            .partition(|agent| BUILT_IN_AGENTS.contains(&agent.id.as_str()));
        let mut list = v_flex().pb_1().child(ListSeparator);
        for agent in &built_in {
            list = list.child(self.render_agent_row(agent, window, cx));
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
                list = list.child(self.render_agent_row(agent, window, cx));
            }
        }
        list = list
            .child(ListSeparator)
            .child(
                self.render_row(
                    Row::new("all-agents-permissions", "Tool Permissions for All Agents")
                        .icon(IconName::Lock, Color::Muted),
                    &page.all_agents_permissions,
                    |this, window, cx| this.show_tool_permissions(Scope::AllAgents, window, cx),
                    window,
                    cx,
                ),
            )
            .child(self.render_row(
                Row::new("new-agent", "Add New Agent").icon(IconName::Plus, Color::Default),
                &page.add_new_agent,
                |this, window, cx| this.show_new_agent_form(None, window, cx),
                window,
                cx,
            ));

        let mut navigable = Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(ProfileModalHeader::new("OpenCode Agents", None))
                .child(list)
                .into_any_element(),
        );
        for agent in built_in.into_iter().chain(custom) {
            navigable = navigable.entry(agent.navigation.clone());
        }
        navigable
            .entry(page.all_agents_permissions.clone())
            .entry(page.add_new_agent.clone())
    }

    fn render_agent_row(
        &self,
        agent: &AgentRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let is_focused = agent.navigation.focus_handle.contains_focused(window, cx);
        let customize = is_focused.then(|| {
            h_flex()
                .gap_1()
                .child(
                    Label::new("Customize")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(KeyBinding::for_action_in(
                    &menu::Confirm,
                    &self.focus_handle,
                    cx,
                ))
                .into_any_element()
        });
        let agent_id = agent.id.clone();
        self.render_row(
            Row::new(
                format!("agent-{}", agent.id),
                self.project_setup.read(cx).format_agent_name(&agent.id),
            )
            .end_slot(customize),
            &agent.navigation,
            move |this, window, cx| this.show_agent(agent_id.clone(), window, cx),
            window,
            cx,
        )
    }

    fn show_tool_permissions(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        let delegate = ToolPermissionsDelegate {
            modal: cx.weak_entity(),
            rows: self.build_tool_rows(&scope, cx),
            scope: scope.clone(),
            filtered: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            let mut picker = Picker::uniform_list(delegate, window, cx).embedded();
            picker.refresh(window, cx);
            picker
        });
        let page = ToolPermissionsPage { scope, picker };
        self.show_page(Page::ToolPermissions(page), window, cx);
    }

    fn show_new_agent_form(
        &mut self,
        forked_from: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Agent name", window, cx);
            editor
        });
        let form = NewAgentForm {
            name_editor,
            forked_from,
        };
        self.show_page(Page::NewAgent(form), window, cx);
    }

    fn render_agent(
        &self,
        page: &AgentPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let agent_id = page.agent_id.clone();
        let can_delete = self.check_can_delete(&agent_id, cx);
        let delete_color = if can_delete.is_ok() {
            Color::Error
        } else {
            Color::Disabled
        };
        let icon = match agent_id.as_str() {
            "build" => IconName::Pencil,
            "plan" => IconName::ListTodo,
            _ => IconName::UserRoundPen,
        };
        let delete = self.render_row(
            Row::new("delete-agent", "Delete Agent")
                .icon(IconName::Trash, delete_color)
                .label_color(delete_color),
            &page.delete,
            {
                let agent_id = agent_id.clone();
                move |this, window, cx| this.delete_agent(agent_id.clone(), window, cx)
            },
            window,
            cx,
        );
        Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(ProfileModalHeader::new(
                    self.project_setup.read(cx).format_agent_name(&agent_id),
                    Some(icon),
                ))
                .child(
                    v_flex()
                        .pb_1()
                        .child(ListSeparator)
                        .child(
                            self.render_row(
                                Row::new("fork-agent", "Fork Agent")
                                    .icon(IconName::Scissors, Color::Muted),
                                &page.fork,
                                {
                                    let agent_id = agent_id.clone();
                                    move |this, window, cx| {
                                        this.show_new_agent_form(Some(agent_id.clone()), window, cx)
                                    }
                                },
                                window,
                                cx,
                            ),
                        )
                        .child(
                            self.render_row(
                                Row::new("configure-default-model", "Configure Default Model")
                                    .icon(IconName::ZedAssistant, Color::Muted),
                                &page.default_model,
                                {
                                    let agent_id = agent_id.clone();
                                    move |this, window, cx| {
                                        this.show_default_model(agent_id.clone(), window, cx)
                                    }
                                },
                                window,
                                cx,
                            ),
                        )
                        .child(
                            self.render_row(
                                Row::new("configure-tools", "Configure Tool Permissions")
                                    .icon(IconName::Settings, Color::Muted),
                                &page.tools,
                                {
                                    let agent_id = agent_id.clone();
                                    move |this, window, cx| {
                                        let scope = Scope::Agent(agent_id.clone());
                                        this.show_tool_permissions(scope, window, cx)
                                    }
                                },
                                window,
                                cx,
                            ),
                        )
                        .child(delete)
                        .when_some(can_delete.err(), |this, reason| {
                            this.child(
                                div().px_3().pb_1().child(
                                    Label::new(reason)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            )
                        })
                        .child(ListSeparator)
                        .child(self.render_back_row(&page.back, window, cx)),
                )
                .into_any_element(),
        )
        .entry(page.fork.clone())
        .entry(page.default_model.clone())
        .entry(page.tools.clone())
        .entry(page.delete.clone())
        .entry(page.back.clone())
    }

    /// Fails with why the agent can't be deleted now.
    fn check_can_delete(&self, agent_id: &str, cx: &App) -> Result<(), &'static str> {
        match self.config.read(cx).state() {
            ConfigFileState::Loaded { config, .. } => config::check_can_delete(config, agent_id),
            ConfigFileState::Loading => Err("OpenCode's config is still loading."),
            ConfigFileState::Unreadable(_) => Err("OpenCode's config couldn't be read."),
            ConfigFileState::Invalid(_) => Err("OpenCode's config file has an error."),
        }
    }

    /// The agent stays listed until OpenCode rereads the file.
    fn delete_agent(&mut self, agent_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.check_can_delete(&agent_id, cx).is_err() {
            return;
        }
        let saved = self.edit_config(
            {
                let agent_id = agent_id.clone();
                move |json| config::delete_agent(json, &agent_id)
            },
            cx,
        );
        self.leave_page_once_saved(
            saved,
            move |page| matches!(page, Page::Agent(page) if page.agent_id == agent_id),
            Self::show_agent_list,
            window,
            cx,
        );
    }

    fn show_agent_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = AgentListPage {
            agents: build_agent_rows(self.project_setup.read(cx), &[], cx),
            all_agents_permissions: NavigableEntry::focusable(cx),
            add_new_agent: NavigableEntry::focusable(cx),
        };
        self.show_page(Page::AgentList(page), window, cx);
    }

    fn show_default_model(
        &mut self,
        agent_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delegate = DefaultModelDelegate {
            modal: cx.weak_entity(),
            agent_id: agent_id.clone(),
            models: self.list_model_choices(cx),
            current_reference: self.find_agent_model(&agent_id, cx),
            filtered: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            let mut picker = Picker::list(delegate, window, cx).embedded();
            picker.refresh(window, cx);
            picker
        });
        let page = DefaultModelPage { agent_id, picker };
        self.show_page(Page::DefaultModel(page), window, cx);
    }

    fn list_model_choices(&self, cx: &App) -> Vec<ModelChoice> {
        let project_setup = self.project_setup.read(cx);
        let mut models: Vec<ModelChoice> = project_setup
            .models
            .iter()
            .map(|info| ModelChoice {
                reference: info.reference(),
                name: info.name.clone().into(),
                provider: project_setup.provider_name(&info.provider_id).into(),
            })
            .collect();
        models.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then_with(|| left.name.cmp(&right.name))
        });
        models
    }

    fn render_new_agent_form(
        &self,
        form: &NewAgentForm,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (title, icon) = match &form.forked_from {
            Some(agent_id) => (
                format!(
                    "Fork {}",
                    self.project_setup.read(cx).format_agent_name(agent_id)
                ),
                IconName::Scissors,
            ),
            None => ("New Agent".to_string(), IconName::Plus),
        };
        v_flex()
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.create_agent_from_form(window, cx)
            }))
            .child(ProfileModalHeader::new(title, Some(icon)))
            .child(ListSeparator)
            .child(
                h_flex()
                    .p_2()
                    // The name field's own Escape goes back one page. Left to
                    // fall through to `menu::Cancel`, it can close the modal.
                    .on_action(
                        cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                            this.go_back(window, cx)
                        }),
                    )
                    .child(form.name_editor.clone()),
            )
    }

    fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.page {
            Page::AgentList(_) => cx.emit(DismissEvent),
            Page::NewAgent(NewAgentForm {
                forked_from: Some(agent_id),
                ..
            })
            | Page::ToolPermissions(ToolPermissionsPage {
                scope: Scope::Agent(agent_id),
                ..
            })
            | Page::DefaultModel(DefaultModelPage { agent_id, .. }) => {
                self.show_agent(agent_id.clone(), window, cx)
            }
            Page::NewAgent(NewAgentForm {
                forked_from: None, ..
            })
            | Page::Agent(_)
            | Page::ToolPermissions(ToolPermissionsPage {
                scope: Scope::AllAgents,
                ..
            }) => self.show_agent_list(window, cx),
        }
    }

    fn render_picker_page(
        &self,
        title: String,
        icon: IconName,
        picker: AnyView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = NavigableEntry {
            focus_handle: self.focus_handle.clone(),
            scroll_anchor: None,
        };
        v_flex()
            .pb_1()
            .child(ProfileModalHeader::new(title, Some(icon)))
            .child(ListSeparator)
            .child(picker)
            .child(ListSeparator)
            .child(self.render_back_row(&back, window, cx))
            .into_any_element()
    }

    fn render_back_row(
        &self,
        entry: &NavigableEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let key_binding = div().child(
            KeyBinding::for_action_in(&menu::Cancel, &self.focus_handle, cx)
                .size(rems_from_px(12_f32)),
        );
        self.render_row(
            Row::new("go-back", "Go Back")
                .icon(IconName::ArrowLeft, Color::Muted)
                .end_slot(Some(key_binding.into_any_element())),
            entry,
            |this, window, cx| this.go_back(window, cx),
            window,
            cx,
        )
    }

    fn render_row(
        &self,
        row: Row,
        entry: &NavigableEntry,
        activate: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let is_focused = entry.focus_handle.contains_focused(window, cx);
        div()
            .id(row.id.clone())
            .track_focus(&entry.focus_handle)
            .on_action({
                let activate = activate.clone();
                cx.listener(move |this, _: &menu::Confirm, window, cx| activate(this, window, cx))
            })
            .child(
                ListItem::new(row.id)
                    .toggle_state(is_focused)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .when_some(row.icon, |this, (icon, color)| {
                        this.start_slot(Icon::new(icon).size(IconSize::Small).color(color))
                    })
                    .child(Label::new(row.label).color(row.label_color).truncate())
                    .when_some(row.end_slot, |this, end_slot| this.end_slot(end_slot))
                    .on_click(cx.listener(move |this, _, window, cx| activate(this, window, cx))),
            )
    }

    fn render_footer(&self, cx: &App) -> impl IntoElement {
        let (location, load_error) = match self.config.read(cx).state() {
            ConfigFileState::Loaded { file, .. } => (Some(file.describe_saved_location()), None),
            ConfigFileState::Unreadable(error) => (None, Some(error.clone())),
            ConfigFileState::Invalid(error) => (
                None,
                Some(
                    format!(
                        "OpenCode's config file has an error, so your custom agents and permission rules aren't loaded: {error}"
                    )
                    .into(),
                ),
            ),
            ConfigFileState::Loading => (None, None),
        };
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
            .children(
                load_error
                    .into_iter()
                    .chain(self.error.clone())
                    .map(|error| {
                        h_flex()
                            .items_start()
                            .gap_1()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            )
                            .child(div().flex_1().min_w_0().child(
                                Label::new(error).size(LabelSize::Small).color(Color::Error),
                            ))
                    }),
            )
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .pt_2()
    }
}

impl ModalView for AgentsModal {}

impl EventEmitter<DismissEvent> for AgentsModal {}

impl Focusable for AgentsModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.page {
            Page::NewAgent(form) => form.name_editor.focus_handle(cx),
            Page::ToolPermissions(page) => page.picker.focus_handle(cx),
            Page::DefaultModel(page) => page.picker.focus_handle(cx),
            Page::AgentList(_) | Page::Agent(_) => self.focus_handle.clone(),
        }
    }
}

enum Page {
    AgentList(AgentListPage),
    Agent(AgentPage),
    NewAgent(NewAgentForm),
    ToolPermissions(ToolPermissionsPage),
    DefaultModel(DefaultModelPage),
}

struct AgentListPage {
    agents: Vec<AgentRow>,
    all_agents_permissions: NavigableEntry,
    add_new_agent: NavigableEntry,
}

struct AgentRow {
    id: String,
    navigation: NavigableEntry,
}

struct AgentPage {
    agent_id: String,
    fork: NavigableEntry,
    default_model: NavigableEntry,
    tools: NavigableEntry,
    delete: NavigableEntry,
    back: NavigableEntry,
}

struct NewAgentForm {
    name_editor: Entity<Editor>,
    forked_from: Option<String>,
}

struct ToolPermissionsPage {
    scope: Scope,
    picker: Entity<Picker<ToolPermissionsDelegate>>,
}

struct DefaultModelPage {
    agent_id: String,
    picker: Entity<Picker<DefaultModelDelegate>>,
}

/// Rows for the agents a thread can switch to. Agents `previous` had keep
/// their focus handles, so a reload doesn't move the focus.
fn build_agent_rows(
    project_setup: &ProjectSetup,
    previous: &[AgentRow],
    cx: &App,
) -> Vec<AgentRow> {
    project_setup
        .list_selectable_agents()
        .into_iter()
        .map(|agent| AgentRow {
            id: agent.id.clone(),
            navigation: previous.iter().find(|row| row.id == agent.id).map_or_else(
                || NavigableEntry::focusable(cx),
                |row| row.navigation.clone(),
            ),
        })
        .collect()
}

/// One row of a list page.
struct Row {
    id: SharedString,
    label: SharedString,
    icon: Option<(IconName, Color)>,
    label_color: Color,
    end_slot: Option<AnyElement>,
}

impl Row {
    fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            label_color: Color::Default,
            end_slot: None,
        }
    }

    fn icon(self, icon: IconName, color: Color) -> Self {
        Self {
            icon: Some((icon, color)),
            ..self
        }
    }

    fn label_color(self, label_color: Color) -> Self {
        Self {
            label_color,
            ..self
        }
    }

    fn end_slot(self, end_slot: Option<AnyElement>) -> Self {
        Self { end_slot, ..self }
    }
}

struct ToolRow {
    tool: &'static Tool,
    permission: Option<PermissionEffect>,
    has_resource_rules: bool,
}

struct ToolPermissionsDelegate {
    modal: WeakEntity<AgentsModal>,
    scope: Scope,
    rows: Vec<ToolRow>,
    filtered: Vec<usize>,
    selected_index: usize,
}

impl PickerDelegate for ToolPermissionsDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "opencode tool permissions"
    }

    fn match_count(&self) -> usize {
        self.filtered.len()
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn selected_index(&self) -> usize {
        self.selected_index
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
                matches_query(row.tool.label, &query) || matches_query(row.tool.action, &query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.filtered.len().saturating_sub(1));
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(row) = self
            .filtered
            .get(self.selected_index)
            .and_then(|index| self.rows.get_mut(*index))
        else {
            return;
        };
        let permission = cycle_permission(&self.scope, row.permission);
        // Shown at once; the reload after the write confirms it.
        row.permission = permission;
        let tool = row.tool;
        let scope = self.scope.clone();
        self.modal
            .update(cx, |modal, cx| {
                let pending_id = if let Scope::Agent(agent_id) = &scope {
                    let id = modal.next_pending_permission_id;
                    modal.next_pending_permission_id += 1;
                    modal.pending_permissions.retain(|pending| {
                        pending.agent_id != *agent_id || pending.action != tool.action
                    });
                    modal.pending_permissions.push(PendingPermission {
                        id,
                        agent_id: agent_id.clone(),
                        action: tool.action,
                        effect: permission,
                    });
                    Some(id)
                } else {
                    None
                };
                let saved = modal.edit_config(
                    move |json| config::set_permission(json, &scope, tool, permission),
                    cx,
                );
                cx.spawn(async move |modal, cx| {
                    if !saved.await
                        && let Some(id) = pending_id
                    {
                        modal
                            .update(cx, |modal, cx| {
                                modal.pending_permissions.retain(|pending| pending.id != id);
                                modal.refresh_page(cx);
                            })
                            .log_err();
                    }
                })
                .detach();
            })
            .log_err();
        cx.notify();
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        go_back_from_picker(&self.modal, window, cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = self.rows.get(*self.filtered.get(ix)?)?;
        let (label, color) = match row.permission {
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
                        .when(row.has_resource_rules, |this| {
                            this.child(
                                Label::new("· some differ")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
                .end_slot(Label::new(label).size(LabelSize::Small).color(color))
                .tooltip(Tooltip::text(if row.has_resource_rules {
                    "Click to change. Rules for particular files, commands or URLs stay as they are."
                } else {
                    "Click to change."
                }))
                .into_any_element(),
        )
    }
}

fn matches_query(label: &str, query: &str) -> bool {
    label.to_lowercase().contains(&query.trim().to_lowercase())
}

/// Allow, then ask, then deny. Every agent's rule can also be removed,
/// leaving OpenCode's own default.
fn cycle_permission(
    scope: &Scope,
    permission: Option<PermissionEffect>,
) -> Option<PermissionEffect> {
    match (scope, permission) {
        (_, Some(PermissionEffect::Allow)) => Some(PermissionEffect::Ask),
        (_, Some(PermissionEffect::Ask)) => Some(PermissionEffect::Deny),
        (Scope::AllAgents, Some(PermissionEffect::Deny)) => None,
        (_, Some(PermissionEffect::Deny) | None) => Some(PermissionEffect::Allow),
    }
}

/// Escape in a picker goes back a page. Deferred, since that drops the
/// picker.
fn go_back_from_picker<D: PickerDelegate>(
    modal: &WeakEntity<AgentsModal>,
    window: &mut Window,
    cx: &mut Context<Picker<D>>,
) {
    let modal = modal.clone();
    window.defer(cx, move |window, cx| {
        modal
            .update(cx, |modal, cx| modal.go_back(window, cx))
            .log_err();
    });
}

struct ModelChoice {
    /// `provider/model`, as the config file names it.
    reference: String,
    name: SharedString,
    provider: SharedString,
}

enum ModelRow {
    /// No model of the agent's own: it uses the thread's.
    ThreadModel,
    Model {
        /// Into the delegate's models.
        index: usize,
    },
}

struct DefaultModelDelegate {
    modal: WeakEntity<AgentsModal>,
    agent_id: String,
    models: Vec<ModelChoice>,
    current_reference: Option<String>,
    filtered: Vec<ModelRow>,
    selected_index: usize,
}

impl DefaultModelDelegate {
    fn reference(&self, row: &ModelRow) -> Option<&str> {
        match row {
            ModelRow::ThreadModel => None,
            ModelRow::Model { index } => Some(self.models.get(*index)?.reference.as_str()),
        }
    }
}

impl PickerDelegate for DefaultModelDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "opencode agent model"
    }

    fn match_count(&self) -> usize {
        self.filtered.len()
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn selected_index(&self) -> usize {
        self.selected_index
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
        let models = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                matches_query(&model.name, &query)
                    || matches_query(&model.provider, &query)
                    || matches_query(&model.reference, &query)
            })
            .map(|(index, _)| ModelRow::Model { index });
        self.filtered = query
            .trim()
            .is_empty()
            .then_some(ModelRow::ThreadModel)
            .into_iter()
            .chain(models)
            .collect();
        self.selected_index = self
            .filtered
            .iter()
            .position(|row| self.reference(row) == self.current_reference.as_deref())
            .unwrap_or(0);
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(row) = self.filtered.get(self.selected_index) else {
            return;
        };
        let model = self.reference(row).map(str::to_string);
        let agent_id = self.agent_id.clone();
        let modal = self.modal.clone();
        // Deferred, since showing the agent drops this picker.
        window.defer(cx, move |window, cx| {
            modal
                .update(cx, |modal, cx| {
                    modal.show_agent(agent_id.clone(), window, cx);
                    modal
                        .edit_config(
                            move |json| config::set_agent_model(json, &agent_id, model.as_deref()),
                            cx,
                        )
                        .detach();
                })
                .log_err();
        });
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        go_back_from_picker(&self.modal, window, cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = self.filtered.get(ix)?;
        let (label, detail) = match row {
            ModelRow::ThreadModel => (SharedString::from("Thread's Model"), None),
            ModelRow::Model { index } => {
                let model = self.models.get(*index)?;
                (model.name.clone(), Some(model.provider.clone()))
            }
        };
        let is_current = self.reference(row) == self.current_reference.as_deref();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::thread_sync_tests::{Durability, Harness, SESSION, primary_agent};
    use acp_thread::{AcpThread, AgentConnection as _};
    use agent_client_protocol::v2::schema as acp;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn locate_config_file() -> PathBuf {
        config::resolve_global_config_directory().join("opencode.jsonc")
    }

    fn locate_config_directory() -> String {
        config::resolve_global_config_directory()
            .to_string_lossy()
            .into_owned()
    }

    /// Also returns the thread, whose session the modal's project setup comes from.
    async fn open_modal<'a>(
        harness: &Harness,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<AcpThread>,
        Entity<AgentsModal>,
        &'a mut VisualTestContext,
    ) {
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        harness.serve(SESSION, |_| {});
        let thread = harness.open(SESSION, cx).await;
        let source = cx
            .update(|cx| harness.connection.modal_source(SESSION, cx))
            .expect("the thread's session");
        let fs = harness
            .project
            .read_with(cx, |project, _| project.fs().clone());
        let (modal, cx) = cx.add_window_view(|_, cx| AgentsModal::new(source, fs, cx));
        cx.run_until_parked();
        (thread, modal, cx)
    }

    fn find_agent_focus(modal: &AgentsModal, agent_id: &str) -> FocusHandle {
        let Page::AgentList(page) = &modal.page else {
            panic!("not the agent list");
        };
        let row = page.agents.iter().find(|row| row.id == agent_id);
        row.expect("a listed agent").navigation.focus_handle.clone()
    }

    fn listed_agents(modal: &AgentsModal) -> Vec<&str> {
        let Page::AgentList(page) = &modal.page else {
            panic!("not the agent list");
        };
        page.agents.iter().map(|row| row.id.as_str()).collect()
    }

    #[gpui::test]
    async fn a_reload_keeps_the_focus_on_the_agent_list(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness
            .service
            .lock()
            .expect("fake service lock")
            .agents
            .push(primary_agent("plan"));
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        modal.update_in(cx, |modal, window, cx| {
            assert_eq!(listed_agents(modal), ["build", "plan"]);
            find_agent_focus(modal, "plan").focus(window, cx);
        });

        harness
            .service
            .lock()
            .expect("fake service lock")
            .agents
            .push(primary_agent("review"));
        harness.send("agent.updated", json!({}), Durability::Live, cx);
        cx.run_until_parked();

        modal.update_in(cx, |modal, window, _| {
            assert_eq!(listed_agents(modal), ["build", "plan", "review"]);
            // The row shown now, not just the handle focused before.
            assert!(find_agent_focus(modal, "plan").is_focused(window));
        });
    }

    #[gpui::test]
    async fn escape_in_the_name_field_goes_back(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        cx.update(|_, cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("escape", editor::actions::Cancel, Some("Editor")),
                gpui::KeyBinding::new("escape", menu::Cancel, None),
            ])
        });
        let dismissed = Rc::new(std::cell::Cell::new(false));
        let _subscription = cx.update(|_, cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&modal, move |_, _: &DismissEvent, _| dismissed.set(true))
        });
        modal.update_in(cx, |modal, window, cx| {
            modal.show_new_agent_form(None, window, cx);
            let Page::NewAgent(form) = &modal.page else {
                panic!("not the new agent form");
            };
            form.name_editor.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!dismissed.get(), "the modal closed");
        modal.read_with(cx, |modal, _| {
            assert!(matches!(modal.page, Page::AgentList(_)));
        });
    }

    #[gpui::test]
    async fn going_back_focuses_the_entry_that_opened_the_page(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        let picker = modal.update_in(cx, |modal, window, cx| {
            modal.show_agent("build".into(), window, cx);
            modal.show_tool_permissions(Scope::Agent("build".into()), window, cx);
            let Page::ToolPermissions(page) = &modal.page else {
                panic!("not the tool permissions");
            };
            page.picker.clone()
        });
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.dismissed(window, cx)
        });
        drop(picker);
        cx.run_until_parked();

        modal.update_in(cx, |modal, window, cx| {
            let Page::Agent(page) = &modal.page else {
                panic!("not the agent page");
            };
            assert!(page.tools.focus_handle.is_focused(window));
            modal.go_back(window, cx);
            assert!(find_agent_focus(modal, "build").is_focused(window));

            modal.show_new_agent_form(None, window, cx);
            modal.go_back(window, cx);
            let Page::AgentList(page) = &modal.page else {
                panic!("not the agent list");
            };
            assert!(page.add_new_agent.focus_handle.is_focused(window));

            modal.show_tool_permissions(Scope::AllAgents, window, cx);
            modal.go_back(window, cx);
            let Page::AgentList(page) = &modal.page else {
                panic!("not the agent list");
            };
            assert!(page.all_agents_permissions.focus_handle.is_focused(window));

            modal.show_agent("build".into(), window, cx);
            modal.show_new_agent_form(Some("build".into()), window, cx);
            modal.go_back(window, cx);
            let Page::Agent(page) = &modal.page else {
                panic!("not the agent page");
            };
            assert!(page.fork.focus_handle.is_focused(window));

            modal.show_default_model("build".into(), window, cx);
            modal.go_back(window, cx);
            let Page::Agent(page) = &modal.page else {
                panic!("not the agent page");
            };
            assert!(page.default_model.focus_handle.is_focused(window));
        });
    }

    #[gpui::test]
    async fn agent_permission_survives_stale_project_setup(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.config_directory = Some(locate_config_directory());
            service.agents.push(primary_agent("review"));
        }
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        let picker = modal.update_in(cx, |modal, window, cx| {
            modal.show_tool_permissions(Scope::Agent("review".into()), window, cx);
            let Page::ToolPermissions(page) = &modal.page else {
                panic!("not the tool permissions");
            };
            page.picker.clone()
        });
        for expected in [PermissionEffect::Allow, PermissionEffect::Ask] {
            picker.update_in(cx, |picker, window, cx| {
                picker.delegate.confirm(false, window, cx);
            });
            cx.run_until_parked();
            picker.read_with(cx, |picker, _| {
                assert_eq!(picker.delegate.rows[0].permission, Some(expected));
            });
        }
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.agents[0]["permissions"] =
                json!([{ "action": "*", "resource": "*", "effect": "ask" }]);
        }
        harness.send("agent.updated", json!({}), Durability::Live, cx);
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.delegate.rows[0].permission,
                Some(PermissionEffect::Ask)
            );
        });
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.agents[0]["permissions"] =
                json!([{ "action": "*", "resource": "*", "effect": "deny" }]);
        }
        harness.send("agent.updated", json!({}), Durability::Live, cx);
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.delegate.rows[0].permission,
                Some(PermissionEffect::Deny)
            );
        });
    }

    #[gpui::test]
    async fn all_agent_permission_is_saved(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness
            .service
            .lock()
            .expect("fake service lock")
            .config_directory = Some(locate_config_directory());
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        let picker = modal.update_in(cx, |modal, window, cx| {
            modal.show_tool_permissions(Scope::AllAgents, window, cx);
            let Page::ToolPermissions(page) = &modal.page else {
                panic!("not the tool permissions");
            };
            page.picker.clone()
        });
        // The first row is the rule for every tool.
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx)
        });
        cx.run_until_parked();

        let fs = harness
            .project
            .read_with(cx, |project, _| project.fs().clone());
        let text = fs.load(&locate_config_file()).await.expect("saved");
        assert_eq!(
            config::parse(&text).expect("valid")["permissions"],
            json!([{ "action": "*", "resource": "*", "effect": "allow" }])
        );
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.delegate.rows[0].permission,
                Some(PermissionEffect::Allow)
            );
        });
        modal.read_with(cx, |modal, _| assert_eq!(modal.error, None));
    }

    #[gpui::test]
    async fn an_all_agents_permission_cycles_back_to_not_set(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness
            .service
            .lock()
            .expect("fake service lock")
            .config_directory = Some(locate_config_directory());
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        let picker = modal.update_in(cx, |modal, window, cx| {
            modal.show_tool_permissions(Scope::AllAgents, window, cx);
            let Page::ToolPermissions(page) = &modal.page else {
                panic!("not the tool permissions");
            };
            page.picker.clone()
        });
        for effect in [
            Some(PermissionEffect::Allow),
            Some(PermissionEffect::Ask),
            Some(PermissionEffect::Deny),
            None,
        ] {
            picker.update_in(cx, |picker, window, cx| {
                picker.delegate.confirm(false, window, cx)
            });
            cx.run_until_parked();
            picker.read_with(cx, |picker, _| {
                assert_eq!(picker.delegate.rows[0].permission, effect)
            });
        }
        let fs = harness
            .project
            .read_with(cx, |project, _| project.fs().clone());
        let text = fs.load(&locate_config_file()).await.expect("saved");
        assert!(
            config::parse(&text)
                .expect("valid")
                .get("permissions")
                .is_none()
        );
    }

    #[test]
    fn only_the_all_agents_cycle_includes_not_set() {
        use PermissionEffect::*;
        let mut effect = None;
        for expected in [Some(Allow), Some(Ask), Some(Deny), None, Some(Allow)] {
            effect = cycle_permission(&Scope::AllAgents, effect);
            assert_eq!(effect, expected);
        }
        let mut effect = None;
        for expected in [Allow, Ask, Deny, Allow] {
            effect = cycle_permission(&Scope::Agent("build".into()), effect);
            assert_eq!(effect, Some(expected));
        }
    }

    #[gpui::test]
    async fn enter_in_the_name_field_submits_the_form(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        cx.update(|_, cx| cx.bind_keys([gpui::KeyBinding::new("enter", menu::Confirm, None)]));
        modal.update_in(cx, |modal, window, cx| {
            modal.show_new_agent_form(None, window, cx);
            let Page::NewAgent(form) = &modal.page else {
                panic!("not the new agent form");
            };
            form.name_editor.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.error.as_deref(), Some("Give the agent a name."));
        });
    }

    #[gpui::test]
    async fn failed_agent_changes_keep_page_open(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness
            .service
            .lock()
            .expect("fake service lock")
            .config_directory = Some(locate_config_directory());
        let fs = harness
            .project
            .read_with(cx, |project, _| project.fs().clone());
        fs.write(&locate_config_file(), br#"{ "agents": { "review": {} } }"#)
            .await
            .expect("written");
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        // Broken after the modal read it, as when the file is edited by hand.
        fs.write(&locate_config_file(), b"{ \"agents\": ")
            .await
            .expect("written");

        modal.update_in(cx, |modal, window, cx| {
            modal.show_agent("review".into(), window, cx);
            modal.delete_agent("review".into(), window, cx);
        });
        cx.run_until_parked();
        modal.read_with(cx, |modal, _| {
            assert!(matches!(&modal.page, Page::Agent(page) if page.agent_id == "review"));
            assert!(modal.error.is_some());
        });

        let create = |cx: &mut VisualTestContext| {
            modal.update_in(cx, |modal, window, cx| {
                modal.show_new_agent_form(None, window, cx);
                let Page::NewAgent(form) = &modal.page else {
                    panic!("not the new agent form");
                };
                form.name_editor
                    .update(cx, |editor, cx| editor.set_text("tester", window, cx));
                modal.create_agent_from_form(window, cx);
            });
            cx.run_until_parked();
        };
        create(cx);
        modal.read_with(cx, |modal, _| {
            assert!(matches!(modal.page, Page::NewAgent(_)));
            assert!(modal.error.is_some());
        });

        fs.write(&locate_config_file(), b"{}")
            .await
            .expect("written");
        // OpenCode reloads its config when the file changes.
        harness.connection.client().notify_config_reloaded();
        cx.run_until_parked();
        create(cx);
        modal.read_with(cx, |modal, _| {
            assert!(matches!(&modal.page, Page::Agent(page) if page.agent_id == "tester"));
            assert_eq!(modal.error, None);
        });
    }

    #[gpui::test]
    async fn project_setup_change_reloads_picker(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.config_directory = Some(locate_config_directory());
            service.models = vec![json!({ "providerID": "test", "id": "other", "name": "Other" })];
        }
        let fs = harness
            .project
            .read_with(cx, |project, _| project.fs().clone());
        let with_effect = |effect: &str| {
            format!(
                r#"{{"permissions": [{{"action": "*", "resource": "*", "effect": "{effect}"}}]}}"#
            )
        };
        fs.write(&locate_config_file(), with_effect("deny").as_bytes())
            .await
            .expect("written");
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        let shown_effect = |cx: &mut VisualTestContext| {
            modal.read_with(cx, |modal, cx| {
                let config = loaded_config(modal, cx).expect("the config is loaded");
                config["permissions"][0]["effect"]
                    .as_str()
                    .map(str::to_string)
            })
        };
        assert_eq!(shown_effect(cx).as_deref(), Some("deny"));

        fs.write(&locate_config_file(), with_effect("allow").as_bytes())
            .await
            .expect("written");
        let selector = harness
            .connection
            .model_selector(&acp::SessionId::new(SESSION))
            .expect("model picker");
        cx.update(|_, cx| selector.select_model(acp_thread::AgentModelId::new("test/other"), cx))
            .await
            .expect("model pick");
        cx.run_until_parked();
        assert_eq!(shown_effect(cx).as_deref(), Some("deny"));

        harness.send("agent.updated", json!({}), Durability::Live, cx);
        cx.run_until_parked();
        assert_eq!(shown_effect(cx).as_deref(), Some("allow"));
    }

    fn loaded_config(modal: &AgentsModal, cx: &App) -> Option<Value> {
        match modal.config.read(cx).state() {
            ConfigFileState::Loaded { config, .. } => Some(config.clone()),
            _ => None,
        }
    }

    #[gpui::test]
    async fn a_broken_config_is_shown_until_a_project_setup_reload_finds_it_fixed(
        cx: &mut TestAppContext,
    ) {
        let harness = Harness::new(cx).await;
        {
            let mut service = harness.service.lock().expect("fake service lock");
            service.config_directory = Some(locate_config_directory());
            service.agents.push(primary_agent("review"));
        }
        let fs = harness
            .project
            .read_with(cx, |project, _| project.fs().clone());
        let valid = br#"{
            "agents": {"review": {"mode": "primary"}},
            "permissions": [{"action": "*", "resource": "*", "effect": "deny"}]
        }"#;
        fs.write(&locate_config_file(), valid)
            .await
            .expect("written");
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        modal.read_with(cx, |modal, cx| {
            assert!(matches!(
                modal.config.read(cx).state(),
                ConfigFileState::Loaded { .. }
            ));
            assert_eq!(listed_agents(modal), ["build", "review"]);
        });

        let broken = "{ \"agents\": ";
        fs.write(&locate_config_file(), broken.as_bytes())
            .await
            .expect("written");
        harness
            .service
            .lock()
            .expect("fake service lock")
            .agents
            .retain(|agent| agent.get("id").and_then(Value::as_str) != Some("review"));
        harness.send("agent.updated", json!({}), Durability::Live, cx);
        cx.run_until_parked();
        let picker = modal.update_in(cx, |modal, window, cx| {
            let ConfigFileState::Invalid(error) = modal.config.read(cx).state() else {
                panic!("the broken config wasn't reported");
            };
            assert!(error.contains("reading OpenCode's config"));
            assert_eq!(listed_agents(modal), ["build"]);
            assert!(modal.check_can_delete("review", cx).is_err());
            modal.show_tool_permissions(Scope::AllAgents, window, cx);
            let Page::ToolPermissions(page) = &modal.page else {
                panic!("not the tool permissions");
            };
            page.picker.clone()
        });
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx)
        });
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.rows[0].permission, None)
        });
        assert_eq!(
            fs.load(&locate_config_file()).await.expect("unchanged"),
            broken
        );

        fs.write(&locate_config_file(), valid).await.expect("fixed");
        harness
            .service
            .lock()
            .expect("fake service lock")
            .agents
            .push(primary_agent("review"));
        harness.send("agent.updated", json!({}), Durability::Live, cx);
        cx.run_until_parked();
        modal.update_in(cx, |modal, window, cx| {
            assert!(matches!(
                modal.config.read(cx).state(),
                ConfigFileState::Loaded { .. }
            ));
            assert_eq!(modal.error, None);
            assert_eq!(modal.check_can_delete("review", cx), Ok(()));
            modal.go_back(window, cx);
            assert_eq!(listed_agents(modal), ["build", "review"]);
        });
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.delegate.rows[0].permission,
                Some(PermissionEffect::Deny)
            );
        });
        modal.update(cx, |modal, cx| {
            modal
                .edit_config(
                    |json| {
                        config::set_permission(
                            json,
                            &Scope::AllAgents,
                            &ALL_TOOLS,
                            Some(PermissionEffect::Ask),
                        )
                    },
                    cx,
                )
                .detach();
        });
        cx.run_until_parked();
        let text = fs.load(&locate_config_file()).await.expect("saved");
        assert_eq!(
            config::parse(&text).expect("valid")["permissions"][0]["effect"],
            json!("ask")
        );
    }

    #[gpui::test]
    async fn broken_config_file_is_preserved(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness
            .service
            .lock()
            .expect("fake service lock")
            .config_directory = Some(locate_config_directory());
        let fs = harness
            .project
            .read_with(cx, |project, _| project.fs().clone());
        fs.write(&locate_config_file(), b"{ \"agents\": ")
            .await
            .expect("written");
        let (_thread, modal, cx) = open_modal(&harness, cx).await;
        modal.update_in(cx, |modal, _, cx| {
            assert!(matches!(
                modal.config.read(cx).state(),
                ConfigFileState::Invalid(_)
            ));
            assert!(modal.check_can_delete("review", cx).is_err());
            modal
                .edit_config(
                    |json| {
                        config::set_permission(
                            json,
                            &Scope::AllAgents,
                            &ALL_TOOLS,
                            Some(PermissionEffect::Ask),
                        )
                    },
                    cx,
                )
                .detach();
            assert!(modal.error.is_some());
        });
        cx.run_until_parked();
        let text = fs.load(&locate_config_file()).await.expect("still there");
        assert_eq!(text, "{ \"agents\": ");
    }
}
