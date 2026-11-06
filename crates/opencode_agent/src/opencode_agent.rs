mod agents_modal;
mod client;
mod connection;
mod form;
mod history;
mod install;
mod mcp;
pub mod opencode_config;
mod render;
mod service;
mod session_sync;
mod types;

pub use client::{ListSessions, MessagePage, OpenCodeClient, Order, SseParser};
pub use connection::{AGENT_ID, DISPLAY_NAME, Hub, OpenCodeConnection};
pub use install::{install, install_dir};
pub use service::{
    NotInstalled, SERVICE_ARGS, ServiceRegistration, find_system_binary, parse_registration,
    read_registration, registration_path, start_service,
};
pub use session_sync::{
    Activity, Applied, Change, ResyncReason, SessionTranscript, Snapshot, SnapshotDiff,
};
pub use types::*;

use gpui::{App, Global, Task, TaskExt as _};
use util::ResultExt as _;
use project::agent_server_store::{AgentId, BuiltInAgent, register_built_in_agent};
use std::rc::Rc;
use std::sync::Arc;

/// Agent icons are loaded from a file path, so the bundled OpenCode icon is
/// copied into Zed's data directory.
fn write_icon(cx: &App) -> Option<gpui::SharedString> {
    let bytes = cx
        .asset_source()
        .load("icons/ai_open_code.svg")
        .log_err()
        .flatten()?;
    let path = paths::data_dir().join("opencode_agent").join("icon.svg");
    if std::fs::read(&path).ok().as_deref() != Some(&*bytes) {
        let directory = path.parent()?;
        std::fs::create_dir_all(directory).log_err()?;
        std::fs::write(&path, &*bytes).log_err()?;
    }
    Some(path.to_string_lossy().into_owned().into())
}

struct ServiceHttpClient(Arc<dyn http_client::HttpClient>);

pub(crate) struct Installed(pub(crate) bool);

impl Global for Installed {}

/// Whether OpenCode was on this machine when Zed started, or has been
/// installed from an OpenCode D thread since.
pub fn is_installed(cx: &App) -> bool {
    cx.try_global::<Installed>()
        .is_some_and(|installed| installed.0)
}

impl Global for ServiceHttpClient {}

/// A client for the OpenCode service, starting the service if needed.
pub fn service_client(cx: &mut App) -> Task<anyhow::Result<OpenCodeClient>> {
    let Some(http) = cx
        .try_global::<ServiceHttpClient>()
        .map(|http| http.0.clone())
    else {
        return Task::ready(Err(anyhow::anyhow!("OpenCode D isn't available here")));
    };
    let hub = Hub::get(http, cx);
    cx.spawn(async move |_| Ok(hub.await?.client().clone()))
}

/// Registers OpenCode D: always offered in the agent picker for local
/// projects, backed by the shared OpenCode service instead of ACP.
pub fn init(http: Arc<dyn http_client::HttpClient>, cx: &mut App) {
    cx.set_global(ServiceHttpClient(http.clone()));
    // Thread history syncs whenever OpenCode is installed, even if OpenCode D
    // is never opened.
    let installed = find_system_binary().is_some()
        || read_registration(&registration_path()).ok().flatten().is_some();
    cx.set_global(Installed(installed));
    if installed {
        Hub::get(http.clone(), cx).detach_and_log_err(cx);
    }

    register_built_in_agent(
        BuiltInAgent {
            id: AgentId::new(AGENT_ID),
            display_name: DISPLAY_NAME.into(),
            icon: write_icon(cx),
        },
        cx,
    );
    agent_ui::agent_menu_providers::register_skills_provider(
        AgentId::new(AGENT_ID),
        Rc::new(|connection, session_id, _cx| {
            connection
                .clone()
                .downcast::<OpenCodeConnection>()
                .map(|connection| connection.session_skills(&session_id.0))
                .unwrap_or_default()
        }),
        cx,
    );
    agent_ui::agent_menu_providers::register_thinking_provider(
        AgentId::new(AGENT_ID),
        Rc::new(|connection, session_id, _cx| {
            connection
                .clone()
                .downcast::<OpenCodeConnection>()?
                .thinking(&session_id.0)
        }),
        cx,
    );
    agent_ui::agent_menu_providers::register_profiles_provider(
        AgentId::new(AGENT_ID),
        Rc::new(|connection, session_id, _cx| {
            connection
                .clone()
                .downcast::<OpenCodeConnection>()?
                .profiles(&session_id.0)
        }),
        cx,
    );
    agents_modal::init(cx);
    agent_ui::agent_menu_providers::register_steering_provider(
        AgentId::new(AGENT_ID),
        Rc::new(|connection, session_id, _cx| {
            connection
                .clone()
                .downcast::<OpenCodeConnection>()?
                .steering(&session_id.0)
        }),
        cx,
    );
    agent_ui::agent_menu_providers::register_session_folders_provider(
        AgentId::new(AGENT_ID),
        Rc::new(|connection, session_id, _cx| {
            connection
                .clone()
                .downcast::<OpenCodeConnection>()?
                .session_folders(&session_id.0)
        }),
        cx,
    );
    agent_ui::agent_menu_providers::register_thread_text_provider(
        AgentId::new(AGENT_ID),
        Rc::new({
            let http = http.clone();
            move |session_id, cx| {
                let hub = Hub::get(http.clone(), cx);
                let session_id = session_id.0.to_string();
                cx.spawn(async move |_| {
                    let hub = hub.await?;
                    connection::thread_summary(&hub, &session_id).await
                })
            }
        }),
        cx,
    );
    agent_servers::register_agent_connection_factory(
        AgentId::new(AGENT_ID),
        Rc::new(move |project, cx| {
            if !project.read(cx).is_local() {
                return gpui::Task::ready(Err(anyhow::anyhow!(
                    "OpenCode D only works with local projects. Use the OpenCode agent for remote projects."
                )));
            }
            if find_system_binary().is_none()
                && read_registration(&registration_path()).ok().flatten().is_none()
            {
                return gpui::Task::ready(Ok(Rc::new(install::InstallConnection::new(http.clone()))
                    as Rc<dyn acp_thread::AgentConnection>));
            }
            let hub = Hub::get(http.clone(), cx);
            let http = http.clone();
            cx.spawn(async move |_| match hub.await {
                Ok(hub) => {
                    Ok(Rc::new(OpenCodeConnection::new(hub)) as Rc<dyn acp_thread::AgentConnection>)
                }
                // A stale registration with no binary to restart the service.
                Err(error) if error.downcast_ref::<NotInstalled>().is_some() => {
                    Ok(Rc::new(install::InstallConnection::new(http))
                        as Rc<dyn acp_thread::AgentConnection>)
                }
                Err(error) => Err(error),
            })
        }),
        cx,
    );
}
