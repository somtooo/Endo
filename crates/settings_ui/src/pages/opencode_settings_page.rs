//! OpenCode D's page, reached from the External Agents page: the service
//! settings that matter in a GUI. Its Providers and Plugins sub-pages live in
//! their own modules.

use std::{path::PathBuf, rc::Rc};

use anyhow::{Result, anyhow};
use editor::Editor;
use fs::Fs;
use gpui::{Entity, ScrollHandle, Subscription, Task, prelude::*};
use opencode_agent::{
    AgentInfo, Integration, ModelInfo, OpenCodeClient, PluginInfo, opencode_config,
};
use serde_json::{Map, Value, json};
use ui::{
    ContextMenu, Divider, DropdownMenu, DropdownStyle, Switch, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::opencode_plugins_page::{PluginForm, open_plugins_page};
use super::opencode_providers_page::{
    ConnectForm, RenameForm, new_provider_filter, open_providers_page,
};
use crate::{SettingsWindow, components::SettingsSectionHeader};

const SECTION: &str = "External Agents";

/// What the OpenCode D pages show, kept while the settings window is open.
pub(crate) struct OpenCodeSettings {
    pub(super) client: Option<OpenCodeClient>,
    /// The directory service requests are made for. Connections and the
    /// global config don't depend on it, but every request names one.
    pub(super) directory: String,
    config_path: Option<PathBuf>,
    pub(super) config_text: String,
    pub(super) loading: bool,
    pub(super) error: Option<SharedString>,
    pub(super) integrations: Vec<Integration>,
    models: Vec<ModelInfo>,
    agents: Vec<AgentInfo>,
    pub(super) plugins: Vec<PluginInfo>,
    last_write: Option<Task<()>>,
    pub(super) provider_filter: Entity<Editor>,
    _provider_filter_subscription: Subscription,
    pub(super) connect_form: Option<ConnectForm>,
    pub(super) rename_form: Option<RenameForm>,
    pub(super) plugin_form: Option<PluginForm>,
    _load: Option<Task<()>>,
}

/// One entry in a settings dropdown.
pub(super) enum Choice {
    Header(SharedString),
    Entry {
        label: SharedString,
        pick: Rc<dyn Fn(&mut SettingsWindow, &mut Window, &mut Context<SettingsWindow>)>,
    },
}

/// A setting stored directly at one place in the config file.
fn value_choice(label: &str, path: &'static [&'static str], value: Value) -> Choice {
    Choice::Entry {
        label: label.to_string().into(),
        pick: Rc::new(move |this, _window, cx| write_value(this, path, Some(value.clone()), cx)),
    }
}

/// The entry for leaving a setting to OpenCode's default.
fn default_choice(label: &str, path: &'static [&'static str]) -> Choice {
    Choice::Entry {
        label: label.to_string().into(),
        pick: Rc::new(move |this, _window, cx| write_value(this, path, None, cx)),
    }
}

pub(crate) fn open_opencode_settings(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let directory = settings_window
        .active_project(cx)
        .and_then(|project| {
            let project = project.read(cx);
            let worktree = project.visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_string_lossy().to_string())
        })
        .unwrap_or_else(|| paths::home_dir().to_string_lossy().to_string());
    let (provider_filter, subscription) = new_provider_filter(window, cx);
    // A change made just before reopening the page still has to reach the file.
    let last_write = settings_window
        .opencode_settings
        .take()
        .and_then(|settings| settings.last_write);
    settings_window.opencode_settings = Some(OpenCodeSettings {
        client: None,
        directory,
        config_path: None,
        config_text: String::new(),
        loading: true,
        error: None,
        integrations: Vec::new(),
        models: Vec::new(),
        agents: Vec::new(),
        plugins: Vec::new(),
        last_write,
        provider_filter,
        _provider_filter_subscription: subscription,
        connect_form: None,
        rename_form: None,
        plugin_form: None,
        _load: None,
    });
    reload(settings_window, cx);
    settings_window.push_dynamic_sub_page(
        "OpenCode D",
        SECTION,
        None,
        false,
        render_opencode_settings_page,
        window,
        cx,
    );
}

/// Fetches everything the pages show. Each part that can't load is reported,
/// and the others still appear.
pub(super) fn reload(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let directory = settings.directory.clone();
    let fs = <dyn Fs>::global(cx);
    let client = opencode_agent::service_client(cx);
    settings._load = Some(cx.spawn(async move |this, cx| {
        let client = match client.await {
            Ok(client) => client,
            Err(error) => {
                this.update(cx, |this, cx| {
                    if let Some(settings) = this.opencode_settings.as_mut() {
                        settings.loading = false;
                        settings.error = Some(service_error(&error));
                        cx.notify();
                    }
                })
                .log_err();
                return;
            }
        };
        let (entries, integrations, models, agents, plugins) = futures::join!(
            client.config_entries(&directory),
            client.integrations(&directory),
            client.models(&directory),
            client.agents(&directory),
            client.plugins(&directory),
        );
        let config = async {
            let path = opencode_config::global_config_path(&entries?)
                .ok_or_else(|| anyhow!("OpenCode didn't report its config directory"))?;
            let text = read_config(fs.as_ref(), &path).await?;
            anyhow::Ok((path, text))
        }
        .await;
        this.update(cx, |this, cx| {
            let Some(settings) = this.opencode_settings.as_mut() else {
                return;
            };
            settings.client = Some(client);
            settings.loading = false;
            let mut problems = Vec::new();
            match config {
                Ok((path, text)) => {
                    settings.config_path = Some(path);
                    settings.config_text = text;
                }
                Err(error) => problems.push(format!("{error:#}")),
            }
            match integrations {
                Ok(integrations) => settings.integrations = integrations,
                Err(error) => problems.push(format!("{error:#}")),
            }
            match models {
                Ok(models) => settings.models = models,
                Err(error) => problems.push(format!("{error:#}")),
            }
            match agents {
                Ok(agents) => settings.agents = agents,
                Err(error) => problems.push(format!("{error:#}")),
            }
            match plugins {
                Ok(plugins) => settings.plugins = plugins,
                Err(error) => problems.push(format!("{error:#}")),
            }
            settings.error = (!problems.is_empty()).then(|| problems.join("\n").into());
            cx.notify();
        })
        .log_err();
    }));
}

fn service_error(error: &anyhow::Error) -> SharedString {
    if error
        .downcast_ref::<opencode_agent::NotInstalled>()
        .is_some()
    {
        "OpenCode isn't installed. Start an OpenCode D thread to install it.".into()
    } else {
        format!("Couldn't reach the OpenCode service: {error:#}").into()
    }
}

async fn read_config(fs: &dyn Fs, path: &std::path::Path) -> Result<String> {
    if fs.is_file(path).await {
        fs.load(path).await
    } else {
        Ok(String::new())
    }
}

/// Applies `edit` to the config file. It runs on what's in memory first so the
/// page updates at once, then on the file as it is now, so changes made
/// elsewhere since the page loaded are kept. Text that doesn't parse is
/// refused, so a typo in the file is reported instead of overwritten.
pub(super) fn write_config(
    settings_window: &mut SettingsWindow,
    edit: Rc<dyn Fn(&str) -> Result<String>>,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let Some(file) = settings.config_path.clone() else {
        return;
    };
    match edit(&settings.config_text) {
        Ok(text) => {
            settings.config_text = text;
            settings.error = None;
        }
        Err(error) => {
            settings.error = Some(format!("{error:#}").into());
            cx.notify();
            return;
        }
    }
    let previous = settings.last_write.take();
    let fs = <dyn Fs>::global(cx);
    settings.last_write = Some(cx.spawn(async move |this, cx| {
        if let Some(previous) = previous {
            previous.await;
        }
        let written = async {
            let text = read_config(fs.as_ref(), &file).await?;
            let updated = edit(&text)?;
            if updated != text {
                if let Some(directory) = file.parent() {
                    fs.create_dir(directory).await?;
                }
                fs.atomic_write(file.clone(), updated.clone()).await?;
            }
            anyhow::Ok(updated)
        }
        .await;
        this.update(cx, |this, cx| {
            if let Some(settings) = this.opencode_settings.as_mut() {
                match written {
                    Ok(text) => settings.config_text = text,
                    Err(error) => {
                        log::error!("Failed to update OpenCode's config: {error:#}");
                        settings.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            }
        })
        .log_err();
    }));
    cx.notify();
}

fn write_value(
    settings_window: &mut SettingsWindow,
    path: &'static [&'static str],
    value: Option<Value>,
    cx: &mut Context<SettingsWindow>,
) {
    write_config(
        settings_window,
        Rc::new(move |text| opencode_config::set_value(text, path, value.as_ref())),
        cx,
    );
}

/// Sets one field of `warming`, which may be `true` until a field is set, and
/// goes back to `true` when the last field is removed.
fn write_warming_field(
    settings_window: &mut SettingsWindow,
    field: &'static str,
    value: Option<&'static str>,
    cx: &mut Context<SettingsWindow>,
) {
    write_config(
        settings_window,
        Rc::new(move |text| {
            let config = opencode_config::parse(text)?;
            let warming = updated_warming(config.get("warming"), field, value);
            opencode_config::set_value(text, &["warming"], Some(&warming))
        }),
        cx,
    );
}

fn updated_warming(current: Option<&Value>, field: &str, value: Option<&str>) -> Value {
    let mut warming = match current {
        Some(Value::Object(object)) => object.clone(),
        _ => Map::new(),
    };
    match value {
        Some(value) => {
            warming.insert(field.to_string(), Value::String(value.to_string()));
        }
        None => {
            warming.remove(field);
        }
    }
    if warming.is_empty() {
        Value::Bool(true)
    } else {
        Value::Object(warming)
    }
}

/// The label of the model a config value names, in either of the forms the
/// config accepts.
fn model_label(value: &Value, models: &[ModelInfo]) -> Option<SharedString> {
    let reference = match value {
        Value::String(reference) => reference.split('#').next()?.to_string(),
        Value::Object(object) => format!(
            "{}/{}",
            object.get("providerID")?.as_str()?,
            object.get("model")?.as_str()?
        ),
        _ => return None,
    };
    let name = models
        .iter()
        .find(|model| format!("{}/{}", model.provider_id, model.id) == reference)
        .map(|model| model.name.clone());
    Some(name.unwrap_or(reference).into())
}

fn model_choices(
    default_label: &str,
    path: &'static [&'static str],
    settings: &OpenCodeSettings,
) -> Vec<Choice> {
    let mut models: Vec<&ModelInfo> = settings
        .models
        .iter()
        .filter(|model| model.kind.as_deref().is_none_or(|kind| kind == "chat"))
        .collect();
    models.sort_by_key(|model| (model.provider_id.clone(), model.name.to_lowercase()));
    let mut choices = vec![default_choice(default_label, path)];
    let mut current_provider = None;
    for model in models {
        if current_provider != Some(&model.provider_id) {
            current_provider = Some(&model.provider_id);
            let provider_name = settings
                .integrations
                .iter()
                .find(|integration| integration.id == model.provider_id)
                .map_or(model.provider_id.as_str(), |integration| {
                    integration.name.as_str()
                });
            choices.push(Choice::Header(provider_name.to_string().into()));
        }
        choices.push(value_choice(
            &model.name,
            path,
            Value::String(format!("{}/{}", model.provider_id, model.id)),
        ));
    }
    choices
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
    let menu_key = format!("{title}-{current_label}-{}", choices.len());
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

fn render_switch(
    title: &'static str,
    description: &'static str,
    is_on: bool,
    on_change: impl Fn(&mut SettingsWindow, bool, &mut Context<SettingsWindow>) + 'static,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    Switch::new(title, is_on.into())
        .tab_index(0_isize)
        .aria_label(title)
        .aria_description(description)
        .on_click(cx.listener(move |this, state: &ToggleState, _window, cx| {
            on_change(this, *state == ToggleState::Selected, cx);
        }))
        .into_any_element()
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

fn reset_value(
    config: &Value,
    path: &'static [&'static str],
) -> Option<Rc<dyn Fn(&mut SettingsWindow, &mut Context<SettingsWindow>)>> {
    opencode_config::value_at(config, path).map(|_| {
        Rc::new(
            move |this: &mut SettingsWindow, cx: &mut Context<SettingsWindow>| {
                write_value(this, path, None, cx);
            },
        ) as Rc<dyn Fn(&mut SettingsWindow, &mut Context<SettingsWindow>)>
    })
}

/// The dropdown label for a stored value that may not be one of the presets.
fn preset_label(stored: Option<&Value>, default: &str, presets: &[(&str, &str)]) -> SharedString {
    let Some(stored) = stored else {
        return default.to_string().into();
    };
    match stored {
        Value::String(value) => presets
            .iter()
            .find(|(preset, _)| preset == value)
            .map_or(value.clone(), |(_, label)| (*label).to_string())
            .into(),
        other => other.to_string().into(),
    }
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

const COMPACTION_AUTO: &[&str] = &["compaction", "auto"];
const COMPACTION_KEEP: &[&str] = &["compaction", "keep", "tokens"];
const COMPACTION_BUFFER: &[&str] = &["compaction", "buffer"];
const COMPACTION_MODEL: &[&str] = &["agents", "compaction", "model"];
const DEFAULT_MODEL: &[&str] = &["model"];
const DEFAULT_AGENT: &[&str] = &["default_agent"];
const WEB_SEARCH: &[&str] = &["websearch"];

const KEEP_PRESETS: &[(u32, &str)] = &[
    (5_000, "5,000 Tokens"),
    (10_000, "10,000 Tokens"),
    (15_000, "15,000 Tokens"),
    (20_000, "20,000 Tokens"),
    (30_000, "30,000 Tokens"),
    (50_000, "50,000 Tokens"),
];
const BUFFER_PRESETS: &[(&str, &str)] = &[
    ("5%", "5%"),
    ("10%", "10%"),
    ("15%", "15%"),
    ("20%", "20%"),
    ("25%", "25%"),
    ("30%", "30%"),
    ("40%", "40%"),
    ("50%", "50%"),
];
const WARMING_INTERVALS: &[(&str, &str)] = &[
    ("auto", "Before Cache Expires"),
    ("2 minutes", "2 Minutes"),
    ("3 minutes", "3 Minutes"),
    ("4 minutes", "4 Minutes"),
    ("5 minutes", "5 Minutes"),
    ("10 minutes", "10 Minutes"),
];
const WARMING_DURATIONS: &[(&str, &str)] = &[
    ("15 minutes", "15 Minutes"),
    ("30 minutes", "30 Minutes"),
    ("1 hour", "1 Hour"),
    ("2 hours", "2 Hours"),
    ("4 hours", "4 Hours"),
];
const WEB_SEARCH_PROVIDERS: &[(&str, &str)] = &[
    ("random", "Any Available"),
    ("exa", "Exa"),
    ("firecrawl", "Firecrawl"),
    ("parallel", "Parallel"),
    ("tavily", "Tavily"),
    ("tinyfish", "TinyFish"),
];

fn web_search_label(config: &Value) -> SharedString {
    match config.get("websearch") {
        None => "Ask First".into(),
        Some(Value::Bool(false)) => "Off".into(),
        Some(value) => preset_label(value.get("provider"), "Ask First", WEB_SEARCH_PROVIDERS),
    }
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
    let config =
        opencode_config::parse(&settings.config_text).unwrap_or_else(|_| Value::Object(Map::new()));
    let unavailable = settings.config_path.is_none();

    let mut page = v_flex()
        .id("opencode-settings-page")
        .size_full()
        .pt_2p5()
        .pb_16()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .when_some(settings.error.clone(), |this, error| {
            this.child(div().px_8().pb_2().child(render_error(error)))
        });

    let section = |label: &'static str| {
        div()
            .pt_4()
            .child(SettingsSectionHeader::new(SharedString::new_static(label)))
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

    let auto_compact = config
        .pointer("/compaction/auto")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let keep_tokens = opencode_config::value_at(&config, COMPACTION_KEEP);
    let keep_label: SharedString = match keep_tokens {
        None => "15,000 Tokens".into(),
        Some(value) => value
            .as_u64()
            .and_then(|tokens| {
                KEEP_PRESETS
                    .iter()
                    .find(|(preset, _)| u64::from(*preset) == tokens)
                    .map(|(_, label)| SharedString::from(*label))
            })
            .unwrap_or_else(|| format!("{value} Tokens").into()),
    };
    let summary_model = opencode_config::value_at(&config, COMPACTION_MODEL)
        .and_then(|value| model_label(value, &settings.models))
        .unwrap_or_else(|| "Session's Model".into());
    let compaction_rows = [
        render_row(
            settings_window,
            "Auto Compact",
            "Summarize older conversation when a session nears the model's context limit.",
            render_switch(
                "Auto Compact",
                "Summarize older conversation when a session nears the model's context limit.",
                auto_compact,
                |this, is_on, cx| {
                    write_value(this, COMPACTION_AUTO, Some(Value::Bool(is_on)), cx);
                },
                cx,
            ),
            reset_value(&config, COMPACTION_AUTO),
            true,
            cx,
        ),
        render_row(
            settings_window,
            "Recent Context to Keep",
            "How much recent conversation stays word for word beside the summary.",
            render_dropdown(
                "Recent Context to Keep",
                "How much recent conversation stays word for word beside the summary.",
                keep_label,
                KEEP_PRESETS
                    .iter()
                    .map(|(tokens, label)| value_choice(label, COMPACTION_KEEP, json!(tokens)))
                    .collect(),
                unavailable,
                window,
                cx,
            ),
            reset_value(&config, COMPACTION_KEEP),
            true,
            cx,
        ),
        render_row(
            settings_window,
            "Space to Keep Free",
            "Share of the context window kept free. A larger share compacts earlier.",
            render_dropdown(
                "Space to Keep Free",
                "Share of the context window kept free. A larger share compacts earlier.",
                preset_label(
                    opencode_config::value_at(&config, COMPACTION_BUFFER),
                    "15%",
                    BUFFER_PRESETS,
                ),
                BUFFER_PRESETS
                    .iter()
                    .map(|(value, label)| value_choice(label, COMPACTION_BUFFER, json!(value)))
                    .collect(),
                unavailable,
                window,
                cx,
            ),
            reset_value(&config, COMPACTION_BUFFER),
            true,
            cx,
        ),
        render_row(
            settings_window,
            "Summary Model",
            "The model that writes the summary.",
            render_dropdown(
                "Summary Model",
                "The model that writes the summary.",
                summary_model,
                model_choices("Session's Model", COMPACTION_MODEL, settings),
                unavailable,
                window,
                cx,
            ),
            reset_value(&config, COMPACTION_MODEL),
            false,
            cx,
        ),
    ];
    page = page.child(section("Compaction")).children(compaction_rows);

    let warming = config.get("warming");
    let warming_on = warming.is_some_and(|value| value != &Value::Bool(false));
    page = page.child(section("Warming")).child(render_row(
        settings_window,
        "Keep Sessions Warm",
        "Send small requests while you're idle so the provider's prompt cache doesn't expire. Each one uses tokens.",
        render_switch(
            "Keep Sessions Warm",
            "Send small requests while you're idle so the provider's prompt cache doesn't expire. Each one uses tokens.",
            warming_on,
            |this, is_on, cx| {
                write_value(this, &["warming"], Some(Value::Bool(is_on)), cx);
            },
            cx,
        ),
        None,
        warming_on,
        cx,
    ));
    if warming_on {
        let warming_row = |settings_window: &SettingsWindow,
                           title: &'static str,
                           description: &'static str,
                           field: &'static str,
                           default: &str,
                           presets: &'static [(&'static str, &'static str)],
                           with_divider: bool,
                           window: &mut Window,
                           cx: &mut Context<SettingsWindow>| {
            let stored = warming.and_then(|warming| warming.get(field));
            let reset = stored.map(|_| {
                Rc::new(
                    move |this: &mut SettingsWindow, cx: &mut Context<SettingsWindow>| {
                        write_warming_field(this, field, None, cx);
                    },
                ) as Rc<dyn Fn(&mut SettingsWindow, &mut Context<SettingsWindow>)>
            });
            let choices = presets
                .iter()
                .map(|(value, label)| Choice::Entry {
                    label: (*label).into(),
                    pick: Rc::new(move |this, _window, cx| {
                        write_warming_field(this, field, Some(value), cx);
                    }),
                })
                .collect();
            let control = render_dropdown(
                title,
                description,
                preset_label(stored, default, presets),
                choices,
                unavailable,
                window,
                cx,
            );
            render_row(
                settings_window,
                title,
                description,
                control,
                reset,
                with_divider,
                cx,
            )
        };
        let interval = warming_row(
            settings_window,
            "Interval",
            "How long a session sits idle before the next warming request.",
            "interval",
            "4 Minutes",
            WARMING_INTERVALS,
            true,
            window,
            cx,
        );
        let duration = warming_row(
            settings_window,
            "Duration",
            "How long warming continues after your last request.",
            "duration",
            "30 Minutes",
            WARMING_DURATIONS,
            false,
            window,
            cx,
        );
        page = page.child(interval).child(duration);
    }

    let default_model = opencode_config::value_at(&config, DEFAULT_MODEL)
        .and_then(|value| model_label(value, &settings.models))
        .unwrap_or_else(|| "Automatic".into());
    let default_agent_id = config.get("default_agent").and_then(Value::as_str);
    let agent_name = |id: &str| {
        settings
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .map_or(id, |agent| agent.name.as_str())
            .to_string()
    };
    let default_agent: SharedString =
        default_agent_id.map_or("Automatic".into(), |id| agent_name(id).into());
    let mut agent_choices = vec![default_choice("Automatic", DEFAULT_AGENT)];
    agent_choices.extend(
        settings
            .agents
            .iter()
            .filter(|agent| !agent.hidden && agent.mode.as_deref() != Some("subagent"))
            .map(|agent| value_choice(&agent.name, DEFAULT_AGENT, json!(agent.id))),
    );
    let mut search_choices = vec![default_choice("Ask First", WEB_SEARCH)];
    search_choices.extend(
        WEB_SEARCH_PROVIDERS
            .iter()
            .map(|(id, label)| value_choice(label, WEB_SEARCH, json!({ "provider": id }))),
    );
    search_choices.push(value_choice("Off", WEB_SEARCH, Value::Bool(false)));
    let default_rows = [
        render_row(
            settings_window,
            "Default Model",
            "The model new sessions start with.",
            render_dropdown(
                "Default Model",
                "The model new sessions start with.",
                default_model,
                model_choices("Automatic", DEFAULT_MODEL, settings),
                unavailable,
                window,
                cx,
            ),
            reset_value(&config, DEFAULT_MODEL),
            true,
            cx,
        ),
        render_row(
            settings_window,
            "Default Agent",
            "The agent new sessions start with.",
            render_dropdown(
                "Default Agent",
                "The agent new sessions start with.",
                default_agent,
                agent_choices,
                unavailable,
                window,
                cx,
            ),
            reset_value(&config, DEFAULT_AGENT),
            true,
            cx,
        ),
        render_row(
            settings_window,
            "Web Search",
            "Which provider the agent searches the web with.",
            render_dropdown(
                "Web Search",
                "Which provider the agent searches the web with.",
                web_search_label(&config),
                search_choices,
                unavailable,
                window,
                cx,
            ),
            reset_value(&config, WEB_SEARCH),
            false,
            cx,
        ),
    ];
    page.child(section("Defaults"))
        .children(default_rows)
        .into_any_element()
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
        .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
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
                .child(Label::new("OpenCode D")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warming_keeps_other_fields_and_falls_back_to_on() {
        let updated = updated_warming(Some(&Value::Bool(true)), "interval", Some("5 minutes"));
        assert_eq!(updated, json!({ "interval": "5 minutes" }));

        let current = json!({ "interval": "5 minutes", "duration": "1 hour" });
        let updated = updated_warming(Some(&current), "interval", None);
        assert_eq!(updated, json!({ "duration": "1 hour" }));

        let updated = updated_warming(Some(&json!({ "interval": "auto" })), "interval", None);
        assert_eq!(updated, Value::Bool(true));
    }

    #[test]
    fn labels_show_values_the_presets_do_not_list() {
        assert_eq!(
            preset_label(Some(&json!("12.5%")), "15%", BUFFER_PRESETS).as_ref(),
            "12.5%"
        );
        assert_eq!(preset_label(None, "15%", BUFFER_PRESETS).as_ref(), "15%");
        assert_eq!(
            preset_label(Some(&json!(20000)), "15%", BUFFER_PRESETS).as_ref(),
            "20000"
        );
    }

    #[test]
    fn web_search_label_covers_each_form() {
        assert_eq!(web_search_label(&json!({})).as_ref(), "Ask First");
        assert_eq!(
            web_search_label(&json!({ "websearch": false })).as_ref(),
            "Off"
        );
        assert_eq!(
            web_search_label(&json!({ "websearch": { "provider": "exa" } })).as_ref(),
            "Exa"
        );
    }

    #[test]
    fn model_labels_read_both_config_forms() {
        let models: Vec<ModelInfo> = serde_json::from_value(json!([
            { "id": "sol", "providerID": "openai", "name": "Sol" }
        ]))
        .expect("models");
        assert_eq!(
            model_label(&json!("openai/sol#high"), &models).as_deref(),
            Some("Sol")
        );
        assert_eq!(
            model_label(
                &json!({ "providerID": "openai", "model": "sol", "variant": "high" }),
                &models
            )
            .as_deref(),
            Some("Sol")
        );
        assert_eq!(
            model_label(&json!("acme/other"), &models).as_deref(),
            Some("acme/other")
        );
    }
}
