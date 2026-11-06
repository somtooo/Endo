//! Reads and edits OpenCode's global config file for the agents modal.
//!
//! Writes V2's `agents` and `permissions`, and moves a V1 `agent` or
//! `permission` across when it's about to change one, since OpenCode reads
//! both. The rest of the file's text and comments stay as they are. The service
//! watches the file and reloads on change.

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
    fn permissions_path(&self) -> Vec<&str> {
        match self {
            Scope::Global => vec!["permissions"],
            Scope::Agent(id) => vec!["agents", id, "permissions"],
        }
    }

    fn legacy_permission_path(&self) -> Vec<&str> {
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

/// The value at `path`, for settings the config file stores directly.
pub fn value_at<'a>(config: &'a Value, path: &[&str]) -> Option<&'a Value> {
    lookup(config, path)
}

/// Sets the value at `path`, or removes it for `None`. Removing what isn't
/// there changes nothing.
pub fn set_value(text: &str, path: &[&str], value: Option<&Value>) -> Result<String> {
    if value.is_none() && lookup(&parse(text)?, path).is_none() {
        return Ok(text.to_string());
    }
    replace(text, path, value)
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

fn rule_value(rule: &PermissionRule) -> Value {
    serde_json::json!({
        "action": rule.action,
        "resource": rule.resource,
        "effect": effect_value(rule.effect),
    })
}

/// `rules` with `tool`'s catch-all set to `effect`. Order matters, because
/// OpenCode applies the last matching rule: it goes before any of the tool's
/// patterns so they still win, and the rule for every tool goes first.
fn updated_rules(
    mut rules: Vec<PermissionRule>,
    tool: &Tool,
    effect: PermissionEffect,
) -> Vec<PermissionRule> {
    rules.retain(|rule| !(rule.action == tool.action && rule.resource == "*"));
    let position = if tool.action == "*" {
        0
    } else {
        rules
            .iter()
            .position(|rule| rule.action == tool.action)
            .unwrap_or(rules.len())
    };
    rules.insert(
        position,
        PermissionRule {
            action: tool.action.to_string(),
            resource: "*".to_string(),
            effect,
        },
    );
    rules
}

/// The rules the file itself sets for `scope`, in the order OpenCode reads
/// them. Global rules from V1 and V2 add up, but an agent in V2's `agents`
/// replaces the same agent in V1's `agent`.
pub fn file_rules(text: &str, scope: &Scope) -> Result<Vec<PermissionRule>> {
    let config = parse(text)?;
    let native = lookup(&config, &scope.permissions_path());
    let legacy_applies = match scope {
        Scope::Global => true,
        Scope::Agent(id) => lookup(&config, &["agents", id]).is_none(),
    };
    let mut rules = Vec::new();
    if legacy_applies {
        if let Scope::Agent(id) = scope
            && let Some(tools) = lookup(&config, &["agent", id, "tools"])
        {
            rules.extend(rules_from_tools(tools));
        }
        if let Some(legacy) = lookup(&config, &scope.legacy_permission_path()) {
            rules.extend(rules_from_value(legacy));
        }
    }
    if let Some(Value::Array(native)) = native {
        rules.extend(
            native
                .iter()
                .filter_map(|rule| serde_json::from_value(rule.clone()).ok()),
        );
    }
    Ok(rules)
}

/// V1's `tools` map, where `true` allows a tool and `false` denies it.
fn rules_from_tools(tools: &Value) -> Vec<PermissionRule> {
    tools
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(tool, enabled)| {
            Some(PermissionRule {
                action: normalize_action(tool).to_string(),
                resource: "*".to_string(),
                effect: if enabled.as_bool()? {
                    PermissionEffect::Allow
                } else {
                    PermissionEffect::Deny
                },
            })
        })
        .collect()
}

/// A V1 agent in V2's shape, the way OpenCode itself converts one when it
/// loads the file.
fn migrated_agent(legacy: &Map<String, Value>) -> Value {
    const CONVERTED: &[&str] = &[
        "model",
        "variant",
        "prompt",
        "description",
        "mode",
        "hidden",
        "color",
        "steps",
        "maxSteps",
        "disable",
        "permission",
        "tools",
        "options",
        "temperature",
        "top_p",
        "name",
    ];
    let mut agent = Map::new();
    if let Some(model) = legacy.get("model").and_then(Value::as_str) {
        let reference = match legacy.get("variant").and_then(Value::as_str) {
            Some(variant) if !variant.is_empty() => format!("{model}#{variant}"),
            _ => model.to_string(),
        };
        agent.insert("model".into(), Value::String(reference));
    }
    for (from, to) in [
        ("prompt", "system"),
        ("description", "description"),
        ("mode", "mode"),
        ("hidden", "hidden"),
        ("disable", "disabled"),
    ] {
        if let Some(value) = legacy.get(from) {
            agent.insert(to.into(), value.clone());
        }
    }
    if let Some(color) = legacy.get("color").and_then(Value::as_str) {
        // V2 only takes hex colors. OpenCode loads V1's theme color names as grey.
        let color = if color.starts_with('#') {
            color
        } else {
            "#aaaaaa"
        };
        agent.insert("color".into(), Value::String(color.into()));
    }
    if let Some(steps) = legacy.get("steps").or_else(|| legacy.get("maxSteps")) {
        agent.insert("steps".into(), steps.clone());
    }
    let mut rules = legacy
        .get("tools")
        .map(rules_from_tools)
        .unwrap_or_default();
    if let Some(permission) = legacy.get("permission") {
        rules.extend(rules_from_value(permission));
    }
    if !rules.is_empty() {
        agent.insert(
            "permissions".into(),
            Value::Array(rules.iter().map(rule_value).collect()),
        );
    }
    // V1 keeps request settings in `options`, in `temperature` and `top_p`, and
    // in any key it doesn't otherwise know.
    let mut body = legacy
        .get("options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (key, value) in legacy {
        if matches!(key.as_str(), "temperature" | "top_p") || !CONVERTED.contains(&key.as_str()) {
            body.insert(key.clone(), value.clone());
        }
    }
    if !body.is_empty() {
        agent.insert("request".into(), serde_json::json!({ "body": body }));
    }
    Value::Object(agent)
}

/// Moves an agent from V1's `agent` to V2's `agents`. An existing V2 entry is
/// the one OpenCode already uses, so it's kept and the V1 one has no effect.
fn with_agent_migrated(text: &str, agent_id: &str) -> Result<String> {
    let config = parse(text)?;
    let Some(legacy) = lookup(&config, &["agent", agent_id]).and_then(Value::as_object) else {
        return Ok(text.to_string());
    };
    let mut text = text.to_string();
    if lookup(&config, &["agents", agent_id]).is_none() {
        text = replace(&text, &["agents", agent_id], Some(&migrated_agent(legacy)))?;
    }
    remove_agent_entry(&text, "agent", agent_id)
}

/// Removes an agent from `key`, and `key` too once nothing is left in it.
fn remove_agent_entry(text: &str, key: &str, agent_id: &str) -> Result<String> {
    let text = set_value(text, &[key, agent_id], None)?;
    if lookup(&parse(&text)?, &[key])
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        return replace(&text, &[key], None);
    }
    Ok(text)
}

pub fn set_permission(
    text: &str,
    scope: &Scope,
    tool: &Tool,
    effect: PermissionEffect,
) -> Result<String> {
    let text = match scope {
        Scope::Global => text.to_string(),
        Scope::Agent(id) => with_agent_migrated(text, id)?,
    };
    let rules = updated_rules(file_rules(&text, scope)?, tool, effect);
    let rules = Value::Array(rules.iter().map(rule_value).collect());
    let text = replace(&text, &scope.permissions_path(), Some(&rules))?;
    set_value(&text, &scope.legacy_permission_path(), None)
}

/// Sets an agent's model, as `provider/model`, or clears it.
pub fn set_agent_model(text: &str, agent_id: &str, model: Option<&str>) -> Result<String> {
    let text = with_agent_migrated(text, agent_id)?;
    set_value(
        &text,
        &["agents", agent_id, "model"],
        model.map(|model| Value::String(model.into())).as_ref(),
    )
}

/// Whether the file defines the agent, so deleting it there removes it.
pub fn defines_agent(text: &str, agent_id: &str) -> bool {
    parse(text).is_ok_and(|config| {
        lookup(&config, &["agents", agent_id]).is_some()
            || lookup(&config, &["agent", agent_id]).is_some()
    })
}

pub fn create_agent(text: &str, agent_id: &str, agent: &Value) -> Result<String> {
    if defines_agent(text, agent_id) {
        return Err(anyhow!("An agent named \"{agent_id}\" already exists"));
    }
    replace(text, &["agents", agent_id], Some(agent))
}

pub fn delete_agent(text: &str, agent_id: &str) -> Result<String> {
    if !defines_agent(text, agent_id) {
        return Err(anyhow!("\"{agent_id}\" isn't defined in this file"));
    }
    let text = remove_agent_entry(text, "agents", agent_id)?;
    remove_agent_entry(&text, "agent", agent_id)
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
        let mut reference = format!("{}/{}", model.provider_id, model.id);
        if let Some(variant) = &model.variant {
            reference = format!("{reference}#{variant}");
        }
        agent.insert("model".into(), Value::String(reference));
    }
    if let Some(system) = &base.system {
        agent.insert("system".into(), Value::String(system.clone()));
    }
    let mut permissions = Vec::new();
    for tool in std::iter::once(&ALL_TOOLS).chain(TOOLS) {
        if let Some(effect) = effective(&base.permissions, tool.action) {
            permissions.push(rule_value(&PermissionRule {
                action: tool.action.to_string(),
                resource: "*".to_string(),
                effect,
            }));
        }
    }
    if !permissions.is_empty() {
        agent.insert("permissions".into(), Value::Array(permissions));
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
        let text = "{\n  // mine\n  \"permissions\": [{\"action\": \"edit\", \"resource\": \"*\", \"effect\": \"deny\"}]\n}\n";
        let updated =
            set_permission(text, &Scope::Global, &ALL_TOOLS, PermissionEffect::Ask).expect("all");
        assert!(updated.contains("// mine"));
        assert_eq!(
            parse(&updated).expect("valid")["permissions"],
            json!([
                {"action": "*", "resource": "*", "effect": "ask"},
                {"action": "edit", "resource": "*", "effect": "deny"},
            ])
        );
    }

    #[test]
    fn setting_a_tool_keeps_its_patterns_winning() {
        let text = r#"{"agents": {"build": {"permissions": [
            {"action": "shell", "resource": "git *", "effect": "allow"},
            {"action": "subagent", "resource": "*", "effect": "ask"}
        ]}}}"#;
        let scope = Scope::Agent("build".into());
        let text =
            set_permission(text, &scope, tool("shell"), PermissionEffect::Deny).expect("shell");
        let text = set_permission(&text, &scope, tool("subagent"), PermissionEffect::Allow)
            .expect("subagent");
        assert_eq!(
            parse(&text).expect("valid")["agents"]["build"]["permissions"],
            json!([
                {"action": "shell", "resource": "*", "effect": "deny"},
                {"action": "shell", "resource": "git *", "effect": "allow"},
                {"action": "subagent", "resource": "*", "effect": "allow"},
            ])
        );
    }

    #[test]
    fn a_change_moves_v1_permissions_to_v2() {
        let text = "{\n  // mine\n  \"permission\": {\"*\": \"allow\", \"bash\": {\"git *\": \"ask\"}},\n  \"permissions\": [{\"action\": \"read\", \"resource\": \"*\", \"effect\": \"deny\"}]\n}\n";
        let updated = set_permission(text, &Scope::Global, tool("edit"), PermissionEffect::Ask)
            .expect("edit");
        assert!(updated.contains("// mine"));
        let config = parse(&updated).expect("valid");
        assert!(config.get("permission").is_none());
        assert_eq!(
            config["permissions"],
            json!([
                {"action": "*", "resource": "*", "effect": "allow"},
                {"action": "shell", "resource": "git *", "effect": "ask"},
                {"action": "read", "resource": "*", "effect": "deny"},
                {"action": "edit", "resource": "*", "effect": "ask"},
            ])
        );
    }

    #[test]
    fn a_change_to_a_v1_agent_moves_all_of_it() {
        let text = r##"{"agent": {"review": {
            "mode": "primary",
            "description": "Reviews",
            "prompt": "Be strict",
            "model": "openai/gpt-5",
            "variant": "high",
            "color": "#ff0000",
            "maxSteps": 8,
            "disable": false,
            "temperature": 0.2,
            "custom": 1,
            "tools": {"write": false},
            "permission": {"bash": {"git *": "ask"}}
        }}}"##;
        let scope = Scope::Agent("review".into());
        let text = set_permission(text, &scope, tool("read"), PermissionEffect::Ask).expect("read");
        let config = parse(&text).expect("valid");
        assert!(config.get("agent").is_none());
        assert_eq!(
            config["agents"]["review"],
            json!({
                "mode": "primary",
                "description": "Reviews",
                "system": "Be strict",
                "model": "openai/gpt-5#high",
                "color": "#ff0000",
                "steps": 8,
                "disabled": false,
                "request": {"body": {"temperature": 0.2, "custom": 1}},
                "permissions": [
                    {"action": "edit", "resource": "*", "effect": "deny"},
                    {"action": "shell", "resource": "git *", "effect": "ask"},
                    {"action": "read", "resource": "*", "effect": "ask"},
                ],
            })
        );
    }

    #[test]
    fn a_v2_agent_wins_over_the_same_v1_agent() {
        let text = r#"{
            "agent": {"review": {"permission": {"edit": "allow"}}, "other": {"mode": "all"}},
            "agents": {"review": {"description": "Mine", "permissions": [
                {"action": "edit", "resource": "*", "effect": "deny"}
            ]}}
        }"#;
        let scope = Scope::Agent("review".into());
        assert_eq!(
            effective(&file_rules(text, &scope).expect("rules"), "edit"),
            Some(PermissionEffect::Deny)
        );
        let updated = set_agent_model(text, "review", Some("openai/gpt-5")).expect("model");
        let config = parse(&updated).expect("valid");
        assert_eq!(config["agents"]["review"]["description"], json!("Mine"));
        assert!(lookup(&config, &["agent", "review"]).is_none());
        assert_eq!(config["agent"]["other"], json!({"mode": "all"}));
    }

    #[test]
    fn agents_are_created_and_deleted_in_an_empty_file() {
        let agent = json!({"mode": "primary", "permissions": []});
        let text = create_agent("", "review", &agent).expect("create");
        assert_eq!(parse(&text).expect("valid")["agents"]["review"], agent);
        assert!(defines_agent(&text, "review"));
        assert!(create_agent(&text, "review", &agent).is_err());

        let text = set_agent_model(&text, "review", Some("openai/gpt-5")).expect("model");
        assert_eq!(
            parse(&text).expect("valid")["agents"]["review"]["model"],
            json!("openai/gpt-5")
        );
        let text = set_agent_model(&text, "review", None).expect("clear");
        assert!(
            parse(&text).expect("valid")["agents"]["review"]
                .get("model")
                .is_none()
        );
        let text = delete_agent(&text, "review").expect("delete");
        assert!(parse(&text).expect("valid").get("agents").is_none());
        assert!(delete_agent(&text, "review").is_err());
    }

    #[test]
    fn v1_agents_can_be_changed_and_deleted() {
        let text = r#"{"agent": {"review": {"model": "openai/old"}}}"#;
        assert!(defines_agent(text, "review"));
        let updated = set_agent_model(text, "review", Some("openai/new")).expect("model");
        let config = parse(&updated).expect("valid");
        assert!(config.get("agent").is_none());
        assert_eq!(config["agents"]["review"]["model"], json!("openai/new"));

        let deleted = delete_agent(text, "review").expect("delete");
        assert!(parse(&deleted).expect("valid").get("agent").is_none());
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
        assert_eq!(agent["model"], json!("openai/gpt-5#high"));
        let permissions = agent["permissions"].as_array().expect("permissions");
        assert_eq!(
            permissions[0],
            json!({"action": "*", "resource": "*", "effect": "allow"})
        );
        assert!(
            permissions.contains(&json!({"action": "edit", "resource": "*", "effect": "deny"}))
        );
        assert!(
            permissions.contains(&json!({"action": "shell", "resource": "*", "effect": "allow"}))
        );
        assert_eq!(agent_id_for_name("My Review  Agent!"), "my-review-agent");
    }
}
