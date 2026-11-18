//! What OpenCode makes available to the threads in a project: commands,
//! skills, agents, models, providers and default choices.
//!
//! It's fetched per project directory, which is the location the OpenCode
//! session reports. A session that moves takes its new directory's lists.
//! Each directory has one project setup, an entity held by whoever reads it
//! (its sessions, the agents modal, the settings page), so it lives as long
//! as they do. Only the connection writes it.

use crate::client::{ConfigDefaults, OpenCodeClient};
use crate::types::{AgentInfo, Command, ModelInfo, ProviderInfo, Skill};
use agent_client_protocol::v2::schema as acp;
use agent_ui::AvailableSkill;
use anyhow::{Context as _, Result};
use gpui::{AsyncApp, Context, EventEmitter};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use util::ResultExt as _;

#[derive(Clone, Default)]
pub struct ProjectSetup {
    pub(crate) commands: Vec<Command>,
    pub(crate) skills: Vec<Skill>,
    pub(crate) agents: Vec<AgentInfo>,
    pub(crate) models: Vec<ModelInfo>,
    pub(crate) providers: Vec<ProviderInfo>,
    pub(crate) defaults: ConfigDefaults,
}

impl ProjectSetup {
    pub fn agents(&self) -> &[AgentInfo] {
        &self.agents
    }

    pub fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    pub(crate) fn has_command(&self, name: &str) -> bool {
        self.commands.iter().any(|command| command.name == name)
    }

    pub(crate) fn has_skill(&self, id: &str) -> bool {
        self.skills.iter().any(|skill| skill.id == id)
    }

    /// Only the connection calls this, with the lists it fetched.
    pub(crate) fn replace(&mut self, fetched: ProjectSetup, cx: &mut Context<Self>) {
        let previous = std::mem::replace(self, fetched);
        cx.emit(ProjectSetupReplaced { previous });
        cx.notify();
    }

    pub(crate) fn provider_name(&self, provider_id: &str) -> String {
        self.providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .map_or(provider_id, |provider| provider.name.as_str())
            .to_string()
    }

    pub(crate) fn find_model(&self, provider_id: &str, id: &str) -> Option<&ModelInfo> {
        self.models
            .iter()
            .find(|model| model.provider_id == provider_id && model.id == id)
    }

    /// Finds a model by its `provider/model` reference.
    pub fn find_model_by_reference(&self, reference: &str) -> Option<&ModelInfo> {
        self.models
            .iter()
            .find(|model| model.reference() == reference)
    }

    /// Providers and their models in name order, as the model pickers list
    /// them.
    pub fn group_models_by_provider(&self) -> Vec<(String, Vec<&ModelInfo>)> {
        let mut models: Vec<&ModelInfo> = self.models.iter().collect();
        models.sort_by(|left, right| {
            self.provider_name(&left.provider_id)
                .cmp(&self.provider_name(&right.provider_id))
                .then_with(|| left.provider_id.cmp(&right.provider_id))
                .then_with(|| left.name.cmp(&right.name))
        });
        let mut groups: Vec<(String, Vec<&ModelInfo>)> = Vec::new();
        let mut current_provider = None;
        for model in models {
            if current_provider != Some(&model.provider_id) {
                current_provider = Some(&model.provider_id);
                groups.push((self.provider_name(&model.provider_id), Vec::new()));
            }
            if let Some((_, group)) = groups.last_mut() {
                group.push(model);
            }
        }
        groups
    }

    pub(crate) fn find_agent(&self, id: &str) -> Option<&AgentInfo> {
        self.agents.iter().find(|agent| agent.id == id)
    }

    /// The agents a thread can switch to, OpenCode's built-in ones first, like
    /// Zed's own profiles.
    pub fn list_selectable_agents(&self) -> Vec<&AgentInfo> {
        let mut agents: Vec<&AgentInfo> = self
            .agents
            .iter()
            .filter(|agent| is_selectable_agent(agent))
            .collect();
        agents.sort_by_key(|agent| !BUILT_IN_AGENTS.contains(&agent.id.as_str()));
        agents
    }

    /// The agent a new thread starts with when nothing else picks one.
    pub(crate) fn find_default_agent_id(&self) -> &str {
        // OpenCode's agent list puts its resolved default first, including
        // defaults supplied by plugins rather than config documents.
        self.agents
            .iter()
            .find(|agent| is_selectable_agent(agent))
            .map_or("build", |agent| agent.id.as_str())
    }

    /// OpenCode names a custom agent by its ID, like `code-review`, which reads
    /// better as "Code Review". An agent missing from the list, say after it
    /// was deleted, is named the same way.
    pub fn format_agent_name(&self, id: &str) -> String {
        match self.find_agent(id) {
            Some(agent) if agent.name != agent.id => agent.name.clone(),
            _ => to_title_case(id),
        }
    }

    /// The commands Zed shows after `/`.
    pub(crate) fn available_commands(&self) -> Vec<acp::AvailableCommand> {
        std::iter::once(acp::AvailableCommand::Command(acp::Command::new(
            "compact",
            "Summarize the conversation so far to free up context",
        )))
        .chain(self.commands.iter().map(|command| {
            acp::AvailableCommand::Command(acp::Command::new(
                command.name.clone(),
                command.description.clone().unwrap_or_default(),
            ))
        }))
        .collect()
    }

    /// The skills Zed shows after `/` and `@`.
    pub(crate) fn available_skills(&self) -> Vec<AvailableSkill> {
        self.skills
            .iter()
            .map(|skill| AvailableSkill {
                name: skill.id.as_str().into(),
                description: skill.description.as_str().into(),
                source: skill.scope.clone().into(),
                skill_file_path: skill.path.clone(),
                warning: None,
            })
            .collect()
    }
}

/// Agents OpenCode ships with, listed ahead of the user's own.
pub(crate) const BUILT_IN_AGENTS: &[&str] = &["build", "plan"];

fn is_selectable_agent(agent: &AgentInfo) -> bool {
    !agent.hidden && agent.mode.as_deref() != Some("subagent")
}

pub(crate) fn to_title_case(value: &str) -> String {
    value
        .split(['_', '-'])
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Holders that compare old and new lists, such as a session whose agent was
/// deleted, read the old ones here.
pub(crate) struct ProjectSetupReplaced {
    pub previous: ProjectSetup,
}

impl EventEmitter<ProjectSetupReplaced> for ProjectSetup {}

/// Fetches every list again. A list that fails to load keeps what `previous`
/// had, so a failed refresh changes nothing. Without models no session can
/// run, so a load that has none fails.
pub(crate) async fn fetch_project_setup(
    client: &OpenCodeClient,
    directory: &str,
    previous: &ProjectSetup,
    cx: &mut AsyncApp,
) -> Result<ProjectSetup> {
    let (commands, skills, agents, models, providers, defaults) = futures::join!(
        client.list_commands(directory),
        client.list_skills(directory),
        client.list_agents(directory),
        client.list_models(directory),
        client.list_providers(directory),
        client.fetch_config_defaults(directory),
    );
    let models = match models {
        Ok(models) => models,
        Err(error) if previous.models.is_empty() => return Err(error),
        Err(error) => {
            log::warn!("Can't refresh OpenCode's models for {directory}: {error:#}");
            previous.models.clone()
        }
    };
    let skills = match skills.log_err() {
        Some(mut skills) => {
            let directory = PathBuf::from(directory);
            cx.background_executor()
                .spawn(async move {
                    let files = paths::data_dir().join("opencode_agent").join("skills");
                    prepare_skills(&mut skills, &directory, &files);
                    skills
                })
                .await
        }
        None => previous.skills.clone(),
    };
    Ok(ProjectSetup {
        commands: commands
            .log_err()
            .unwrap_or_else(|| previous.commands.clone()),
        skills,
        agents: agents.log_err().unwrap_or_else(|| previous.agents.clone()),
        models,
        providers: providers
            .log_err()
            .unwrap_or_else(|| previous.providers.clone()),
        defaults: defaults
            .log_err()
            .unwrap_or_else(|| previous.defaults.clone()),
    })
}

/// Built-in skills have no file, but Zed reads a skill's file when it's
/// mentioned, so their content is written to `files`. Skills left without a
/// readable file are dropped.
fn prepare_skills(skills: &mut Vec<Skill>, project_directory: &Path, files: &Path) {
    skills.retain_mut(|skill| {
        if skill.path.is_file() {
            skill.scope = match project_directory.file_name() {
                Some(name) if skill.path.starts_with(project_directory) => {
                    name.to_string_lossy().into_owned()
                }
                _ => "global".to_string(),
            };
            return true;
        }
        if skill.id.is_empty() || skill.id.starts_with('.') || skill.id.contains(['/', '\\']) {
            return false;
        }
        // Named by content too: another directory can serve a different
        // skill under the same ID, and each mention must open its own.
        let version: String = Sha256::digest(skill.content.as_bytes())
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path = files.join(&skill.id).join(version).join("SKILL.md");
        if !path.is_file() {
            let written = path
                .parent()
                .context("skill path has no parent")
                .and_then(|parent| Ok(std::fs::create_dir_all(parent)?))
                .and_then(|()| Ok(std::fs::write(&path, &skill.content)?));
            if written.log_err().is_none() {
                return false;
            }
        }
        skill.path = path;
        skill.scope = "built-in".to_string();
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built_in(id: &str, content: &str) -> Skill {
        Skill {
            id: id.to_string(),
            description: String::new(),
            path: PathBuf::from("/built-in/does-not-exist"),
            content: content.to_string(),
            scope: String::new(),
        }
    }

    #[test]
    fn built_in_skills_with_the_same_id_get_their_own_files() {
        let files = tempfile::tempdir().expect("a temporary directory");
        let mut first = vec![built_in("review", "first version")];
        let mut second = vec![built_in("review", "second version")];
        prepare_skills(&mut first, Path::new("/one"), files.path());
        prepare_skills(&mut second, Path::new("/two"), files.path());

        assert_ne!(first[0].path, second[0].path);
        let read = |skills: &[Skill]| std::fs::read_to_string(&skills[0].path).expect("the file");
        assert_eq!(read(&first), "first version");
        assert_eq!(read(&second), "second version");
        assert_eq!(first[0].scope, "built-in");
    }
}
