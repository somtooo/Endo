//! OpenCode D's Providers sub-page and its connect and rename forms, on top of
//! the same service API as OpenCode's `/connect`.

use std::rc::Rc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    ClipboardItem, Entity, Focusable as _, PromptLevel, ScrollHandle, Subscription, Task,
    prelude::*,
};
use opencode_agent::{
    FormField, Integration, IntegrationConnection, IntegrationMethod, OAuthAttempt,
};
use serde_json::{Map, Value};
use ui::{Divider, LoadingLabel, Switch, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;

use super::opencode_settings_page::{
    Choice, OpenCodeSettings, reload, render_dropdown, render_error,
};
use crate::{SettingsWindow, components::SettingsSectionHeader};

/// Providers shown before searching, in the order OpenCode lists them.
const POPULAR: &[&str] = &[
    "opencode",
    "opencode-go",
    "openai",
    "github-copilot",
    "anthropic",
    "google",
];
const MAX_SEARCH_RESULTS: usize = 50;
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) struct RenameForm {
    credential_id: String,
    label: Entity<Editor>,
    error: Option<SharedString>,
}

struct FieldState {
    field: FormField,
    /// Text and number fields type into this; the others use `value`.
    editor: Option<Entity<Editor>>,
    value: Option<Value>,
}

enum ConnectStage {
    Form,
    Starting,
    OAuth {
        attempt: OAuthAttempt,
        code: Option<Entity<Editor>>,
    },
    Command {
        attempt_id: String,
        output: String,
    },
}

pub(crate) struct ConnectForm {
    integration: Integration,
    method_index: usize,
    fields: Vec<FieldState>,
    key: Entity<Editor>,
    stage: ConnectStage,
    error: Option<SharedString>,
    _task: Option<Task<()>>,
}

impl ConnectForm {
    fn methods(&self) -> Vec<&IntegrationMethod> {
        connect_methods(&self.integration)
    }

    fn method(&self) -> Option<&IntegrationMethod> {
        self.methods().get(self.method_index).copied()
    }
}

/// The ways to connect that need something from the user, with sign-ins first:
/// someone with a subscription rarely wants the API key route.
fn connect_methods(integration: &Integration) -> Vec<&IntegrationMethod> {
    let mut methods: Vec<&IntegrationMethod> = integration
        .methods
        .iter()
        .filter(|method| {
            !matches!(
                method,
                IntegrationMethod::Env { .. } | IntegrationMethod::Other
            )
        })
        .collect();
    methods.sort_by_key(|method| matches!(method, IntegrationMethod::Key { .. }));
    methods
}

fn method_label(method: &IntegrationMethod) -> String {
    match method {
        IntegrationMethod::OAuth { label, .. }
        | IntegrationMethod::Command { label, .. }
        | IntegrationMethod::External { label, .. } => label.clone(),
        IntegrationMethod::Key { label, .. } => label.clone().unwrap_or_else(|| "API Key".into()),
        IntegrationMethod::Env { .. } | IntegrationMethod::Other => String::new(),
    }
}

fn method_form(method: &IntegrationMethod) -> &[FormField] {
    match method {
        IntegrationMethod::OAuth { form, .. }
        | IntegrationMethod::Key { form, .. }
        | IntegrationMethod::External { form, .. } => form,
        _ => &[],
    }
}

/// Whether a field applies, given the answers before it.
fn field_is_active(field: &FormField, answers: &Map<String, Value>) -> bool {
    field.when.iter().all(|condition| {
        let Some(answer) = answers.get(&condition.key) else {
            return false;
        };
        let hit = match answer {
            Value::Array(selected) => selected.contains(&condition.value),
            other => other == &condition.value,
        };
        if condition.op == "eq" { hit } else { !hit }
    })
}

/// The value typed into a text or number field, as the form's type wants it.
fn typed_value(field: &FormField, text: &str) -> Result<Option<Value>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match field.kind.as_str() {
        "number" => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(|number| Some(Value::Number(number)))
            .ok_or_else(|| format!("{} must be a number.", field_title(field))),
        "integer" => text
            .parse::<i64>()
            .map(|number| Some(Value::from(number)))
            .map_err(|_| format!("{} must be a whole number.", field_title(field))),
        _ => Ok(Some(Value::String(text.to_string()))),
    }
}

fn field_title(field: &FormField) -> String {
    field.title.clone().unwrap_or_else(|| field.key.clone())
}

/// Reads the form in order, since a field's `when` depends on earlier answers.
fn collect_answers(
    fields: &[(&FormField, Option<String>, Option<Value>)],
) -> Result<Map<String, Value>, String> {
    let mut answers = Map::new();
    for (field, text, value) in fields {
        if field.kind == "external" || !field_is_active(field, &answers) {
            continue;
        }
        let answer = if field.hidden {
            field.default.clone()
        } else if let Some(text) = text {
            typed_value(field, text)?
        } else {
            value.clone()
        };
        match answer {
            Some(answer) => {
                answers.insert(field.key.clone(), answer);
            }
            None if field.required && !field.hidden => {
                return Err(format!("This field is required: {}", field_title(field)));
            }
            None => {}
        }
    }
    Ok(answers)
}

/// The device code OpenCode's instructions mention, which is what the user has
/// to copy for a device sign-in.
fn device_code(instructions: &str) -> Option<&str> {
    regex::Regex::new(r"[A-Z0-9]{4}-[A-Z0-9]{4,5}")
        .ok()?
        .find(instructions)
        .map(|found| found.as_str())
}

fn provider_rank(integration: &Integration) -> (usize, String) {
    (
        POPULAR
            .iter()
            .position(|id| *id == integration.id)
            .unwrap_or(POPULAR.len()),
        integration.name.to_lowercase(),
    )
}

/// Providers to offer: the popular ones until a search says otherwise.
fn available_providers<'a>(integrations: &'a [Integration], filter: &str) -> Vec<&'a Integration> {
    let filter = filter.trim().to_lowercase();
    let mut available: Vec<&Integration> = integrations
        .iter()
        .filter(|integration| {
            integration.connections.is_empty() && !connect_methods(integration).is_empty()
        })
        .filter(|integration| {
            if filter.is_empty() {
                POPULAR.contains(&integration.id.as_str())
            } else {
                integration.name.to_lowercase().contains(&filter)
                    || integration.id.to_lowercase().contains(&filter)
            }
        })
        .collect();
    available.sort_by_key(|integration| provider_rank(integration));
    available.truncate(MAX_SEARCH_RESULTS);
    available
}

fn provider_icon(id: &str) -> Option<IconName> {
    Some(match id {
        "anthropic" => IconName::AiAnthropic,
        "openai" => IconName::AiOpenAi,
        "google" | "google-vertex" => IconName::AiGoogle,
        "amazon-bedrock" => IconName::AiBedrock,
        "claude-agent" => IconName::AiClaude,
        "deepseek" => IconName::AiDeepSeek,
        "lmstudio" => IconName::AiLmStudio,
        "mistral" => IconName::AiMistral,
        "github-copilot" => IconName::Copilot,
        "ollama" => IconName::AiOllama,
        "opencode" | "opencode-go" => IconName::AiOpenCode,
        "openrouter" => IconName::AiOpenRouter,
        "vercel" => IconName::AiVercel,
        "xai" => IconName::AiXAi,
        _ => return None,
    })
}

pub(super) fn new_provider_filter(
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> (Entity<Editor>, Subscription) {
    let editor = cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text("Search all providers…", window, cx);
        editor
    });
    let subscription = cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
        if matches!(event, EditorEvent::BufferEdited) {
            cx.notify();
        }
    });
    (editor, subscription)
}

pub(super) fn open_providers_page(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    reload(settings_window, cx);
    settings_window.push_dynamic_sub_page(
        "Providers",
        "OpenCode D",
        None,
        false,
        render_providers_page,
        window,
        cx,
    );
}

pub(super) fn render_input(editor: &Entity<Editor>, cx: &App) -> AnyElement {
    let colors = cx.theme().colors();
    let focus_handle = editor.focus_handle(cx).tab_index(0).tab_stop(true);
    h_flex()
        .w_full()
        .min_w_64()
        .h_8()
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .track_focus(&focus_handle)
        .focus(|style| style.border_color(colors.border_focused))
        .child(editor.clone())
        .into_any_element()
}

pub(super) fn render_form_field(
    title: SharedString,
    description: Option<SharedString>,
    required: bool,
    control: AnyElement,
) -> AnyElement {
    v_flex()
        .w_full()
        .gap_1p5()
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(Label::new(title))
                        .when(required, |this| {
                            this.child(
                                Label::new("*")
                                    .size(LabelSize::Small)
                                    .color(Color::Error)
                                    .mb_2(),
                            )
                        }),
                )
                .when_some(description, |this, description| {
                    this.child(
                        Label::new(description)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                }),
        )
        .child(control)
        .into_any_element()
}

/// The footer Zed's settings forms use: any error, then Cancel and the primary
/// action.
pub(super) fn render_form_footer(
    error: Option<SharedString>,
    save_label: Option<&'static str>,
    busy: bool,
    cancel: impl Fn(&mut SettingsWindow, &mut Window, &mut Context<SettingsWindow>) + 'static,
    save: impl Fn(&mut SettingsWindow, &mut Window, &mut Context<SettingsWindow>) + 'static,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    v_flex()
        .px_8()
        .py_2p5()
        .gap_1()
        .border_t_1()
        .border_color(cx.theme().colors().border_variant)
        .when_some(error, |this, error| this.child(render_error(error)))
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("opencode-form-cancel", "Cancel")
                        .tab_index(0_isize)
                        .on_click(cx.listener(move |this, _, window, cx| cancel(this, window, cx))),
                )
                .when_some(save_label, |this, save_label| {
                    this.child(
                        Button::new("opencode-form-save", save_label)
                            .style(ButtonStyle::Filled)
                            .tab_index(0_isize)
                            .disabled(busy)
                            .on_click(
                                cx.listener(move |this, _, window, cx| save(this, window, cx)),
                            ),
                    )
                }),
        )
}

fn render_providers_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(settings) = settings_window.opencode_settings.as_ref() else {
        return div().into_any_element();
    };
    let filter = settings.provider_filter.read(cx).text(cx);
    let mut connected: Vec<Integration> = settings
        .integrations
        .iter()
        .filter(|integration| !integration.connections.is_empty())
        .cloned()
        .collect();
    connected.sort_by_key(provider_rank);
    let available: Vec<Integration> = available_providers(&settings.integrations, &filter)
        .into_iter()
        .cloned()
        .collect();
    let searching = !filter.trim().is_empty();
    let loading = settings.loading;
    let provider_filter = settings.provider_filter.clone();
    let error = settings.error.clone();

    let connected_list = if connected.is_empty() {
        h_flex()
            .p_4()
            .justify_center()
            .border_1()
            .border_dashed()
            .border_color(cx.theme().colors().border.opacity(0.6))
            .rounded_sm()
            .child(
                Label::new(if loading {
                    "Loading providers…"
                } else {
                    "No providers connected yet. Connect one below to get started."
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
                connected
                    .iter()
                    .map(|integration| render_connected_provider(integration, cx)),
                || Divider::horizontal().into_any_element(),
            ))
            .into_any_element()
    };

    let available_list = if available.is_empty() {
        Label::new(if searching && !loading {
            "No providers match your search."
        } else {
            ""
        })
        .size(LabelSize::Small)
        .color(Color::Muted)
        .into_any_element()
    } else {
        v_flex()
            .w_full()
            .gap_1()
            .children(itertools::intersperse_with(
                available
                    .iter()
                    .map(|integration| render_available_provider(integration, cx)),
                || Divider::horizontal().into_any_element(),
            ))
            .into_any_element()
    };

    v_flex()
        .id("opencode-providers-page")
        .size_full()
        .pt_2p5()
        .pb_16()
        .gap_4()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .when_some(error, |this, error| {
            this.child(div().px_8().child(render_error(error)))
        })
        .child(SettingsSectionHeader::new("Connected"))
        .child(div().px_8().child(connected_list))
        .child(SettingsSectionHeader::new(if searching {
            "Search Results"
        } else {
            "Popular"
        }))
        .child(
            v_flex()
                .px_8()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Icon::new(IconName::MagnifyingGlass)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(render_input(&provider_filter, cx)),
                )
                .child(available_list),
        )
        .into_any_element()
}

fn render_provider_identity(integration: &Integration, cx: &App) -> impl IntoElement {
    let icon = match provider_icon(&integration.id) {
        Some(icon) => Icon::new(icon)
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element(),
        None => {
            let letter = integration
                .name
                .chars()
                .next()
                .unwrap_or('?')
                .to_ascii_uppercase();
            h_flex()
                .size_5()
                .flex_none()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().element_active.opacity(0.2))
                .child(
                    Label::new(SharedString::from(letter.to_string()))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
                .into_any_element()
        }
    };
    h_flex()
        .min_w_0()
        .gap_2()
        .child(icon)
        .child(Label::new(integration.name.clone()).truncate())
}

fn render_available_provider(
    integration: &Integration,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let id = integration.id.clone();
    h_flex()
        .w_full()
        .min_w_0()
        .py_1()
        .justify_between()
        .child(render_provider_identity(integration, cx))
        .child(
            Button::new(SharedString::from(format!("connect-{id}")), "Connect")
                .aria_label(format!("Connect {}", integration.name))
                .style(ButtonStyle::Outlined)
                .label_size(LabelSize::Small)
                .tab_index(0_isize)
                .on_click(cx.listener(move |this, _, window, cx| {
                    open_connect_form(this, &id, window, cx);
                })),
        )
        .into_any_element()
}

fn connection_method_label(method: &str) -> &'static str {
    match method {
        "oauth" => "Signed In",
        "key" => "API Key",
        "external" => "External Login",
        _ => "Connected",
    }
}

fn render_connected_provider(
    integration: &Integration,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let id = integration.id.clone();
    let accounts = integration
        .connections
        .iter()
        .filter(|connection| matches!(connection, IntegrationConnection::Credential { .. }))
        .count();
    let can_add_account = !connect_methods(integration).is_empty();
    v_flex()
        .w_full()
        .min_w_0()
        .py_1()
        .gap_1()
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .justify_between()
                .child(render_provider_identity(integration, cx))
                .when(can_add_account, |this| {
                    this.child(
                        Button::new(
                            SharedString::from(format!("add-account-{id}")),
                            "Add Account",
                        )
                        .aria_label(format!("Add a {} account", integration.name))
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .tab_index(0_isize)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                open_connect_form(this, &id, window, cx);
                            },
                        )),
                    )
                }),
        )
        .children(
            integration
                .connections
                .iter()
                .enumerate()
                .map(|(index, connection)| {
                    // The service lists the account in use first.
                    render_connection(
                        &integration.name,
                        connection,
                        accounts > 1 && index == 0,
                        accounts > 1,
                        cx,
                    )
                }),
        )
        .into_any_element()
}

fn render_connection(
    provider_name: &str,
    connection: &IntegrationConnection,
    is_active: bool,
    can_switch: bool,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let row = h_flex().w_full().min_w_0().pl_7().gap_2().justify_between();
    match connection {
        IntegrationConnection::Credential {
            id,
            label,
            method,
            status,
        } => {
            let summary = format!("{label} · {}", connection_method_label(method));
            let switch_id = id.clone();
            let rename_id = id.clone();
            let rename_label = label.clone();
            let remove_id = id.clone();
            let remove_message = format!("Disconnect {label} from {provider_name}?");
            v_flex()
                .w_full()
                .child(
                    row.child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .child(
                                Label::new(summary)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .when(is_active, |this| {
                                this.child(
                                    Label::new("In Use")
                                        .size(LabelSize::Small)
                                        .color(Color::Success),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .when(can_switch && !is_active, |this| {
                                this.child(
                                    Button::new(SharedString::from(format!("use-{id}")), "Use")
                                        .style(ButtonStyle::Subtle)
                                        .label_size(LabelSize::Small)
                                        .tab_index(0_isize)
                                        .on_click(cx.listener(move |this, _, _window, cx| {
                                            let credential_id = switch_id.clone();
                                            run_account_action(
                                                this,
                                                cx,
                                                move |client| async move {
                                                    client.activate_credential(&credential_id).await
                                                },
                                            );
                                        })),
                                )
                            })
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("rename-{id}")),
                                    IconName::Pencil,
                                )
                                .icon_color(Color::Muted)
                                .icon_size(IconSize::Small)
                                .tab_index(0_isize)
                                .tooltip(Tooltip::text("Rename Account"))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        open_rename_form(
                                            this,
                                            rename_id.clone(),
                                            &rename_label,
                                            window,
                                            cx,
                                        );
                                    },
                                )),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!("disconnect-{id}")),
                                    IconName::Trash,
                                )
                                .icon_color(Color::Muted)
                                .icon_size(IconSize::Small)
                                .tab_index(0_isize)
                                .tooltip(Tooltip::text("Disconnect Account"))
                                .on_click(cx.listener(
                                    move |_this, _, window, cx| {
                                        let answer = window.prompt(
                                        PromptLevel::Warning,
                                        &remove_message,
                                        Some("Its saved key or sign-in is deleted from OpenCode."),
                                        &["Disconnect", "Cancel"],
                                        cx,
                                    );
                                        let credential_id = remove_id.clone();
                                        cx.spawn(async move |this, cx| {
                                            if answer.await != Ok(0) {
                                                return;
                                            }
                                            this.update(cx, |this, cx| {
                                                run_account_action(
                                                    this,
                                                    cx,
                                                    move |client| async move {
                                                        client
                                                            .remove_credential(&credential_id)
                                                            .await
                                                    },
                                                );
                                            })
                                            .log_err();
                                        })
                                        .detach();
                                    },
                                )),
                            ),
                    ),
                )
                .when_some(status.clone(), |this, status| {
                    this.child(div().pl_7().child(render_error(status.message.into())))
                })
                .into_any_element()
        }
        IntegrationConnection::Env { name, status } => v_flex()
            .w_full()
            .child(
                row.child(
                    Label::new(format!("From the {name} environment variable"))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .when_some(status.clone(), |this, status| {
                this.child(div().pl_7().child(render_error(status.message.into())))
            })
            .into_any_element(),
        IntegrationConnection::Other => div().into_any_element(),
    }
}

/// Runs an account change on the service, then reloads the page so it shows
/// the result, or the error if there was one.
fn run_account_action<F>(
    settings_window: &mut SettingsWindow,
    cx: &mut Context<SettingsWindow>,
    action: impl FnOnce(opencode_agent::OpenCodeClient) -> F + 'static,
) where
    F: Future<Output = anyhow::Result<()>> + 'static,
{
    let Some(client) = settings_window
        .opencode_settings
        .as_ref()
        .and_then(|settings| settings.client.clone())
    else {
        return;
    };
    cx.spawn(async move |this, cx| {
        let result = action(client).await;
        this.update(cx, |this, cx| match result {
            Ok(()) => reload(this, cx),
            Err(error) => {
                if let Some(settings) = this.opencode_settings.as_mut() {
                    settings.error = Some(format!("{error:#}").into());
                }
                cx.notify();
            }
        })
        .log_err();
    })
    .detach();
}

fn open_rename_form(
    settings_window: &mut SettingsWindow,
    credential_id: String,
    label: &str,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let editor = cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text(label, window, cx);
        editor
    });
    editor.focus_handle(cx).focus(window, cx);
    settings.rename_form = Some(RenameForm {
        credential_id,
        label: editor,
        error: None,
    });
    settings_window.push_dynamic_sub_page(
        "Rename Account",
        "Providers",
        None,
        false,
        render_rename_form_page,
        window,
        cx,
    );
}

fn render_rename_form_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(form) = settings_window
        .opencode_settings
        .as_ref()
        .and_then(|settings| settings.rename_form.as_ref())
    else {
        return div().into_any_element();
    };
    v_flex()
        .size_full()
        .child(
            v_flex()
                .id("opencode-rename-form-page")
                .track_scroll(scroll_handle)
                .flex_1()
                .pt_2p5()
                .px_8()
                .pb_16()
                .overflow_y_scroll()
                .child(render_form_field(
                    "Account Name".into(),
                    Some("A name to tell this account apart from your others.".into()),
                    true,
                    render_input(&form.label, cx),
                )),
        )
        .child(render_form_footer(
            form.error.clone(),
            Some("Save"),
            false,
            |this, window, cx| {
                if let Some(settings) = this.opencode_settings.as_mut() {
                    settings.rename_form = None;
                }
                this.pop_sub_page(window, cx);
            },
            save_rename_form,
            cx,
        ))
        .into_any_element()
}

fn save_rename_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let Some(form) = settings.rename_form.as_mut() else {
        return;
    };
    let label = form.label.read(cx).text(cx).trim().to_string();
    if label.is_empty() {
        form.error = Some("Enter a name.".into());
        cx.notify();
        return;
    }
    let credential_id = form.credential_id.clone();
    settings.rename_form = None;
    settings_window.pop_sub_page(window, cx);
    run_account_action(settings_window, cx, move |client| async move {
        client.rename_credential(&credential_id, &label).await
    });
}

fn open_connect_form(
    settings_window: &mut SettingsWindow,
    integration_id: &str,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let Some(integration) = settings
        .integrations
        .iter()
        .find(|integration| integration.id == integration_id)
        .cloned()
    else {
        return;
    };
    let title = format!("Connect {}", integration.name);
    let key = cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_masked(true, cx);
        editor.set_placeholder_text("Paste your key", window, cx);
        editor
    });
    clear_error_when_edited(&key, cx);
    let mut form = ConnectForm {
        integration,
        method_index: 0,
        fields: Vec::new(),
        key,
        stage: ConnectStage::Form,
        error: None,
        _task: None,
    };
    rebuild_fields(&mut form, window, cx);
    settings.connect_form = Some(form);
    settings_window.push_dynamic_sub_page(
        title,
        "Providers",
        None,
        false,
        render_connect_page,
        window,
        cx,
    );
}

fn clear_error_when_edited(editor: &Entity<Editor>, cx: &mut Context<SettingsWindow>) {
    cx.subscribe(editor, |this, _, event: &EditorEvent, cx| {
        if matches!(event, EditorEvent::BufferEdited) {
            if let Some(form) = this
                .opencode_settings
                .as_mut()
                .and_then(|settings| settings.connect_form.as_mut())
            {
                form.error = None;
            }
            cx.notify();
        }
    })
    .detach();
}

fn rebuild_fields(form: &mut ConnectForm, window: &mut Window, cx: &mut Context<SettingsWindow>) {
    let fields: Vec<FormField> = form
        .method()
        .map(|method| method_form(method).to_vec())
        .unwrap_or_default();
    form.fields = fields
        .into_iter()
        .map(|field| {
            let uses_editor = matches!(field.kind.as_str(), "string" | "number" | "integer")
                && field.options.is_empty();
            let default_text = field.default.as_ref().map(|default| match default {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            });
            let editor = uses_editor.then(|| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    if let Some(placeholder) = &field.placeholder {
                        editor.set_placeholder_text(placeholder.as_str(), window, cx);
                    }
                    if let Some(text) = &default_text {
                        editor.set_text(text.clone(), window, cx);
                    }
                    editor
                })
            });
            // Later fields can appear depending on what's typed, so this also
            // redraws the form.
            if let Some(editor) = &editor {
                clear_error_when_edited(editor, cx);
            }
            let value = if uses_editor {
                None
            } else {
                field.default.clone()
            };
            FieldState {
                field,
                editor,
                value,
            }
        })
        .collect();
}

/// The answers so far, used to decide which fields apply.
fn form_answers(form: &ConnectForm, cx: &App) -> Result<Map<String, Value>, String> {
    let fields: Vec<(&FormField, Option<String>, Option<Value>)> = form
        .fields
        .iter()
        .map(|state| {
            (
                &state.field,
                state.editor.as_ref().map(|editor| editor.read(cx).text(cx)),
                state.value.clone(),
            )
        })
        .collect();
    collect_answers(&fields)
}

fn set_field_value(
    settings: &mut OpenCodeSettings,
    index: usize,
    value: Value,
    cx: &mut Context<SettingsWindow>,
) {
    if let Some(state) = settings
        .connect_form
        .as_mut()
        .and_then(|form| form.fields.get_mut(index))
    {
        state.value = Some(value);
    }
    cx.notify();
}

fn render_connect_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(form) = settings_window
        .opencode_settings
        .as_ref()
        .and_then(|settings| settings.connect_form.as_ref())
    else {
        return div().into_any_element();
    };
    let error = form.error.clone();
    let body = v_flex()
        .id("opencode-connect-page")
        .track_scroll(scroll_handle)
        .flex_1()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_4()
        .overflow_y_scroll();

    match &form.stage {
        ConnectStage::Form | ConnectStage::Starting => {
            let busy = matches!(form.stage, ConnectStage::Starting);
            let save_label = match form.method() {
                Some(IntegrationMethod::OAuth { .. }) => "Continue",
                Some(IntegrationMethod::Command { .. }) => "Run",
                _ => "Connect",
            };
            let body = body.children(render_form_controls(form, window, cx));
            v_flex()
                .size_full()
                .child(body)
                .child(render_form_footer(
                    error,
                    Some(save_label),
                    busy,
                    |this, window, cx| close_connect_form(this, window, cx),
                    submit_connect_form,
                    cx,
                ))
                .into_any_element()
        }
        ConnectStage::OAuth { attempt, code } => {
            let url = attempt.url.clone();
            let copy_value: String = device_code(&attempt.instructions)
                .map(str::to_string)
                .unwrap_or_else(|| attempt.url.clone());
            let copy_label = if device_code(&attempt.instructions).is_some() {
                "Copy Code"
            } else {
                "Copy Link"
            };
            let body = body
                .child(Label::new(attempt.instructions.clone()))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("opencode-oauth-open", "Open in Browser")
                                .style(ButtonStyle::Filled)
                                .tab_index(0_isize)
                                .end_icon(
                                    Icon::new(IconName::ArrowUpRight)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                        .child(
                            Button::new("opencode-oauth-copy", copy_label)
                                .style(ButtonStyle::Outlined)
                                .tab_index(0_isize)
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_value.clone(),
                                    ));
                                }),
                        ),
                )
                .map(|this| match code {
                    Some(code) => this.child(render_form_field(
                        "Authorization Code".into(),
                        Some("Paste the code the provider gives you.".into()),
                        true,
                        render_input(code, cx),
                    )),
                    None => this.child(
                        LoadingLabel::new("Waiting for authorization")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                });
            let needs_code = code.is_some();
            v_flex()
                .size_full()
                .child(body)
                .child(render_form_footer(
                    error,
                    needs_code.then_some("Connect"),
                    false,
                    |this, window, cx| close_connect_form(this, window, cx),
                    submit_oauth_code,
                    cx,
                ))
                .into_any_element()
        }
        ConnectStage::Command { output, .. } => {
            let body = body
                .child(
                    LoadingLabel::new("Waiting for the command to finish")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .when(!output.is_empty(), |this| {
                    this.child(
                        Label::new(output.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                });
            v_flex()
                .size_full()
                .child(body)
                .child(render_form_footer(
                    error,
                    None,
                    false,
                    |this, window, cx| close_connect_form(this, window, cx),
                    |_, _, _| {},
                    cx,
                ))
                .into_any_element()
        }
    }
}

fn render_form_controls(
    form: &ConnectForm,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Vec<AnyElement> {
    let mut controls = Vec::new();
    let methods = form.methods();
    if methods.len() > 1 {
        let current = form.method().map(method_label).unwrap_or_default();
        let choices = methods
            .iter()
            .enumerate()
            .map(|(index, method)| Choice::Entry {
                label: method_label(method).into(),
                pick: Rc::new(move |this, window, cx| select_method(this, index, window, cx)),
            })
            .collect();
        controls.push(render_form_field(
            "Sign-In Method".into(),
            Some("How OpenCode connects to this provider.".into()),
            false,
            h_flex()
                .child(render_dropdown(
                    "Sign-In Method",
                    "How OpenCode connects to this provider.",
                    current.into(),
                    choices,
                    false,
                    window,
                    cx,
                ))
                .into_any_element(),
        ));
    }
    if matches!(form.method(), Some(IntegrationMethod::Key { .. })) {
        controls.push(render_form_field(
            "API Key".into(),
            Some("Stored by OpenCode's service, not in a config file.".into()),
            true,
            render_input(&form.key, cx),
        ));
    }
    if let Some(IntegrationMethod::Command { .. }) = form.method() {
        controls.push(
            Label::new("OpenCode runs this provider's sign-in command and saves the result.")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        );
    }
    let mut answers = Map::new();
    for (index, state) in form.fields.iter().enumerate() {
        let field = &state.field;
        let active = field.hidden || field.kind == "external" || field_is_active(field, &answers);
        let answer = state
            .editor
            .as_ref()
            .and_then(|editor| typed_value(field, &editor.read(cx).text(cx)).ok().flatten())
            .or_else(|| state.value.clone());
        if field.kind != "external"
            && field_is_active(field, &answers)
            && let Some(answer) = answer
        {
            answers.insert(field.key.clone(), answer);
        }
        if !active || field.hidden {
            continue;
        }
        controls.push(render_field(index, state, window, cx));
    }
    controls
}

fn render_field(
    index: usize,
    state: &FieldState,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let field = &state.field;
    let title: SharedString = field_title(field).into();
    let description: Option<SharedString> = field.description.clone().map(Into::into);
    if field.kind == "external" {
        let url = field.url.clone().unwrap_or_default();
        return render_form_field(
            title,
            description,
            false,
            Button::new(("opencode-field-link", index), "Open Link")
                .style(ButtonStyle::Outlined)
                .tab_index(0_isize)
                .on_click(move |_, _, cx| cx.open_url(&url))
                .into_any_element(),
        );
    }
    if let Some(editor) = &state.editor {
        return render_form_field(title, description, field.required, render_input(editor, cx));
    }
    let control = match field.kind.as_str() {
        "boolean" => {
            let is_on = state
                .value
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Switch::new(("opencode-field-switch", index), is_on.into())
                .tab_index(0_isize)
                .on_click(
                    cx.listener(move |this, toggled: &ToggleState, _window, cx| {
                        if let Some(settings) = this.opencode_settings.as_mut() {
                            let value = Value::Bool(*toggled == ToggleState::Selected);
                            set_field_value(settings, index, value, cx);
                        }
                    }),
                )
                .into_any_element()
        }
        "multiselect" => {
            let selected: Vec<String> = state
                .value
                .as_ref()
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            v_flex()
                .gap_1()
                .children(
                    field
                        .options
                        .iter()
                        .enumerate()
                        .map(|(option_index, option)| {
                            let value = option.value.clone();
                            let is_selected = selected.contains(&value);
                            let selected = selected.clone();
                            ui::Checkbox::new(
                                SharedString::from(format!(
                                    "opencode-field-{index}-{option_index}"
                                )),
                                is_selected.into(),
                            )
                            .label(option.label.clone())
                            .on_click(cx.listener(
                                move |this, toggled: &ToggleState, _window, cx| {
                                    let mut updated = selected.clone();
                                    updated.retain(|item| item != &value);
                                    if *toggled == ToggleState::Selected {
                                        updated.push(value.clone());
                                    }
                                    if let Some(settings) = this.opencode_settings.as_mut() {
                                        set_field_value(
                                            settings,
                                            index,
                                            serde_json::json!(updated),
                                            cx,
                                        );
                                    }
                                },
                            ))
                            .into_any_element()
                        }),
                )
                .into_any_element()
        }
        _ => {
            let current = state.value.as_ref().and_then(Value::as_str);
            let label: SharedString = field
                .options
                .iter()
                .find(|option| Some(option.value.as_str()) == current)
                .map(|option| option.label.clone())
                .or_else(|| current.map(str::to_string))
                .unwrap_or_else(|| "Choose…".to_string())
                .into();
            let choices = field
                .options
                .iter()
                .map(|option| {
                    let value = option.value.clone();
                    Choice::Entry {
                        label: option.label.clone().into(),
                        pick: Rc::new(move |this, _window, cx| {
                            if let Some(settings) = this.opencode_settings.as_mut() {
                                set_field_value(settings, index, Value::String(value.clone()), cx);
                            }
                        }),
                    }
                })
                .collect();
            h_flex()
                .id(("opencode-field-choice", index))
                .child(render_dropdown(
                    "Choice", "", label, choices, false, window, cx,
                ))
                .into_any_element()
        }
    };
    render_form_field(title, description, field.required, control)
}

fn select_method(
    settings_window: &mut SettingsWindow,
    index: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    if let Some(form) = settings_window
        .opencode_settings
        .as_mut()
        .and_then(|settings| settings.connect_form.as_mut())
    {
        form.method_index = index;
        form.error = None;
        rebuild_fields(form, window, cx);
    }
    cx.notify();
}

fn close_connect_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    if let Some(settings) = settings_window.opencode_settings.as_mut()
        && let Some(form) = settings.connect_form.take()
        && let Some(client) = settings.client.clone()
    {
        let directory = settings.directory.clone();
        let integration_id = form.integration.id.clone();
        match form.stage {
            ConnectStage::OAuth { attempt, .. } => {
                cx.background_spawn(async move {
                    client
                        .cancel_oauth(&directory, &integration_id, &attempt.attempt_id)
                        .await
                        .log_err();
                })
                .detach();
            }
            ConnectStage::Command { attempt_id, .. } => {
                cx.background_spawn(async move {
                    client
                        .cancel_command(&directory, &integration_id, &attempt_id)
                        .await
                        .log_err();
                })
                .detach();
            }
            _ => {}
        }
    }
    settings_window.pop_sub_page(window, cx);
}

/// Leaves the form after a successful connection. The form's own sub-page is
/// only popped if it's still showing, since the user may have gone back while a
/// sign-in finished in their browser.
fn finish_connecting(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let form_title = settings_window
        .opencode_settings
        .as_mut()
        .and_then(|settings| settings.connect_form.take())
        .map(|form| format!("Connect {}", form.integration.name));
    let showing_form = settings_window
        .sub_page_stack
        .last()
        .is_some_and(|sub_page| Some(sub_page.link.title.to_string()) == form_title);
    if showing_form {
        settings_window.pop_sub_page(window, cx);
    }
    if let Some(settings) = settings_window.opencode_settings.as_mut() {
        settings.error = None;
        // What was searched for is connected now, so the search is done.
        settings
            .provider_filter
            .update(cx, |filter, cx| filter.set_text("", window, cx));
    }
    reload(settings_window, cx);
}

fn fail_connecting(
    settings_window: &mut SettingsWindow,
    message: String,
    cx: &mut Context<SettingsWindow>,
) {
    if let Some(form) = settings_window
        .opencode_settings
        .as_mut()
        .and_then(|settings| settings.connect_form.as_mut())
    {
        form.stage = ConnectStage::Form;
        form.error = Some(message.into());
        form._task = None;
    }
    cx.notify();
}

/// The readable part of a failed request, which ends with the service's JSON.
fn request_error(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    text.find('{')
        .and_then(|start| serde_json::from_str::<Value>(&text[start..]).ok())
        .and_then(|body| {
            body.get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or(text)
}

fn submit_connect_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let Some(client) = settings.client.clone() else {
        return;
    };
    let directory = settings.directory.clone();
    let Some(form) = settings.connect_form.as_mut() else {
        return;
    };
    let answers = match form_answers(form, cx) {
        Ok(answers) => answers,
        Err(message) => {
            form.error = Some(message.into());
            cx.notify();
            return;
        }
    };
    let answer = (!answers.is_empty()).then_some(Value::Object(answers));
    let integration_id = form.integration.id.clone();
    let Some(method) = form.method().cloned() else {
        return;
    };
    let key = form.key.read(cx).text(cx).trim().to_string();
    if matches!(method, IntegrationMethod::Key { .. }) && key.is_empty() {
        form.error = Some("Enter your API key.".into());
        cx.notify();
        return;
    }
    form.stage = ConnectStage::Starting;
    form.error = None;
    let window_handle = window.window_handle();
    form._task = Some(cx.spawn(async move |this, cx| {
        let outcome = match method {
            IntegrationMethod::Key { .. } => client
                .connect_with_key(&directory, &integration_id, &key, answer.as_ref())
                .await
                .map(|()| None),
            IntegrationMethod::External { id, .. } => client
                .connect_external(&directory, &integration_id, &id, answer.as_ref())
                .await
                .map(|()| None),
            IntegrationMethod::OAuth { id, .. } => client
                .start_oauth(&directory, &integration_id, &id, answer.as_ref())
                .await
                .map(|attempt| Some(ConnectStarted::OAuth(attempt))),
            IntegrationMethod::Command { id, .. } => client
                .start_command(&directory, &integration_id, &id)
                .await
                .map(|attempt| Some(ConnectStarted::Command(attempt.attempt_id))),
            IntegrationMethod::Env { .. } | IntegrationMethod::Other => Ok(None),
        };
        window_handle
            .update(cx, |_, window, cx| {
                this.update(cx, |this, cx| match outcome {
                    Ok(None) => finish_connecting(this, window, cx),
                    Ok(Some(started)) => {
                        follow_attempt(this, started, client, directory, window, cx)
                    }
                    Err(error) => fail_connecting(this, request_error(&error), cx),
                })
                .log_err();
            })
            .log_err();
    }));
    cx.notify();
}

enum ConnectStarted {
    OAuth(OAuthAttempt),
    Command(String),
}

/// Shows a started sign-in and polls until the service says how it ended.
fn follow_attempt(
    settings_window: &mut SettingsWindow,
    started: ConnectStarted,
    client: opencode_agent::OpenCodeClient,
    directory: String,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(form) = settings_window
        .opencode_settings
        .as_mut()
        .and_then(|settings| settings.connect_form.as_mut())
    else {
        return;
    };
    let integration_id = form.integration.id.clone();
    let poll_attempt = match started {
        ConnectStarted::OAuth(attempt) => {
            let code = (attempt.mode == "code").then(|| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_masked(false, cx);
                    editor
                })
            });
            let attempt_id = attempt.attempt_id.clone();
            let needs_code = code.is_some();
            form.stage = ConnectStage::OAuth { attempt, code };
            // A code-based sign-in finishes when the code is submitted.
            (!needs_code).then_some((attempt_id, true))
        }
        ConnectStarted::Command(attempt_id) => {
            form.stage = ConnectStage::Command {
                attempt_id: attempt_id.clone(),
                output: String::new(),
            };
            Some((attempt_id, false))
        }
    };
    let window_handle = window.window_handle();
    form._task = poll_attempt.map(|(attempt_id, is_oauth)| {
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                let status = if is_oauth {
                    client
                        .oauth_status(&directory, &integration_id, &attempt_id)
                        .await
                } else {
                    client
                        .command_status(&directory, &integration_id, &attempt_id)
                        .await
                };
                let keep_polling = window_handle
                    .update(cx, |_, window, cx| {
                        this.update(cx, |this, cx| match status {
                            Ok(status) => match status.status.as_str() {
                                "pending" => {
                                    if let Some(ConnectStage::Command { output, .. }) = this
                                        .opencode_settings
                                        .as_mut()
                                        .and_then(|settings| settings.connect_form.as_mut())
                                        .map(|form| &mut form.stage)
                                        && let Some(message) = status.message
                                    {
                                        *output = message;
                                        cx.notify();
                                    }
                                    true
                                }
                                "complete" => {
                                    finish_connecting(this, window, cx);
                                    false
                                }
                                "failed" => {
                                    fail_connecting(
                                        this,
                                        status.message.unwrap_or_else(|| "Sign-in failed.".into()),
                                        cx,
                                    );
                                    false
                                }
                                _ => {
                                    fail_connecting(
                                        this,
                                        "The sign-in expired. Try again.".into(),
                                        cx,
                                    );
                                    false
                                }
                            },
                            Err(error) => {
                                fail_connecting(this, request_error(&error), cx);
                                false
                            }
                        })
                    })
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(false);
                if !keep_polling {
                    return;
                }
                executor.timer(POLL_INTERVAL).await;
            }
        })
    });
    cx.notify();
}

fn submit_oauth_code(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let Some(client) = settings.client.clone() else {
        return;
    };
    let directory = settings.directory.clone();
    let Some(form) = settings.connect_form.as_mut() else {
        return;
    };
    let ConnectStage::OAuth {
        attempt,
        code: Some(code),
    } = &form.stage
    else {
        return;
    };
    let code_text = code.read(cx).text(cx).trim().to_string();
    if code_text.is_empty() {
        form.error = Some("Enter the code.".into());
        cx.notify();
        return;
    }
    let attempt_id = attempt.attempt_id.clone();
    let integration_id = form.integration.id.clone();
    form.error = None;
    let window_handle = window.window_handle();
    form._task = Some(cx.spawn(async move |this, cx| {
        let result = client
            .complete_oauth(&directory, &integration_id, &attempt_id, &code_text)
            .await;
        window_handle
            .update(cx, |_, window, cx| {
                this.update(cx, |this, cx| match result {
                    Ok(()) => finish_connecting(this, window, cx),
                    // The attempt is still open, so the user can correct the
                    // code instead of starting over.
                    Err(error) => {
                        if let Some(form) = this
                            .opencode_settings
                            .as_mut()
                            .and_then(|settings| settings.connect_form.as_mut())
                        {
                            form.error = Some(request_error(&error).into());
                        }
                        cx.notify();
                    }
                })
                .log_err();
            })
            .log_err();
    }));
    cx.notify();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn integration(value: Value) -> Integration {
        serde_json::from_value(value).expect("integration")
    }

    fn field(value: Value) -> FormField {
        serde_json::from_value(value).expect("field")
    }

    #[test]
    fn sign_ins_come_before_api_keys_and_env_is_left_out() {
        let openai = integration(json!({
            "id": "openai",
            "name": "OpenAI",
            "methods": [
                { "type": "key" },
                { "type": "env", "names": ["OPENAI_API_KEY"] },
                { "type": "oauth", "id": "chatgpt", "label": "Sign in with ChatGPT" },
            ],
            "connections": [],
        }));
        let labels: Vec<String> = connect_methods(&openai)
            .into_iter()
            .map(method_label)
            .collect();
        assert_eq!(labels, ["Sign in with ChatGPT", "API Key"]);
    }

    #[test]
    fn only_popular_providers_show_until_a_search() {
        let integrations = vec![
            integration(json!({
                "id": "anthropic", "name": "Anthropic", "methods": [{ "type": "key" }],
            })),
            integration(json!({
                "id": "acme", "name": "Acme AI", "methods": [{ "type": "key" }],
            })),
            integration(json!({
                "id": "connected", "name": "Connected AI", "methods": [{ "type": "key" }],
                "connections": [{ "type": "env", "name": "X" }],
            })),
            integration(json!({
                "id": "envonly", "name": "Env Only",
                "methods": [{ "type": "env", "names": ["X"] }],
            })),
        ];
        let names = |filter: &str| -> Vec<String> {
            available_providers(&integrations, filter)
                .into_iter()
                .map(|integration| integration.name.clone())
                .collect()
        };
        assert_eq!(names(""), ["Anthropic"]);
        assert_eq!(names("acme"), ["Acme AI"]);
        assert!(names("connected").is_empty());
        assert!(names("env only").is_empty());
    }

    #[test]
    fn required_fields_are_checked_and_hidden_ones_use_their_default() {
        let required = field(
            json!({ "key": "resource", "type": "string", "title": "Resource", "required": true }),
        );
        let hidden =
            field(json!({ "key": "mode", "type": "string", "hidden": true, "default": "auto" }));
        let empty = collect_answers(&[(&required, Some("  ".into()), None)]);
        assert_eq!(empty.unwrap_err(), "This field is required: Resource");

        let answers = collect_answers(&[
            (&required, Some("mine".into()), None),
            (&hidden, Some(String::new()), None),
        ])
        .unwrap();
        assert_eq!(answers["resource"], "mine");
        assert_eq!(answers["mode"], "auto");
    }

    #[test]
    fn fields_only_apply_when_their_condition_holds() {
        let deployment = field(json!({ "key": "deployment", "type": "string", "options": [] }));
        let domain = field(json!({
            "key": "domain", "type": "string", "required": true,
            "when": [{ "key": "deployment", "op": "eq", "value": "enterprise" }],
        }));
        let hosted = collect_answers(&[
            (&deployment, None, Some(json!("github.com"))),
            (&domain, Some(String::new()), None),
        ])
        .unwrap();
        assert!(!hosted.contains_key("domain"));

        let enterprise = collect_answers(&[
            (&deployment, None, Some(json!("enterprise"))),
            (&domain, Some(String::new()), None),
        ]);
        assert!(enterprise.is_err());
    }

    #[test]
    fn numbers_are_validated() {
        let port = field(json!({ "key": "port", "type": "integer", "title": "Port" }));
        assert_eq!(typed_value(&port, "8080").unwrap(), Some(json!(8080)));
        assert_eq!(typed_value(&port, "").unwrap(), None);
        assert_eq!(
            typed_value(&port, "x").unwrap_err(),
            "Port must be a whole number."
        );
    }

    #[test]
    fn device_codes_are_found_in_instructions() {
        assert_eq!(
            device_code("Enter code ABCD-1234 at github"),
            Some("ABCD-1234")
        );
        assert_eq!(device_code("Open the link to sign in"), None);
    }

    #[test]
    fn request_errors_show_the_service_message() {
        let error = anyhow::anyhow!(
            "OpenCode request /x failed with status 400: {{\"message\":\"Bad key\",\"kind\":\"integration_authorization\"}}"
        );
        assert_eq!(request_error(&error), "Bad key");
        assert_eq!(request_error(&anyhow::anyhow!("offline")), "offline");
    }
}
