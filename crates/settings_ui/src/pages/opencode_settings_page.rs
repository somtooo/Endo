//! OpenCode D's page, reached from the External Agents page: the service
//! settings that matter in a GUI. Its Providers and Plugins sub-pages live in
//! their own modules.

use std::rc::Rc;

use anyhow::Result;
use editor::Editor;
use fs::Fs;
use gpui::{App, Entity, ScrollHandle, Subscription, Task, prelude::*};
use opencode_agent::opencode_config::{self, ConfigFileState, LoadedConfig};
use opencode_agent::{Integration, OpenCodeClient, PluginInfo, ProjectSetup};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use ui::{
    ContextMenu, Divider, DropdownMenu, DropdownStyle, Switch, ToggleState, Tooltip, prelude::*,
    utils::format_number_with_commas,
};
use util::ResultExt as _;

use super::opencode_plugins_page::open_plugins_page;
use super::opencode_providers_page::{
    ConnectForm, OneFieldFormState, new_provider_filter, open_providers_page,
};
use crate::{SettingsPageItem, SettingsWindow, components::SettingsSectionHeader};

const SECTION: &str = "External Agents";
const PAGE_TITLE: &str = "OpenCode D";

/// What the OpenCode D pages show, kept while the settings window is open.
pub(crate) struct OpenCodeSettings {
    pub(super) client: Option<OpenCodeClient>,
    /// The directory service requests are made for. Connections and the
    /// global config don't depend on it, but every request names one.
    pub(super) directory: String,
    /// The global config file, shared with the agents modal.
    config: Option<Entity<LoadedConfig>>,
    /// An edit not saved yet, shown over `config` so it shows at once.
    pending_edit: Option<PendingEdit>,
    next_edit: u64,
    /// The directory's models and agents, shared with its threads.
    project_setup: Option<Entity<ProjectSetup>>,
    _observations: Vec<Subscription>,
    pub(super) loading: bool,
    pub(super) errors: PageErrors,
    pub(super) integrations: Vec<Integration>,
    pub(super) plugins: Vec<PluginInfo>,
    pub(super) provider_filter: Entity<Editor>,
    _provider_filter_subscription: Subscription,
    pub(super) connect_form: Option<ConnectForm>,
    pub(super) one_field_form: Option<OneFieldFormState>,
    warming_before_off: Option<WarmingOptions>,
    _load: Option<Task<()>>,
    /// Reloads after a plugin change, while the service installs it.
    pub(super) plugin_reloads: Option<Task<()>>,
}

struct PendingEdit {
    id: u64,
    config: Value,
}

/// Each page shows only its own errors.
#[derive(Default)]
pub(super) struct PageErrors {
    pub(super) main: Option<SharedString>,
    pub(super) providers: Option<SharedString>,
    pub(super) plugins: Option<SharedString>,
}

#[derive(Clone, Copy)]
pub(super) enum Page {
    Main,
    Plugins,
}

impl PageErrors {
    fn of(&mut self, page: Page) -> &mut Option<SharedString> {
        match page {
            Page::Main => &mut self.main,
            Page::Plugins => &mut self.plugins,
        }
    }
}

impl OpenCodeSettings {
    /// The config as the pages show it: an edit shows before it's saved.
    /// `None` until a file that parses is loaded, since only that one can be
    /// changed.
    pub(super) fn config_values<'a>(&'a self, cx: &'a App) -> Option<&'a Value> {
        if let Some(pending) = &self.pending_edit {
            return Some(&pending.config);
        }
        match self.config.as_ref()?.read(cx).state() {
            ConfigFileState::Loaded { config, .. } => Some(config),
            _ => None,
        }
    }

    /// Why the config file can't be shown or changed.
    pub(super) fn describe_config_error(&self, cx: &App) -> Option<SharedString> {
        match self.config.as_ref()?.read(cx).state() {
            ConfigFileState::Unreadable(error) => Some(error.clone()),
            ConfigFileState::Invalid(error) => Some(
                format!(
                    "OpenCode's config file has an error, so these settings can't be changed: {error}"
                )
                .into(),
            ),
            ConfigFileState::Loading | ConfigFileState::Loaded { .. } => None,
        }
    }

    fn project_setup<'a>(&self, cx: &'a App) -> Option<&'a ProjectSetup> {
        Some(self.project_setup.as_ref()?.read(cx))
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct WarmingOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    interval: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

/// One entry in a settings dropdown.
pub(super) enum Choice {
    Header(SharedString),
    Entry {
        label: SharedString,
        pick: Rc<dyn Fn(&mut SettingsWindow, &mut Window, &mut Context<SettingsWindow>)>,
    },
}

/// A value at one path in the config, and how the page changes it.
struct Setting {
    title: &'static str,
    description: &'static str,
    path: &'static [&'static str],
    control: Control,
}

enum Control {
    Switch { default: bool },
    Choices(ChoiceList),
    Model { unset_label: &'static str },
    Agent,
}

struct ChoiceList {
    unset_label: SharedString,
    /// A choice of `None` removes the value, leaving OpenCode's default.
    choices: Vec<(SharedString, Option<Value>)>,
    /// Labels a stored value no choice has.
    format_other: fn(&Value) -> SharedString,
}

impl ChoiceList {
    fn find_label(&self, stored: Option<&Value>) -> SharedString {
        let Some(stored) = stored else {
            return self.unset_label.clone();
        };
        self.choices
            .iter()
            .find(|(_, value)| value.as_ref() == Some(stored))
            .map_or_else(|| (self.format_other)(stored), |(label, _)| label.clone())
    }
}

/// OpenCode D's row on the External Agents page. It isn't an ACP agent, so it
/// has no entry in the list of agent servers below it.
pub(crate) fn render_opencode_row(cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    h_flex()
        .w_full()
        .mt_2()
        .py_1()
        .pl_1()
        .gap_2()
        .justify_between()
        .child(
            h_flex()
                .gap_2()
                .child(
                    Icon::new(IconName::AiOpenCode)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(PAGE_TITLE)),
        )
        .child(
            IconButton::new("configure-opencode-d", IconName::Settings)
                .icon_color(Color::Muted)
                .icon_size(IconSize::Small)
                .size(ButtonSize::Medium)
                .tab_index(0isize)
                .tooltip(Tooltip::text("Configure OpenCode D"))
                .on_click(cx.listener(|this, _event, window, cx| {
                    open_opencode_settings(this, window, cx);
                })),
        )
}

/// Whether OpenCode D's page is the one showing.
pub(super) fn is_showing_opencode_settings(settings_window: &SettingsWindow) -> bool {
    settings_window.opencode_settings.is_some()
        && settings_window
            .sub_page_stack
            .last()
            .is_some_and(|sub_page| sub_page.link.title.as_ref() == PAGE_TITLE)
}

/// Opens OpenCode D's page with its parents behind it, so the back button
/// walks up the way it would have from clicking through.
pub(super) fn open_opencode_settings_from_anywhere(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let ai_page_entry = settings_window.navbar_entries.iter().position(|entry| {
        entry.is_root
            && settings_window.pages[entry.page_index]
                .items
                .iter()
                .any(|item| match item {
                    SettingsPageItem::SubPageLink(link) => link.json_path == Some("agent_servers"),
                    _ => false,
                })
    });
    if let Some(ai_page_entry) = ai_page_entry {
        settings_window.open_and_scroll_to_navbar_entry(ai_page_entry, None, false, window, cx);
    }
    settings_window.navigate_to_sub_page("agent_servers", window, cx);
    open_opencode_settings(settings_window, window, cx);
}

pub(crate) fn open_opencode_settings(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    // The pages edit global settings, so their model and agent menus list
    // the most global choices: a project's own agents or providers would
    // break as a default in every other project. OpenCode's API has no
    // "global only" list, so the home folder stands in for it. That's wrong
    // only when the home folder has its own OpenCode project config.
    let directory = paths::home_dir().to_string_lossy().to_string();
    let (provider_filter, subscription) = new_provider_filter(window, cx);
    settings_window.opencode_settings = Some(OpenCodeSettings {
        client: None,
        directory,
        config: None,
        pending_edit: None,
        next_edit: 0,
        project_setup: None,
        _observations: Vec::new(),
        loading: true,
        errors: PageErrors::default(),
        integrations: Vec::new(),
        plugins: Vec::new(),
        provider_filter,
        _provider_filter_subscription: subscription,
        connect_form: None,
        one_field_form: None,
        warming_before_off: None,
        _load: None,
        plugin_reloads: None,
    });
    reload(settings_window, cx);
    settings_window.push_dynamic_sub_page(
        PAGE_TITLE,
        SECTION,
        None,
        false,
        render_opencode_settings_page,
        window,
        cx,
    );
}

/// Fetches what the pages show, the folder's models and agents included:
/// providers connected elsewhere aren't announced. Each part that can't load
/// is reported on its page, and the others still appear.
pub(super) fn reload(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    if let Some(settings) = settings_window.opencode_settings.as_ref()
        && settings.project_setup.is_some()
    {
        opencode_agent::refresh_project_setup(settings.directory.clone(), cx);
    }
    fetch_service_state(settings_window, cx);
}

/// The config file and the project setup keep themselves current, so a
/// config change refetches only the rest.
fn fetch_service_state(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let directory = settings.directory.clone();
    let fs = <dyn Fs>::global(cx);
    let client = match settings.client.clone() {
        Some(client) => Task::ready(Ok(client)),
        None => opencode_agent::service_client(cx),
    };
    let project_setup = settings
        .project_setup
        .is_none()
        .then(|| opencode_agent::load_project_setup(directory.clone(), cx));
    settings._load = Some(cx.spawn(async move |this, cx| {
        let client = match client.await {
            Ok(client) => client,
            Err(error) => {
                this.update(cx, |this, cx| {
                    if let Some(settings) = this.opencode_settings.as_mut() {
                        settings.loading = false;
                        let error: SharedString = opencode_agent::describe_error(
                            &error.context("Couldn't reach the OpenCode service"),
                        )
                        .into();
                        settings.errors = PageErrors {
                            main: Some(error.clone()),
                            providers: Some(error.clone()),
                            plugins: Some(error),
                        };
                        cx.notify();
                    }
                })
                .log_err();
                return;
            }
        };
        this.update(cx, |this, cx| {
            let Some(settings) = this.opencode_settings.as_mut() else {
                return;
            };
            if settings.config.is_none() {
                let config = LoadedConfig::shared(client.clone(), directory.clone(), fs, cx);
                // A config change can change which plugins run.
                settings
                    ._observations
                    .push(cx.observe(&config, |this, _, cx| {
                        fetch_service_state(this, cx);
                        cx.notify();
                    }));
                settings.config = Some(config);
            }
        })
        .log_err();
        let project_setup = async {
            match project_setup {
                Some(project_setup) => Some(project_setup.await),
                None => None,
            }
        };
        let (integrations, plugins, project_setup) = futures::join!(
            client.list_integrations(&directory),
            client.list_plugins(&directory),
            project_setup,
        );
        this.update(cx, |this, cx| {
            let Some(settings) = this.opencode_settings.as_mut() else {
                return;
            };
            settings.client = Some(client);
            settings.loading = false;
            // An error a write left stays unless this load replaces it.
            match project_setup {
                Some(Ok(project_setup)) => {
                    settings
                        ._observations
                        .push(cx.observe(&project_setup, |_, _, cx| cx.notify()));
                    settings.project_setup = Some(project_setup);
                    settings.errors.main = None;
                }
                Some(Err(error)) => {
                    settings.errors.main = Some(opencode_agent::describe_error(&error).into())
                }
                None => {}
            }
            settings.errors.providers = match integrations {
                Ok(integrations) => {
                    settings.integrations = integrations;
                    None
                }
                Err(error) => Some(opencode_agent::describe_error(&error).into()),
            };
            settings.errors.plugins = match plugins {
                Ok(plugins) => {
                    settings.plugins = plugins;
                    None
                }
                Err(error) => Some(opencode_agent::describe_error(&error).into()),
            };
            cx.notify();
        })
        .log_err();
    }));
}

const WARMING: &[&str] = &["warming"];

fn list_compaction_settings() -> Vec<Setting> {
    const KEEP_PRESETS: &[u64] = &[5_000, 10_000, 15_000, 20_000, 30_000, 50_000];
    const BUFFER_PRESETS: &[&str] = &["5%", "10%", "15%", "20%", "25%", "30%", "40%", "50%"];
    vec![
        Setting {
            title: "Auto Compact",
            description: "Summarize older conversation when a session nears the model's context limit.",
            path: &["compaction", "auto"],
            control: Control::Switch { default: true },
        },
        Setting {
            title: "Recent Context to Keep",
            description: "How much recent conversation stays word for word beside the summary.",
            path: &["compaction", "keep", "tokens"],
            control: Control::Choices(ChoiceList {
                unset_label: format_tokens(15_000),
                choices: KEEP_PRESETS
                    .iter()
                    .map(|tokens| (format_tokens(*tokens), Some(json!(tokens))))
                    .collect(),
                format_other: |value| {
                    value
                        .as_u64()
                        .map_or_else(|| format!("{value} Tokens").into(), format_tokens)
                },
            }),
        },
        Setting {
            title: "Space to Keep Free",
            description: "Share of the context window kept free. A larger share compacts earlier.",
            path: &["compaction", "buffer"],
            control: Control::Choices(ChoiceList {
                unset_label: "15%".into(),
                choices: BUFFER_PRESETS
                    .iter()
                    .map(|share| ((*share).into(), Some(json!(share))))
                    .collect(),
                format_other: format_value,
            }),
        },
        Setting {
            title: "Summary Model",
            description: "The model that writes the summary.",
            path: &["agents", "compaction", "model"],
            control: Control::Model {
                unset_label: "Session's Model",
            },
        },
    ]
}

/// Shown while warming is on.
fn list_warming_settings() -> Vec<Setting> {
    let choices = |presets: &[(&'static str, &'static str)]| {
        presets
            .iter()
            .map(|(value, label)| ((*label).into(), Some(json!(value))))
            .collect()
    };
    vec![
        Setting {
            title: "Interval",
            description: "How long a session sits idle before the next warming request.",
            path: &["warming", "interval"],
            control: Control::Choices(ChoiceList {
                unset_label: "Before Cache Expires".into(),
                choices: choices(&[
                    ("auto", "Before Cache Expires"),
                    ("2 minutes", "2 Minutes"),
                    ("3 minutes", "3 Minutes"),
                    ("4 minutes", "4 Minutes"),
                    ("5 minutes", "5 Minutes"),
                    ("10 minutes", "10 Minutes"),
                ]),
                format_other: format_value,
            }),
        },
        Setting {
            title: "Duration",
            description: "How long warming continues after your last request.",
            path: &["warming", "duration"],
            control: Control::Choices(ChoiceList {
                unset_label: "30 Minutes".into(),
                choices: choices(&[
                    ("15 minutes", "15 Minutes"),
                    ("30 minutes", "30 Minutes"),
                    ("1 hour", "1 Hour"),
                    ("2 hours", "2 Hours"),
                    ("4 hours", "4 Hours"),
                ]),
                format_other: format_value,
            }),
        },
    ]
}

fn list_default_settings() -> Vec<Setting> {
    const WEB_SEARCH_PROVIDERS: &[(&str, &str)] = &[
        ("random", "Any Available"),
        ("exa", "Exa"),
        ("firecrawl", "Firecrawl"),
        ("parallel", "Parallel"),
        ("tavily", "Tavily"),
        ("tinyfish", "TinyFish"),
    ];
    let mut search_choices = vec![("Ask First".into(), None)];
    search_choices.extend(
        WEB_SEARCH_PROVIDERS
            .iter()
            .map(|(id, label)| ((*label).into(), Some(json!({ "provider": id })))),
    );
    search_choices.push(("Off".into(), Some(Value::Bool(false))));
    vec![
        Setting {
            title: "Default Model",
            description: "The model new sessions start with.",
            path: &["model"],
            control: Control::Model {
                unset_label: "Automatic",
            },
        },
        Setting {
            title: "Default Agent",
            description: "The agent new sessions start with.",
            path: &["default_agent"],
            control: Control::Agent,
        },
        Setting {
            title: "Web Search",
            description: "Which provider the agent searches the web with.",
            path: &["websearch"],
            control: Control::Choices(ChoiceList {
                unset_label: "Ask First".into(),
                choices: search_choices,
                // A provider the list doesn't have yet.
                format_other: |value| {
                    value
                        .get("provider")
                        .map_or_else(|| format_value(value), format_value)
                },
            }),
        },
    ]
}

fn render_opencode_settings_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(settings) = settings_window.opencode_settings.as_ref() else {
        return div().into_any_element();
    };
    // A file that doesn't parse can't be edited safely, so the controls are
    // off until it's fixed.
    let unavailable = settings.config_values(cx).is_none();
    let config = &settings
        .config_values(cx)
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let error = settings
        .describe_config_error(cx)
        .or_else(|| settings.errors.main.clone());
    let saved_location = match settings
        .config
        .as_ref()
        .map(|config| config.read(cx).state())
    {
        Some(ConfigFileState::Loaded { file, .. }) => Some(file.describe_saved_location()),
        _ => None,
    };

    let mut page = v_flex()
        .id("opencode-settings-page")
        .size_full()
        .pt_2p5()
        .pb_16()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .when_some(error, |this, error| {
            this.child(div().px_8().pb_2().child(render_error(error)))
        })
        .when_some(saved_location, |this, location| {
            this.child(
                div().px_8().child(
                    Label::new(location)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
        });

    let section = |label: &'static str| {
        div()
            .pt_4()
            .child(SettingsSectionHeader::new(SharedString::new_static(label)))
    };
    let render_settings =
        |settings_list: Vec<Setting>, window: &mut Window, cx: &mut Context<SettingsWindow>| {
            let last = settings_list.len().saturating_sub(1);
            settings_list
                .into_iter()
                .enumerate()
                .map(|(index, setting)| {
                    render_setting(
                        settings_window,
                        setting,
                        config,
                        unavailable,
                        index < last,
                        window,
                        cx,
                    )
                })
                .collect::<Vec<_>>()
        };

    let connected = settings
        .integrations
        .iter()
        .filter(|integration| !integration.connections.is_empty())
        .count();
    page = page
        .child(section("Connections"))
        .child(render_sub_page_link(
            settings_window,
            "Providers",
            if connected == 0 {
                "Connect the model providers OpenCode uses. None are connected yet."
            } else {
                "Connect the model providers OpenCode uses, and manage their accounts."
            },
            open_providers_page,
            true,
            cx,
        ))
        .child(render_sub_page_link(
            settings_window,
            "Plugins",
            "Add plugins to OpenCode's service and turn them on or off.",
            open_plugins_page,
            false,
            cx,
        ));

    page = page.child(section("Compaction")).children(render_settings(
        list_compaction_settings(),
        window,
        cx,
    ));

    let warming_on = config
        .get("warming")
        .is_some_and(|value| value != &Value::Bool(false));
    let description = "Send small requests while you're idle so the provider's prompt cache doesn't expire. Each one uses tokens.";
    let warming_switch = render_switch(
        "Keep Sessions Warm",
        description,
        warming_on,
        unavailable,
        set_warming,
        cx,
    );
    page = page.child(section("Warming")).child(render_row(
        settings_window,
        "Keep Sessions Warm",
        description,
        warming_switch,
        None,
        warming_on,
        cx,
    ));
    if warming_on {
        page = page.children(render_settings(list_warming_settings(), window, cx));
    }

    page.child(section("Defaults"))
        .children(render_settings(list_default_settings(), window, cx))
        .child(section("Tool Permissions"))
        .child(
            div().px_8().child(
                Label::new(
                    "OpenCode D uses OpenCode's own tool permissions, not Zed's. \
                     Set them with Configure in an OpenCode D thread's profile picker.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            ),
        )
        .into_any_element()
}

/// The row for `setting`: its control, showing the stored value, and the
/// reset arrow while one is stored.
fn render_setting(
    settings_window: &SettingsWindow,
    setting: Setting,
    config: &Value,
    unavailable: bool,
    with_divider: bool,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Setting {
        title,
        description,
        path,
        control,
    } = setting;
    let stored = opencode_config::lookup(config, path);
    let project_setup = settings_window
        .opencode_settings
        .as_ref()
        .and_then(|settings| settings.project_setup(cx));
    let pick = |label: SharedString, value: Option<Value>| Choice::Entry {
        label,
        pick: Rc::new(move |this, _window, cx| write_setting(this, path, value.clone(), cx)),
    };
    let (current_label, choices) = match control {
        Control::Switch { default } => {
            let is_on = stored.and_then(Value::as_bool).unwrap_or(default);
            let switch = render_switch(
                title,
                description,
                is_on,
                unavailable,
                move |this, is_on, _window, cx| {
                    write_setting(this, path, Some(Value::Bool(is_on)), cx)
                },
                cx,
            );
            let reset = stored.map(|_| build_reset_handler(path));
            return render_row(
                settings_window,
                title,
                description,
                switch,
                reset,
                with_divider,
                cx,
            );
        }
        Control::Choices(list) => {
            let current_label = list.find_label(stored);
            let choices = list
                .choices
                .into_iter()
                .map(|(label, value)| pick(label, value))
                .collect();
            (current_label, choices)
        }
        Control::Model { unset_label } => {
            let current_label = stored
                .and_then(|value| find_model_label(value, project_setup))
                .unwrap_or_else(|| unset_label.into());
            let mut choices = vec![pick(unset_label.into(), None)];
            for (provider_name, models) in project_setup
                .map(ProjectSetup::group_models_by_provider)
                .unwrap_or_default()
            {
                let models: Vec<_> = models
                    .into_iter()
                    .filter(|model| model.kind.as_deref().is_none_or(|kind| kind == "chat"))
                    .collect();
                if models.is_empty() {
                    continue;
                }
                choices.push(Choice::Header(provider_name.into()));
                choices.extend(models.into_iter().map(|model| {
                    pick(
                        model.name.clone().into(),
                        Some(Value::String(model.reference())),
                    )
                }));
            }
            (current_label, choices)
        }
        Control::Agent => {
            let format_name = |id: &str| -> SharedString {
                project_setup
                    .map_or_else(|| id.to_string(), |setup| setup.format_agent_name(id))
                    .into()
            };
            let current_label = stored
                .and_then(Value::as_str)
                .map_or_else(|| "Automatic".into(), format_name);
            let mut choices = vec![pick("Automatic".into(), None)];
            choices.extend(
                project_setup
                    .map(ProjectSetup::list_selectable_agents)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|agent| pick(format_name(&agent.id), Some(json!(agent.id)))),
            );
            (current_label, choices)
        }
    };
    let dropdown = render_dropdown(
        title,
        description,
        current_label,
        choices,
        unavailable,
        window,
        cx,
    );
    let reset = stored.map(|_| build_reset_handler(path));
    render_row(
        settings_window,
        title,
        description,
        dropdown,
        reset,
        with_divider,
        cx,
    )
}

fn build_reset_handler(
    path: &'static [&'static str],
) -> Rc<dyn Fn(&mut SettingsWindow, &mut Context<SettingsWindow>)> {
    Rc::new(move |this, cx| write_setting(this, path, None, cx))
}

pub(super) fn render_error(error: SharedString) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .items_start()
        .child(
            Icon::new(IconName::XCircle)
                .size(IconSize::Small)
                .color(Color::Error),
        )
        // Long errors wrap instead of running off the page.
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
        )
}

fn render_sub_page_link(
    settings_window: &SettingsWindow,
    title: &'static str,
    description: &'static str,
    open: fn(&mut SettingsWindow, &mut Window, &mut Context<SettingsWindow>),
    with_divider: bool,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let button = Button::new(
        SharedString::from(format!("configure-{title}")),
        "Configure",
    )
    .aria_label(format!("Configure {title}"))
    .tab_index(0_isize)
    .end_icon(
        Icon::new(IconName::ChevronRight)
            .size(IconSize::Small)
            .color(Color::Muted),
    )
    .style(ButtonStyle::OutlinedGhost)
    .size(ButtonSize::Medium)
    .on_click(cx.listener(move |this, _, window, cx| open(this, window, cx)))
    .into_any_element();
    render_row(
        settings_window,
        title,
        description,
        button,
        None,
        with_divider,
        cx,
    )
}

fn format_tokens(tokens: u64) -> SharedString {
    format!("{} Tokens", format_number_with_commas(tokens.into())).into()
}

/// A stored string shows as it is.
fn format_value(value: &Value) -> SharedString {
    match value {
        Value::String(value) => value.clone().into(),
        other => other.to_string().into(),
    }
}

/// The label of the model a config value names, in either of the forms the
/// config accepts.
fn find_model_label(value: &Value, project_setup: Option<&ProjectSetup>) -> Option<SharedString> {
    let reference = match value {
        Value::String(reference) => reference.split('#').next()?.to_string(),
        Value::Object(object) => format!(
            "{}/{}",
            object.get("providerID")?.as_str()?,
            object.get("model")?.as_str()?
        ),
        _ => return None,
    };
    let name = project_setup
        .and_then(|project_setup| project_setup.find_model_by_reference(&reference))
        .map(|model| model.name.clone());
    Some(name.unwrap_or(reference).into())
}

/// A row with Zed's settings layout. `reset`, when set, adds the reset arrow.
fn render_row(
    settings_window: &SettingsWindow,
    title: &'static str,
    description: &'static str,
    control: AnyElement,
    reset: Option<Rc<dyn Fn(&mut SettingsWindow, &mut Context<SettingsWindow>)>>,
    with_divider: bool,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let reset = reset.map(|reset| {
        let this = cx.entity().downgrade();
        Box::new(move |_window: &mut Window, cx: &mut App| {
            let reset = reset.clone();
            this.update(cx, |this, cx| reset(this, cx)).log_err();
        }) as Box<dyn Fn(&mut Window, &mut App)>
    });
    let row = crate::render_settings_item_layout(
        settings_window,
        title,
        description,
        control,
        reset,
        None,
        None,
        false,
        cx,
    );
    v_flex()
        .px_8()
        .child(row.py_4())
        .when(with_divider, |this| this.child(Divider::horizontal()))
        .into_any_element()
}

fn render_switch(
    title: &'static str,
    description: &'static str,
    is_on: bool,
    disabled: bool,
    on_change: impl Fn(&mut SettingsWindow, bool, &mut Window, &mut Context<SettingsWindow>) + 'static,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    Switch::new(title, is_on.into())
        .disabled(disabled)
        .tab_index(0_isize)
        .aria_label(title)
        .aria_description(description)
        .on_click(cx.listener(move |this, state: &ToggleState, window, cx| {
            cx.stop_propagation();
            on_change(this, *state == ToggleState::Selected, window, cx);
        }))
        .into_any_element()
}

/// Sets the value at `path`, or removes it for `None`. `warming` is on as
/// `true` or as an object of options, so setting an option turns `true` into
/// an object, and removing the last one leaves `{}`, which is still on.
fn write_setting(
    settings_window: &mut SettingsWindow,
    path: &'static [&'static str],
    value: Option<Value>,
    cx: &mut Context<SettingsWindow>,
) {
    let is_warming_option = path.len() > WARMING.len() && path.starts_with(WARMING);
    write_config(
        settings_window,
        Page::Main,
        Rc::new(move |config| {
            if is_warming_option && opencode_config::lookup(config, WARMING) == Some(&json!(true)) {
                opencode_config::set(config, WARMING, json!({}))?;
            }
            match &value {
                Some(value) => opencode_config::set(config, path, value.clone())?,
                None => {
                    opencode_config::remove(config, path);
                }
            }
            if is_warming_option && opencode_config::lookup(config, WARMING).is_none() {
                opencode_config::set(config, WARMING, json!({}))?;
            }
            Ok(())
        }),
        cx,
    )
    .detach();
}

/// Turning warming off remembers its options, and turning it back on
/// restores them.
fn set_warming(
    settings_window: &mut SettingsWindow,
    on: bool,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    if !on {
        settings.warming_before_off = None;
        if let Some(config) = settings.config_values(cx)
            && let Some(warming @ Value::Object(_)) = config.get("warming")
        {
            match serde_json::from_value(warming.clone()) {
                Ok(options) => settings.warming_before_off = Some(options),
                Err(error) => {
                    settings.errors.main = Some(format!("{error:#}").into());
                    cx.notify();
                    return;
                }
            }
        }
    }
    let remembered = settings.warming_before_off.clone();
    write_config(
        settings_window,
        Page::Main,
        Rc::new(move |config| {
            let warming = match (on, &remembered) {
                (true, Some(options)) => serde_json::to_value(options)?,
                (true, None) => Value::Bool(true),
                (false, _) => Value::Bool(false),
            };
            opencode_config::set(config, WARMING, warming)
        }),
        cx,
    )
    .detach();
}

/// Applies `edit` to the config file. It runs on what's shown first so the
/// page updates at once, then on the file as it is now, so changes made
/// elsewhere since the page loaded are kept. A failed edit is undone, and
/// its error shows on `page`.
pub(super) fn write_config(
    settings_window: &mut SettingsWindow,
    page: Page,
    edit: Rc<dyn Fn(&mut Value) -> Result<()>>,
    cx: &mut Context<SettingsWindow>,
) -> Task<bool> {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return Task::ready(false);
    };
    let (Some(config), Some(shown)) = (settings.config.clone(), settings.config_values(cx)) else {
        return Task::ready(false);
    };
    let mut shown = shown.clone();
    if let Err(error) = edit(&mut shown) {
        *settings.errors.of(page) = Some(format!("{error:#}").into());
        cx.notify();
        return Task::ready(false);
    }
    let id = settings.next_edit;
    settings.next_edit += 1;
    settings.pending_edit = Some(PendingEdit { id, config: shown });
    *settings.errors.of(page) = None;
    let written = config.update(cx, |config, cx| config.edit(move |json| edit(json), cx));
    cx.notify();
    cx.spawn(async move |this, cx| {
        let written = written.await;
        let saved = written.is_ok();
        this.update(cx, |this, cx| {
            let Some(settings) = this.opencode_settings.as_mut() else {
                return;
            };
            if settings
                .pending_edit
                .as_ref()
                .is_some_and(|pending| pending.id == id)
            {
                settings.pending_edit = None;
            }
            if let Err(error) = written {
                log::error!("Failed to update OpenCode's config: {error:#}");
                // A file that can't be read or doesn't parse has its own
                // message.
                if settings.describe_config_error(cx).is_none() {
                    *settings.errors.of(page) = Some(format!("{error:#}").into());
                }
            }
            cx.notify();
        })
        .log_err();
        saved
    })
}

pub(super) fn render_dropdown(
    title: &'static str,
    description: &'static str,
    current_label: SharedString,
    choices: Vec<Choice>,
    disabled: bool,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let this = cx.entity().downgrade();
    // The menu is rebuilt only when its key changes, so the key covers every
    // entry, not just how many there are.
    let labels: Vec<&str> = choices
        .iter()
        .map(|choice| match choice {
            Choice::Header(label) | Choice::Entry { label, .. } => label.as_ref(),
        })
        .collect();
    let menu_key = format!("{title}-{current_label}-{}", labels.join("\u{1f}"));
    let selected_label = current_label.clone();
    let menu = window.use_keyed_state(SharedString::from(menu_key), cx, move |window, cx| {
        ContextMenu::new(window, cx, move |mut menu, _, _| {
            for choice in &choices {
                match choice {
                    Choice::Header(label) => menu = menu.header(label.clone()),
                    Choice::Entry { label, pick } => {
                        let this = this.clone();
                        let pick = pick.clone();
                        menu = menu.toggleable_entry(
                            label.clone(),
                            *label == selected_label,
                            IconPosition::End,
                            None,
                            move |window, cx| {
                                let pick = pick.clone();
                                this.update(cx, |this, cx| pick(this, window, cx)).log_err();
                            },
                        );
                    }
                }
            }
            menu
        })
    });
    DropdownMenu::new(title, current_label, menu)
        .aria_label(title)
        .aria_description(description)
        .tab_index(0_isize)
        .disabled(disabled)
        .trigger_size(ButtonSize::Medium)
        .style(DropdownStyle::Outlined)
        .attach(gpui::Anchor::BottomRight)
        .offset(gpui::Point {
            x: px(0.0),
            y: px(2.0),
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn locate_config_file() -> std::path::PathBuf {
        opencode_config::resolve_global_config_directory().join("opencode.jsonc")
    }

    /// Opens the page against a fake service whose `failing_route` answers
    /// with an error.
    async fn open_page<'a>(
        config_text: &str,
        failing_route: Option<&'static str>,
        cx: &'a mut gpui::TestAppContext,
    ) -> (
        Entity<SettingsWindow>,
        &'a mut gpui::VisualTestContext,
        Arc<fs::FakeFs>,
        OpenCodeClient,
    ) {
        let fs = fs::FakeFs::new(cx.executor());
        let config_directory = opencode_config::resolve_global_config_directory();
        fs.insert_tree(&config_directory, json!({ "opencode.jsonc": config_text }))
            .await;
        let config_directory = config_directory.to_string_lossy().into_owned();
        let http = http_client::FakeHttpClient::create(move |request| {
            let config_directory = config_directory.clone();
            async move {
                let path = request.uri().path();
                let (status, body) = if Some(path) == failing_route {
                    (500, json!({ "error": "unavailable" }))
                } else if path == "/api/config" {
                    (
                        200,
                        json!([{ "type": "directory", "path": config_directory }]),
                    )
                } else if path == "/api/model" {
                    let model = json!({ "id": "model", "providerID": "openai", "name": "Model" });
                    (200, json!({ "data": [model] }))
                } else {
                    (200, json!({ "data": [] }))
                };
                Ok(http_client::Response::builder()
                    .status(status)
                    .body(http_client::AsyncBody::from(body.to_string()))?)
            }
        });
        let registration = serde_json::from_value(json!({
            "id": "test", "version": "2.2.5", "url": "http://localhost:4096", "pid": 1, "password": ""
        })).expect("registration");
        let client = OpenCodeClient::new(http, &registration).expect("client");
        cx.update(|cx| {
            crate::test::register_settings(cx);
            <dyn Fs>::set_global(fs.clone(), cx);
            opencode_agent::use_test_client(client.clone(), cx);
        });
        let (settings_window, cx) = cx.add_window_view(|window, cx| {
            let mut settings_window = SettingsWindow::test(window, cx);
            open_opencode_settings(&mut settings_window, window, cx);
            settings_window
        });
        cx.run_until_parked();
        (settings_window, cx, fs, client)
    }

    fn shown_value(this: &SettingsWindow, path: &[&str], cx: &App) -> Option<Value> {
        let settings = this.opencode_settings.as_ref().expect("settings");
        opencode_config::lookup(settings.config_values(cx)?, path).cloned()
    }

    fn errors(this: &SettingsWindow) -> &PageErrors {
        &this.opencode_settings.as_ref().expect("settings").errors
    }

    #[gpui::test]
    async fn config_errors_clear_after_the_file_is_fixed(cx: &mut gpui::TestAppContext) {
        let (settings_window, cx, fs, client) =
            open_page("{\"model\": \"openai/original\"}", None, cx).await;
        settings_window.read_with(cx, |this, cx| {
            assert_eq!(
                shown_value(this, &["model"], cx),
                Some(json!("openai/original"))
            );
        });

        fs.atomic_write(locate_config_file(), "{broken".into())
            .await
            .expect("break file");
        settings_window.update(cx, |this, cx| {
            write_setting(this, &["compaction", "auto"], Some(json!(false)), cx)
        });
        cx.run_until_parked();
        settings_window.read_with(cx, |this, cx| {
            let settings = this.opencode_settings.as_ref().expect("settings");
            assert!(settings.config_values(cx).is_none());
            assert!(settings.errors.main.is_none());
            assert!(
                settings
                    .describe_config_error(cx)
                    .expect("banner")
                    .starts_with(
                        "OpenCode's config file has an error, so these settings can't be changed"
                    )
            );
        });

        fs.atomic_write(
            locate_config_file(),
            json!({"model": "openai/fixed"}).to_string(),
        )
        .await
        .expect("fix file");
        client.notify_config_reloaded();
        cx.run_until_parked();
        settings_window.read_with(cx, |this, cx| {
            let settings = this.opencode_settings.as_ref().expect("settings");
            assert_eq!(
                shown_value(this, &["model"], cx),
                Some(json!("openai/fixed"))
            );
            assert!(settings.errors.main.is_none());
            assert!(settings.describe_config_error(cx).is_none());
        });
    }

    #[gpui::test]
    async fn a_write_shows_at_once_and_is_undone_when_the_file_cant_be_written(
        cx: &mut gpui::TestAppContext,
    ) {
        let (settings_window, cx, fs, _client) = open_page("{}", None, cx).await;

        let path = &locate_config_file();
        fs.remove_file(path, Default::default())
            .await
            .expect("remove file");
        fs.create_dir(path).await.expect("folder in its place");
        settings_window.update(cx, |this, cx| {
            write_setting(this, &["compaction", "auto"], Some(json!(false)), cx);
            assert_eq!(
                shown_value(this, &["compaction", "auto"], cx),
                Some(json!(false))
            );
        });
        cx.run_until_parked();
        settings_window.read_with(cx, |this, cx| {
            assert_eq!(shown_value(this, &["compaction", "auto"], cx), None);
            assert!(errors(this).main.is_some());
            assert!(errors(this).providers.is_none());
            assert!(errors(this).plugins.is_none());
        });
    }

    #[gpui::test]
    async fn a_failed_plugin_change_keeps_its_error(cx: &mut gpui::TestAppContext) {
        let (settings_window, cx, fs, _client) = open_page("{}", None, cx).await;
        let path = &locate_config_file();
        fs.remove_file(path, Default::default())
            .await
            .expect("remove file");
        fs.create_dir(path).await.expect("folder in its place");
        settings_window.update(cx, |this, cx| {
            super::super::opencode_plugins_page::write_plugins(
                this,
                Rc::new(|config| opencode_config::set(config, &["plugin"], json!(["a"]))),
                cx,
            );
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(30));
        cx.run_until_parked();
        settings_window.read_with(cx, |this, _| {
            assert!(errors(this).plugins.is_some());
        });
    }

    #[gpui::test]
    async fn an_error_shows_only_on_its_own_page(cx: &mut gpui::TestAppContext) {
        let (settings_window, cx, _fs, _client) = open_page("{}", Some("/api/plugin"), cx).await;
        settings_window.read_with(cx, |this, _| {
            assert!(errors(this).plugins.is_some());
            assert!(errors(this).main.is_none());
            assert!(errors(this).providers.is_none());
        });
    }

    #[gpui::test]
    async fn warming_options_keep_warming_on(cx: &mut gpui::TestAppContext) {
        let (settings_window, cx, _fs, _client) = open_page("{\"warming\": true}", None, cx).await;
        let interval: &'static [&'static str] = &["warming", "interval"];
        settings_window.update(cx, |this, cx| {
            write_setting(this, interval, Some(json!("5 minutes")), cx)
        });
        cx.run_until_parked();
        settings_window.read_with(cx, |this, cx| {
            assert_eq!(
                shown_value(this, WARMING, cx),
                Some(json!({ "interval": "5 minutes" }))
            );
        });
        settings_window.update(cx, |this, cx| write_setting(this, interval, None, cx));
        cx.run_until_parked();
        settings_window.read_with(cx, |this, cx| {
            assert_eq!(shown_value(this, WARMING, cx), Some(json!({})));
        });
    }

    #[gpui::test]
    async fn warming_restores_options_after_being_turned_off(cx: &mut gpui::TestAppContext) {
        let options = json!({"interval": "10 minutes", "duration": "1 hour", "prompt": "Hello"});
        let config = json!({ "warming": options }).to_string();
        let (settings_window, cx, _fs, _client) = open_page(&config, None, cx).await;
        settings_window.update_in(cx, |this, window, cx| set_warming(this, false, window, cx));
        cx.run_until_parked();
        settings_window.read_with(cx, |this, cx| {
            assert_eq!(shown_value(this, WARMING, cx), Some(json!(false)));
        });
        settings_window.update_in(cx, |this, window, cx| set_warming(this, true, window, cx));
        cx.run_until_parked();
        settings_window.read_with(cx, |this, cx| {
            assert_eq!(shown_value(this, WARMING, cx), Some(options.clone()));
        });
    }

    #[test]
    fn token_labels_group_custom_values_like_presets() {
        assert_eq!(format_tokens(12_345).as_ref(), "12,345 Tokens");
        assert_eq!(format_tokens(15_000).as_ref(), "15,000 Tokens");
        assert_eq!(format_tokens(1_234_567).as_ref(), "1,234,567 Tokens");
        assert_eq!(format_tokens(0).as_ref(), "0 Tokens");
    }

    fn find_choice_list(settings: Vec<Setting>, title: &str) -> ChoiceList {
        settings
            .into_iter()
            .find_map(|setting| match setting.control {
                Control::Choices(list) if setting.title == title => Some(list),
                _ => None,
            })
            .expect("a setting with choices")
    }

    #[test]
    fn labels_show_values_the_choices_do_not_list() {
        let buffer = find_choice_list(list_compaction_settings(), "Space to Keep Free");
        assert_eq!(buffer.find_label(Some(&json!("12.5%"))).as_ref(), "12.5%");
        assert_eq!(buffer.find_label(None).as_ref(), "15%");
        assert_eq!(buffer.find_label(Some(&json!("20%"))).as_ref(), "20%");
        let keep = find_choice_list(list_compaction_settings(), "Recent Context to Keep");
        assert_eq!(
            keep.find_label(Some(&json!(12_345))).as_ref(),
            "12,345 Tokens"
        );
        let interval = find_choice_list(list_warming_settings(), "Interval");
        assert_eq!(interval.find_label(None).as_ref(), "Before Cache Expires");
    }

    #[test]
    fn web_search_label_covers_each_form() {
        let search = find_choice_list(list_default_settings(), "Web Search");
        assert_eq!(search.find_label(None).as_ref(), "Ask First");
        assert_eq!(search.find_label(Some(&json!(false))).as_ref(), "Off");
        assert_eq!(
            search
                .find_label(Some(&json!({ "provider": "exa" })))
                .as_ref(),
            "Exa"
        );
        assert_eq!(
            search
                .find_label(Some(&json!({ "provider": "new" })))
                .as_ref(),
            "new"
        );
    }

    #[gpui::test]
    async fn model_labels_read_both_config_forms(cx: &mut gpui::TestAppContext) {
        let (settings_window, cx, _fs, _client) = open_page("{}", None, cx).await;
        settings_window.read_with(cx, |this, cx| {
            let project_setup = this
                .opencode_settings
                .as_ref()
                .and_then(|settings| settings.project_setup(cx));
            assert!(project_setup.is_some());
            assert_eq!(
                find_model_label(&json!("openai/model#high"), project_setup).as_deref(),
                Some("Model")
            );
            assert_eq!(
                find_model_label(
                    &json!({ "providerID": "openai", "model": "model", "variant": "high" }),
                    project_setup
                )
                .as_deref(),
                Some("Model")
            );
            assert_eq!(
                find_model_label(&json!("acme/other"), project_setup).as_deref(),
                Some("acme/other")
            );
        });
    }
}
