//! Reads and edits OpenCode's global config file for the agents modal.
//!
//! Writes the documented V1 shape (`agent`, `permission`), which OpenCode
//! reads alongside its V2 keys, and leaves the rest of the file's text and
//! comments alone. The service watches the file and reloads on change.

use crate::types::{ConfigEntry, PermissionEffect, PermissionRule};
use anyhow::{Context as _, Result, anyhow};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// A permission OpenCode checks, as shown in the agents modal.
pub struct Tool {
    /// The action the service checks, after it renames V1 keys.
    pub action: &'static str,
    /// Keys in the file that mean this action. The first is written when
    /// none is present yet.
    pub keys: &'static [&'static str],
    pub label: &'static str,
}

/// The rule for every action without one of its own.
pub const ALL_TOOLS: Tool = Tool {
    action: "*",
    keys: &["*"],
    label: "All Tools",
};

pub const TOOLS: &[Tool] = &[
    Tool {
        action: "read",
        keys: &["read"],
        label: "Read Files",
    },
    Tool {
        action: "edit",
        keys: &["edit", "write", "patch", "apply_patch"],
        label: "Edit Files",
    },
    Tool {
        action: "glob",
        keys: &["glob"],
        label: "Find Files",
    },
    Tool {
        action: "grep",
        keys: &["grep"],
        label: "Search File Contents",
    },
    Tool {
        action: "shell",
        keys: &["bash", "shell"],
        label: "Run Shell Commands",
    },
    Tool {
        action: "webfetch",
        keys: &["webfetch"],
        label: "Fetch Web Pages",
    },
    Tool {
        action: "websearch",
        keys: &["websearch"],
        label: "Search the Web",
    },
    Tool {
        action: "subagent",
        keys: &["task", "subagent"],
        label: "Start Subagents",
    },
    Tool {
        action: "skill",
        keys: &["skill"],
        label: "Load Skills",
    },
    Tool {
        action: "question",
        keys: &["question"],
        label: "Ask Questions",
    },
    Tool {
        action: "todowrite",
        keys: &["todowrite"],
        label: "Update the Todo List",
    },
    Tool {
        action: "external_directory",
        keys: &["external_directory"],
        label: "Work Outside the Project",
    },
];

/// The global config file the service reads, chosen the way the service
/// picks the file it writes: the last of `opencode.json` and `opencode.jsonc`
/// that exists, else `opencode.jsonc`.
///
/// The global directory is the first directory the service reports, since
/// it loads global sources before project ones. With global config turned
/// off that would be a project's folder, which is refused.
pub fn global_config_path(entries: &[ConfigEntry]) -> Option<PathBuf> {
    let directory = entries.iter().find_map(|entry| match entry {
        ConfigEntry::Directory { path } => Some(Path::new(path)),
        _ => None,
    })?;
    let is_project_folder = directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| [".opencode", ".claude", ".agents"].contains(&name));
    if is_project_folder {
        return None;
    }
    let existing = ["opencode.json", "opencode.jsonc"]
        .iter()
        .map(|name| directory.join(name))
        .rfind(|path| path.is_file());
    Some(existing.unwrap_or_else(|| directory.join("opencode.jsonc")))
}

fn matches(value: &str, pattern: &str) -> bool {
    fn glob(value: &[char], pattern: &[char]) -> bool {
        match pattern.split_first() {
            None => value.is_empty(),
            Some(('*', rest)) => (0..=value.len()).any(|skip| glob(&value[skip..], rest)),
            Some(('?', rest)) => !value.is_empty() && glob(&value[1..], rest),
            Some((character, rest)) => value.first() == Some(character) && glob(&value[1..], rest),
        }
    }
    let value: Vec<char> = value.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    // OpenCode lets a trailing ` *` also match nothing, so `git *` matches
    // `git`.
    glob(&value, &pattern)
        || pattern
            .strip_suffix(&[' ', '*'])
            .is_some_and(|prefix| glob(&value, prefix))
}

/// What OpenCode does for `action` when no narrower pattern applies, or
/// `None` when no rule covers it.
pub fn effective(rules: &[PermissionRule], action: &str) -> Option<PermissionEffect> {
    rules
        .iter()
        .rfind(|rule| matches(action, &rule.action) && matches("*", &rule.resource))
        .map(|rule| rule.effect)
}

/// Whether some rule for `action` only covers some files, commands or URLs.
pub fn has_pattern_rules(rules: &[PermissionRule], action: &str) -> bool {
    rules
        .iter()
        .any(|rule| rule.action == action && rule.resource != "*")
}

fn effect_value(effect: PermissionEffect) -> Value {
    Value::String(
        match effect {
            PermissionEffect::Allow => "allow",
            PermissionEffect::Ask => "ask",
            PermissionEffect::Deny => "deny",
        }
        .into(),
    )
}

fn parse_effect(value: &Value) -> Option<PermissionEffect> {
    match value.as_str()? {
        "allow" => Some(PermissionEffect::Allow),
        "ask" => Some(PermissionEffect::Ask),
        "deny" => Some(PermissionEffect::Deny),
        _ => None,
    }
}

fn normalize_action(key: &str) -> &str {
    TOOLS
        .iter()
        .find(|tool| tool.keys.contains(&key))
        .map_or(key, |tool| tool.action)
}

/// The rules a `permission` value stands for, in V1's order.
pub fn rules_from_value(value: &Value) -> Vec<PermissionRule> {
    let rule = |action: &str, resource: &str, effect| PermissionRule {
        action: normalize_action(action).to_string(),
        resource: resource.to_string(),
        effect,
    };
    match value {
        Value::String(_) => parse_effect(value)
            .map(|effect| vec![rule("*", "*", effect)])
            .unwrap_or_default(),
        Value::Object(object) => object
            .iter()
            .flat_map(|(action, value)| match value {
                Value::Object(patterns) => patterns
                    .iter()
                    .filter_map(|(resource, effect)| {
                        Some(rule(action, resource, parse_effect(effect)?))
                    })
                    .collect::<Vec<_>>(),
                _ => parse_effect(value)
                    .map(|effect| vec![rule(action, "*", effect)])
                    .unwrap_or_default(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Whose permissions to change.
#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    /// Every agent, through the top-level `permission`.
    Global,
    Agent(String),
}

impl Scope {
    fn permission_path(&self) -> Vec<&str> {
        match self {
            Scope::Global => vec!["permission"],
            Scope::Agent(id) => vec!["agent", id, "permission"],
        }
    }
}

pub fn parse(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value =
        settings_json::parse_json_with_comments(text).context("reading OpenCode's config")?;
    if !value.is_object() {
        return Err(anyhow!("OpenCode's config isn't a JSON object"));
    }
    Ok(value)
}

fn lookup<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |value, key| value.get(*key))
}

/// Replaces the value at `path`, or removes it for `None`, keeping the rest
/// of the text as it is.
fn replace(text: &str, path: &[&str], value: Option<&Value>) -> Result<String> {
    let mut text = if text.trim().is_empty() {
        "{\n}\n".to_string()
    } else {
        text.to_string()
    };
    // Checked first, since the editor assumes a valid object.
    parse(&text)?;
    let tab_size = settings_json::infer_json_indent_size(&text);
    let (range, replacement) =
        settings_json::replace_value_in_json_text(&text, path, tab_size, value, None);
    text.replace_range(range, &replacement);
    Ok(text)
}

/// The `permission` value with `tool` set to `effect`. Built whole, because
/// order matters: OpenCode applies the last matching rule, so `*` has to stay
/// ahead of the per-tool rules.
fn updated_permission(current: Option<&Value>, tool: &Tool, effect: PermissionEffect) -> Value {
    let mut object = match current {
        Some(Value::Object(object)) => object.clone(),
        Some(value @ Value::String(_)) if tool.action != "*" => {
            let mut object = Map::new();
            object.insert("*".into(), value.clone());
            object
        }
        _ => Map::new(),
    };
    if tool.action == "*" && object.is_empty() {
        return effect_value(effect);
    }
    let existing_key = object
        .keys()
        .rfind(|key| tool.keys.contains(&key.as_str()))
        .cloned();
    match existing_key {
        // Patterns stay; their catch-all changes.
        Some(key) if object.get(&key).is_some_and(Value::is_object) => {
            if let Some(Value::Object(patterns)) = object.get_mut(&key) {
                if patterns.contains_key("*") {
                    patterns.insert("*".into(), effect_value(effect));
                } else {
                    let mut reordered = Map::new();
                    reordered.insert("*".into(), effect_value(effect));
                    reordered.extend(std::mem::take(patterns));
                    *patterns = reordered;
                }
            }
        }
        Some(key) => {
            object.insert(key, effect_value(effect));
        }
        None if tool.action == "*" => {
            let mut reordered = Map::new();
            reordered.insert("*".into(), effect_value(effect));
            reordered.extend(object);
            object = reordered;
        }
        None => {
            object.insert(tool.keys[0].into(), effect_value(effect));
        }
    }
    Value::Object(object)
}

pub fn set_permission(
    text: &str,
    scope: &Scope,
    tool: &Tool,
    effect: PermissionEffect,
) -> Result<String> {
    let path = scope.permission_path();
    let config = parse(text)?;
    let permission = updated_permission(lookup(&config, &path), tool, effect);
    replace(text, &path, Some(&permission))
}

/// The rules the file itself sets for `scope`.
pub fn file_rules(text: &str, scope: &Scope) -> Result<Vec<PermissionRule>> {
    let config = parse(text)?;
    Ok(lookup(&config, &scope.permission_path())
        .map(rules_from_value)
        .unwrap_or_default())
}

/// Sets an agent's model, as `provider/model`, or clears it.
pub fn set_agent_model(text: &str, agent_id: &str, model: Option<&str>) -> Result<String> {
    let path = ["agent", agent_id, "model"];
    let config = parse(text)?;
    if model.is_none() && lookup(&config, &path).is_none() {
        return Ok(text.to_string());
    }
    replace(
        text,
        &path,
        model.map(|model| Value::String(model.into())).as_ref(),
    )
}

/// Whether the file defines the agent, so deleting it there removes it.
pub fn defines_agent(text: &str, agent_id: &str) -> bool {
    parse(text)
        .ok()
        .is_some_and(|config| lookup(&config, &["agent", agent_id]).is_some())
}

pub fn create_agent(text: &str, agent_id: &str, agent: &Value) -> Result<String> {
    if defines_agent(text, agent_id) {
        return Err(anyhow!("An agent named \"{agent_id}\" already exists"));
    }
    replace(text, &["agent", agent_id], Some(agent))
}

pub fn delete_agent(text: &str, agent_id: &str) -> Result<String> {
    let config = parse(text)?;
    if lookup(&config, &["agent", agent_id]).is_none() {
        return Err(anyhow!("\"{agent_id}\" isn't defined in this file"));
    }
    let text = replace(text, &["agent", agent_id], None)?;
    // An emptied `agent` object is left out too.
    let config = parse(&text)?;
    if config
        .get("agent")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        return replace(&text, &["agent"], None);
    }
    Ok(text)
}

/// The ID OpenCode would give an agent named `name`: lowercase words joined
/// by hyphens.
pub fn agent_id_for_name(name: &str) -> String {
    name.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

/// A new primary agent that starts as a copy of `base`, with each listed
/// permission set to what `base` does now.
pub fn forked_agent(base: Option<&crate::types::AgentInfo>) -> Value {
    let mut agent = Map::new();
    agent.insert("mode".into(), Value::String("primary".into()));
    let Some(base) = base else {
        return Value::Object(agent);
    };
    if let Some(description) = &base.description {
        agent.insert("description".into(), Value::String(description.clone()));
    }
    if let Some(model) = &base.model {
        agent.insert(
            "model".into(),
            Value::String(format!("{}/{}", model.provider_id, model.id)),
        );
        if let Some(variant) = &model.variant {
            agent.insert("variant".into(), Value::String(variant.clone()));
        }
    }
    if let Some(system) = &base.system {
        agent.insert("prompt".into(), Value::String(system.clone()));
    }
    let mut permission = Map::new();
    if let Some(effect) = effective(&base.permissions, "*") {
        permission.insert("*".into(), effect_value(effect));
    }
    for tool in TOOLS {
        if let Some(effect) = effective(&base.permissions, tool.action) {
            permission.insert(tool.keys[0].into(), effect_value(effect));
        }
    }
    if !permission.is_empty() {
        agent.insert("permission".into(), Value::Object(permission));
    }
    Value::Object(agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(action: &str) -> &'static Tool {
        TOOLS
            .iter()
            .find(|tool| tool.action == action)
            .expect("known tool")
    }

    fn rule(action: &str, resource: &str, effect: PermissionEffect) -> PermissionRule {
        PermissionRule {
            action: action.into(),
            resource: resource.into(),
            effect,
        }
    }

    #[test]
    fn the_last_matching_rule_wins() {
        use PermissionEffect::*;
        let rules = vec![
            rule("*", "*", Allow),
            rule("read", "*.env", Ask),
            rule("edit", "*", Deny),
            rule("edit", "/plans/*", Allow),
        ];
        assert_eq!(effective(&rules, "edit"), Some(Deny));
        assert_eq!(effective(&rules, "read"), Some(Allow));
        assert!(has_pattern_rules(&rules, "read"));
        assert!(!has_pattern_rules(&rules, "shell"));
        assert_eq!(effective(&[], "read"), None);
        assert!(matches("git", "git *"));
        assert!(matches("git status", "git *"));
        assert!(!matches("*", "git *"));
    }

    #[test]
    fn setting_a_tool_keeps_the_catch_all_first() {
        let text = "{\n  // mine\n  \"permission\": \"allow\",\n  \"plugin\": []\n}\n";
        let text = set_permission(text, &Scope::Global, tool("edit"), PermissionEffect::Ask)
            .expect("edit");
        assert!(text.contains("// mine"), "{text}");
        assert_eq!(
            parse(&text).expect("valid")["permission"],
            json!({"*": "allow", "edit": "ask"})
        );
        let rules = file_rules(&text, &Scope::Global).expect("rules");
        assert_eq!(effective(&rules, "edit"), Some(PermissionEffect::Ask));
        assert_eq!(effective(&rules, "read"), Some(PermissionEffect::Allow));

        // The catch-all goes ahead of existing tools, so it doesn't override them.
        let text = "{\"permission\": {\"edit\": \"deny\"}}";
        let text =
            set_permission(text, &Scope::Global, &ALL_TOOLS, PermissionEffect::Ask).expect("all");
        assert_eq!(
            parse(&text).expect("valid")["permission"],
            json!({"*": "ask", "edit": "deny"})
        );
    }

    #[test]
    fn setting_a_tool_updates_its_existing_key_and_patterns() {
        let text =
            r#"{"agent": {"build": {"permission": {"bash": {"git *": "allow"}, "task": "ask"}}}}"#;
        let scope = Scope::Agent("build".into());
        let text =
            set_permission(text, &scope, tool("shell"), PermissionEffect::Deny).expect("shell");
        let text = set_permission(&text, &scope, tool("subagent"), PermissionEffect::Allow)
            .expect("subagent");
        assert_eq!(
            parse(&text).expect("valid")["agent"]["build"]["permission"],
            json!({"bash": {"*": "deny", "git *": "allow"}, "task": "allow"})
        );
    }

    #[test]
    fn agents_are_created_and_deleted_in_an_empty_file() {
        let agent = json!({"mode": "primary", "permission": {"edit": "deny"}});
        let text = create_agent("", "review", &agent).expect("create");
        assert_eq!(parse(&text).expect("valid")["agent"]["review"], agent);
        assert!(defines_agent(&text, "review"));
        assert!(create_agent(&text, "review", &agent).is_err());

        let text = set_agent_model(&text, "review", Some("openai/gpt-5")).expect("model");
        assert_eq!(
            parse(&text).expect("valid")["agent"]["review"]["model"],
            json!("openai/gpt-5")
        );
        let text = set_agent_model(&text, "review", None).expect("clear");
        assert!(
            parse(&text).expect("valid")["agent"]["review"]
                .get("model")
                .is_none()
        );

        let text = delete_agent(&text, "review").expect("delete");
        assert!(
            parse(&text).expect("valid").get("agent").is_none(),
            "{text}"
        );
        assert!(delete_agent(&text, "review").is_err());
    }

    #[test]
    fn a_broken_file_is_left_alone() {
        assert!(
            set_permission(
                "{\"a\": ",
                &Scope::Global,
                &ALL_TOOLS,
                PermissionEffect::Ask
            )
            .is_err()
        );
        assert!(parse("[]").is_err());
    }

    #[test]
    fn the_global_folder_is_the_first_reported() {
        let directory = |path: &str| ConfigEntry::Directory { path: path.into() };
        let document = ConfigEntry::Document {
            path: Some("/managed/policy.json".into()),
        };
        let missing = std::env::temp_dir().join("opencode-config-test-missing");
        let path = global_config_path(&[
            document,
            directory(missing.to_str().expect("utf-8")),
            directory("/project/.opencode"),
        ]);
        assert_eq!(path, Some(missing.join("opencode.jsonc")));
        assert_eq!(global_config_path(&[directory("/project/.opencode")]), None);
        assert_eq!(global_config_path(&[]), None);
    }

    #[test]
    fn forks_copy_what_the_base_does() {
        let base: crate::types::AgentInfo = serde_json::from_value(json!({
            "id": "plan",
            "name": "Plan",
            "description": "Plans",
            "model": {"providerID": "openai", "id": "gpt-5", "variant": "high"},
            "permissions": [
                {"action": "*", "resource": "*", "effect": "allow"},
                {"action": "edit", "resource": "*", "effect": "deny"},
            ],
        }))
        .expect("agent");
        let agent = forked_agent(Some(&base));
        assert_eq!(agent["model"], json!("openai/gpt-5"));
        assert_eq!(agent["variant"], json!("high"));
        assert_eq!(agent["permission"]["*"], json!("allow"));
        assert_eq!(agent["permission"]["edit"], json!("deny"));
        assert_eq!(agent["permission"]["bash"], json!("allow"));
        assert_eq!(agent_id_for_name("My Review  Agent!"), "my-review-agent");
    }
}
