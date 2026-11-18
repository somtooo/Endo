//! OpenCode D's Plugins sub-page.
//!
//! The service only lists plugins it's running, so rows combine that list with
//! the `plugins` entries in the global config. A `-<id>` entry turning a plugin
//! off is OpenCode's documented rule. Placing it right after the plugin's
//! entry is this page's own convention: a plugin that's off isn't running, so
//! its ID is known only by where its `-<id>` sits.

use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use gpui::{ScrollHandle, prelude::*};
use opencode_agent::{PluginInfo, PluginSource, PluginState, opencode_config};
use serde_json::Value;
use ui::{
    AiSettingItem, AiSettingItemSource, AiSettingItemStatus, Switch, ToggleState, Tooltip,
    prelude::*,
};
use util::ResultExt as _;

use super::opencode_providers_page::{OneFieldForm, open_one_field_form};
use super::opencode_settings_page::{Page, reload, render_error, write_config};
use crate::SettingsWindow;

#[derive(Debug, PartialEq)]
enum PluginStatus {
    Active,
    Failed(String),
    Off,
    /// Listed in the config but not running, usually because it's still
    /// installing.
    NotLoaded,
}

#[derive(Debug, PartialEq)]
struct PluginRow {
    name: String,
    detail: Option<String>,
    status: PluginStatus,
    /// The ID that turns it off or on. A plugin that hasn't loaded has none.
    id: Option<String>,
    /// Its entry in the global config, if that's where it's configured.
    config_entry: Option<String>,
}

pub(super) fn open_plugins_page(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    reload(settings_window, cx);
    settings_window.push_dynamic_sub_page(
        "Plugins",
        "OpenCode D",
        Some(PLUGINS_PAGE_PATH),
        false,
        render_plugins_page,
        window,
        cx,
    );
}

/// Marks the sub-page so its header gets the Add Plugin button.
pub(crate) const PLUGINS_PAGE_PATH: &str = "opencode_plugins";

pub(crate) fn render_add_plugin_button(cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    Button::new("add-opencode-plugin", "Add Plugin")
        .style(ButtonStyle::Outlined)
        .tab_index(0_isize)
        .start_icon(
            Icon::new(IconName::Plus)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .label_size(LabelSize::Small)
        .on_click(cx.listener(|this, _, window, cx| open_plugin_form(this, window, cx)))
}

fn render_plugins_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(settings) = settings_window.opencode_settings.as_ref() else {
        return div().into_any_element();
    };
    let config = settings.config_values(cx).cloned().unwrap_or(Value::Null);
    let rows = build_plugin_rows(&config, &settings.plugins);
    let error = settings
        .describe_config_error(cx)
        .or_else(|| settings.errors.plugins.clone());
    let built_in_count = settings
        .plugins
        .iter()
        .filter(|plugin| matches!(plugin.source, PluginSource::Builtin))
        .count();

    let list = if rows.is_empty() {
        h_flex()
            .p_4()
            .justify_center()
            .border_1()
            .border_dashed()
            .border_color(cx.theme().colors().border.opacity(0.6))
            .rounded_sm()
            .child(
                Label::new(if settings.loading {
                    "Loading plugins…"
                } else {
                    "No plugins added yet. Click \"Add Plugin\" to get started."
                })
                .color(Color::Muted)
                .size(LabelSize::Small),
            )
            .into_any_element()
    } else {
        v_flex()
            .w_full()
            .gap_1()
            .children(itertools::intersperse_with(
                rows.into_iter()
                    .map(|row| render_plugin_row(row, cx).into_any_element()),
                || ui::Divider::horizontal().into_any_element(),
            ))
            .into_any_element()
    };

    v_flex()
        .id("opencode-plugins-page")
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_2()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .child(
            v_flex().child(Label::new("Plugins")).child(
                Label::new(if built_in_count > 0 {
                    format!(
                        "Plugins add tools, integrations and agents to OpenCode's service. {built_in_count} built-in plugins aren't shown."
                    )
                } else {
                    "Plugins add tools, integrations and agents to OpenCode's service.".to_string()
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            ),
        )
        .when_some(error, |this, error| this.child(render_error(error)))
        .child(list)
        .into_any_element()
}

/// What one entry of the config's `plugins` list is to this page.
#[derive(Clone, Copy, PartialEq)]
enum PluginEntry<'a> {
    /// A package or a path.
    Plugin { target: &'a str },
    /// `-<id>` right after a plugin, turning that plugin off.
    OffSwitch { id: &'a str },
    /// `-<id>` on its own, for a plugin configured elsewhere.
    Off { id: &'a str },
    /// Wildcards and built-in plugins' IDs, which stand on their own.
    Rule,
}

fn classify_entries(entries: &[Value]) -> Vec<PluginEntry<'_>> {
    let mut classified: Vec<PluginEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        let is_group = |id: &str| id == "*" || id.ends_with(".*");
        let class = match entry {
            Value::Object(object) => object
                .get("package")
                .and_then(Value::as_str)
                .map_or(PluginEntry::Rule, |target| PluginEntry::Plugin { target }),
            Value::String(text) => match text.strip_prefix('-') {
                None if is_group(text) => PluginEntry::Rule,
                None => PluginEntry::Plugin { target: text },
                Some(id) if is_group(id) || id.starts_with("opencode.") => PluginEntry::Rule,
                Some(id) if matches!(classified.last(), Some(PluginEntry::Plugin { .. })) => {
                    PluginEntry::OffSwitch { id }
                }
                Some(id) => PluginEntry::Off { id },
            },
            _ => PluginEntry::Rule,
        };
        classified.push(class);
    }
    classified
}

fn read_plugin_entries(config: &Value) -> Vec<Value> {
    config
        .get("plugins")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn build_plugin_rows(config: &Value, plugins: &[PluginInfo]) -> Vec<PluginRow> {
    let entries = read_plugin_entries(config);
    let classified = classify_entries(&entries);
    let describe = |plugin: &PluginInfo| {
        let version = match &plugin.source {
            PluginSource::Package { version, .. } => version.clone(),
            _ => None,
        };
        let status = match &plugin.state {
            PluginState::Failed { error } => PluginStatus::Failed(error.clone()),
            PluginState::Active => PluginStatus::Active,
        };
        (version, status)
    };
    let mut rows = Vec::new();
    for (index, class) in classified.iter().enumerate() {
        match *class {
            PluginEntry::Plugin { target } => {
                let off_id = match classified.get(index + 1) {
                    Some(PluginEntry::OffSwitch { id }) => Some(*id),
                    _ => None,
                };
                let plugin = plugins.iter().find(|plugin| matches_entry(plugin, target));
                let (detail, status) = match (off_id, plugin) {
                    (Some(_), plugin) => (
                        plugin.and_then(|plugin| describe(plugin).0),
                        PluginStatus::Off,
                    ),
                    (None, Some(plugin)) => describe(plugin),
                    (None, None) => (None, PluginStatus::NotLoaded),
                };
                rows.push(PluginRow {
                    name: target.to_string(),
                    detail,
                    status,
                    id: off_id
                        .map(str::to_string)
                        .or_else(|| plugin.and_then(|plugin| plugin.id.clone())),
                    config_entry: Some(target.to_string()),
                });
            }
            PluginEntry::Off { id } => rows.push(PluginRow {
                name: id.to_string(),
                detail: None,
                status: PluginStatus::Off,
                id: Some(id.to_string()),
                config_entry: None,
            }),
            PluginEntry::OffSwitch { .. } | PluginEntry::Rule => {}
        }
    }

    // Plugins discovered from a plugins folder, or set in a project's config.
    for plugin in plugins {
        let configured = classified.iter().any(|class| {
            matches!(class, PluginEntry::Plugin { target } if matches_entry(plugin, target))
        });
        if configured || matches!(plugin.source, PluginSource::Builtin) {
            continue;
        }
        let name = match &plugin.source {
            PluginSource::Package { target, .. } => target.clone(),
            PluginSource::Local { path } => path.clone(),
            _ => plugin.id.clone().unwrap_or_default(),
        };
        let (detail, status) = describe(plugin);
        rows.push(PluginRow {
            name,
            detail,
            status,
            id: plugin.id.clone(),
            config_entry: None,
        });
    }
    rows
}

fn matches_entry(plugin: &PluginInfo, target: &str) -> bool {
    match &plugin.source {
        PluginSource::Package {
            target: plugin_target,
            ..
        } => strip_version(plugin_target) == strip_version(target),
        PluginSource::Local { path } => {
            path == target || std::path::Path::new(path).ends_with(target.trim_start_matches("./"))
        }
        _ => false,
    }
}

/// `pkg@1.2.0` and `@scope/pkg` both name the package before any version.
fn strip_version(target: &str) -> &str {
    match target.rfind('@') {
        Some(index) if index > 0 => &target[..index],
        _ => target,
    }
}

fn render_plugin_row(row: PluginRow, cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    let id_prefix = row.name.clone();
    let status = match row.status {
        PluginStatus::Active => AiSettingItemStatus::Running,
        PluginStatus::Failed(_) => AiSettingItemStatus::Error,
        PluginStatus::Off => AiSettingItemStatus::Stopped,
        PluginStatus::NotLoaded => AiSettingItemStatus::Starting,
    };
    let source = if row.config_entry.is_some() {
        AiSettingItemSource::Custom
    } else {
        AiSettingItemSource::Extension
    };
    let is_on = row.status != PluginStatus::Off;
    let details = match &row.status {
        PluginStatus::Failed(error) => Some(render_error(error.clone().into()).into_any_element()),
        PluginStatus::NotLoaded => Some(
            Label::new("Installing or waiting to load.")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        ),
        _ => None,
    };
    let switch = row.id.clone().map(|id| {
        let config_entry = row.config_entry.clone();
        Switch::new(
            SharedString::from(format!("plugin-toggle-{id_prefix}")),
            is_on.into(),
        )
        .tab_index(0_isize)
        .aria_label(format!("Turn {} on or off", row.name))
        .on_click(cx.listener(move |this, state: &ToggleState, _window, cx| {
            let enabled = *state == ToggleState::Selected;
            let id = id.clone();
            let config_entry = config_entry.clone();
            write_plugins(
                this,
                Rc::new(move |config| {
                    set_plugin_enabled(config, &id, config_entry.as_deref(), enabled)
                }),
                cx,
            );
        }))
    });
    let remove = row.config_entry.clone().map(|target| {
        IconButton::new(
            SharedString::from(format!("plugin-remove-{id_prefix}")),
            IconName::Trash,
        )
        .icon_color(Color::Muted)
        .icon_size(IconSize::Small)
        .tab_index(0_isize)
        .tooltip(Tooltip::text("Remove Plugin"))
        .on_click(cx.listener(move |this, _, _window, cx| {
            let target = target.clone();
            write_plugins(
                this,
                Rc::new(move |config| remove_plugin(config, &target)),
                cx,
            );
        }))
    });
    AiSettingItem::new(
        SharedString::from(format!("plugin-{id_prefix}")),
        row.name,
        status,
        source,
    )
    .when_some(row.detail, |this, detail| this.detail_label(detail))
    .when_some(remove, |this, remove| this.action(remove))
    .when_some(switch, |this, switch| this.action(switch))
    .when_some(details, |this, details| this.details(details))
}

/// Turns a plugin off with `-<id>` right after its entry, or on by removing it.
fn set_plugin_enabled(
    config: &mut Value,
    id: &str,
    config_entry: Option<&str>,
    enabled: bool,
) -> Result<()> {
    let mut entries = read_plugin_entries(config);
    let off_switch = Value::String(format!("-{id}"));
    entries.retain(|entry| entry != &off_switch);
    if !enabled {
        let position = config_entry.and_then(|config_entry| {
            classify_entries(&entries).iter().position(
                |class| matches!(class, PluginEntry::Plugin { target } if *target == config_entry),
            )
        });
        match position {
            Some(position) => entries.insert(position + 1, off_switch),
            None => entries.push(off_switch),
        }
    }
    if entries.is_empty() {
        opencode_config::remove(config, &["plugins"]);
        return Ok(());
    }
    opencode_config::set(config, &["plugins"], Value::Array(entries))
}

/// Changes the plugins in the config file, then reloads while the service
/// installs them.
pub(super) fn write_plugins(
    settings_window: &mut SettingsWindow,
    edit: Rc<dyn Fn(&mut Value) -> anyhow::Result<()>>,
    cx: &mut Context<SettingsWindow>,
) {
    let saved = write_config(settings_window, Page::Plugins, edit, cx);
    cx.spawn(async move |this, cx| {
        // Nothing changed if the write failed, and a reload would clear its
        // error.
        if saved.await {
            this.update(cx, poll_while_installing).log_err();
        }
    })
    .detach();
}

/// The service reloads when it sees the config change and installs new
/// packages in the background, so the list is read again a few times. Each
/// change replaces the polling of the change before it.
fn poll_while_installing(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let executor = cx.background_executor().clone();
    settings.plugin_reloads = Some(cx.spawn(async move |this, cx| {
        for delay in [1, 2, 5, 10] {
            executor.timer(Duration::from_secs(delay)).await;
            if this.update(cx, |this, cx| reload(this, cx)).is_err() {
                return;
            }
        }
    }));
}

/// Removes a plugin's entry and the `-<id>` that follows it, if any.
fn remove_plugin(config: &mut Value, config_entry: &str) -> Result<()> {
    let mut entries = read_plugin_entries(config);
    let classified = classify_entries(&entries);
    if let Some(position) = classified.iter().position(
        |class| matches!(class, PluginEntry::Plugin { target } if *target == config_entry),
    ) {
        let has_off_switch = matches!(
            classified.get(position + 1),
            Some(PluginEntry::OffSwitch { .. })
        );
        let removed = if has_off_switch { 2 } else { 1 };
        entries.drain(position..position + removed);
    }
    if entries.is_empty() {
        opencode_config::remove(config, &["plugins"]);
        return Ok(());
    }
    opencode_config::set(config, &["plugins"], Value::Array(entries))
}

fn open_plugin_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let form = OneFieldForm {
        page_title: "Add Plugin",
        parent_page: "Plugins",
        field_title: "Package or Path",
        field_description: "An npm package with an optional version, a Git package, or a local plugin directory.",
        placeholder: Some("opencode-acme-plugin@1.2.0"),
        initial_text: String::new(),
        save_label: "Add Plugin",
        empty_error: "Enter a package or path.",
        save: Rc::new(|this, target, cx| {
            write_plugins(this, Rc::new(move |config| add_plugin(config, &target)), cx);
        }),
    };
    open_one_field_form(settings_window, form, window, cx);
}

/// A plugin already listed isn't added again.
fn add_plugin(config: &mut Value, config_entry: &str) -> Result<()> {
    let mut entries = read_plugin_entries(config);
    let listed = classify_entries(&entries)
        .iter()
        .any(|class| matches!(class, PluginEntry::Plugin { target } if *target == config_entry));
    if !listed {
        entries.push(Value::String(config_entry.to_string()));
    }
    opencode_config::set(config, &["plugins"], Value::Array(entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plugin(id: &str, target: &str) -> PluginInfo {
        serde_json::from_value(json!({
            "id": id,
            "source": { "type": "package", "target": target, "version": "1.0.0" },
            "state": { "status": "active" },
        }))
        .expect("plugin")
    }

    #[test]
    fn plugin_responses_parse() {
        let plugins: Vec<PluginInfo> = serde_json::from_value(json!([
            { "id": "a", "source": { "type": "builtin" }, "features": { "server": true }, "state": { "status": "active" } },
            { "id": "b", "source": { "type": "local", "path": "/p" }, "features": {}, "state": { "status": "failed", "error": "boom", "ref": "r" } },
            { "source": { "type": "package", "target": "c@1", "version": "1", "outdated": true }, "features": {}, "state": { "status": "active" } },
            { "id": "d", "source": { "type": "sdk" }, "features": {}, "state": { "status": "active" } },
        ]))
        .expect("plugins");
        assert_eq!(plugins.len(), 4);
    }

    #[test]
    fn built_in_plugins_are_left_out() {
        let built_in: PluginInfo = serde_json::from_value(json!({
            "id": "opencode.config.mcp",
            "source": { "type": "builtin" },
            "state": { "status": "active" },
        }))
        .expect("plugin");
        assert!(build_plugin_rows(&json!({}), &[built_in]).is_empty());
    }

    #[test]
    fn a_configured_plugin_combines_with_the_running_one() {
        let rows = build_plugin_rows(
            &json!({ "plugins": ["acme@1.0.0"] }),
            &[plugin("acme.id", "acme@1.0.0")],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PluginStatus::Active);
        assert_eq!(rows[0].id.as_deref(), Some("acme.id"));
        assert_eq!(rows[0].config_entry.as_deref(), Some("acme@1.0.0"));
    }

    #[test]
    fn a_plugin_still_loading_shows_as_not_loaded() {
        let rows = build_plugin_rows(&json!({ "plugins": ["acme"] }), &[]);
        assert_eq!(rows[0].status, PluginStatus::NotLoaded);
        assert_eq!(rows[0].id, None);
    }

    #[test]
    fn turning_off_and_on_keeps_the_entry_paired() {
        let mut config = json!({ "plugins": ["acme", "other"] });
        set_plugin_enabled(&mut config, "acme.id", Some("acme"), false).unwrap();
        assert_eq!(config["plugins"], json!(["acme", "-acme.id", "other"]));
        let rows = build_plugin_rows(&config, &[]);
        assert_eq!(rows[0].status, PluginStatus::Off);
        assert_eq!(rows[0].id.as_deref(), Some("acme.id"));
        assert_eq!(rows.len(), 2);

        set_plugin_enabled(&mut config, "acme.id", Some("acme"), true).unwrap();
        assert_eq!(config["plugins"], json!(["acme", "other"]));
    }

    #[test]
    fn rules_for_built_ins_are_not_rows() {
        let config = json!({ "plugins": ["*", "-opencode.provider.*", "-opencode.provider.openai-context-caps"] });
        assert!(build_plugin_rows(&config, &[]).is_empty());
    }

    #[test]
    fn removing_a_plugin_removes_its_disable_rule_and_the_empty_list() {
        let mut config = json!({ "plugins": ["acme", "-acme.id"] });
        remove_plugin(&mut config, "acme").unwrap();
        assert_eq!(config, json!({}));
    }

    #[test]
    fn removing_a_plugin_keeps_a_built_in_rule_after_it() {
        let mut config = json!({ "plugins": ["acme", "-opencode.provider.openai-context-caps"] });
        remove_plugin(&mut config, "acme").unwrap();
        assert_eq!(
            config["plugins"],
            json!(["-opencode.provider.openai-context-caps"])
        );
    }

    #[test]
    fn adding_a_plugin_twice_lists_it_once() {
        let mut config = json!({});
        add_plugin(&mut config, "acme").unwrap();
        add_plugin(&mut config, "acme").unwrap();
        assert_eq!(config["plugins"], json!(["acme"]));
    }

    #[test]
    fn versions_are_ignored_when_matching_scoped_packages() {
        assert_eq!(strip_version("@acme/plugin@1.2.0"), "@acme/plugin");
        assert_eq!(strip_version("@acme/plugin"), "@acme/plugin");
        assert_eq!(strip_version("plugin@latest"), "plugin");
    }
}
