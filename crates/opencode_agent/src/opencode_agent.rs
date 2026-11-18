mod agents_modal;
mod client;
mod connection;
mod form;
mod history;
mod install;
mod mcp;
pub mod opencode_config;
mod opencode_session;
mod project_setup;
mod prompt;
mod service;
mod session_messages;
mod session_selectors;
mod shell_terminals;
mod translate;
mod types;

pub use client::{OpenCodeClient, OpenCodeError};
pub use project_setup::ProjectSetup;
pub use service::NotInstalled;
pub use session_selectors::find_provider_icon;
pub use types::*;

use acp_thread::AgentConnection;
use agent_client_protocol::v2::schema as acp;
use agent_ui::agent_ui_hooks::{
    AgentSteering, AgentThinking, AgentUiHooks, register_agent_ui_hooks,
};
use agent_ui::{AvailableSkill, ProfileProvider};
use anyhow::Context as _;
use connection::{AGENT_ID, DISPLAY_NAME, OpenCodeConnection};
use gpui::{App, Entity, Global, Task, TaskExt as _};
use project::agent_server_store::{AgentId, BuiltInAgent, register_built_in_agent};
use std::rc::Rc;
use std::sync::Arc;
use util::ResultExt as _;
use util::path_list::PathList;

/// Registers OpenCode D: always offered in the agent picker for local
/// projects, backed by the shared OpenCode service instead of ACP.
/// `download_http` downloads OpenCode when it isn't installed. `http` talks
/// to the OpenCode service on this machine, so it must not use a proxy.
pub fn init(
    download_http: Arc<dyn http_client::HttpClient>,
    http: Arc<dyn http_client::HttpClient>,
    cx: &mut App,
) {
    cx.set_global(ServiceHttpClient(http));
    // Thread history syncs whenever OpenCode is installed, even if OpenCode D
    // is never opened.
    let connecting = OpenCodeConnection::shared(cx);
    cx.spawn(async move |_| {
        if let Err(error) = connecting.await
            && error.downcast_ref::<NotInstalled>().is_none()
        {
            log::warn!("Can't connect to the OpenCode service: {error:#}");
        }
    })
    .detach();

    register_built_in_agent(
        BuiltInAgent {
            id: AgentId::new(AGENT_ID),
            display_name: DISPLAY_NAME.into(),
            icon: bundled_icon_path(cx),
        },
        cx,
    );
    register_agent_ui_hooks(AgentId::new(AGENT_ID), Rc::new(OpenCodeUiHooks), cx);
    agents_modal::init(cx);
    agent_servers::register_agent_connection_factory(
        AgentId::new(AGENT_ID),
        Rc::new(move |project, cx| {
            if !project.read(cx).is_local() {
                return Task::ready(Err(anyhow::anyhow!(
                    "OpenCode D only works with local projects. Use the OpenCode agent for remote projects."
                )));
            }
            let connection = OpenCodeConnection::shared(cx);
            let download_http = download_http.clone();
            cx.spawn(async move |_| match connection.await {
                Ok(connection) => Ok(connection as Rc<dyn AgentConnection>),
                Err(error) if error.downcast_ref::<NotInstalled>().is_some() => {
                    Ok(Rc::new(install::InstallConnection::new(download_http))
                        as Rc<dyn AgentConnection>)
                }
                Err(error) => Err(error),
            })
        }),
        cx,
    );
}

/// The agent UI OpenCode D's threads get, asked of the session's connection.
struct OpenCodeUiHooks;

impl AgentUiHooks for OpenCodeUiHooks {
    fn skills(
        &self,
        connection: &Rc<dyn AgentConnection>,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<Vec<AvailableSkill>> {
        Some(downcast_connection(connection)?.session_skills(&session_id.0, cx))
    }

    fn can_mention_threads(&self) -> bool {
        true
    }

    fn thread_text(
        &self,
        session_id: &acp::SessionId,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<String>>> {
        let connection = OpenCodeConnection::shared(cx);
        let session_id = session_id.0.to_string();
        // The text sent when a thread is mentioned with `@`: a summary
        // written by the thread's own model from its context, using the
        // native agent's prompt. OpenCode prompts can't reference another
        // session, and generating leaves the mentioned thread's history
        // unchanged.
        Some(cx.spawn(async move |_| {
            connection
                .await?
                .client()
                .generate_text(
                    &session_id,
                    agent_settings::SUMMARIZE_THREAD_DETAILED_PROMPT,
                )
                .await
                .context("Failed to generate summary")
        }))
    }

    fn thinking(
        &self,
        connection: &Rc<dyn AgentConnection>,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<Rc<dyn AgentThinking>> {
        downcast_connection(connection)?.thinking(&session_id.0, cx)
    }

    fn profiles(
        &self,
        connection: &Rc<dyn AgentConnection>,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Arc<dyn ProfileProvider>> {
        downcast_connection(connection)?.profiles(&session_id.0)
    }

    fn steering(
        &self,
        connection: &Rc<dyn AgentConnection>,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn AgentSteering>> {
        downcast_connection(connection)?.steering(&session_id.0)
    }

    fn session_folders(
        &self,
        connection: &Rc<dyn AgentConnection>,
        session_id: &acp::SessionId,
        cx: &App,
    ) -> Option<PathList> {
        downcast_connection(connection)?.session_folders(&session_id.0, cx)
    }
}

fn downcast_connection(connection: &Rc<dyn AgentConnection>) -> Option<Rc<OpenCodeConnection>> {
    connection.clone().downcast::<OpenCodeConnection>()
}

/// A client for the OpenCode service, starting the service if needed.
pub fn service_client(cx: &mut App) -> Task<anyhow::Result<OpenCodeClient>> {
    let connection = OpenCodeConnection::shared(cx);
    cx.spawn(async move |_| Ok(connection.await?.client().clone()))
}

/// The error as the user should read it: the service's own message when it
/// sent one, since the rest of a failed request is its JSON.
pub fn describe_error(error: &anyhow::Error) -> String {
    if error.downcast_ref::<NotInstalled>().is_some() {
        return "OpenCode isn't installed. Start an OpenCode D thread to install it.".into();
    }
    error
        .downcast_ref::<OpenCodeError>()
        .and_then(|error| error.message.clone())
        .unwrap_or_else(|| format!("{error:#}"))
}

/// `directory`'s project setup, shared with the threads open there. The
/// connection keeps it current, starting the service if needed.
pub fn load_project_setup(
    directory: String,
    cx: &mut App,
) -> Task<anyhow::Result<Entity<ProjectSetup>>> {
    let connection = OpenCodeConnection::shared(cx);
    cx.spawn(async move |cx| {
        let connection = connection.await?;
        cx.update(|cx| connection.load_project_setup(&directory, cx))
            .await
    })
}

/// Fetches `directory`'s project setup again, for changes the service doesn't
/// announce, such as a provider connected from the settings page.
pub fn refresh_project_setup(directory: String, cx: &mut App) {
    let connection = OpenCodeConnection::shared(cx);
    cx.spawn(async move |cx| {
        let connection = connection.await?;
        cx.update(|cx| connection.refresh_project_setup(directory, cx));
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Makes `client` the shared connection's, for tests that have no service.
#[cfg(any(test, feature = "test-support"))]
pub fn use_test_client(client: OpenCodeClient, cx: &mut App) {
    OpenCodeConnection::use_test_client(client, cx);
}

/// Whether OpenCode was found the last time Zed connected to its service.
/// A service that fails to start still counts.
pub fn is_installed(cx: &App) -> bool {
    cx.try_global::<Installed>()
        .is_some_and(|installed| installed.0)
}

pub(crate) struct ServiceHttpClient(pub(crate) Arc<dyn http_client::HttpClient>);

impl Global for ServiceHttpClient {}

pub(crate) struct Installed(pub(crate) bool);

impl Global for Installed {}

/// Agent icons are loaded from a file path, so the bundled OpenCode icon is
/// copied into Zed's data directory.
fn bundled_icon_path(cx: &App) -> Option<gpui::SharedString> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_show_the_service_message() {
        let error = anyhow::Error::from(OpenCodeError {
            path: "/x".into(),
            status: 400,
            message: Some("Bad key".into()),
            body: r#"{"message":"Bad key","kind":"integration_authorization"}"#.into(),
        })
        .context("connecting");
        assert_eq!(describe_error(&error), "Bad key");
        assert_eq!(describe_error(&anyhow::anyhow!("offline")), "offline");
        assert_eq!(
            describe_error(&anyhow::Error::from(NotInstalled).context("starting")),
            "OpenCode isn't installed. Start an OpenCode D thread to install it."
        );
    }
}
