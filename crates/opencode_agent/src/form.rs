//! OpenCode forms (the agent's typed questions) shown as ACP form
//! elicitations, which Zed renders natively.
//!
//! Ported from OpenCode's own ACP bridge (`cli/src/acp/elicitation.ts`), so a
//! form is shown, or declined, the same way in both clients.

use agent_client_protocol::v2::schema as acp;
use serde_json::{Map, Value, json};

/// Form kinds safe to show in a form: they never ask for secrets.
const SHOWN_KINDS: &[&str] = &["question", "websearch.provider"];

fn kind(form: &Value) -> Option<&str> {
    form.get("metadata")?.get("kind")?.as_str()
}

fn fields(form: &Value) -> &[Value] {
    form.get("fields")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// The elicitation request for `form`, or `None` when the form can't be
/// shown as one (secrets, external links, conditional fields, and so on).
pub(crate) fn elicitation_request(
    form: &Value,
    session_id: &str,
    tool_call_id: Option<&str>,
) -> Option<acp::CreateElicitationRequest> {
    if !kind(form).is_some_and(|kind| SHOWN_KINDS.contains(&kind)) {
        return None;
    }
    let fields = fields(form);
    if fields.is_empty() || fields.iter().any(credential_like) {
        return None;
    }
    for field in fields {
        if str_field(field, "type") == Some("external") {
            return None;
        }
        if field
            .get("when")
            .and_then(Value::as_array)
            .is_some_and(|when| !when.is_empty())
        {
            return None;
        }
        if bool_field(field, "hidden")
            && bool_field(field, "required")
            && field.get("default").is_none()
        {
            return None;
        }
    }
    let keys: Vec<&str> = fields
        .iter()
        .filter_map(|field| str_field(field, "key"))
        .collect();
    let visible: Vec<&Value> = fields
        .iter()
        .filter(|field| !bool_field(field, "hidden"))
        .collect();
    if !visible.iter().all(|field| representable(field, &keys)) {
        return None;
    }

    let mut properties = Map::new();
    for field in &visible {
        for (key, property) in field_properties(field) {
            properties.insert(key, property);
        }
    }
    let required: Vec<&str> = visible
        .iter()
        .filter(|field| bool_field(field, "required"))
        .filter_map(|field| str_field(field, "key"))
        .collect();

    let mut request = json!({
        "mode": "form",
        "sessionId": session_id,
        "message": str_field(form, "title").unwrap_or_default(),
        "requestedSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
    });
    if let Some(tool_call_id) = tool_call_id {
        request["toolCallId"] = json!(tool_call_id);
    }
    serde_json::from_value(request)
        .map_err(|error| log::warn!("Can't build an elicitation for an OpenCode form: {error}"))
        .ok()
}

fn credential_like(field: &Value) -> bool {
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
    let option_labels = field
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| str_field(option, "label"));
    ["key", "title", "description"]
        .iter()
        .filter_map(|key| str_field(field, key))
        .chain(option_labels)
        .any(|text| {
            let text = text.to_lowercase();
            WORDS.iter().any(|word| text.contains(word))
        })
}

fn has_options(field: &Value) -> bool {
    str_field(field, "type") == Some("multiselect") || field.get("options").is_some()
}

fn custom_key(field: &Value) -> String {
    format!("{}_custom", str_field(field, "key").unwrap_or_default())
}

fn representable(field: &Value, keys: &[&str]) -> bool {
    let field_type = str_field(field, "type");
    if !matches!(field_type, Some("string") | Some("multiselect")) || !has_options(field) {
        return true;
    }
    let values: Vec<&str> = field
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| str_field(option, "value"))
        .collect();
    let defaults: Vec<&str> = match field.get("default") {
        Some(Value::String(value)) => vec![value.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if defaults.iter().any(|value| !values.contains(value)) {
        return false;
    }
    if !bool_field(field, "custom") {
        return true;
    }
    if bool_field(field, "required") || keys.contains(&custom_key(field).as_str()) {
        return false;
    }
    field_type == Some("string")
        || (field.get("minItems").is_none() && field.get("maxItems").is_none())
}

fn field_properties(field: &Value) -> Vec<(String, Value)> {
    let key = str_field(field, "key").unwrap_or_default().to_string();
    let mut base = Map::new();
    for name in ["title", "description"] {
        if let Some(value) = field.get(name) {
            base.insert(name.into(), value.clone());
        }
    }
    let with = |mut object: Map<String, Value>, extra: &[(&str, Option<&Value>)]| {
        for (name, value) in extra {
            if let Some(value) = value {
                object.insert((*name).into(), (*value).clone());
            }
        }
        Value::Object(object)
    };
    let get = |name: &str| field.get(name);
    match str_field(field, "type") {
        Some("string") if !has_options(field) => {
            let mut object = base.clone();
            object.insert("type".into(), json!("string"));
            object.extend(string_constraints(field));
            vec![(key, with(object, &[("default", get("default"))]))]
        }
        Some("string") => {
            let mut object = base.clone();
            object.insert("type".into(), json!("string"));
            object.insert("oneOf".into(), options(field));
            let select = with(object, &[("default", get("default"))]);
            let mut properties = vec![(key, select)];
            if bool_field(field, "custom") {
                properties.push(other(field, "Type your own answer"));
            }
            properties
        }
        Some("multiselect") => {
            let mut object = base.clone();
            object.insert("type".into(), json!("array"));
            object.insert("items".into(), json!({ "anyOf": options(field) }));
            let min_items = field.get("minItems").and_then(Value::as_u64);
            let min_items = if bool_field(field, "required") {
                Some(min_items.unwrap_or(0).max(1))
            } else {
                min_items
            };
            if let Some(min_items) = min_items {
                object.insert("minItems".into(), json!(min_items));
            }
            let select = with(
                object,
                &[("maxItems", get("maxItems")), ("default", get("default"))],
            );
            let mut properties = vec![(key, select)];
            if bool_field(field, "custom") {
                properties.push(other(field, "Add your own answer"));
            }
            properties
        }
        Some(number @ ("number" | "integer")) => {
            let mut object = base.clone();
            object.insert("type".into(), json!(number));
            vec![(
                key,
                with(
                    object,
                    &[
                        ("minimum", get("minimum")),
                        ("maximum", get("maximum")),
                        ("default", get("default")),
                    ],
                ),
            )]
        }
        Some("boolean") => {
            let mut object = base.clone();
            object.insert("type".into(), json!("boolean"));
            vec![(key, with(object, &[("default", get("default"))]))]
        }
        _ => Vec::new(),
    }
}

fn other(field: &Value, description: &str) -> (String, Value) {
    let title = str_field(field, "title")
        .or_else(|| str_field(field, "key"))
        .unwrap_or_default();
    let mut object = Map::new();
    object.insert("type".into(), json!("string"));
    object.insert("title".into(), json!(format!("{title} (other)")));
    object.insert("description".into(), json!(description));
    if str_field(field, "type") == Some("string") {
        object.extend(string_constraints(field));
    }
    (custom_key(field), Value::Object(object))
}

/// Core rejects an empty string for a required field.
fn string_constraints(field: &Value) -> Map<String, Value> {
    let mut object = Map::new();
    for name in ["format", "maxLength", "pattern"] {
        if let Some(value) = field.get(name) {
            object.insert(name.into(), value.clone());
        }
    }
    let min_length = field.get("minLength").and_then(Value::as_u64);
    let min_length = if bool_field(field, "required") {
        Some(min_length.unwrap_or(0).max(1))
    } else {
        min_length
    };
    if let Some(min_length) = min_length {
        object.insert("minLength".into(), json!(min_length));
    }
    object
}

fn options(field: &Value) -> Value {
    let options: Vec<Value> = field
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|option| {
            let mut object = Map::new();
            object.insert("const".into(), option.get("value").cloned().unwrap_or_default());
            object.insert("title".into(), option.get("label").cloned().unwrap_or_default());
            if let Some(description) = option.get("description") {
                object.insert("description".into(), description.clone());
            }
            Value::Object(object)
        })
        .collect();
    Value::Array(options)
}

/// The form answer for an elicitation response, or `None` when the user
/// declined or cancelled.
pub(crate) fn answer(form: &Value, response: &acp::CreateElicitationResponse) -> Option<Value> {
    let response = serde_json::to_value(response).ok()?;
    if str_field(&response, "action") != Some("accept") {
        return None;
    }
    let content = response
        .get("content")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut answer = Map::new();
    for field in fields(form) {
        let Some(key) = str_field(field, "key") else {
            continue;
        };
        if let Some(value) = field_answer(field, &content) {
            answer.insert(key.to_string(), value);
        }
    }
    Some(Value::Object(answer))
}

fn field_answer(field: &Value, content: &Map<String, Value>) -> Option<Value> {
    let field_type = str_field(field, "type");
    if field_type != Some("external") && bool_field(field, "hidden") {
        return field.get("default").cloned();
    }
    let value = content.get(str_field(field, "key")?).cloned();
    if !matches!(field_type, Some("string") | Some("multiselect"))
        || !bool_field(field, "custom")
        || !has_options(field)
    {
        return value;
    }
    let custom = content
        .get(&custom_key(field))
        .and_then(Value::as_str)
        .filter(|custom| !custom.trim().is_empty());
    let Some(custom) = custom else {
        return value;
    };
    if field_type == Some("string") {
        return Some(json!(custom));
    }
    let mut items = match value {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    items.push(json!(custom));
    Some(Value::Array(items))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question() -> Value {
        json!({
            "id": "frm_1",
            "sessionID": "ses_1",
            "title": "Which database?",
            "metadata": { "kind": "question" },
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

    #[test]
    fn builds_a_select_elicitation() {
        let request = elicitation_request(&question(), "ses_1", Some("call_1")).expect("request");
        let request = serde_json::to_value(request).expect("serialize");
        assert_eq!(request["mode"], "form");
        assert_eq!(request["message"], "Which database?");
        assert_eq!(request["toolCallId"], "call_1");
        let db = &request["requestedSchema"]["properties"]["db"];
        assert_eq!(db["oneOf"][1]["const"], "sqlite");
        assert_eq!(request["requestedSchema"]["required"][0], "db");
    }

    #[test]
    fn declines_forms_that_ask_for_secrets() {
        let mut form = question();
        form["fields"][0]["title"] = json!("API key");
        assert!(elicitation_request(&form, "ses_1", None).is_none());
        let mut form = question();
        form["metadata"]["kind"] = json!("mcp.auth");
        assert!(elicitation_request(&form, "ses_1", None).is_none());
    }

    #[test]
    fn maps_custom_answers() {
        let mut form = question();
        form["fields"][0]["custom"] = json!(true);
        form["fields"][0]["required"] = json!(false);
        let response: acp::CreateElicitationResponse = serde_json::from_value(json!({
            "action": "accept",
            "content": { "db": "pg", "db_custom": "duckdb" },
        }))
        .expect("response");
        assert_eq!(answer(&form, &response), Some(json!({ "db": "duckdb" })));
        let declined: acp::CreateElicitationResponse =
            serde_json::from_value(json!({ "action": "decline" })).expect("response");
        assert_eq!(answer(&form, &declined), None);
    }
}
