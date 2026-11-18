//! OpenCode's global config file, which Zed edits itself: the server's only
//! config write route takes nothing but `shell`.
//!
//! Edits keep the rest of the file's text and comments as they are, and the
//! service reloads when the file changes. Zed writes only what OpenCode's
//! documented V2 config holds: agents in `agents`, and permission rules in
//! `permissions`. V1 entries are left as they are: Zed never reads them for
//! editing and never removes them.

use crate::client::OpenCodeClient;
use crate::project_setup::BUILT_IN_AGENTS;
use crate::types::{AgentInfo, ConfigEntry, PermissionEffect, PermissionRule};
use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashMap;
use fs::Fs;
use futures::FutureExt as _;
use futures::future::Shared;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, SharedString, Task, WeakEntity,
};
use serde::Deserialize as _;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use util::ResultExt as _;

/// OpenCode's global config file, shared by the settings pages and the
/// agents modal.
#[derive(Clone)]
pub struct GlobalConfigFile {
    path: PathBuf,
    fs: Arc<dyn Fs>,
}

impl GlobalConfigFile {
    pub async fn locate(client: &OpenCodeClient, directory: &str, fs: Arc<dyn Fs>) -> Result<Self> {
        let entries = client.list_config_entries(directory).await?;
        let path =
            find_global_config_path(&entries, &resolve_global_config_directory(), fs.as_ref())
                .await?;
        Ok(Self { path, fs })
    }

    /// Where edits are saved, for the pages that make them.
    pub fn describe_saved_location(&self) -> String {
        let path = display_path(&self.path, util::paths::home_dir());
        format!("Saved to {path}. Project config files can override it.")
    }

    /// The file's text, once the edits started before are done.
    pub fn read(&self, cx: &App) -> Task<Result<String>> {
        let latest_edit = self.latest_edit(cx);
        let file = self.clone();
        cx.spawn(async move |_| {
            if let Some(latest_edit) = latest_edit {
                // Its own caller reports how it went.
                latest_edit.await.ok();
            }
            file.load_from_disk().await
        })
    }

    /// Applies `change` to the file's JSON as it is once the edits started
    /// before are done, so changes made elsewhere meanwhile are kept. Only
    /// the values that differ are rewritten, so comments elsewhere stay, and
    /// a changed list is written whole. A file that doesn't parse is
    /// refused, so a typo is reported instead of overwritten. Returns the
    /// new JSON. The edit finishes even if the returned task is dropped.
    pub fn edit(
        &self,
        change: impl FnOnce(&mut Value) -> Result<()> + 'static,
        cx: &mut App,
    ) -> Task<Result<Value>> {
        let previous = self.latest_edit(cx);
        let file = self.clone();
        let edit = cx
            .spawn(async move |_| {
                if let Some(previous) = previous {
                    // Its own caller reports how it went.
                    previous.await.ok();
                }
                file.apply(change).await.map_err(Arc::new)
            })
            .shared();
        cx.default_global::<LatestEdits>()
            .0
            .insert(self.path.clone(), edit.clone());
        cx.spawn(async move |_| edit.await.map_err(|error| anyhow!("{error:#}")))
    }

    fn latest_edit(&self, cx: &App) -> Option<Shared<Task<Result<Value, Arc<anyhow::Error>>>>> {
        cx.try_global::<LatestEdits>()?.0.get(&self.path).cloned()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn load_from_disk(&self) -> Result<String> {
        if self.fs.is_file(&self.path).await {
            self.fs.load(&self.path).await
        } else {
            Ok(String::new())
        }
    }

    async fn apply(&self, change: impl FnOnce(&mut Value) -> Result<()>) -> Result<Value> {
        let mut text = self.load_from_disk().await?;
        let old = parse(&text)?;
        let mut new = old.clone();
        change(&mut new)?;
        if new != old {
            if text.trim().is_empty() {
                text = "{\n}\n".to_string();
            }
            let tab_size = settings_json::infer_json_indent_size(&text);
            settings_json::update_value_in_json_text(
                &mut text,
                &mut Vec::new(),
                tab_size,
                &old,
                &new,
                &mut Vec::new(),
            );
            if let Some(directory) = self.path.parent() {
                self.fs.create_dir(directory).await?;
            }
            self.fs.atomic_write(self.path.clone(), text).await?;
        }
        Ok(new)
    }
}

fn display_path(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The global config file's contents, shared by the agents modal and the
/// settings pages. It rereads the file when the service reports a config
/// reload, and takes an edit's result when the edit finishes, so its readers
/// never read the file themselves.
pub struct LoadedConfig {
    state: ConfigFileState,
    client: OpenCodeClient,
    /// Names the service requests. The global file is the same for every
    /// directory.
    directory: String,
    fs: Arc<dyn Fs>,
    /// Starting another load cancels this one, so the newest read wins.
    _load: Task<()>,
    _reloads: Task<()>,
}

pub enum ConfigFileState {
    Loading,
    /// The file couldn't be found or read.
    Unreadable(SharedString),
    /// The file was read but doesn't parse, so it can't be edited.
    Invalid(SharedString),
    Loaded {
        file: GlobalConfigFile,
        config: Value,
    },
}

impl ConfigFileState {
    /// Reads `file` once the edits started before are done.
    async fn read(file: GlobalConfigFile, cx: &mut AsyncApp) -> Self {
        match cx.update(|cx| file.read(cx)).await {
            Err(error) => Self::Unreadable(format!("{error:#}").into()),
            Ok(text) => match parse(&text) {
                Ok(config) => Self::Loaded { file, config },
                Err(error) => Self::Invalid(format!("{error:#}").into()),
            },
        }
    }
}

/// The config held by the current readers, if any.
#[derive(Default)]
struct SharedLoadedConfig(Option<WeakEntity<LoadedConfig>>);

impl Global for SharedLoadedConfig {}

impl LoadedConfig {
    /// The config its other readers hold, or a new one that starts loading.
    pub fn shared(
        client: OpenCodeClient,
        directory: String,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) -> Entity<Self> {
        if let Some(config) = cx
            .try_global::<SharedLoadedConfig>()
            .and_then(|shared| shared.0.as_ref()?.upgrade())
        {
            return config;
        }
        let config = cx.new(|cx| {
            let mut changes = client.watch_config_changes();
            let reloads = cx.spawn(async move |this, cx| {
                while changes.changed().await.is_ok() {
                    if this.update(cx, Self::reload).is_err() {
                        break;
                    }
                }
            });
            let mut config = Self {
                state: ConfigFileState::Loading,
                client,
                directory,
                fs,
                _load: Task::ready(()),
                _reloads: reloads,
            };
            config.reload(cx);
            config
        });
        cx.set_global(SharedLoadedConfig(Some(config.downgrade())));
        config
    }

    pub fn state(&self) -> &ConfigFileState {
        &self.state
    }

    /// Logs a config edit failure and returns the message shown to the user.
    pub fn report_edit_error(error: &anyhow::Error) -> SharedString {
        log::error!("Failed to update OpenCode's config: {error:#}");
        format!("{error:#}").into()
    }

    /// Applies `change` the way [`GlobalConfigFile::edit`] does. Fails when
    /// the file isn't loaded or the edit fails. After a failed edit the file
    /// is read again, as it may have changed.
    pub fn edit(
        &mut self,
        change: impl FnOnce(&mut Value) -> Result<()> + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let ConfigFileState::Loaded { file, .. } = &self.state else {
            return Task::ready(Err(anyhow!("OpenCode's config file isn't loaded.")));
        };
        let file = file.clone();
        let written = file.edit(change, cx);
        cx.spawn(async move |this, cx| {
            let (state, result) = match written.await {
                Ok(config) => (ConfigFileState::Loaded { file, config }, Ok(())),
                Err(error) => (ConfigFileState::read(file, cx).await, Err(error)),
            };
            this.update(cx, |this, cx| {
                // A reload still running may have read the file before this
                // edit wrote it, so its result would be older than this one.
                this._load = Task::ready(());
                this.state = state;
                cx.notify();
            })?;
            result
        })
    }

    /// Locates the file again too, since which file the service reads can
    /// change.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let directory = self.directory.clone();
        let fs = self.fs.clone();
        self._load = cx.spawn(async move |this, cx| {
            let state = match GlobalConfigFile::locate(&client, &directory, fs).await {
                Ok(file) => ConfigFileState::read(file, cx).await,
                Err(error) => ConfigFileState::Unreadable(format!("{error:#}").into()),
            };
            this.update(cx, |this, cx| {
                this.state = state;
                cx.notify();
            })
            .log_err();
        });
    }
}

/// The latest edit of each config file. Each edit waits for the one before,
/// so it applies to the file as that one left it.
#[derive(Default)]
struct LatestEdits(HashMap<PathBuf, Shared<Task<Result<Value, Arc<anyhow::Error>>>>>);

impl Global for LatestEdits {}

/// The global config folder OpenCode's docs name: `~/.config/opencode`.
/// The docs mention `$XDG_CONFIG_HOME` only for the CLI's own config file,
/// but it moves the same config root, so it's honored here too.
pub fn resolve_global_config_directory() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| util::paths::home_dir().join(".config"))
        .join("opencode")
}

/// The file in `directory` to edit, once the service confirms it reads that
/// folder. A service started with another environment reads another folder,
/// and a file it never reads mustn't be edited.
///
/// The docs allow `opencode.json` or `opencode.jsonc` and don't say which
/// wins if both exist. Zed's choice is `opencode.jsonc`, unless only
/// `opencode.json` exists.
async fn find_global_config_path(
    entries: &[ConfigEntry],
    directory: &Path,
    fs: &dyn Fs,
) -> Result<PathBuf> {
    let read_by_service = entries.iter().any(|entry| match entry {
        ConfigEntry::Directory { path } => Path::new(path) == directory,
        _ => false,
    });
    if !read_by_service {
        bail!(
            "OpenCode isn't reading its global config from {}, so Zed can't change it. \
             The OpenCode service may have been started with a different environment.",
            directory.display()
        );
    }
    let jsonc = directory.join("opencode.jsonc");
    let json = directory.join("opencode.json");
    if !fs.is_file(&jsonc).await && fs.is_file(&json).await {
        Ok(json)
    } else {
        Ok(jsonc)
    }
}

/// Sets the value at `path`, adding the objects on the way. Fails when one
/// of them is something else, rather than replace the user's value.
pub fn set(config: &mut Value, path: &[&str], value: Value) -> Result<()> {
    let (key, parents) = path.split_last().context("no path to set")?;
    let mut object = config
        .as_object_mut()
        .context("OpenCode's config isn't a JSON object")?;
    for parent in parents {
        object = object
            .entry(*parent)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .with_context(|| format!("OpenCode's config has a `{parent}` that isn't an object"))?;
    }
    object.insert(key.to_string(), value);
    Ok(())
}

/// Removes the value at `path`, and the parents it was the only entry of.
/// Returns whether the value was there.
pub fn remove(config: &mut Value, path: &[&str]) -> bool {
    let Some((key, rest)) = path.split_first() else {
        return false;
    };
    let Some(object) = config.as_object_mut() else {
        return false;
    };
    if rest.is_empty() {
        return object.remove(*key).is_some();
    }
    let Some(child) = object.get_mut(*key) else {
        return false;
    };
    if !remove(child, rest) {
        return false;
    }
    if child.as_object().is_some_and(Map::is_empty) {
        object.remove(*key);
    }
    true
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

pub fn lookup<'a>(config: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(config, |value, key| value.get(*key))
}

/// A permission OpenCode checks, as shown in the agents modal.
pub(crate) struct Tool {
    /// The action OpenCode's permission rules name.
    pub action: &'static str,
    pub label: &'static str,
}

/// The rule for every action without one of its own.
pub(crate) const ALL_TOOLS: Tool = Tool {
    action: "*",
    label: "All Tools",
};

/// The built-in tools' actions from OpenCode's permission docs.
pub(crate) const TOOLS: &[Tool] = &[
    Tool {
        action: "read",
        label: "Read Files",
    },
    Tool {
        action: "edit",
        label: "Edit Files",
    },
    Tool {
        action: "glob",
        label: "Find Files",
    },
    Tool {
        action: "grep",
        label: "Search File Contents",
    },
    Tool {
        action: "shell",
        label: "Run Shell Commands",
    },
    Tool {
        action: "webfetch",
        label: "Fetch Web Pages",
    },
    Tool {
        action: "websearch",
        label: "Search the Web",
    },
    Tool {
        action: "subagent",
        label: "Start Subagents",
    },
    Tool {
        action: "skill",
        label: "Load Skills",
    },
    Tool {
        action: "question",
        label: "Ask Questions",
    },
    Tool {
        action: "external_directory",
        label: "Work Outside the Project",
    },
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Scope {
    AllAgents,
    Agent(String),
}

impl Scope {
    /// Where the file keeps the scope's permission rules.
    fn locate_permissions(&self) -> Vec<&str> {
        match self {
            Scope::AllAgents => vec!["permissions"],
            Scope::Agent(agent_id) => vec!["agents", agent_id, "permissions"],
        }
    }
}

/// Whether some rule for `action` only covers some files, commands or URLs.
pub(crate) fn has_resource_rules(rules: &[PermissionRule], action: &str) -> bool {
    rules
        .iter()
        .any(|rule| rule.action == action && rule.resource != "*")
}

/// Sets `tool`'s general rule to `effect`, or removes it. The scope's list
/// is written back whole.
pub(crate) fn set_permission(
    config: &mut Value,
    scope: &Scope,
    tool: &Tool,
    effect: Option<PermissionEffect>,
) -> Result<()> {
    let mut rules = read_file_rules(config, scope)?;
    rules.retain(|rule| !(rule.action == tool.action && rule.resource == "*"));
    if let Some(effect) = effect {
        // OpenCode applies the last matching rule, so the general rule goes
        // before the action's narrower patterns, which still win, and the
        // rule for every action goes first.
        let position = if tool.action == "*" {
            0
        } else {
            rules
                .iter()
                .position(|rule| rule.action == tool.action)
                .unwrap_or(rules.len())
        };
        rules.insert(position, PermissionRule::general(tool.action, effect));
        if find_general_effect(&rules, tool.action) != Some(effect) {
            bail!(
                "A later permission rule overrides the selected setting for {}.",
                tool.label
            );
        }
    }
    let path = scope.locate_permissions();
    if rules.is_empty() {
        remove(config, &path);
        Ok(())
    } else {
        set(config, &path, serde_json::to_value(rules)?)
    }
}

/// The rules the file itself sets for `scope`, in order.
pub(crate) fn read_file_rules(config: &Value, scope: &Scope) -> Result<Vec<PermissionRule>> {
    let Some(rules) = lookup(config, &scope.locate_permissions()) else {
        return Ok(Vec::new());
    };
    // The list is written back whole, so a rule that can't be read is an
    // error rather than something to drop from the user's file.
    let rules = rules
        .as_array()
        .context("OpenCode's config has a `permissions` value that isn't a list")?;
    rules
        .iter()
        .map(|rule| {
            PermissionRule::deserialize(rule).with_context(|| {
                format!("OpenCode's config has a permission rule Zed can't read: {rule}")
            })
        })
        .collect()
}

/// Sets an agent's model, as `provider/model`, or clears it.
pub(crate) fn set_agent_model(
    config: &mut Value,
    agent_id: &str,
    model: Option<&str>,
) -> Result<()> {
    let path = ["agents", agent_id, "model"];
    match model {
        Some(model) => set(config, &path, Value::String(model.into())),
        None => {
            remove(config, &path);
            Ok(())
        }
    }
}

/// Adds a primary agent that starts as a copy of `base`, with each listed
/// permission set to what `base` does now.
pub(crate) fn create_agent(
    config: &mut Value,
    agent_id: &str,
    base: Option<&AgentInfo>,
) -> Result<()> {
    if defines_agent(config, agent_id) {
        bail!("An agent named \"{agent_id}\" already exists.");
    }
    set(config, &["agents", agent_id], build_agent_entry(base)?)
}

pub(crate) fn delete_agent(config: &mut Value, agent_id: &str) -> Result<()> {
    check_can_delete(config, agent_id).map_err(|reason| anyhow!(reason))?;
    remove(config, &["agents", agent_id]);
    Ok(())
}

/// Only the agents this file defines can be deleted, and OpenCode's built-in
/// ones can't, since they'd come back.
pub(crate) fn check_can_delete(config: &Value, agent_id: &str) -> Result<(), &'static str> {
    if BUILT_IN_AGENTS.contains(&agent_id) {
        Err("OpenCode's built-in agents can't be deleted.")
    } else if !defines_agent(config, agent_id) {
        Err("This agent is defined in a file other than OpenCode's global config.")
    } else {
        Ok(())
    }
}

/// The ID Zed uses for an agent created with the name `name`: lowercase
/// words joined by hyphens.
pub(crate) fn to_agent_id(name: &str) -> String {
    name.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

fn build_agent_entry(base: Option<&AgentInfo>) -> Result<Value> {
    let mut agent = Map::new();
    agent.insert("mode".into(), Value::String("primary".into()));
    let Some(base) = base else {
        return Ok(Value::Object(agent));
    };
    if let Some(description) = &base.description {
        agent.insert("description".into(), Value::String(description.clone()));
    }
    if let Some(model) = &base.model {
        agent.insert("model".into(), Value::String(model.to_config_reference()));
    }
    if let Some(system) = &base.system {
        agent.insert("system".into(), Value::String(system.clone()));
    }
    let permissions: Vec<PermissionRule> = std::iter::once(&ALL_TOOLS)
        .chain(TOOLS)
        .filter_map(|tool| {
            let effect = find_general_effect(&base.permissions, tool.action)?;
            Some(PermissionRule::general(tool.action, effect))
        })
        .collect();
    if !permissions.is_empty() {
        agent.insert("permissions".into(), serde_json::to_value(permissions)?);
    }
    Ok(Value::Object(agent))
}

/// Finds what OpenCode does for `action` when no narrower pattern applies:
/// the effect of the last rule covering the action with any resource. `None`
/// when no rule does.
pub(crate) fn find_general_effect(
    rules: &[PermissionRule],
    action: &str,
) -> Option<PermissionEffect> {
    rules
        .iter()
        .rfind(|rule| {
            matches_wildcard(action, &rule.action) && matches_wildcard("*", &rule.resource)
        })
        .map(|rule| rule.effect)
}

fn defines_agent(config: &Value, agent_id: &str) -> bool {
    lookup(config, &["agents", agent_id]).is_some()
}

/// The wildcards OpenCode's permission docs define: `*` matches any run of
/// characters, `?` exactly one, and a pattern ending in ` *` also matches
/// the bare command, so `git *` matches `git`.
fn matches_wildcard(value: &str, pattern: &str) -> bool {
    fn matches_glob(value: &[char], pattern: &[char]) -> bool {
        match pattern.split_first() {
            None => value.is_empty(),
            Some(('*', rest)) => (0..=value.len()).any(|skip| matches_glob(&value[skip..], rest)),
            Some(('?', rest)) => !value.is_empty() && matches_glob(&value[1..], rest),
            Some((character, rest)) => {
                value.first() == Some(character) && matches_glob(&value[1..], rest)
            }
        }
    }
    let value: Vec<char> = value.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    matches_glob(&value, &pattern)
        || pattern
            .strip_suffix(&[' ', '*'])
            .is_some_and(|prefix| matches_glob(&value, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
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
    fn home_display_respects_path_components() {
        assert_eq!(
            display_path(Path::new("/home/som/config"), Path::new("/home/som")),
            "~/config"
        );
        assert_eq!(
            display_path(Path::new("/home/som2/config"), Path::new("/home/som")),
            "/home/som2/config"
        );
    }

    #[test]
    fn overridden_permission_is_refused() {
        let mut config = json!({"permissions": [
            {"action": "shell", "resource": "git *", "effect": "allow"},
            {"action": "*", "resource": "*", "effect": "deny"}
        ]});
        let original = config.clone();
        assert!(
            set_permission(
                &mut config,
                &Scope::AllAgents,
                tool("shell"),
                Some(PermissionEffect::Allow)
            )
            .is_err()
        );
        assert_eq!(config, original);
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
        assert_eq!(find_general_effect(&rules, "edit"), Some(Deny));
        assert_eq!(find_general_effect(&rules, "read"), Some(Allow));
        assert!(has_resource_rules(&rules, "read"));
        assert!(!has_resource_rules(&rules, "shell"));
        assert_eq!(find_general_effect(&[], "read"), None);
        assert!(matches_wildcard("git", "git *"));
        assert!(matches_wildcard("git status", "git *"));
        assert!(!matches_wildcard("*", "git *"));
    }

    #[test]
    fn setting_a_tool_keeps_the_general_rule_first() {
        let mut config =
            json!({"permissions": [{"action": "edit", "resource": "*", "effect": "deny"}]});
        set_permission(
            &mut config,
            &Scope::AllAgents,
            &ALL_TOOLS,
            Some(PermissionEffect::Ask),
        )
        .expect("all");
        assert_eq!(
            config["permissions"],
            json!([
                {"action": "*", "resource": "*", "effect": "ask"},
                {"action": "edit", "resource": "*", "effect": "deny"},
            ])
        );
    }

    #[test]
    fn setting_a_tool_keeps_its_patterns_winning() {
        let mut config = json!({"agents": {"build": {"permissions": [
            {"action": "shell", "resource": "git *", "effect": "allow"},
            {"action": "subagent", "resource": "*", "effect": "ask"}
        ]}}});
        let scope = Scope::Agent("build".into());
        set_permission(
            &mut config,
            &scope,
            tool("shell"),
            Some(PermissionEffect::Deny),
        )
        .expect("shell");
        set_permission(
            &mut config,
            &scope,
            tool("subagent"),
            Some(PermissionEffect::Allow),
        )
        .expect("subagent");
        assert_eq!(
            config["agents"]["build"]["permissions"],
            json!([
                {"action": "shell", "resource": "*", "effect": "deny"},
                {"action": "shell", "resource": "git *", "effect": "allow"},
                {"action": "subagent", "resource": "*", "effect": "allow"},
            ])
        );
    }

    #[test]
    fn removing_all_agents_permission_preserves_v1_entries() {
        let mut config = json!({
            "tools": {"bash": false},
            "permission": {"shell": {"*": "ask", "git *": "allow"}},
            "permissions": [
                {"action": "shell", "resource": "*", "effect": "deny"},
                {"action": "read", "resource": "*", "effect": "ask"}
            ],
            "agents": {"review": {"mode": "primary"}}
        });
        set_permission(&mut config, &Scope::AllAgents, tool("shell"), None).expect("removed");
        assert_eq!(config["tools"], json!({"bash": false}));
        assert_eq!(
            config["permission"],
            json!({"shell": {"*": "ask", "git *": "allow"}})
        );
        assert_eq!(
            config["permissions"],
            json!([{"action": "read", "resource": "*", "effect": "ask"}])
        );
        assert_eq!(config["agents"]["review"], json!({"mode": "primary"}));
    }

    #[test]
    fn removing_the_last_all_agents_permission_removes_the_list() {
        let mut config = json!({});
        set_permission(
            &mut config,
            &Scope::AllAgents,
            &ALL_TOOLS,
            Some(PermissionEffect::Deny),
        )
        .expect("set");
        set_permission(&mut config, &Scope::AllAgents, &ALL_TOOLS, None).expect("removed");
        assert_eq!(config, json!({}));
    }

    #[test]
    fn agent_edits_work_on_an_empty_config() {
        let mut config = json!({});
        create_agent(&mut config, "review", None).expect("create");
        assert_eq!(config["agents"]["review"], json!({"mode": "primary"}));
        assert_eq!(check_can_delete(&config, "review"), Ok(()));
        assert!(create_agent(&mut config, "review", None).is_err());

        set_agent_model(&mut config, "review", Some("openai/gpt-5")).expect("model");
        assert_eq!(config["agents"]["review"]["model"], json!("openai/gpt-5"));
        set_agent_model(&mut config, "review", None).expect("clear");
        assert!(config["agents"]["review"].get("model").is_none());
        delete_agent(&mut config, "review").expect("delete");
        assert_eq!(config, json!({}));
        assert!(delete_agent(&mut config, "review").is_err());
    }

    #[test]
    fn built_in_agents_are_not_deleted() {
        let mut config = json!({"agents": {"build": {"model": "openai/gpt-5"}}});
        assert!(check_can_delete(&config, "build").is_err());
        assert!(delete_agent(&mut config, "build").is_err());
    }

    #[test]
    fn removing_a_value_prunes_only_parents_it_empties() {
        let path = &["agents", "compaction", "model"];
        let mut config = json!({"agents": {"compaction": {"model": "openai/gpt-5"}}, "empty": {}});
        assert!(remove(&mut config, path));
        assert_eq!(config, json!({"empty": {}}));

        let mut config = json!({"agents": {"compaction": {"model": "openai/gpt-5", "hidden": true}, "empty": {}}});
        remove(&mut config, path);
        assert_eq!(
            config,
            json!({"agents": {"compaction": {"hidden": true}, "empty": {}}})
        );

        let unchanged = json!({"agents": {"compaction": {}, "empty": {}}});
        let mut config = unchanged.clone();
        assert!(!remove(&mut config, path));
        assert_eq!(config, unchanged);
    }

    #[test]
    fn setting_refuses_to_replace_a_value_on_the_way() {
        let mut config = json!({"agents": "mine"});
        assert!(set(&mut config, &["agents", "review", "mode"], json!("primary")).is_err());
        assert_eq!(config, json!({"agents": "mine"}));
    }

    #[test]
    fn creating_an_agent_copies_what_the_base_does() {
        let base: AgentInfo = serde_json::from_value(json!({
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
        let mut config = json!({});
        create_agent(&mut config, "review", Some(&base)).expect("create");
        let agent = &config["agents"]["review"];
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
        assert_eq!(to_agent_id("My Review  Agent!"), "my-review-agent");
    }

    #[gpui::test]
    async fn the_global_file_is_edited_only_where_the_service_reads_it(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/config", json!({ "opencode.json": "{}" }))
            .await;
        fs.insert_tree("/other", json!({})).await;
        let directory = |path: &str| ConfigEntry::Directory { path: path.into() };
        let document = ConfigEntry::Document {
            path: Some("/managed/policy.json".into()),
            info: Value::Null,
        };
        let entries = vec![
            document,
            directory("/project/.opencode"),
            directory("/config"),
        ];
        let path = |global: &'static str| {
            let fs = fs.clone();
            let entries = entries.clone();
            async move { find_global_config_path(&entries, Path::new(global), fs.as_ref()).await }
        };
        assert_eq!(
            path("/config").await.expect("listed"),
            PathBuf::from("/config/opencode.json")
        );
        assert!(path("/other").await.is_err());
        let fs_entries = vec![directory("/other")];
        assert_eq!(
            find_global_config_path(&fs_entries, Path::new("/other"), fs.as_ref())
                .await
                .expect("listed"),
            PathBuf::from("/other/opencode.jsonc")
        );
    }

    #[gpui::test]
    async fn sequential_edits_preserve_comments(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/config",
            json!({ "opencode.jsonc": "{\n  // mine\n  \"plugins\": [\"a\"]\n}\n" }),
        )
        .await;
        let file = GlobalConfigFile {
            path: PathBuf::from("/config/opencode.jsonc"),
            fs: fs.clone(),
        };
        let (first, second, read) = cx.update(|cx| {
            let first = file.edit(|config| set(config, &["model"], json!("openai/gpt-5")), cx);
            let second = file.edit(|config| set(config, &["default_agent"], json!("plan")), cx);
            (first, second, file.read(cx))
        });
        // Dropped: the edit still reaches the file.
        drop(first);
        second.await.expect("second edit");
        let text = read.await.expect("read");
        assert!(text.contains("// mine"));
        let config = parse(&text).expect("valid");
        assert_eq!(config["model"], json!("openai/gpt-5"));
        assert_eq!(config["default_agent"], json!("plan"));
        assert_eq!(config["plugins"], json!(["a"]));
    }

    #[gpui::test]
    async fn an_edit_refuses_a_file_that_does_not_parse(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/config", json!({ "opencode.jsonc": "{\"agents\": {}, " }))
            .await;
        let file = GlobalConfigFile {
            path: PathBuf::from("/config/opencode.jsonc"),
            fs: fs.clone(),
        };
        let edit =
            cx.update(|cx| file.edit(|config| set(config, &["model"], json!("openai/gpt-5")), cx));
        let error = edit.await.expect_err("broken");
        assert!(format!("{error:#}").contains("reading OpenCode's config"));
        assert_eq!(
            fs.load(Path::new("/config/opencode.jsonc"))
                .await
                .expect("load"),
            "{\"agents\": {}, "
        );
    }
}
