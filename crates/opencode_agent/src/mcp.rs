//! Forwards Zed's MCP servers for a project folder to OpenCode.
//!
//! Servers go through `PUT /api/experimental/mcp/:server` in the config
//! format OpenCode's ACP bridge uses (`cli/src/acp/sessions.ts`). The service
//! applies them to the whole folder and keeps them in memory, so they're sent
//! again after a reconnect.
//!
//! `DELETE` doesn't undo an add: it switches the name off, hiding a server of
//! the same name in `opencode.json` too. So Zed only removes names it added,
//! recorded across Zed restarts, and never sends a name OpenCode's own config
//! already has.

use crate::client::OpenCodeClient;
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use extension_host::ExtensionStore;
use futures::channel::oneshot;
use gpui::{App, AsyncApp, Entity, Subscription, Task};
use project::Project;
use project::context_server_store::ContextServerConfiguration;
use project::project_settings::ContextServerSettings;
use serde::Serialize;
use std::cell::RefCell;
use std::rc::Rc;
use util::ResultExt as _;

const KVP_NAMESPACE: &str = "opencode_agent_mcp";

pub(crate) struct McpForwarder {
    client: OpenCodeClient,
    directories: RefCell<HashMap<String, DirectoryServers>>,
}

impl McpForwarder {
    pub(crate) fn new(client: OpenCodeClient) -> Rc<Self> {
        Rc::new(Self {
            client,
            directories: RefCell::default(),
        })
    }

    /// Sends `project`'s MCP servers for `directory` and follows later
    /// changes. The task finishes once the servers are sent; failures are
    /// logged, since a missing MCP server shouldn't stop a thread opening.
    pub(crate) fn forward_servers(
        self: &Rc<Self>,
        project: &Entity<Project>,
        directory: &str,
        cx: &mut App,
    ) -> Task<()> {
        let store = project.read(cx).context_server_store();
        // Observed rather than subscribed to: status events fire before a
        // settings change reaches the store (the settings page stops a
        // server, then writes `enabled: false`), and the store's pass over
        // the new settings only notifies.
        let subscription = cx.observe(&store, {
            let this = Rc::downgrade(self);
            let project = project.downgrade();
            let directory = directory.to_string();
            move |_, cx| {
                let (Some(this), Some(project)) = (this.upgrade(), project.upgrade()) else {
                    return;
                };
                if this.update_servers(&directory, project_servers(&project, cx)) {
                    this.send_changes(&directory, cx).detach();
                }
            }
        });
        let servers = project_servers(project, cx);
        match self.directories.borrow_mut().entry(directory.to_string()) {
            collections::hash_map::Entry::Occupied(mut entry) => {
                let entry = entry.get_mut();
                entry.servers = servers;
                entry._subscription = subscription;
            }
            collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(DirectoryServers {
                    servers,
                    added_by_zed: read_added_by_zed(directory, cx),
                    sending: SendState::Idle,
                    _subscription: subscription,
                });
            }
        }
        self.send_changes(directory, cx)
    }

    /// The service forgot every added server; send them again.
    pub(crate) fn resend_all(self: &Rc<Self>, cx: &mut App) {
        let directories: Vec<String> = {
            let mut directories = self.directories.borrow_mut();
            for entry in directories.values_mut() {
                for sent in entry.added_by_zed.values_mut() {
                    *sent = None;
                }
            }
            directories.keys().cloned().collect()
        };
        for directory in directories {
            self.send_changes(&directory, cx).detach();
        }
    }

    /// Returns whether they changed, since the store notifies for more than
    /// its servers.
    fn update_servers(&self, directory: &str, servers: ProjectServers) -> bool {
        let mut directories = self.directories.borrow_mut();
        let Some(entry) = directories.get_mut(directory) else {
            return false;
        };
        if entry.servers == servers {
            return false;
        }
        entry.servers = servers;
        true
    }

    /// The task finishes once a pass that started after this call is done.
    fn send_changes(self: &Rc<Self>, directory: &str, cx: &mut App) -> Task<()> {
        let (done_tx, done_rx) = oneshot::channel();
        let start = {
            let mut directories = self.directories.borrow_mut();
            let Some(entry) = directories.get_mut(directory) else {
                return Task::ready(());
            };
            match &mut entry.sending {
                SendState::Sending { again, waiters } => {
                    *again = true;
                    waiters.push(done_tx);
                    false
                }
                SendState::Idle => {
                    entry.sending = SendState::Sending {
                        again: false,
                        waiters: vec![done_tx],
                    };
                    true
                }
            }
        };
        if start {
            let this = self.clone();
            let directory = directory.to_string();
            cx.spawn(async move |cx| this.send_passes(directory, cx).await)
                .detach();
        }
        cx.spawn(async move |_| {
            // Dropped without an answer when the forwarder goes, which ends
            // the wait just the same.
            done_rx.await.ok();
        })
    }

    async fn send_passes(self: Rc<Self>, directory: String, cx: &mut AsyncApp) {
        loop {
            if let Err(error) = self.send_pass(&directory, cx).await {
                log::warn!("Can't send MCP servers to OpenCode for {directory}: {error:#}");
                break;
            }
            let again = match self
                .directories
                .borrow_mut()
                .get_mut(&directory)
                .map(|entry| &mut entry.sending)
            {
                Some(SendState::Sending { again, .. }) => std::mem::take(again),
                _ => false,
            };
            if !again {
                break;
            }
        }
        self.wake_waiters(&directory);
    }

    async fn send_pass(&self, directory: &str, cx: &mut AsyncApp) -> anyhow::Result<()> {
        // Without the config, every server would look unconfigured: the
        // user's own would be overwritten. The next pass, such as the one
        // after a reconnect, tries again.
        let configured = self.client.list_configured_mcp_names(directory).await?;
        let (adds, removes) = {
            let mut directories = self.directories.borrow_mut();
            let Some(entry) = directories.get_mut(directory) else {
                return Ok(());
            };
            let servers = &entry.servers;
            let adds: Vec<(String, McpConfig)> = servers
                .configs
                .iter()
                .filter(|(name, config)| {
                    let sent = entry.added_by_zed.get(*name).and_then(Option::as_ref);
                    !configured.contains(*name) && sent != Some(*config)
                })
                .map(|(name, config)| (name.clone(), config.clone()))
                .collect();
            let removes: Vec<String> = entry
                .added_by_zed
                .keys()
                .filter(|name| {
                    !configured.contains(*name)
                        && !servers.sources.contains(*name)
                        && !servers.configs.contains_key(*name)
                })
                .cloned()
                .collect();
            // Recorded before sending, so a server added just before Zed
            // quits is still removed later.
            for (name, _) in &adds {
                entry.added_by_zed.entry(name.clone()).or_insert(None);
            }
            (adds, removes)
        };
        if !adds.is_empty() {
            self.write_added_by_zed(directory, cx).await;
        }

        for (name, config) in adds {
            match self.client.add_mcp(directory, &name, &config).await {
                Ok(()) => {
                    log::info!("Registered MCP server {name} for {directory}");
                    if let Some(entry) = self.directories.borrow_mut().get_mut(directory) {
                        entry.added_by_zed.insert(name, Some(config));
                    }
                }
                Err(error) => {
                    log::warn!("Can't register MCP server {name} with OpenCode: {error:#}")
                }
            }
        }
        let mut removed_any = false;
        for name in removes {
            match self.client.remove_mcp(directory, &name).await {
                Ok(()) => {
                    log::info!("Removed MCP server {name} for {directory}");
                    if let Some(entry) = self.directories.borrow_mut().get_mut(directory) {
                        entry.added_by_zed.remove(&name);
                    }
                    removed_any = true;
                }
                Err(error) => {
                    log::warn!("Can't remove MCP server {name} from OpenCode: {error:#}")
                }
            }
        }
        if removed_any {
            self.write_added_by_zed(directory, cx).await;
        }
        Ok(())
    }

    fn wake_waiters(&self, directory: &str) {
        let waiters = match self.directories.borrow_mut().get_mut(directory) {
            Some(entry) => match std::mem::replace(&mut entry.sending, SendState::Idle) {
                SendState::Sending { waiters, .. } => waiters,
                SendState::Idle => Vec::new(),
            },
            None => Vec::new(),
        };
        for waiter in waiters {
            waiter.send(()).ok();
        }
    }

    async fn write_added_by_zed(&self, directory: &str, cx: &mut AsyncApp) {
        let names: Vec<String> = {
            let directories = self.directories.borrow();
            let Some(entry) = directories.get(directory) else {
                return;
            };
            let mut names: Vec<String> = entry.added_by_zed.keys().cloned().collect();
            names.sort();
            names
        };
        let store = cx.update(|cx| KeyValueStore::global(cx));
        let store = store.scoped(KVP_NAMESPACE);
        let result = if names.is_empty() {
            store.delete(directory.to_string()).await
        } else {
            match serde_json::to_string(&names) {
                Ok(value) => store.write(directory.to_string(), value).await,
                Err(error) => Err(error.into()),
            }
        };
        result.log_err();
    }
}

struct DirectoryServers {
    servers: ProjectServers,
    /// Every name Zed added and hasn't removed, with the config last sent for
    /// it since Zed connected, if any.
    added_by_zed: HashMap<String, Option<McpConfig>>,
    sending: SendState,
    _subscription: Subscription,
}

enum SendState {
    Idle,
    Sending {
        /// The servers changed during this pass, so another one follows.
        again: bool,
        /// Callers waiting for the passes to finish.
        waiters: Vec<oneshot::Sender<()>>,
    },
}

#[derive(Default, PartialEq)]
struct ProjectServers {
    configs: HashMap<String, McpConfig>,
    /// Servers Zed still has a source for, running or not: an enabled
    /// settings entry, or an installed extension. Only names outside this
    /// set are removed, so a server that's still starting stays.
    sources: HashSet<String>,
}

/// A server as OpenCode's config takes it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum McpConfig {
    Local {
        command: Vec<String>,
        environment: HashMap<String, String>,
    },
    Remote {
        url: String,
        headers: HashMap<String, String>,
        oauth: bool,
    },
}

/// Not sent yet since Zed started, so each is sent again.
fn read_added_by_zed(directory: &str, cx: &App) -> HashMap<String, Option<McpConfig>> {
    KeyValueStore::global(cx)
        .scoped(KVP_NAMESPACE)
        .read(directory)
        .log_err()
        .flatten()
        .and_then(|value| serde_json::from_str::<Vec<String>>(&value).log_err())
        .unwrap_or_default()
        .into_iter()
        .map(|name| (name, None))
        .collect()
}

fn project_servers(project: &Entity<Project>, cx: &App) -> ProjectServers {
    let project = project.read(cx);
    let store = project.context_server_store().read(cx);
    // The extension index is read synchronously at startup, before
    // extensions register their servers, so it tells a server that's still
    // loading apart from one that was uninstalled.
    let installed: HashSet<String> = ExtensionStore::try_global(cx)
        .map(|extensions| {
            extensions
                .read(cx)
                .installed_extensions()
                .values()
                .flat_map(|entry| entry.manifest.context_servers.keys())
                .map(|id| id.to_string())
                .collect()
        })
        .unwrap_or_default();
    let mut sources: HashSet<String> = store
        .server_ids()
        .iter()
        .filter(|id| match store.settings_for_server(id) {
            Some(settings) if !settings.enabled() => false,
            Some(ContextServerSettings::Stdio { .. } | ContextServerSettings::Http { .. }) => true,
            Some(ContextServerSettings::Extension { .. }) | None => {
                installed.contains(id.0.as_ref())
            }
        })
        .map(|id| id.0.to_string())
        .collect();
    // Installed but not registered yet; disabled ones have a settings entry
    // and so are among the store's IDs.
    let known: HashSet<&str> = store.server_ids().iter().map(|id| id.0.as_ref()).collect();
    sources.extend(
        installed
            .iter()
            .filter(|id| !known.contains(id.as_str()))
            .cloned(),
    );
    // Not `configured_server_ids`: that only lists servers with a settings
    // entry, and extension servers such as GitHub's run without one.
    let configs = store
        .server_ids()
        .iter()
        .filter(|id| store.is_server_enabled(id, cx))
        .filter_map(|id| {
            let configuration = store.configuration_for_server(id)?;
            Some((id.0.to_string(), server_config(&configuration)))
        })
        .collect();
    ProjectServers { configs, sources }
}

fn server_config(configuration: &ContextServerConfiguration) -> McpConfig {
    match configuration {
        ContextServerConfiguration::Custom { command, .. }
        | ContextServerConfiguration::Extension { command, .. } => {
            let mut argv = vec![command.path.to_string_lossy().into_owned()];
            argv.extend(command.args.iter().cloned());
            McpConfig::Local {
                command: argv,
                environment: command
                    .env
                    .iter()
                    .flatten()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
            }
        }
        ContextServerConfiguration::Http { url, headers, .. } => McpConfig::Remote {
            url: url.to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            oauth: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn configs_take_the_shape_opencode_reads() {
        let local = McpConfig::Local {
            command: vec!["npx".into(), "server".into()],
            environment: [("TOKEN".to_string(), "1".to_string())]
                .into_iter()
                .collect(),
        };
        assert_eq!(
            serde_json::to_value(&local).expect("serialize"),
            json!({ "type": "local", "command": ["npx", "server"], "environment": { "TOKEN": "1" } })
        );
        let remote = McpConfig::Remote {
            url: "https://example.com/mcp".into(),
            headers: HashMap::default(),
            oauth: false,
        };
        assert_eq!(
            serde_json::to_value(&remote).expect("serialize"),
            json!({ "type": "remote", "url": "https://example.com/mcp", "headers": {}, "oauth": false })
        );
    }
}
