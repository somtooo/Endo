//! OpenCode D's Providers sub-page and its connect and rename forms, on top of
//! the same service API as OpenCode's `/connect`.

use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use futures::FutureExt as _;
use futures::future::LocalBoxFuture;
use gpui::{
    ClipboardItem, Entity, Focusable as _, PromptLevel, ScrollHandle, Subscription, Task,
    prelude::*,
};
use opencode_agent::{
    AttemptStatus, CredentialMethod, FormField, FormFieldKind, Integration, IntegrationConnection,
    IntegrationMethod, OAuthAttempt, OAuthMode, describe_error,
};
use regex::Regex;
use serde_json::{Map, Value};
use ui::{Divider, LoadingLabel, Switch, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;

use super::opencode_settings_page::{
    Choice, OpenCodeSettings, is_showing_opencode_settings, open_opencode_settings_from_anywhere,
    reload, render_dropdown, render_error,
};
use crate::{SettingsWindow, components::SettingsSectionHeader};

/// Providers shown before searching, in this order. The list is our own
/// choice, copied from the ones OpenCode's connect dialog puts first: the API
/// doesn't say which providers are popular.
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

/// A form with one text field, like Rename Account and Add Plugin.
pub(super) struct OneFieldForm {
    pub(super) page_title: &'static str,
    pub(super) parent_page: &'static str,
    pub(super) field_title: &'static str,
    pub(super) field_description: &'static str,
    pub(super) placeholder: Option<&'static str>,
    pub(super) initial_text: String,
    pub(super) save_label: &'static str,
    /// Shown when the field is saved empty.
    pub(super) empty_error: &'static str,
    /// Gets the trimmed text, after the form has closed.
    pub(super) save: Rc<dyn Fn(&mut SettingsWindow, String, &mut Context<SettingsWindow>)>,
}

pub(crate) struct OneFieldFormState {
    form: OneFieldForm,
    editor: Entity<Editor>,
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
    /// The title of the form's own sub-page.
    page_title: SharedString,
    methods: Vec<IntegrationMethod>,
    method_index: usize,
    fields: Vec<FieldState>,
    key: Entity<Editor>,
    stage: ConnectStage,
    error: Option<SharedString>,
    _task: Option<Task<()>>,
}

impl ConnectForm {
    fn method(&self) -> Option<&IntegrationMethod> {
        self.methods.get(self.method_index)
    }

    fn read_answers(&self, cx: &App) -> FormAnswers {
        let inputs: Vec<FieldInput> = self
            .fields
            .iter()
            .map(|state| FieldInput {
                field: &state.field,
                text: state.editor.as_ref().map(|editor| editor.read(cx).text(cx)),
                value: state.value.clone(),
            })
            .collect();
        FormAnswers::read(&inputs)
    }
}

/// One field of the connect form as the user left it.
struct FieldInput<'a> {
    field: &'a FormField,
    /// What's typed, for the fields typed into.
    text: Option<String>,
    value: Option<Value>,
}

/// What the connect form's fields hold. They're read in order, since a
/// field's `when` depends on the answers before it.
struct FormAnswers {
    answers: Map<String, Value>,
    /// For each field, whether its `when` holds.
    active: Vec<bool>,
    /// The first answer that's missing or doesn't parse. It stops the form
    /// from being sent.
    problem: Option<String>,
}

impl FormAnswers {
    fn read(inputs: &[FieldInput]) -> Self {
        let mut answers = Map::new();
        let mut active = Vec::with_capacity(inputs.len());
        let mut problem = None;
        for input in inputs {
            let field = input.field;
            let is_active = field_is_active(field, &answers);
            active.push(is_active);
            // A link has no answer.
            if !is_active || field.kind == FormFieldKind::External {
                continue;
            }
            let answer = if field.hidden {
                Ok(field.default.clone())
            } else if let Some(text) = &input.text {
                parse_typed_value(field, text)
            } else {
                Ok(input.value.clone())
            };
            match answer {
                Ok(Some(answer)) => {
                    answers.insert(field.key.clone(), answer);
                }
                Ok(None) if field.required && !field.hidden => {
                    problem.get_or_insert_with(|| {
                        format!("This field is required: {}", format_field_title(field))
                    });
                }
                Ok(None) => {}
                Err(message) => {
                    problem.get_or_insert(message);
                }
            }
        }
        Self {
            answers,
            active,
            problem,
        }
    }
}

/// The ways to connect that need something from the user, with sign-ins first:
/// someone with a subscription rarely wants the API key route.
fn list_connect_methods(integration: &Integration) -> Vec<&IntegrationMethod> {
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

/// Opens from anywhere: when OpenCode D's page isn't showing, it opens
/// first with its parents behind it, so the back button walks up the way it
/// would have from clicking through.
pub(crate) fn open_providers_page(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    if is_showing_opencode_settings(settings_window) {
        reload(settings_window, cx);
    } else {
        open_opencode_settings_from_anywhere(settings_window, window, cx);
    }
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
    connected.sort_by_key(rank_provider);
    let available: Vec<Integration> = list_available_providers(&settings.integrations, &filter)
        .into_iter()
        .cloned()
        .collect();
    let searching = !filter.trim().is_empty();
    let loading = settings.loading;
    let provider_filter = settings.provider_filter.clone();
    let error = settings.errors.providers.clone();

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

/// Popular providers first, then by name.
fn rank_provider(integration: &Integration) -> (usize, String) {
    (
        POPULAR
            .iter()
            .position(|id| *id == integration.id)
            .unwrap_or(POPULAR.len()),
        integration.name.to_lowercase(),
    )
}

/// Providers to offer: the popular ones until a search says otherwise.
fn list_available_providers<'a>(
    integrations: &'a [Integration],
    filter: &str,
) -> Vec<&'a Integration> {
    let filter = filter.trim().to_lowercase();
    let mut available: Vec<&Integration> = integrations
        .iter()
        .filter(|integration| {
            integration.connections.is_empty() && !list_connect_methods(integration).is_empty()
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
    available.sort_by_key(|integration| rank_provider(integration));
    available.truncate(MAX_SEARCH_RESULTS);
    available
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

fn render_provider_identity(integration: &Integration, cx: &App) -> impl IntoElement {
    let icon = match opencode_agent::find_provider_icon(&integration.id) {
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
    let can_add_account = !list_connect_methods(integration).is_empty();
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
        .children({
            // The service lists the account in use first among the accounts.
            let first_account = integration.connections.iter().position(|connection| {
                matches!(connection, IntegrationConnection::Credential { .. })
            });
            integration
                .connections
                .iter()
                .enumerate()
                .map(move |(index, connection)| {
                    let is_first_account = Some(index) == first_account;
                    render_connection(
                        &integration.name,
                        connection,
                        accounts > 1 && is_first_account,
                        accounts > 1,
                        cx,
                    )
                })
        })
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
            let method_label = match method {
                CredentialMethod::OAuth => "Signed In",
                CredentialMethod::Key => "API Key",
                CredentialMethod::External => "External Login",
                CredentialMethod::Other => "Connected",
            };
            let summary = format!("{label} · {method_label}");
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
                    settings.errors.providers = Some(describe_error(&error).into());
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
    let form = OneFieldForm {
        page_title: "Rename Account",
        parent_page: "Providers",
        field_title: "Account Name",
        field_description: "A name to tell this account apart from your others.",
        placeholder: None,
        initial_text: label.to_string(),
        save_label: "Save",
        empty_error: "Enter a name.",
        save: Rc::new(move |this, label, cx| {
            let credential_id = credential_id.clone();
            run_account_action(this, cx, move |client| async move {
                client.rename_credential(&credential_id, &label).await
            });
        }),
    };
    open_one_field_form(settings_window, form, window, cx);
}

pub(super) fn open_one_field_form(
    settings_window: &mut SettingsWindow,
    form: OneFieldForm,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let editor = cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        if let Some(placeholder) = form.placeholder {
            editor.set_placeholder_text(placeholder, window, cx);
        }
        editor.set_text(form.initial_text.clone(), window, cx);
        editor
    });
    cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
        if matches!(event, EditorEvent::BufferEdited)
            && let Some(state) = this
                .opencode_settings
                .as_mut()
                .and_then(|settings| settings.one_field_form.as_mut())
        {
            state.error = None;
            cx.notify();
        }
    })
    .detach();
    editor.focus_handle(cx).focus(window, cx);
    let (page_title, parent_page) = (form.page_title, form.parent_page);
    settings.one_field_form = Some(OneFieldFormState {
        form,
        editor,
        error: None,
    });
    settings_window.push_dynamic_sub_page(
        page_title,
        parent_page,
        None,
        false,
        render_one_field_form_page,
        window,
        cx,
    );
}

fn render_one_field_form_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(state) = settings_window
        .opencode_settings
        .as_ref()
        .and_then(|settings| settings.one_field_form.as_ref())
    else {
        return div().into_any_element();
    };
    v_flex()
        .size_full()
        .child(
            v_flex()
                .id("opencode-one-field-form-page")
                .track_scroll(scroll_handle)
                .flex_1()
                .pt_2p5()
                .px_8()
                .pb_16()
                .overflow_y_scroll()
                .child(render_form_field(
                    state.form.field_title.into(),
                    Some(state.form.field_description.into()),
                    true,
                    render_input(&state.editor, cx),
                )),
        )
        .child(render_form_footer(
            state.error.clone(),
            Some(state.form.save_label),
            false,
            close_one_field_form,
            save_one_field_form,
            cx,
        ))
        .into_any_element()
}

fn close_one_field_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    if let Some(settings) = settings_window.opencode_settings.as_mut() {
        settings.one_field_form = None;
    }
    settings_window.pop_sub_page(window, cx);
}

fn save_one_field_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(settings) = settings_window.opencode_settings.as_mut() else {
        return;
    };
    let Some(state) = settings.one_field_form.as_mut() else {
        return;
    };
    let text = state.editor.read(cx).text(cx).trim().to_string();
    if text.is_empty() {
        state.error = Some(state.form.empty_error.into());
        cx.notify();
        return;
    }
    let Some(state) = settings.one_field_form.take() else {
        return;
    };
    settings_window.pop_sub_page(window, cx);
    (state.form.save)(settings_window, text, cx);
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
    let page_title: SharedString = format!("Connect {}", integration.name).into();
    let methods = list_connect_methods(&integration)
        .into_iter()
        .cloned()
        .collect();
    let key = cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_masked(true, cx);
        editor.set_placeholder_text("Paste your key", window, cx);
        editor
    });
    clear_error_when_edited(&key, cx);
    let mut form = ConnectForm {
        integration,
        page_title: page_title.clone(),
        methods,
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
        page_title,
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
        .map(|method| method.form().to_vec())
        .unwrap_or_default();
    form.fields = fields
        .into_iter()
        .map(|field| {
            let uses_editor = matches!(
                field.kind,
                FormFieldKind::String | FormFieldKind::Number | FormFieldKind::Integer
            ) && field.options.is_empty();
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
            let device_code = find_device_code(&attempt.instructions);
            let copy_value = device_code.unwrap_or(&attempt.url).to_string();
            let copy_label = if device_code.is_some() {
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
    let method_label = |method: &IntegrationMethod| -> SharedString {
        method.label().unwrap_or("API Key").to_string().into()
    };
    if form.methods.len() > 1 {
        let current = form.method().map(method_label).unwrap_or_default();
        let choices = form
            .methods
            .iter()
            .enumerate()
            .map(|(index, method)| Choice::Entry {
                label: method_label(method),
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
                    current,
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
    let answers = form.read_answers(cx);
    for (index, (state, is_active)) in form.fields.iter().zip(answers.active).enumerate() {
        if is_active && !state.field.hidden {
            controls.push(render_field(index, state, window, cx));
        }
    }
    controls
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
fn parse_typed_value(field: &FormField, text: &str) -> Result<Option<Value>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match field.kind {
        FormFieldKind::Number => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(|number| Some(Value::Number(number)))
            .ok_or_else(|| format!("{} must be a number.", format_field_title(field))),
        FormFieldKind::Integer => text
            .parse::<i64>()
            .map(|number| Some(Value::from(number)))
            .map_err(|_| format!("{} must be a whole number.", format_field_title(field))),
        _ => Ok(Some(Value::String(text.to_string()))),
    }
}

fn format_field_title(field: &FormField) -> String {
    field.title.clone().unwrap_or_else(|| field.key.clone())
}

/// The device code OpenCode's instructions mention, which is what the user has
/// to copy for a device sign-in.
fn find_device_code(instructions: &str) -> Option<&str> {
    static DEVICE_CODE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"[A-Z0-9]{4}-[A-Z0-9]{4,5}").log_err());
    DEVICE_CODE
        .as_ref()?
        .find(instructions)
        .map(|found| found.as_str())
}

fn render_field(
    index: usize,
    state: &FieldState,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let field = &state.field;
    let title: SharedString = format_field_title(field).into();
    let description: Option<SharedString> = field.description.clone().map(Into::into);
    if field.kind == FormFieldKind::External {
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
    let control = match field.kind {
        FormFieldKind::Boolean => {
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
        FormFieldKind::Multiselect => {
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
                        .cancel_login_command(&directory, &integration_id, &attempt_id)
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
    let form_answers = form.read_answers(cx);
    if let Some(problem) = form_answers.problem {
        form.error = Some(problem.into());
        cx.notify();
        return;
    }
    let answer = (!form_answers.answers.is_empty()).then_some(Value::Object(form_answers.answers));
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
                .map(|attempt| {
                    Some(ConnectStage::OAuth {
                        attempt,
                        code: None,
                    })
                }),
            IntegrationMethod::Command { id, .. } => client
                .start_login_command(&directory, &integration_id, &id)
                .await
                .map(|attempt| {
                    Some(ConnectStage::Command {
                        attempt_id: attempt.attempt_id,
                        output: String::new(),
                    })
                }),
            IntegrationMethod::Env { .. } | IntegrationMethod::Other => Ok(None),
        };
        window_handle
            .update(cx, |_, window, cx| {
                this.update(cx, |this, cx| match outcome {
                    Ok(None) => finish_connecting(this, window, cx),
                    Ok(Some(stage)) => follow_attempt(this, stage, window, cx),
                    Err(error) => fail_connecting(this, describe_error(&error), cx),
                })
                .log_err();
            })
            .log_err();
    }));
    cx.notify();
}

/// Leaves the form after a successful connection. The form's own sub-page is
/// only popped if it's still showing, since the user may have gone back while a
/// sign-in finished in their browser.
fn finish_connecting(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let form_page = settings_window
        .opencode_settings
        .as_mut()
        .and_then(|settings| settings.connect_form.take())
        .map(|form| form.page_title);
    let showing_form = settings_window
        .sub_page_stack
        .last()
        .is_some_and(|sub_page| Some(&sub_page.link.title) == form_page.as_ref());
    if showing_form {
        settings_window.pop_sub_page(window, cx);
    }
    if let Some(settings) = settings_window.opencode_settings.as_mut() {
        settings.errors.providers = None;
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

/// Shows a started sign-in and polls until the service says how it ended.
fn follow_attempt(
    settings_window: &mut SettingsWindow,
    mut stage: ConnectStage,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    if let ConnectStage::OAuth { attempt, code } = &mut stage
        && attempt.mode == OAuthMode::Code
    {
        *code = Some(cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(false, cx);
            editor
        }));
    }
    let Some(form) = settings_window
        .opencode_settings
        .as_mut()
        .and_then(|settings| settings.connect_form.as_mut())
    else {
        return;
    };
    form.stage = stage;
    let window_handle = window.window_handle();
    let executor = cx.background_executor().clone();
    form._task = Some(cx.spawn(async move |this, cx| {
        loop {
            let Some(status) = this
                .read_with(cx, |this, _| fetch_attempt_status(this))
                .ok()
                .flatten()
            else {
                return;
            };
            let status = status.await;
            let keep_polling = window_handle
                .update(cx, |_, window, cx| {
                    this.update(cx, |this, cx| match status {
                        Ok(AttemptStatus::Pending { message }) => {
                            if let Some(ConnectStage::Command { output, .. }) = this
                                .opencode_settings
                                .as_mut()
                                .and_then(|settings| settings.connect_form.as_mut())
                                .map(|form| &mut form.stage)
                                && let Some(message) = message
                            {
                                *output = message;
                                cx.notify();
                            }
                            true
                        }
                        Ok(AttemptStatus::Complete {}) => {
                            finish_connecting(this, window, cx);
                            false
                        }
                        Ok(AttemptStatus::Failed { message }) => {
                            fail_connecting(
                                this,
                                message.unwrap_or_else(|| "Sign-in failed.".into()),
                                cx,
                            );
                            false
                        }
                        Ok(AttemptStatus::Expired) => {
                            fail_connecting(this, "The sign-in expired. Try again.".into(), cx);
                            false
                        }
                        Err(error) => {
                            fail_connecting(this, describe_error(&error), cx);
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
    }));
    cx.notify();
}

/// The status request for the sign-in the connect form waits on. `None` once
/// the form has moved on, which ends the polling, and for a sign-in that
/// finishes when the user submits its code.
fn fetch_attempt_status(
    settings_window: &SettingsWindow,
) -> Option<LocalBoxFuture<'static, anyhow::Result<AttemptStatus>>> {
    let settings = settings_window.opencode_settings.as_ref()?;
    let client = settings.client.clone()?;
    let directory = settings.directory.clone();
    let form = settings.connect_form.as_ref()?;
    let integration_id = form.integration.id.clone();
    match &form.stage {
        ConnectStage::OAuth {
            attempt,
            code: None,
        } => {
            let attempt_id = attempt.attempt_id.clone();
            Some(
                async move {
                    client
                        .fetch_oauth_status(&directory, &integration_id, &attempt_id)
                        .await
                }
                .boxed_local(),
            )
        }
        ConnectStage::Command { attempt_id, .. } => {
            let attempt_id = attempt_id.clone();
            Some(
                async move {
                    client
                        .fetch_login_command_status(&directory, &integration_id, &attempt_id)
                        .await
                }
                .boxed_local(),
            )
        }
        ConnectStage::Form | ConnectStage::Starting | ConnectStage::OAuth { .. } => None,
    }
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
                            form.error = Some(describe_error(&error).into());
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

    fn typed<'a>(field: &'a FormField, text: &str) -> FieldInput<'a> {
        FieldInput {
            field,
            text: Some(text.to_string()),
            value: None,
        }
    }

    fn picked(field: &FormField, value: Value) -> FieldInput<'_> {
        FieldInput {
            field,
            text: None,
            value: Some(value),
        }
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
        let labels: Vec<Option<&str>> = list_connect_methods(&openai)
            .into_iter()
            .map(IntegrationMethod::label)
            .collect();
        assert_eq!(labels, [Some("Sign in with ChatGPT"), None]);
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
            list_available_providers(&integrations, filter)
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
        let empty = FormAnswers::read(&[typed(&required, "  ")]);
        assert_eq!(
            empty.problem.as_deref(),
            Some("This field is required: Resource")
        );

        let answers = FormAnswers::read(&[typed(&required, "mine"), typed(&hidden, "")]);
        assert_eq!(answers.problem, None);
        assert_eq!(answers.answers["resource"], "mine");
        assert_eq!(answers.answers["mode"], "auto");
    }

    #[test]
    fn fields_only_apply_when_their_condition_holds() {
        let deployment = field(json!({ "key": "deployment", "type": "string", "options": [] }));
        let domain = field(json!({
            "key": "domain", "type": "string", "required": true,
            "when": [{ "key": "deployment", "op": "eq", "value": "enterprise" }],
        }));
        let setup_link = field(json!({
            "key": "setup", "type": "external", "url": "https://example.com",
            "when": [{ "key": "deployment", "op": "eq", "value": "enterprise" }],
        }));
        let hosted = FormAnswers::read(&[
            picked(&deployment, json!("github.com")),
            typed(&domain, ""),
            picked(&setup_link, Value::Null),
        ]);
        assert_eq!(hosted.problem, None);
        assert!(!hosted.answers.contains_key("domain"));
        assert_eq!(hosted.active, [true, false, false]);

        let enterprise = FormAnswers::read(&[
            picked(&deployment, json!("enterprise")),
            typed(&domain, ""),
            picked(&setup_link, Value::Null),
        ]);
        assert!(enterprise.problem.is_some());
        assert_eq!(enterprise.active, [true, true, true]);
    }

    #[test]
    fn numbers_are_validated() {
        let port = field(json!({ "key": "port", "type": "integer", "title": "Port" }));
        assert_eq!(parse_typed_value(&port, "8080").unwrap(), Some(json!(8080)));
        assert_eq!(parse_typed_value(&port, "").unwrap(), None);
        assert_eq!(
            parse_typed_value(&port, "x").unwrap_err(),
            "Port must be a whole number."
        );
    }

    #[test]
    fn device_codes_are_found_in_instructions() {
        assert_eq!(
            find_device_code("Enter code ABCD-1234 at github"),
            Some("ABCD-1234")
        );
        assert_eq!(find_device_code("Open the link to sign in"), None);
    }
}
