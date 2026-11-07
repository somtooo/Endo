//! Keeps OpenCode's MCP servers for a project folder in step with Zed's.
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
use futures::channel::oneshot;
use gpui::{App, AsyncApp, Entity, Subscription, Task};
use project::Project;
use extension_host::ExtensionStore;
use project::context_server_store::ContextServerConfiguration;
use project::project_settings::ContextServerSettings;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::rc::Rc;
use util::ResultExt as _;

const KVP_NAMESPACE: &str = "opencode_agent_mcp";

#[derive(PartialEq)]
struct ProjectServers {
    /// Running servers, ready to send.
    configs: HashMap<String, Value>,
    /// Servers Zed still has a source for, running or not: an enabled
    /// settings entry, or an installed extension. Only names outside this
    /// set are removed, so a server that's still starting stays.
    sources: HashSet<String>,
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
    let enabled: HashSet<String> = store
        .server_ids()
        .iter()
        .filter(|id| store.is_server_enabled(id, cx))
        .map(|id| id.0.to_string())
        .collect();
    let configs = store
        .server_ids()
        .iter()
        .filter(|id| enabled.contains(id.0.as_ref()))
        .filter_map(|id| {
            let configuration = store.configuration_for_server(id)?;
            let config = match &*configuration {
                ContextServerConfiguration::Custom { command, .. }
                | ContextServerConfiguration::Extension { command, .. } => {
                    let mut argv = vec![command.path.to_string_lossy().into_owned()];
                    argv.extend(command.args.iter().cloned());
                    json!({
                        "type": "local",
                        "command": argv,
                        "environment": command.env.clone().unwrap_or_default(),
                    })
                }
                ContextServerConfiguration::Http { url, headers, .. } => json!({
                    "type": "remote",
                    "url": url.to_string(),
                    "headers": headers,
                    "oauth": false,
                }),
            };
            Some((id.0.to_string(), config))
        })
        .collect();
    ProjectServers { configs, sources }
}

#[derive(Default)]
struct Directory {
    servers: Option<ProjectServers>,
    /// Sent since the service last (re)connected.
    sent: HashMap<String, Value>,
    /// Names Zed added and hasn't removed yet.
    owned: HashSet<String>,
    syncing: bool,
    dirty: bool,
    waiters: Vec<oneshot::Sender<()>>,
    _subscription: Option<Subscription>,
}

pub(crate) struct McpSync {
    client: OpenCodeClient,
    directories: RefCell<HashMap<String, Directory>>,
}

impl McpSync {
    pub(crate) fn new(client: OpenCodeClient) -> Rc<Self> {
        Rc::new(Self {
            client,
            directories: RefCell::default(),
        })
    }

    /// Sends `project`'s MCP servers for `directory` and follows later
    /// changes. The task finishes once the servers are sent; failures are
    /// logged, since a missing MCP server shouldn't stop a thread opening.
    pub(crate) fn watch(
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
                if this.set_servers(&directory, project_servers(&project, cx), cx) {
                    this.sync(&directory, cx).detach();
                }
            }
        });
        self.set_servers(directory, project_servers(project, cx), cx);
        if let Some(entry) = self.directories.borrow_mut().get_mut(directory) {
            entry._subscription = Some(subscription);
        }
        self.sync(directory, cx)
    }

    /// The service forgot every added server; send them again.
    pub(crate) fn reconnected(self: &Rc<Self>, cx: &mut App) {
        let directories: Vec<String> = {
            let mut directories = self.directories.borrow_mut();
            for entry in directories.values_mut() {
                entry.sent.clear();
            }
            directories.keys().cloned().collect()
        };
        for directory in directories {
            self.sync(&directory, cx).detach();
        }
    }

    /// Returns whether the servers changed.
    fn set_servers(&self, directory: &str, servers: ProjectServers, cx: &App) -> bool {
        let mut directories = self.directories.borrow_mut();
        let entry = directories
            .entry(directory.to_string())
            .or_insert_with(|| Directory {
                owned: read_owned(directory, cx),
                ..Directory::default()
            });
        if entry.servers.as_ref() == Some(&servers) {
            return false;
        }
        entry.servers = Some(servers);
        true
    }

    /// Runs one sync for `directory` at a time; a change during a sync runs
    /// another pass after it.
    fn sync(self: &Rc<Self>, directory: &str, cx: &mut App) -> Task<()> {
        let (done_tx, done_rx) = oneshot::channel();
        let start = {
            let mut directories = self.directories.borrow_mut();
            let Some(entry) = directories.get_mut(directory) else {
                return Task::ready(());
            };
            entry.waiters.push(done_tx);
            if entry.syncing {
                entry.dirty = true;
                false
            } else {
                entry.syncing = true;
                true
            }
        };
        if start {
            let this = self.clone();
            let directory = directory.to_string();
            cx.spawn(async move |cx| this.run(directory, cx).await)
                .detach();
        }
        cx.spawn(async move |_| {
            done_rx.await.ok();
        })
    }

    async fn run(self: Rc<Self>, directory: String, cx: &mut AsyncApp) {
        loop {
            // Without the config, every server would look unconfigured: the
            // user's own would be overwritten. The next sync, such as the one
            // after a reconnect, tries again.
            let configured = match self.client.configured_mcp_names(&directory).await {
                Ok(configured) => configured,
                Err(error) => {
                    log::warn!("Can't read OpenCode's MCP config for {directory}: {error:#}");
                    self.finish_sync(&directory);
                    return;
                }
            };
            let (adds, removes) = {
                let mut directories = self.directories.borrow_mut();
                let Some(entry) = directories.get_mut(&directory) else {
                    return;
                };
                let no_servers = ProjectServers {
                    configs: HashMap::default(),
                    sources: HashSet::default(),
                };
                // Unknown servers send nothing and keep everything.
                let (servers, removable) = match &entry.servers {
                    Some(servers) => (servers, true),
                    None => (&no_servers, false),
                };
                let adds: Vec<(String, Value)> = servers
                    .configs
                    .iter()
                    .filter(|(name, config)| {
                        !configured.contains(*name) && entry.sent.get(*name) != Some(config)
                    })
                    .map(|(name, config)| (name.clone(), config.clone()))
                    .collect();
                let removes: Vec<String> = entry
                    .owned
                    .iter()
                    .filter(|name| {
                        removable
                            && !configured.contains(*name)
                            && !servers.sources.contains(*name)
                            && !servers.configs.contains_key(*name)
                    })
                    .cloned()
                    .collect();
                // Recorded before sending, so a server added just before Zed
                // quits is still removed later.
                entry.owned.extend(adds.iter().map(|(name, _)| name.clone()));
                (adds, removes)
            };
            if !adds.is_empty() {
                self.write_owned(&directory, cx).await;
            }

            for (name, config) in adds {
                match self.client.add_mcp(&directory, &name, &config).await {
                    Ok(()) => {
                        log::info!("Registered MCP server {name} for {directory}");
                        if let Some(entry) = self.directories.borrow_mut().get_mut(&directory) {
                            entry.sent.insert(name, config);
                        }
                    }
                    Err(error) => {
                        log::warn!("Can't register MCP server {name} with OpenCode: {error:#}")
                    }
                }
            }
            let mut removed_any = false;
            for name in removes {
                match self.client.remove_mcp(&directory, &name).await {
                    Ok(()) => {
                        log::info!("Removed MCP server {name} for {directory}");
                        if let Some(entry) = self.directories.borrow_mut().get_mut(&directory) {
                            entry.owned.remove(&name);
                            entry.sent.remove(&name);
                        }
                        removed_any = true;
                    }
                    Err(error) => {
                        log::warn!("Can't remove MCP server {name} from OpenCode: {error:#}")
                    }
                }
            }
            if removed_any {
                self.write_owned(&directory, cx).await;
            }

            let dirty = self
                .directories
                .borrow_mut()
                .get_mut(&directory)
                .is_some_and(|entry| std::mem::take(&mut entry.dirty));
            if dirty {
                continue;
            }
            self.finish_sync(&directory);
            return;
        }
    }

    fn finish_sync(&self, directory: &str) {
        let waiters = {
            let mut directories = self.directories.borrow_mut();
            let Some(entry) = directories.get_mut(directory) else {
                return;
            };
            entry.syncing = false;
            entry.dirty = false;
            std::mem::take(&mut entry.waiters)
        };
        for waiter in waiters {
            waiter.send(()).ok();
        }
    }

    async fn write_owned(&self, directory: &str, cx: &mut AsyncApp) {
        let names: Vec<String> = {
            let directories = self.directories.borrow();
            let Some(entry) = directories.get(directory) else {
                return;
            };
            let mut names: Vec<String> = entry.owned.iter().cloned().collect();
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

fn read_owned(directory: &str, cx: &App) -> HashSet<String> {
    KeyValueStore::global(cx)
        .scoped(KVP_NAMESPACE)
        .read(directory)
        .log_err()
        .flatten()
        .and_then(|value| serde_json::from_str::<Vec<String>>(&value).log_err())
        .unwrap_or_default()
        .into_iter()
        .collect()
}
