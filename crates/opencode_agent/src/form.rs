//! The agent can ask the user to fill in a form, such as a multiple-choice
//! question or a pick of web search provider. Zed shows it in the thread and
//! sends the answers back.
//!
//! Zed refuses forms that might ask for a secret, or that it can't draw
//! faithfully, and leaves them for the user to answer in another OpenCode
//! client. A choice that also takes the user's own answer gets a second
//! "(other)" text field, and the two are merged into one answer.

use crate::types::{Form, FormField, FormFieldKind, FormOption};
use agent_client_protocol::v2::schema as acp;
use collections::BTreeMap;
use serde_json::{Map, Value};
use util::ResultExt as _;

/// A form shown in the thread, kept to read the user's answers.
pub(crate) struct FormElicitation {
    pub request: acp::CreateElicitationRequest,
    form: Form,
}

#[derive(Debug, PartialEq)]
pub(crate) enum FormReply {
    Answer(Value),
    Decline,
}

impl FormElicitation {
    /// `None` when the form is left for another client.
    pub(crate) fn new(form: &Form, session_id: &str) -> Option<Self> {
        if !can_show(form) {
            return None;
        }
        let mut schema = acp::ElicitationSchema::new();
        for field in form.fields.iter().filter(|field| !field.hidden) {
            schema = schema.property(field.key.clone(), build_property(field), field.required);
            if let Some(other) = build_other_answer_property(field) {
                schema = schema.property(other_answer_key(field), other, false);
            }
        }
        let scope = acp::ElicitationSessionScope::new(session_id)
            .tool_call_id(form.tool_call_id().map(acp::ToolCallId::new));
        let request = acp::CreateElicitationRequest::new(
            acp::ElicitationFormMode::new(scope, schema),
            form.title.clone(),
        );
        Some(Self {
            request,
            form: form.clone(),
        })
    }

    pub(crate) fn answer(&self, response: &acp::CreateElicitationResponse) -> FormReply {
        let acp::ElicitationAction::Accept(accept) = &response.action else {
            return FormReply::Decline;
        };
        let content = accept.content.clone().unwrap_or_default();
        let answer: Map<String, Value> = self
            .form
            .fields
            .iter()
            .filter_map(|field| Some((field.key.clone(), read_answer(field, &content)?)))
            .collect();
        FormReply::Answer(Value::Object(answer))
    }
}

/// Whether Zed can show the form. Every rule for leaving it to another
/// client is a reason this returns false.
fn can_show(form: &Form) -> bool {
    // Forms of other kinds can ask for secrets.
    const SHOWN_KINDS: &[&str] = &["question", "websearch.provider"];
    form.kind().is_some_and(|kind| SHOWN_KINDS.contains(&kind))
        && !form.fields.is_empty()
        && !form.fields.iter().any(may_ask_for_secret)
        && form.fields.iter().all(|field| can_show_field(field, form))
}

fn may_ask_for_secret(field: &FormField) -> bool {
    const WORDS: &[&str] = &[
        "password",
        "passphrase",
        "secret",
        "token",
        "apikey",
        "api_key",
        "api-key",
        "api key",
        "credential",
        "privatekey",
        "private_key",
        "private-key",
        "private key",
    ];
    let option_labels = field.options.iter().map(|option| &option.label);
    [
        Some(&field.key),
        field.title.as_ref(),
        field.description.as_ref(),
    ]
    .into_iter()
    .flatten()
    .chain(option_labels)
    .any(|text| {
        let text = text.to_lowercase();
        WORDS.iter().any(|word| text.contains(word))
    })
}

fn can_show_field(field: &FormField, form: &Form) -> bool {
    // External links, and types Zed doesn't know, have no property.
    if matches!(field.kind, FormFieldKind::External | FormFieldKind::Other) {
        return false;
    }
    // They depend on answers given while the form is open.
    if !field.when.is_empty() {
        return false;
    }
    // A default of the wrong type, or not among the choices, couldn't be
    // shown or sent back.
    if field.default.is_some() && validate_default(field).is_none() {
        return false;
    }
    // A hidden field is answered with its default, without being shown.
    if field.hidden {
        return !field.required || field.default.is_some();
    }
    if field.kind == FormFieldKind::Integer
        && [field.minimum, field.maximum]
            .into_iter()
            .flatten()
            .any(|bound| as_whole_number(bound).is_none())
    {
        return false;
    }
    if !field.custom || !is_choice(field) {
        return true;
    }
    // Either answer would do for a required choice with an "(other)" answer,
    // which a schema can't say, and the count of a multiselect would have to
    // include the "(other)" answers.
    let other_key = other_answer_key(field);
    !field.required
        && !form.fields.iter().any(|field| field.key == other_key)
        && (field.kind == FormFieldKind::String
            || field.min_items.is_none() && field.max_items.is_none())
}

/// Only for the kinds `can_show_field` accepts.
fn build_property(field: &FormField) -> acp::ElicitationPropertySchema {
    let (title, description) = (field.title.clone(), field.description.clone());
    match field.kind {
        FormFieldKind::Multiselect => {
            acp::MultiSelectPropertySchema::titled(to_enum_options(&field.options))
                .title(title)
                .description(description)
                .min_items(require_at_least_one(field.min_items, field.required))
                .max_items(field.max_items)
                .default_value(field.default.as_ref().and_then(parse_string_list))
                .into()
        }
        FormFieldKind::Number => acp::NumberPropertySchema::new()
            .title(title)
            .description(description)
            .minimum(field.minimum)
            .maximum(field.maximum)
            .default_value(field.default.as_ref().and_then(Value::as_f64))
            .into(),
        FormFieldKind::Integer => acp::IntegerPropertySchema::new()
            .title(title)
            .description(description)
            .minimum(field.minimum.and_then(as_whole_number))
            .maximum(field.maximum.and_then(as_whole_number))
            .default_value(field.default.as_ref().and_then(Value::as_i64))
            .into(),
        FormFieldKind::Boolean => acp::BooleanPropertySchema::new()
            .title(title)
            .description(description)
            .default_value(field.default.as_ref().and_then(Value::as_bool))
            .into(),
        FormFieldKind::String | FormFieldKind::External | FormFieldKind::Other => {
            let property = acp::StringPropertySchema::new()
                .title(title)
                .description(description)
                .default_value(
                    field
                        .default
                        .as_ref()
                        .and_then(Value::as_str)
                        .map(str::to_string),
                );
            if is_choice(field) {
                property.one_of(to_enum_options(&field.options)).into()
            } else {
                constrain_string(property, field).into()
            }
        }
    }
}

/// The property for an answer of the user's own, beside a choice's.
fn build_other_answer_property(field: &FormField) -> Option<acp::StringPropertySchema> {
    if !field.custom || !is_choice(field) {
        return None;
    }
    let title = field.title.as_ref().unwrap_or(&field.key);
    let property = acp::StringPropertySchema::new().title(format!("{title} (other)"));
    Some(if field.kind == FormFieldKind::Multiselect {
        property.description("Add your own answer".to_string())
    } else {
        constrain_string(property, field).description("Type your own answer".to_string())
    })
}

fn constrain_string(
    property: acp::StringPropertySchema,
    field: &FormField,
) -> acp::StringPropertySchema {
    property
        .format(field.format.as_deref().map(to_string_format))
        .min_length(require_at_least_one(field.min_length, field.required))
        .max_length(field.max_length)
        .pattern(field.pattern.clone())
}

/// OpenCode turns down an empty answer for a required field, whether an
/// empty string or no items picked.
fn require_at_least_one<T: Copy + Ord + From<u8>>(minimum: Option<T>, required: bool) -> Option<T> {
    if required {
        Some(minimum.unwrap_or(T::from(0)).max(T::from(1)))
    } else {
        minimum
    }
}

fn to_string_format(format: &str) -> acp::StringFormat {
    match format {
        "email" => acp::StringFormat::Email,
        "uri" => acp::StringFormat::Uri,
        "date" => acp::StringFormat::Date,
        "date-time" => acp::StringFormat::DateTime,
        other => acp::StringFormat::Other(other.to_string()),
    }
}

fn to_enum_options(options: &[FormOption]) -> Vec<acp::EnumOption> {
    options
        .iter()
        .map(|option| {
            acp::EnumOption::new(option.value.clone(), option.label.clone())
                .description(option.description.clone())
        })
        .collect()
}

fn is_choice(field: &FormField) -> bool {
    field.kind == FormFieldKind::Multiselect || !field.options.is_empty()
}

fn other_answer_key(field: &FormField) -> String {
    format!("{}_custom", field.key)
}

/// An integer field's bounds are JSON numbers, which a schema for integers
/// only takes when whole.
fn as_whole_number(value: f64) -> Option<i64> {
    (value.fract() == 0.0).then_some(value as i64)
}

fn parse_string_list(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| Some(item.as_str()?.to_string()))
        .collect()
}

/// The field's default, when it has the field's type and, for a choice, is
/// among its options.
fn validate_default(field: &FormField) -> Option<Value> {
    let default = field.default.as_ref()?;
    let is_option = |value: &str| field.options.iter().any(|option| option.value == value);
    let valid = match field.kind {
        FormFieldKind::String => default
            .as_str()
            .is_some_and(|value| field.options.is_empty() || is_option(value)),
        FormFieldKind::Multiselect => parse_string_list(default)
            .is_some_and(|values| values.iter().all(|value| is_option(value))),
        FormFieldKind::Number => default.is_number(),
        FormFieldKind::Integer => default.is_i64() || default.is_u64(),
        FormFieldKind::Boolean => default.is_boolean(),
        FormFieldKind::External | FormFieldKind::Other => false,
    };
    valid.then(|| default.clone())
}

/// A choice's "(other)" answer is merged into it: it replaces a single
/// choice and joins a multiselect's picks.
fn read_answer(
    field: &FormField,
    content: &BTreeMap<String, acp::ElicitationContentValue>,
) -> Option<Value> {
    if field.hidden {
        return validate_default(field);
    }
    let answer = content
        .get(&field.key)
        .and_then(|value| serde_json::to_value(value).log_err());
    let other = match content.get(&other_answer_key(field)) {
        Some(acp::ElicitationContentValue::String(other))
            if field.custom && is_choice(field) && !other.trim().is_empty() =>
        {
            Value::String(other.clone())
        }
        _ => return answer,
    };
    if field.kind != FormFieldKind::Multiselect {
        return Some(other);
    }
    let mut items = match answer {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    items.push(other);
    Some(Value::Array(items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form(value: Value) -> Form {
        serde_json::from_value(value).expect("form")
    }

    fn question() -> Value {
        json!({
            "id": "frm_1",
            "sessionID": "ses_1",
            "title": "Which database?",
            "metadata": { "kind": "question", "tool": { "id": "call_1" } },
            "fields": [{
                "key": "db",
                "type": "string",
                "title": "Database",
                "required": true,
                "custom": false,
                "options": [
                    { "value": "pg", "label": "Postgres" },
                    { "value": "sqlite", "label": "SQLite" },
                ],
            }],
        })
    }

    fn accept(content: Value) -> acp::CreateElicitationResponse {
        serde_json::from_value(json!({ "action": "accept", "content": content })).expect("response")
    }

    #[test]
    fn builds_a_select_elicitation() {
        let elicitation = FormElicitation::new(&form(question()), "ses_1").expect("request");
        let request = serde_json::to_value(elicitation.request).expect("serialize");
        assert_eq!(request["mode"], "form");
        assert_eq!(request["message"], "Which database?");
        assert_eq!(request["toolCallId"], "call_1");
        let db = &request["requestedSchema"]["properties"]["db"];
        assert_eq!(db["oneOf"][1]["const"], "sqlite");
        assert_eq!(request["requestedSchema"]["required"][0], "db");
    }

    #[test]
    fn builds_each_field_type() {
        let elicitation = FormElicitation::new(
            &form(json!({
                "id": "frm_1",
                "title": "Setup",
                "metadata": { "kind": "question" },
                "fields": [
                    { "key": "name", "type": "string", "required": true, "format": "email" },
                    { "key": "size", "type": "integer", "minimum": 1, "maximum": 10, "default": 3 },
                    { "key": "ratio", "type": "number", "minimum": 0.5 },
                    { "key": "fast", "type": "boolean", "default": true },
                    { "key": "tags", "type": "multiselect", "required": true,
                      "options": [{ "value": "a", "label": "A" }] },
                    { "key": "hidden", "type": "string", "hidden": true, "default": "x" },
                ],
            })),
            "ses_1",
        )
        .expect("request");
        let schema =
            &serde_json::to_value(elicitation.request).expect("serialize")["requestedSchema"];
        let properties = &schema["properties"];
        assert_eq!(properties["name"]["minLength"], 1);
        assert_eq!(properties["name"]["format"], "email");
        assert_eq!(properties["size"]["type"], "integer");
        assert_eq!(properties["size"]["default"], 3);
        assert_eq!(properties["ratio"]["minimum"], 0.5);
        assert_eq!(properties["fast"]["default"], true);
        assert_eq!(properties["tags"]["minItems"], 1);
        assert!(properties.get("hidden").is_none());
        assert_eq!(schema["required"], json!(["name", "tags"]));
    }

    #[test]
    fn declines_forms_it_cant_show() {
        let declined = |change: &dyn Fn(&mut Value)| {
            let mut value = question();
            change(&mut value);
            FormElicitation::new(&form(value), "ses_1").is_none()
        };
        assert!(declined(&|form| form["fields"][0]["type"] = json!("color")));
        assert!(declined(
            &|form| form["fields"][0]["type"] = json!("external")
        ));
        assert!(declined(
            &|form| form["fields"][0]["title"] = json!("API key")
        ));
        assert!(declined(
            &|form| form["metadata"]["kind"] = json!("mcp.auth")
        ));
        assert!(declined(
            &|form| form["fields"][0]["default"] = json!("mysql")
        ));
        assert!(declined(&|form| {
            form["fields"][0]["when"] = json!([{ "key": "x", "op": "eq", "value": 1 }])
        }));
        // Required and custom: either answer would do, which a schema can't say.
        assert!(declined(&|form| form["fields"][0]["custom"] = json!(true)));
        assert!(declined(&|form| {
            form["fields"][0] = json!({ "key": "n", "type": "integer", "minimum": 0.5 })
        }));
    }

    #[test]
    fn merges_other_answers() {
        let mut value = question();
        value["fields"][0]["custom"] = json!(true);
        value["fields"][0]["required"] = json!(false);
        let form = form(value);
        let elicitation = FormElicitation::new(&form, "ses_1").expect("shown");
        let response = accept(json!({ "db": "pg", "db_custom": "duckdb" }));
        assert_eq!(
            elicitation.answer(&response),
            FormReply::Answer(json!({ "db": "duckdb" }))
        );
        let declined: acp::CreateElicitationResponse =
            serde_json::from_value(json!({ "action": "decline" })).expect("response");
        assert_eq!(elicitation.answer(&declined), FormReply::Decline);
    }

    #[test]
    fn answers_hidden_fields_with_their_default() {
        let form = form(json!({
            "id": "frm_1",
            "title": "Search",
            "metadata": { "kind": "websearch.provider" },
            "fields": [
                { "key": "tags", "type": "multiselect", "custom": true,
                  "options": [{ "value": "a", "label": "A" }] },
                { "key": "region", "type": "string", "hidden": true, "default": "eu" },
            ],
        }));
        let elicitation = FormElicitation::new(&form, "ses_1").expect("shown");
        let response = accept(json!({ "tags": ["a"], "tags_custom": "b" }));
        assert_eq!(
            elicitation.answer(&response),
            FormReply::Answer(json!({ "tags": ["a", "b"], "region": "eu" }))
        );
    }
}
