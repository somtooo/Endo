//! OpenCode D's Plugins sub-page.
//!
//! The service only lists plugins it's running, so rows combine that list with
//! the `plugins` entries in the global config. Turning a plugin off adds
//! `-<id>` directly after its entry, which is how OpenCode disables a plugin
//! and how this page knows which entry the `-<id>` belongs to.

use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use editor::{Editor, EditorEvent};
use gpui::{Entity, Focusable as _, ScrollHandle, prelude::*};
use opencode_agent::{PluginInfo, PluginSource, opencode_config};
use serde_json::Value;
use ui::{
    AiSettingItem, AiSettingItemSource, AiSettingItemStatus, Switch, ToggleState, Tooltip,
    prelude::*,
};

use super::opencode_settings_page::{reload, render_error, write_config};
use crate::SettingsWindow;

/// The Add Plugin form's state.
pub(crate) struct PluginForm {
    package: Entity<Editor>,
    error: Option<SharedString>,
}

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

fn entry_target(entry: &Value) -> Option<&str> {
    match entry {
        Value::String(target) => Some(target),
        Value::Object(object) => object.get("package")?.as_str(),
        _ => None,
    }
}

/// `pkg@1.2.0` and `@scope/pkg` both name the package before any version.
fn without_version(target: &str) -> &str {
    match target.rfind('@') {
        Some(index) if index > 0 => &target[..index],
        _ => target,
    }
}

fn matches_entry(plugin: &PluginInfo, target: &str) -> bool {
    match &plugin.source {
        PluginSource::Package {
            target: plugin_target,
            ..
        } => without_version(plugin_target) == without_version(target),
        PluginSource::Local { path } => {
            path == target || std::path::Path::new(path).ends_with(target.trim_start_matches("./"))
        }
        _ => false,
    }
}

fn is_rule(entry: &str) -> bool {
    entry == "*" || entry.ends_with(".*") || entry.starts_with('-')
}

fn plugin_rows(config: &Value, plugins: &[PluginInfo]) -> Vec<PluginRow> {
    let entries = config
        .get("plugins")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut rows = Vec::new();
    let mut listed = vec![false; plugins.len()];
    let mut paired_disables = vec![false; entries.len()];

    for (index, entry) in entries.iter().enumerate() {
        let Some(target) = entry_target(entry).filter(|target| !is_rule(target)) else {
            continue;
        };
        let disabled_id = entries
            .get(index + 1)
            .and_then(Value::as_str)
            .and_then(|next| next.strip_prefix('-'))
            .filter(|next| !next.ends_with(".*") && !next.starts_with("opencode."));
        if disabled_id.is_some() {
            paired_disables[index + 1] = true;
        }
        let running = plugins
            .iter()
            .position(|plugin| matches_entry(plugin, target));
        if let Some(position) = running {
            listed[position] = true;
        }
        let plugin = running.and_then(|position| plugins.get(position));
        rows.push(PluginRow {
            name: target.to_string(),
            detail: plugin.and_then(plugin_version),
            status: match (disabled_id, plugin) {
                (Some(_), _) => PluginStatus::Off,
                (None, Some(plugin)) => plugin_status(plugin),
                (None, None) => PluginStatus::NotLoaded,
            },
            id: disabled_id
                .map(str::to_string)
                .or_else(|| plugin.and_then(|plugin| plugin.id.clone())),
            config_entry: Some(target.to_string()),
        });
    }

    for (index, entry) in entries.iter().enumerate() {
        let Some(id) = entry.as_str().and_then(|entry| entry.strip_prefix('-')) else {
            continue;
        };
        if paired_disables[index] || id.ends_with(".*") || id == "*" || id.starts_with("opencode.")
        {
            continue;
        }
        rows.push(PluginRow {
            name: id.to_string(),
            detail: None,
            status: PluginStatus::Off,
            id: Some(id.to_string()),
            config_entry: None,
        });
    }

    // Plugins discovered from a plugins folder, or set in a project's config.
    for (position, plugin) in plugins.iter().enumerate() {
        if listed[position] || matches!(plugin.source, PluginSource::Builtin) {
            continue;
        }
        let name = match &plugin.source {
            PluginSource::Package { target, .. } => target.clone(),
            PluginSource::Local { path } => path.clone(),
            _ => plugin.id.clone().unwrap_or_default(),
        };
        rows.push(PluginRow {
            name,
            detail: plugin_version(plugin),
            status: plugin_status(plugin),
            id: plugin.id.clone(),
            config_entry: None,
        });
    }
    rows
}

fn plugin_version(plugin: &PluginInfo) -> Option<String> {
    match &plugin.source {
        PluginSource::Package { version, .. } => version.clone(),
        _ => None,
    }
}

fn plugin_status(plugin: &PluginInfo) -> PluginStatus {
    if plugin.state.status == "failed" {
        PluginStatus::Failed(plugin.state.error.clone().unwrap_or_default())
    } else {
        PluginStatus::Active
    }
}

fn plugin_entries(text: &str) -> Result<Vec<Value>> {
    Ok(opencode_config::parse(text)?
        .get("plugins")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn write_entries(text: &str, entries: Vec<Value>) -> Result<String> {
    let value = (!entries.is_empty()).then_some(Value::Array(entries));
    opencode_config::set_value(text, &["plugins"], value.as_ref())
}

/// Turns a plugin off with `-<id>` right after its entry, or on by removing it.
fn set_plugin_enabled(
    text: &str,
    id: &str,
    config_entry: Option<&str>,
    enabled: bool,
) -> Result<String> {
    let mut entries = plugin_entries(text)?;
    let rule = Value::String(format!("-{id}"));
    entries.retain(|entry| entry != &rule);
    if !enabled {
        let position = config_entry.and_then(|target| {
            entries
                .iter()
                .position(|entry| entry_target(entry) == Some(target))
        });
        match position {
            Some(position) => entries.insert(position + 1, rule),
            None => entries.push(rule),
        }
    }
    write_entries(text, entries)
}

fn add_plugin(text: &str, target: &str) -> Result<String> {
    let mut entries = plugin_entries(text)?;
    if !entries
        .iter()
        .any(|entry| entry_target(entry) == Some(target))
    {
        entries.push(Value::String(target.to_string()));
    }
    write_entries(text, entries)
}

/// Removes a plugin's entry and the `-<id>` that follows it, if any.
fn remove_plugin(text: &str, target: &str) -> Result<String> {
    let mut entries = plugin_entries(text)?;
    if let Some(position) = entries
        .iter()
        .position(|entry| entry_target(entry) == Some(target))
    {
        entries.remove(position);
        if entries
            .get(position)
            .and_then(Value::as_str)
            .is_some_and(|next| next.starts_with('-') && !next.ends_with(".*"))
        {
            entries.remove(position);
        }
    }
    write_entries(text, entries)
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
    let config = opencode_config::parse(&settings.config_text).unwrap_or(Value::Null);
    let rows = plugin_rows(&config, &settings.plugins);
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
        .when_some(settings.error.clone(), |this, error| {
            this.child(render_error(error))
        })
        .child(list)
        .into_any_element()
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
            write_config(
                this,
                Rc::new(move |text| {
                    set_plugin_enabled(text, &id, config_entry.as_deref(), enabled)
                }),
                cx,
            );
            reload_after_service_notices(cx);
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
            write_config(this, Rc::new(move |text| remove_plugin(text, &target)), cx);
            reload_after_service_notices(cx);
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

/// The service reloads when it sees the config change and installs new
/// packages in the background, so the list is read again a few times.
fn reload_after_service_notices(cx: &mut Context<SettingsWindow>) {
    let executor = cx.background_executor().clone();
    cx.spawn(async move |this, cx| {
        for delay in [1, 2, 5, 10] {
            executor.timer(Duration::from_secs(delay)).await;
            if this.update(cx, |this, cx| reload(this, cx)).is_err() {
                return;
            }
        }
    })
    .detach();
}

fn open_plugin_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let package = cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text("opencode-acme-plugin@1.2.0", window, cx);
        editor
    });
    cx.subscribe(&package, |this, _, event: &EditorEvent, cx| {
        if matches!(event, EditorEvent::BufferEdited)
            && let Some(form) = this
                .opencode_settings
                .as_mut()
                .and_then(|settings| settings.plugin_form.as_mut())
        {
            form.error = None;
            cx.notify();
        }
    })
    .detach();
    package.focus_handle(cx).focus(window, cx);
    settings.plugin_form = Some(PluginForm {
        package,
        error: None,
    });
    settings_window.push_dynamic_sub_page(
        "Add Plugin",
        "Plugins",
        None,
        false,
        render_plugin_form_page,
        window,
        cx,
    );
}

fn render_plugin_form_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(form) = settings_window
        .opencode_settings
        .as_ref()
        .and_then(|settings| settings.plugin_form.as_ref())
    else {
        return div().into_any_element();
    };
    let body = v_flex()
        .id("opencode-plugin-form-page")
        .track_scroll(scroll_handle)
        .flex_1()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_4()
        .overflow_y_scroll()
        .child(super::opencode_providers_page::render_form_field(
            "Package or Path".into(),
            Some(
                "An npm package with an optional version, a Git package, or a local plugin directory."
                    .into(),
            ),
            true,
            super::opencode_providers_page::render_input(&form.package, cx),
        ));
    v_flex()
        .size_full()
        .child(body)
        .child(super::opencode_providers_page::render_form_footer(
            form.error.clone(),
            Some("Add Plugin"),
            false,
            |this, window, cx| {
                if let Some(settings) = this.opencode_settings.as_mut() {
                    settings.plugin_form = None;
                }
                this.pop_sub_page(window, cx);
            },
            save_plugin_form,
            cx,
        ))
        .into_any_element()
}

fn save_plugin_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(form) = settings_window
        .opencode_settings
        .as_mut()
        .and_then(|settings| settings.plugin_form.as_mut())
    else {
        return;
    };
    let target = form.package.read(cx).text(cx).trim().to_string();
    if target.is_empty() {
        form.error = Some("Enter a package or path.".into());
        cx.notify();
        return;
    }
    write_config(
        settings_window,
        Rc::new(move |text| add_plugin(text, &target)),
        cx,
    );
    if let Some(settings) = settings_window.opencode_settings.as_mut() {
        settings.plugin_form = None;
    }
    settings_window.pop_sub_page(window, cx);
    reload_after_service_notices(cx);
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
        assert!(plugin_rows(&json!({}), &[built_in]).is_empty());
    }

    #[test]
    fn a_configured_plugin_combines_with_the_running_one() {
        let rows = plugin_rows(
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
        let rows = plugin_rows(&json!({ "plugins": ["acme"] }), &[]);
        assert_eq!(rows[0].status, PluginStatus::NotLoaded);
        assert_eq!(rows[0].id, None);
    }

    #[test]
    fn turning_off_and_on_keeps_the_entry_paired() {
        let text = "{\n  \"plugins\": [\"acme\", \"other\"]\n}\n";
        let off = set_plugin_enabled(text, "acme.id", Some("acme"), false).unwrap();
        let config = opencode_config::parse(&off).unwrap();
        assert_eq!(config["plugins"], json!(["acme", "-acme.id", "other"]));
        let rows = plugin_rows(&config, &[]);
        assert_eq!(rows[0].status, PluginStatus::Off);
        assert_eq!(rows[0].id.as_deref(), Some("acme.id"));
        assert_eq!(rows.len(), 2);

        let on = set_plugin_enabled(&off, "acme.id", Some("acme"), true).unwrap();
        assert_eq!(
            opencode_config::parse(&on).unwrap()["plugins"],
            json!(["acme", "other"])
        );
    }

    #[test]
    fn rules_for_built_ins_are_not_rows() {
        let config = json!({ "plugins": ["*", "-opencode.provider.*", "-opencode.provider.openai-context-caps"] });
        assert!(plugin_rows(&config, &[]).is_empty());
    }

    #[test]
    fn removing_a_plugin_removes_its_disable_rule_and_the_empty_list() {
        let text = "{\n  // Mine\n  \"plugins\": [\"acme\", \"-acme.id\"]\n}\n";
        let updated = remove_plugin(text, "acme").unwrap();
        assert!(updated.contains("// Mine"));
        assert!(
            opencode_config::parse(&updated)
                .unwrap()
                .get("plugins")
                .is_none()
        );
    }

    #[test]
    fn adding_a_plugin_twice_lists_it_once() {
        let once = add_plugin("", "acme").unwrap();
        let twice = add_plugin(&once, "acme").unwrap();
        assert_eq!(
            opencode_config::parse(&twice).unwrap()["plugins"],
            json!(["acme"])
        );
    }

    #[test]
    fn versions_are_ignored_when_matching_scoped_packages() {
        assert_eq!(without_version("@acme/plugin@1.2.0"), "@acme/plugin");
        assert_eq!(without_version("@acme/plugin"), "@acme/plugin");
        assert_eq!(without_version("plugin@latest"), "plugin");
    }
}
