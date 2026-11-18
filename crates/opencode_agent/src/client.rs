use crate::service::ServiceRegistration;
use crate::types::{
    AgentInfo, AttemptStatus, Command, CommandAttempt, ConfigEntry, EventFrame, Form, InboxEntry,
    Integration, Message, ModelInfo, ModelRef, OAuthAttempt, PageCursor, PermissionReply,
    PermissionRequest, PluginInfo, ProviderInfo, ServerInfo, SessionInfo, SessionPage, ShellOutput,
    ShellStatus, Skill,
};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use collections::{HashMap, HashSet};
use futures::stream::BoxStream;
use futures::{AsyncReadExt as _, StreamExt as _};
use http_client::{AsyncBody, HttpClient, Method, Request, StatusCode, Url};
use parking_lot::RwLock;
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::Value;
use std::sync::Arc;

const MAX_CHILD_SESSIONS: usize = 2000;

/// Every clone talks to the same service: when the service moves to another
/// address or password, [`OpenCodeClient::set_endpoint`] updates them all.
#[derive(Clone)]
pub struct OpenCodeClient {
    http: Arc<dyn HttpClient>,
    endpoint: Arc<RwLock<Endpoint>>,
    config_changes: Arc<RwLock<watch::Sender<()>>>,
    child_sessions: Arc<RwLock<HashSet<String>>>,
}

impl OpenCodeClient {
    pub fn new(http: Arc<dyn HttpClient>, registration: &ServiceRegistration) -> Result<Self> {
        Ok(Self {
            http,
            endpoint: Arc::new(RwLock::new(Endpoint::new(registration)?)),
            config_changes: Arc::new(RwLock::new(watch::channel(()).0)),
            child_sessions: Arc::new(RwLock::new(HashSet::default())),
        })
    }

    pub fn set_endpoint(&self, registration: &ServiceRegistration) -> Result<()> {
        let endpoint = Endpoint::new(registration)?;
        let mut current = self.endpoint.write();
        if *current != endpoint {
            log::info!("The OpenCode service moved to {}", endpoint.base_url);
            *current = endpoint;
            self.child_sessions.write().clear();
        }
        Ok(())
    }

    pub async fn fetch_service_info(&self) -> Result<InfoResponse> {
        let (status, body) = self
            .request_raw(Method::GET, "/api/info", &[], None)
            .await?;
        Ok(InfoResponse {
            status: status.as_u16(),
            info: if status.is_success() {
                Some(serde_json::from_slice(&body).context("decoding OpenCode service info")?)
            } else {
                serde_json::from_slice(&body).ok()
            },
        })
    }

    /// The newest root sessions, up to `limit`.
    pub async fn list_all_sessions(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        const PAGE_SIZE: usize = 200;
        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::default();
        while sessions.len() < limit {
            let page = self
                .list_sessions_page(ListSessions {
                    directory: None,
                    cursor: cursor.as_deref(),
                    limit: PAGE_SIZE,
                })
                .await?;
            if page.data.is_empty() {
                break;
            }
            sessions.extend(page.data);
            match page.cursor.next {
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                _ => break,
            }
        }
        sessions.truncate(limit);
        Ok(sessions)
    }

    pub async fn list_sessions_page(&self, request: ListSessions<'_>) -> Result<SessionPage> {
        let limit = request.limit.to_string();
        let mut query = vec![("parentID", "null"), ("limit", limit.as_str())];
        if let Some(directory) = request.directory {
            query.push(("directory", directory));
        }
        if let Some(cursor) = request.cursor {
            query.push(("cursor", cursor));
        }
        self.request_json(Method::GET, "/api/session", &query, None)
            .await
    }

    /// Sessions the server is executing right now.
    pub async fn list_running_sessions(&self) -> Result<HashSet<String>> {
        let running: HashMap<String, IgnoredAny> = self
            .request_data(Method::GET, "/api/session/active", &[], None)
            .await?;
        Ok(running.into_keys().collect())
    }

    pub(crate) fn is_child_session(&self, session_id: &str) -> bool {
        self.child_sessions.read().contains(session_id)
    }

    pub(crate) fn remember_child_session(&self, session_id: String) {
        let mut sessions = self.child_sessions.write();
        if sessions.len() >= MAX_CHILD_SESSIONS {
            // Bound memory; forgetting an ID costs only another fetch.
            sessions.clear();
        }
        sessions.insert(session_id);
    }

    pub async fn fetch_session(&self, session_id: &str) -> Result<SessionInfo> {
        let path = format!("/api/session/{}", path_segment(session_id));
        self.request_data(Method::GET, &path, &[], None).await
    }

    pub async fn list_all_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        #[derive(Deserialize)]
        struct Page {
            data: Vec<Value>,
            #[serde(default)]
            cursor: PageCursor,
        }
        const PAGE_SIZE: usize = 200;
        let path = format!("/api/session/{}/message", path_segment(session_id));
        let limit = PAGE_SIZE.to_string();
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::default();
        loop {
            // The cursor already encodes the order; the server rejects both.
            let query = match &cursor {
                Some(cursor) => [("limit", limit.as_str()), ("cursor", cursor.as_str())],
                None => [("limit", limit.as_str()), ("order", "asc")],
            };
            let page: Page = self.request_json(Method::GET, &path, &query, None).await?;
            if page.data.is_empty() {
                break;
            }
            for message in page.data {
                match Message::from_value(message) {
                    Ok(message) => messages.push(message),
                    Err(error) => log::warn!("Skipping invalid OpenCode message: {error:#}"),
                }
            }
            match page.cursor.next {
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                _ => break,
            }
        }
        Ok(messages)
    }

    pub async fn create_session(&self, directory: &str) -> Result<SessionInfo> {
        let body = serde_json::json!({ "location": { "directory": directory } });
        self.request_data(Method::POST, "/api/session", &[], Some(&body))
            .await
    }

    /// Admits a prompt and returns once the server has accepted it; the turn
    /// itself is reported through `session.execution.*` events. `message_id`
    /// lets the client know the ID of the user message before it exists.
    pub async fn send_prompt(
        &self,
        session_id: &str,
        message_id: &str,
        prompt: &PromptBody,
        display_metadata: Value,
        delivery: Delivery,
    ) -> Result<()> {
        let mut body = prompt.to_json();
        body.insert("id".into(), Value::String(message_id.to_string()));
        body.insert("metadata".into(), display_metadata);
        let delivery = match delivery {
            Delivery::Steer => "steer",
            Delivery::Queue => "queue",
        };
        body.insert("delivery".into(), Value::String(delivery.to_string()));
        let path = format!("/api/session/{}/prompt", path_segment(session_id));
        self.request_ok(Method::POST, &path, &[], Some(&Value::Object(body)))
            .await
    }

    pub async fn run_command(
        &self,
        session_id: &str,
        name: &str,
        arguments: &PromptBody,
    ) -> Result<()> {
        let mut body = arguments.to_json();
        body.insert("name".into(), Value::String(name.to_string()));
        let path = format!("/api/session/{}/command", path_segment(session_id));
        self.request_ok(Method::POST, &path, &[], Some(&Value::Object(body)))
            .await
    }

    pub async fn compact(&self, session_id: &str) -> Result<()> {
        let path = format!("/api/session/{}/compact", path_segment(session_id));
        self.request_ok(Method::POST, &path, &[], Some(&serde_json::json!({})))
            .await
    }

    pub async fn list_inbox(&self, session_id: &str) -> Result<Vec<InboxEntry>> {
        let path = format!("/api/session/{}/inbox", path_segment(session_id));
        self.request_data(Method::GET, &path, &[], None).await
    }

    /// Takes back a prompt the session hasn't delivered yet. A delivered one
    /// is left alone.
    pub async fn cancel_inbox_entry(&self, session_id: &str, inbox_id: &str) -> Result<()> {
        let path = format!(
            "/api/session/{}/inbox/{}",
            path_segment(session_id),
            path_segment(inbox_id)
        );
        self.request_ok(Method::DELETE, &path, &[], None).await
    }

    pub async fn interrupt(&self, session_id: &str) -> Result<()> {
        let path = format!("/api/session/{}/interrupt", path_segment(session_id));
        self.request_ok(Method::POST, &path, &[], Some(&serde_json::json!({})))
            .await
    }

    pub async fn wait_until_idle(&self, session_id: &str) -> Result<()> {
        let path = format!(
            "/api/experimental/session/{}/wait",
            path_segment(session_id)
        );
        self.request_ok(Method::POST, &path, &[], Some(&serde_json::json!({})))
            .await
    }

    /// Hides `message_id` and everything after it, and restores the files
    /// those messages changed. The next prompt makes it permanent.
    pub async fn stage_revert(&self, session_id: &str, message_id: &str) -> Result<()> {
        let path = format!("/api/session/{}/revert/stage", path_segment(session_id));
        let body = serde_json::json!({ "messageID": message_id });
        self.request_ok(Method::POST, &path, &[], Some(&body)).await
    }

    /// Text from a session's context, without adding to its history.
    pub async fn generate_text(&self, session_id: &str, prompt: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Generated {
            text: String,
        }
        let path = format!("/api/session/{}/generate", path_segment(session_id));
        let body = serde_json::json!({ "prompt": prompt });
        let generated: Generated = self
            .request_data(Method::POST, &path, &[], Some(&body))
            .await?;
        Ok(generated.text)
    }

    pub async fn set_model(&self, session_id: &str, model: &ModelRef) -> Result<()> {
        let mut model_json = serde_json::json!({ "providerID": model.provider_id, "id": model.id });
        if let Some(variant) = &model.variant {
            model_json["variant"] = Value::String(variant.clone());
        }
        let path = format!("/api/session/{}/model", path_segment(session_id));
        let body = serde_json::json!({ "model": model_json });
        self.request_ok(Method::POST, &path, &[], Some(&body)).await
    }

    pub async fn set_agent(&self, session_id: &str, agent: &str) -> Result<()> {
        let path = format!("/api/session/{}/agent", path_segment(session_id));
        let body = serde_json::json!({ "agent": agent });
        self.request_ok(Method::POST, &path, &[], Some(&body)).await
    }

    pub async fn set_title(&self, session_id: &str, title: &str) -> Result<()> {
        let path = format!("/api/session/{}", path_segment(session_id));
        let body = serde_json::json!({ "title": title });
        self.request_ok(Method::PATCH, &path, &[], Some(&body))
            .await
    }

    pub async fn set_archived(&self, session_id: &str, archived: bool) -> Result<()> {
        let path = format!("/api/session/{}", path_segment(session_id));
        let body = serde_json::json!({ "archived": archived });
        self.request_ok(Method::PATCH, &path, &[], Some(&body))
            .await
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let path = format!("/api/session/{}", path_segment(session_id));
        self.request_ok(Method::DELETE, &path, &[], None).await
    }

    pub async fn list_pending_permissions(
        &self,
        session_id: &str,
    ) -> Result<Vec<PermissionRequest>> {
        let path = format!("/api/session/{}/permission", path_segment(session_id));
        self.request_data(Method::GET, &path, &[], None).await
    }

    pub async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        reply: PermissionReply,
    ) -> Result<()> {
        let path = format!(
            "/api/session/{}/permission/{}/reply",
            path_segment(session_id),
            path_segment(request_id)
        );
        let body = serde_json::json!({ "decision": reply });
        self.request_ok(Method::POST, &path, &[], Some(&body)).await
    }

    pub async fn list_pending_forms(&self, session_id: &str) -> Result<Vec<Form>> {
        let path = format!("/api/session/{}/form", path_segment(session_id));
        self.request_data(Method::GET, &path, &[], None).await
    }

    pub async fn reply_form(&self, session_id: &str, form_id: &str, answer: &Value) -> Result<()> {
        let path = format!(
            "/api/session/{}/form/{}/reply",
            path_segment(session_id),
            path_segment(form_id)
        );
        let body = serde_json::json!({ "answer": answer });
        self.request_ok(Method::POST, &path, &[], Some(&body)).await
    }

    /// `message` tells the asker why nobody answered.
    pub async fn cancel_form(
        &self,
        session_id: &str,
        form_id: &str,
        message: Option<&str>,
    ) -> Result<()> {
        let path = format!(
            "/api/session/{}/form/{}",
            path_segment(session_id),
            path_segment(form_id)
        );
        let query: Vec<(&str, &str)> = message
            .map(|message| ("message", message))
            .into_iter()
            .collect();
        self.request_ok(Method::DELETE, &path, &query, None).await
    }

    /// A shell command's status, or `None` once OpenCode has dropped it.
    pub async fn fetch_shell_status(
        &self,
        directory: &str,
        shell_id: &str,
    ) -> Result<Option<ShellStatus>> {
        #[derive(Deserialize)]
        struct Info {
            status: ShellStatus,
        }
        let path = format!("/api/shell/{}", path_segment(shell_id));
        let info: Option<Info> = none_if_missing(
            self.request_data(Method::GET, &path, &location_query(directory), None)
                .await,
        )?;
        Ok(info.map(|info| info.status))
    }

    /// Output a shell command wrote from byte `cursor` on, or `None` once
    /// OpenCode has dropped it.
    pub async fn fetch_shell_output(
        &self,
        directory: &str,
        shell_id: &str,
        cursor: u64,
    ) -> Result<Option<ShellOutput>> {
        let path = format!("/api/shell/{}/output", path_segment(shell_id));
        let cursor = cursor.to_string();
        let query = [("location[directory]", directory), ("cursor", &cursor)];
        none_if_missing(self.request_data(Method::GET, &path, &query, None).await)
    }

    pub async fn list_models(&self, directory: &str) -> Result<Vec<ModelInfo>> {
        self.request_data(Method::GET, "/api/model", &location_query(directory), None)
            .await
    }

    pub async fn fetch_default_model(&self, directory: &str) -> Result<ModelInfo> {
        self.request_data(
            Method::GET,
            "/api/model/default",
            &location_query(directory),
            None,
        )
        .await
    }

    pub async fn list_providers(&self, directory: &str) -> Result<Vec<ProviderInfo>> {
        self.request_data(
            Method::GET,
            "/api/provider",
            &location_query(directory),
            None,
        )
        .await
    }

    pub async fn list_agents(&self, directory: &str) -> Result<Vec<AgentInfo>> {
        self.request_data(Method::GET, "/api/agent", &location_query(directory), None)
            .await
    }

    pub async fn list_commands(&self, directory: &str) -> Result<Vec<Command>> {
        self.request_data(
            Method::GET,
            "/api/command",
            &location_query(directory),
            None,
        )
        .await
    }

    pub async fn list_skills(&self, directory: &str) -> Result<Vec<Skill>> {
        self.request_data(Method::GET, "/api/skill", &location_query(directory), None)
            .await
    }

    pub async fn list_plugins(&self, directory: &str) -> Result<Vec<PluginInfo>> {
        self.request_data(Method::GET, "/api/plugin", &location_query(directory), None)
            .await
    }

    pub(crate) async fn fetch_config_defaults(&self, directory: &str) -> Result<ConfigDefaults> {
        #[derive(Deserialize)]
        struct Model {
            #[serde(rename = "providerID")]
            provider_id: String,
            model: String,
            variant: Option<String>,
        }

        let mut defaults = ConfigDefaults::default();
        for entry in self.list_config_entries(directory).await? {
            let ConfigEntry::Document { info, .. } = entry else {
                continue;
            };
            if let Some(agent) = info.get("default_agent").and_then(Value::as_str) {
                defaults.agent = Some(agent.to_string());
            }
            if let Some(model) = info.get("model") {
                let model: Model = if let Some(model) = model.as_str() {
                    let (provider_id, model) = model
                        .split_once('/')
                        .context("invalid configured default model")?;
                    Model {
                        provider_id: provider_id.to_string(),
                        model: model.to_string(),
                        variant: None,
                    }
                } else {
                    serde_json::from_value(model.clone())
                        .context("decoding OpenCode's configured default model")?
                };
                defaults.model = Some(ModelRef {
                    provider_id: model.provider_id,
                    id: model.model,
                    variant: model.variant,
                });
            }
        }
        Ok(defaults)
    }

    pub async fn list_config_entries(&self, directory: &str) -> Result<Vec<ConfigEntry>> {
        self.request_json(Method::GET, "/api/config", &location_query(directory), None)
            .await
    }

    /// Config reloads reported by the shared connection, including reconnects
    /// where a reload event may have been missed.
    pub fn watch_config_changes(&self) -> watch::Receiver<()> {
        self.config_changes.read().receiver()
    }

    /// Tells the config's readers to read it again.
    pub fn notify_config_reloaded(&self) {
        // No receiver means no settings page is open.
        self.config_changes.write().send(()).ok();
    }

    /// Names of the MCP servers in OpenCode's own config files for
    /// `directory`, such as `opencode.json`. Servers added through the API
    /// aren't included.
    pub async fn list_configured_mcp_names(&self, directory: &str) -> Result<HashSet<String>> {
        let entries = self.list_config_entries(directory).await?;
        Ok(entries
            .iter()
            .filter_map(|entry| match entry {
                ConfigEntry::Document { info, .. } => info.pointer("/mcp/servers")?.as_object(),
                ConfigEntry::Directory { .. } | ConfigEntry::Other => None,
            })
            .flat_map(|servers| servers.keys().cloned())
            .collect())
    }

    /// Adds or replaces an MCP server for every session in `directory`.
    pub async fn add_mcp(
        &self,
        directory: &str,
        name: &str,
        config: &impl serde::Serialize,
    ) -> Result<()> {
        let body = serde_json::json!({ "config": config });
        self.request_ok(
            Method::PUT,
            &mcp_path(name),
            &location_query(directory),
            Some(&body),
        )
        .await
    }

    /// A server the service doesn't have (it restarted since) counts as removed.
    pub async fn remove_mcp(&self, directory: &str, name: &str) -> Result<()> {
        let removed = self
            .request_ok(
                Method::DELETE,
                &mcp_path(name),
                &location_query(directory),
                None,
            )
            .await;
        none_if_missing(removed).map(drop)
    }

    pub async fn list_integrations(&self, directory: &str) -> Result<Vec<Integration>> {
        self.request_data(
            Method::GET,
            "/api/integration",
            &location_query(directory),
            None,
        )
        .await
    }

    pub async fn connect_api_key(
        &self,
        directory: &str,
        integration_id: &str,
        key: &str,
        answer: Option<&Value>,
    ) -> Result<()> {
        let mut body = serde_json::json!({ "key": key });
        if let Some(answer) = answer {
            body["answer"] = answer.clone();
        }
        let path = connect_path(integration_id, "key");
        self.request_ok(Method::POST, &path, &location_query(directory), Some(&body))
            .await
    }

    pub async fn connect_external(
        &self,
        directory: &str,
        integration_id: &str,
        method_id: &str,
        answer: Option<&Value>,
    ) -> Result<()> {
        let path = connect_path(integration_id, "external");
        let body = connect_body(method_id, answer);
        self.request_ok(Method::POST, &path, &location_query(directory), Some(&body))
            .await
    }

    pub async fn start_oauth(
        &self,
        directory: &str,
        integration_id: &str,
        method_id: &str,
        answer: Option<&Value>,
    ) -> Result<OAuthAttempt> {
        let path = connect_path(integration_id, "oauth");
        let body = connect_body(method_id, answer);
        self.request_data(Method::POST, &path, &location_query(directory), Some(&body))
            .await
    }

    pub async fn fetch_oauth_status(
        &self,
        directory: &str,
        integration_id: &str,
        attempt_id: &str,
    ) -> Result<AttemptStatus> {
        let path = connect_path(
            integration_id,
            &format!("oauth/{}", path_segment(attempt_id)),
        );
        self.request_data(Method::GET, &path, &location_query(directory), None)
            .await
    }

    pub async fn complete_oauth(
        &self,
        directory: &str,
        integration_id: &str,
        attempt_id: &str,
        code: &str,
    ) -> Result<()> {
        let path = connect_path(
            integration_id,
            &format!("oauth/{}/complete", path_segment(attempt_id)),
        );
        let body = serde_json::json!({ "code": code });
        self.request_ok(Method::POST, &path, &location_query(directory), Some(&body))
            .await
    }

    pub async fn cancel_oauth(
        &self,
        directory: &str,
        integration_id: &str,
        attempt_id: &str,
    ) -> Result<()> {
        let path = connect_path(
            integration_id,
            &format!("oauth/{}", path_segment(attempt_id)),
        );
        self.request_ok(Method::DELETE, &path, &location_query(directory), None)
            .await
    }

    pub async fn start_login_command(
        &self,
        directory: &str,
        integration_id: &str,
        method_id: &str,
    ) -> Result<CommandAttempt> {
        let path = connect_path(integration_id, "command");
        let body = connect_body(method_id, None);
        self.request_data(Method::POST, &path, &location_query(directory), Some(&body))
            .await
    }

    pub async fn fetch_login_command_status(
        &self,
        directory: &str,
        integration_id: &str,
        attempt_id: &str,
    ) -> Result<AttemptStatus> {
        let path = connect_path(
            integration_id,
            &format!("command/{}", path_segment(attempt_id)),
        );
        self.request_data(Method::GET, &path, &location_query(directory), None)
            .await
    }

    pub async fn cancel_login_command(
        &self,
        directory: &str,
        integration_id: &str,
        attempt_id: &str,
    ) -> Result<()> {
        let path = connect_path(
            integration_id,
            &format!("command/{}", path_segment(attempt_id)),
        );
        self.request_ok(Method::DELETE, &path, &location_query(directory), None)
            .await
    }

    pub async fn activate_credential(&self, credential_id: &str) -> Result<()> {
        let path = format!("/api/credential/{}/activate", path_segment(credential_id));
        self.request_ok(Method::POST, &path, &[], Some(&serde_json::json!({})))
            .await
    }

    pub async fn rename_credential(&self, credential_id: &str, label: &str) -> Result<()> {
        let path = format!("/api/credential/{}", path_segment(credential_id));
        let body = serde_json::json!({ "label": label });
        self.request_ok(Method::PATCH, &path, &[], Some(&body))
            .await
    }

    pub async fn remove_credential(&self, credential_id: &str) -> Result<()> {
        let path = format!("/api/credential/{}", path_segment(credential_id));
        self.request_ok(Method::DELETE, &path, &[], None).await
    }

    /// The server drops a subscriber that falls behind, and never replays
    /// missed events, so callers must resync their state after the stream
    /// ends. Each read from the stream yields at least one item, `None` when
    /// it completed no event (the heartbeat), so a caller can tell a quiet
    /// stream from a dead one.
    pub async fn subscribe_to_events(
        &self,
    ) -> Result<BoxStream<'static, Result<Option<EventFrame>>>> {
        let request = self
            .request_builder(Method::GET, "/api/event", &[])
            .header("Accept", "text/event-stream")
            .body(AsyncBody::empty())?;
        let mut response = self.http.send(request).await?;
        let status = response.status();
        if !status.is_success() {
            let mut body = Vec::new();
            response.body_mut().read_to_end(&mut body).await.ok();
            return Err(OpenCodeError::new("/api/event", status, &body).into());
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("text/event-stream") {
            return Err(anyhow!(
                "OpenCode's event stream answered with {content_type:?}, not text/event-stream"
            ));
        }
        let reader = Some(EventReader {
            body: response.into_body(),
            parser: SseParser::default(),
            chunk: vec![0; 64 * 1024],
        });
        let frames = futures::stream::unfold(reader, |reader| async move {
            let mut reader = reader?;
            let read = match reader.body.read(&mut reader.chunk).await {
                Ok(read) => read,
                Err(error) => return Some((vec![Err(error.into())], None)),
            };
            if read == 0 {
                // A last event without its blank line still counts, as in
                // OpenCode's own client.
                let last = reader
                    .parser
                    .finish()
                    .map(|data| decode_frame(&data).map(Some));
                return last.map(|last| (vec![last], None));
            }
            match reader.parser.push(&reader.chunk[..read]) {
                Ok(events) if events.is_empty() => Some((vec![Ok(None)], Some(reader))),
                Ok(events) => {
                    let frames = events
                        .iter()
                        .map(|data| decode_frame(data).map(Some))
                        .collect();
                    Some((frames, Some(reader)))
                }
                Err(error) => Some((vec![Err(error)], None)),
            }
        });
        Ok(frames.flat_map(futures::stream::iter).boxed())
    }

    async fn request_data<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<T> {
        let response: DataEnvelope<T> = self.request_json(method, path, query, body).await?;
        Ok(response.data)
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<T> {
        let body = self.request_body(method, path, query, body).await?;
        serde_json::from_slice(&body).with_context(|| format!("decoding OpenCode response {path}"))
    }

    async fn request_ok(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<()> {
        self.request_body(method, path, query, body).await.map(drop)
    }

    /// The response body, or an [`OpenCodeError`] for an error status.
    async fn request_body(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Vec<u8>> {
        let (status, response) = self.request_raw(method, path, query, body).await?;
        if !status.is_success() {
            return Err(OpenCodeError::new(path, status, &response).into());
        }
        Ok(response)
    }

    async fn request_raw(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let builder = self.request_builder(method, path, query);
        let request = match body {
            Some(body) => builder
                .header("Content-Type", "application/json")
                .body(AsyncBody::from(serde_json::to_string(body)?))?,
            None => builder.body(AsyncBody::empty())?,
        };
        let mut response = self.http.send(request).await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        Ok((response.status(), body))
    }

    fn request_builder(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
    ) -> http_client::http::request::Builder {
        let endpoint = self.endpoint.read();
        Request::builder()
            .method(method)
            .uri(endpoint.url(path, query).as_str())
            .header("Authorization", endpoint.authorization.as_str())
    }
}

#[derive(PartialEq)]
struct Endpoint {
    base_url: Url,
    authorization: String,
}

impl Endpoint {
    fn new(registration: &ServiceRegistration) -> Result<Self> {
        let base_url = Url::parse(&registration.url).context("invalid OpenCode service URL")?;
        let credentials = base64::engine::general_purpose::STANDARD
            .encode(format!("opencode:{}", registration.password));
        Ok(Self {
            base_url,
            authorization: format!("Basic {credentials}"),
        })
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> Url {
        // `Url::join` would drop a path the base URL has, such as a proxy's.
        let mut url = self.base_url.clone();
        let base_path = url.path().trim_end_matches('/').to_string();
        url.set_path(&format!("{base_path}{path}"));
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }
}

/// An error status from the OpenCode service.
#[derive(Debug)]
pub struct OpenCodeError {
    pub path: String,
    pub status: u16,
    /// The body's `message`, which OpenCode writes for people to read.
    pub message: Option<String>,
    pub body: String,
}

impl OpenCodeError {
    fn new(path: &str, status: StatusCode, body: &[u8]) -> Self {
        let message = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|body| Some(body.get("message")?.as_str()?.to_string()));
        Self {
            path: path.to_string(),
            status: status.as_u16(),
            message,
            body: String::from_utf8_lossy(body).into_owned(),
        }
    }
}

impl std::fmt::Display for OpenCodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "OpenCode request {} failed with status {}: {}",
            self.path, self.status, self.body
        )
    }
}

impl std::error::Error for OpenCodeError {}

/// The service's answer to `GET /api/info`, whatever its status.
pub struct InfoResponse {
    pub status: u16,
    pub info: Option<ServerInfo>,
}

/// One page of sessions, roots only.
pub struct ListSessions<'a> {
    pub directory: Option<&'a str>,
    pub cursor: Option<&'a str>,
    pub limit: usize,
}

#[derive(Clone, Default)]
pub(crate) struct ConfigDefaults {
    pub model: Option<ModelRef>,
    pub agent: Option<String>,
}

/// When a prompt sent to a running session reaches it.
#[derive(Clone, Copy, Debug)]
pub enum Delivery {
    /// At the turn's next step. OpenCode's default.
    Steer,
    /// After the turn and anything queued before it.
    Queue,
}

/// What a prompt or a command sends besides its name.
pub struct PromptBody {
    pub text: String,
    pub files: Vec<Value>,
    /// IDs of mentioned skills. OpenCode loads a skill itself, so Zed sends
    /// only its ID, like the TUI does.
    pub skills: Vec<String>,
}

impl PromptBody {
    fn to_json(&self) -> serde_json::Map<String, Value> {
        let skills: Vec<Value> = self
            .skills
            .iter()
            .map(|id| serde_json::json!({ "id": id }))
            .collect();
        let mut body = serde_json::Map::new();
        body.insert("text".into(), Value::String(self.text.clone()));
        body.insert("files".into(), Value::Array(self.files.clone()));
        body.insert("skills".into(), Value::Array(skills));
        body
    }
}

#[derive(Deserialize)]
struct DataEnvelope<T> {
    data: T,
}

/// The official client's limit (`maxSseEventBytes`). A larger event fails the
/// stream, and reconnecting resyncs.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

fn location_query(directory: &str) -> [(&'static str, &str); 1] {
    [("location[directory]", directory)]
}

/// `None` for a 404: the server doesn't have what was asked for.
fn none_if_missing<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if error
                .downcast_ref::<OpenCodeError>()
                .is_some_and(|error| error.status == 404) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn path_segment(value: &str) -> String {
    const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(value, PATH_SEGMENT).to_string()
}

/// Well-known integrations use their URL as their ID, so it has to be escaped.
fn connect_path(integration_id: &str, rest: &str) -> String {
    let integration_id = path_segment(integration_id);
    format!("/api/integration/{integration_id}/connect/{rest}")
}

/// MCP server names come from the user's settings and can contain any
/// character.
fn mcp_path(name: &str) -> String {
    let name = path_segment(name);
    format!("/api/experimental/mcp/{name}")
}

fn connect_body(method_id: &str, answer: Option<&Value>) -> Value {
    let mut body = serde_json::json!({ "methodID": method_id });
    if let Some(answer) = answer {
        body["answer"] = answer.clone();
    }
    body
}

struct EventReader {
    body: AsyncBody,
    parser: SseParser,
    chunk: Vec<u8>,
}

fn decode_frame(data: &str) -> Result<EventFrame> {
    serde_json::from_str(data).context("decoding OpenCode event")
}

/// Server-sent events framing: `data:` lines accumulate until a blank line
/// dispatches them; lines starting with `:` (the server's heartbeats) are
/// comments.
#[derive(Default)]
struct SseParser {
    /// Bytes after the last line break.
    pending: Vec<u8>,
    /// How much of `pending` is known to hold no line break.
    scanned: usize,
    data: Vec<String>,
    data_bytes: usize,
}

impl SseParser {
    /// The data of an event the stream ended in the middle of.
    fn finish(&mut self) -> Option<String> {
        let rest = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
        self.scanned = 0;
        // A line that isn't blank never completes an event on its own.
        self.push_line(&rest);
        self.push_line("")
    }

    fn push_line(&mut self, line: &str) -> Option<String> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if self.data.is_empty() {
                return None;
            }
            let data = self.data.join("\n");
            self.data.clear();
            self.data_bytes = 0;
            return Some(data);
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            self.data_bytes += value.len();
            self.data.push(value.to_string());
        }
        None
    }

    /// Each event's data, in the order the bytes complete them.
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut line_start = 0;
        while let Some(offset) = self.pending[self.scanned..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let line_end = self.scanned + offset;
            let line = String::from_utf8_lossy(&self.pending[line_start..line_end]).into_owned();
            events.extend(self.push_line(&line));
            line_start = line_end + 1;
            self.scanned = line_start;
        }
        self.pending.drain(..line_start);
        self.scanned = self.pending.len();
        if self.pending.len() + self.data_bytes > MAX_EVENT_BYTES {
            anyhow::bail!("an OpenCode event is larger than 16 MB");
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paging_client() -> (OpenCodeClient, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let requests = Arc::new(AtomicUsize::new(0));
        let http = http_client::FakeHttpClient::create({
            let requests = requests.clone();
            move |request| {
                let requests = requests.clone();
                async move {
                    let count = requests.fetch_add(1, Ordering::SeqCst);
                    if count >= 2 {
                        anyhow::bail!("repeated cursor was requested again");
                    }
                    let second = request
                        .uri()
                        .query()
                        .unwrap_or_default()
                        .contains("cursor=");
                    let id = if second { "second" } else { "first" };
                    let item = if request.uri().path().ends_with("/message") {
                        serde_json::json!({ "id": id, "type": "future" })
                    } else {
                        serde_json::json!({ "id": id })
                    };
                    let body = serde_json::json!({ "data": [item], "cursor": { "next": "page2" } });
                    Ok(http_client::Response::builder()
                        .status(200)
                        .body(AsyncBody::from(body.to_string()))?)
                }
            }
        });
        let registration = ServiceRegistration {
            id: "test".into(),
            version: "1".into(),
            url: "http://127.0.0.1:4096".into(),
            pid: 1,
            password: String::new(),
        };
        (
            OpenCodeClient::new(http, &registration).expect("client"),
            requests,
        )
    }

    #[gpui::test]
    async fn sessions_follow_short_pages() {
        use std::sync::atomic::Ordering;

        let (client, requests) = paging_client();
        let sessions = client.list_all_sessions(10).await.expect("sessions");
        let ids: Vec<_> = sessions.iter().map(|session| session.id.as_str()).collect();
        assert_eq!(ids, ["first", "second"]);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[gpui::test]
    async fn messages_follow_short_pages() {
        use std::sync::atomic::Ordering;

        let (client, requests) = paging_client();
        let messages = client.list_all_messages("session").await.expect("messages");
        let ids: Vec<_> = messages.iter().map(|message| message.id.as_str()).collect();
        assert_eq!(ids, ["first", "second"]);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    fn empty_page_client() -> (OpenCodeClient, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let requests = Arc::new(AtomicUsize::new(0));
        let http = http_client::FakeHttpClient::create({
            let requests = requests.clone();
            move |request| {
                let requests = requests.clone();
                async move {
                    let count = requests.fetch_add(1, Ordering::SeqCst);
                    if count >= 2 {
                        anyhow::bail!("empty page was followed");
                    }
                    let data = if count == 0 {
                        if request.uri().path().ends_with("/message") {
                            serde_json::json!([{ "id": "first", "type": "future" }])
                        } else {
                            serde_json::json!([{ "id": "first" }])
                        }
                    } else {
                        serde_json::json!([])
                    };
                    let next = if count == 0 { "page2" } else { "page3" };
                    let body = serde_json::json!({ "data": data, "cursor": { "next": next } });
                    Ok(http_client::Response::builder()
                        .status(200)
                        .body(AsyncBody::from(body.to_string()))?)
                }
            }
        });
        let registration = ServiceRegistration {
            id: "test".into(),
            version: "1".into(),
            url: "http://127.0.0.1:4096".into(),
            pid: 1,
            password: String::new(),
        };
        (
            OpenCodeClient::new(http, &registration).expect("client"),
            requests,
        )
    }

    #[gpui::test]
    async fn sessions_stop_at_empty_page() {
        use std::sync::atomic::Ordering;
        let (client, requests) = empty_page_client();
        assert_eq!(
            client.list_all_sessions(10).await.expect("sessions").len(),
            1
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[gpui::test]
    async fn messages_stop_at_empty_page() {
        use std::sync::atomic::Ordering;
        let (client, requests) = empty_page_client();
        assert_eq!(
            client
                .list_all_messages("session")
                .await
                .expect("messages")
                .len(),
            1
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn parser_tracks_scanned_bytes() {
        let mut parser = SseParser::default();
        parser.push(b"data: part").expect("first chunk");
        assert_eq!(parser.scanned, parser.pending.len());
        parser.push(b"ial").expect("second chunk");
        assert_eq!(parser.scanned, parser.pending.len());
        assert_eq!(parser.push(b"\n\n").expect("last chunk"), ["partial"]);
    }

    #[gpui::test]
    async fn invalid_service_info_is_an_error() {
        let client = client_answering("application/json", "{}");
        assert!(client.fetch_service_info().await.is_err());
    }

    #[gpui::test]
    async fn string_model_is_accepted() {
        let client = client_answering(
            "application/json",
            r#"[{"type":"document","info":{"model":"openai/gpt-5"}}]"#,
        );
        let defaults = client
            .fetch_config_defaults("/project")
            .await
            .expect("defaults");
        let model = defaults.model.expect("model");
        assert_eq!(model.provider_id, "openai");
        assert_eq!(model.id, "gpt-5");
    }

    #[test]
    fn reserved_path_characters_are_encoded() {
        assert_eq!(path_segment("a/b?# %"), "a%2Fb%3F%23%20%25");
        let registration = ServiceRegistration {
            id: "a".into(),
            version: "1".into(),
            url: "http://localhost".into(),
            pid: 1,
            password: String::new(),
        };
        let endpoint = Endpoint::new(&registration).expect("endpoint");
        assert_eq!(
            endpoint
                .url(&format!("/api/session/{}", path_segment("a/b?# %")), &[])
                .path(),
            "/api/session/a%2Fb%3F%23%20%25"
        );
    }

    #[test]
    fn mcp_names_are_escaped_in_the_path() {
        assert_eq!(
            mcp_path("my server/v2?x"),
            "/api/experimental/mcp/my%20server%2Fv2%3Fx"
        );
    }

    #[test]
    fn urls_keep_the_base_path() {
        let registration = ServiceRegistration {
            id: "a".into(),
            version: "2.2.5".into(),
            url: "http://127.0.0.1:4096/opencode/".into(),
            pid: 1,
            password: String::new(),
        };
        let endpoint = Endpoint::new(&registration).expect("valid URL");
        assert_eq!(
            endpoint
                .url(&mcp_path("a b"), &location_query("/project"))
                .as_str(),
            "http://127.0.0.1:4096/opencode/api/experimental/mcp/a%20b?location%5Bdirectory%5D=%2Fproject"
        );
    }

    fn client_answering(content_type: &'static str, body: &'static str) -> OpenCodeClient {
        let http = http_client::FakeHttpClient::create(move |_| async move {
            Ok(http_client::Response::builder()
                .status(200)
                .header("content-type", content_type)
                .body(AsyncBody::from(body))?)
        });
        let registration = ServiceRegistration {
            id: "a".into(),
            version: "2.2.5".into(),
            url: "http://127.0.0.1:4096".into(),
            pid: 1,
            password: String::new(),
        };
        OpenCodeClient::new(http, &registration).expect("valid registration")
    }

    #[gpui::test]
    async fn configured_defaults_use_the_last_document_that_sets_each_field() {
        let client = client_answering(
            "application/json",
            r#"[
            {"type":"document","info":{"default_agent":"global","model":{"providerID":"test","model":"global"}}},
            {"type":"directory","path":"/project/.opencode"},
            {"type":"document","info":{"model":{"providerID":"test","model":"project","variant":"high"}}},
            {"type":"document","info":{}}
        ]"#,
        );
        let defaults = client
            .fetch_config_defaults("/project")
            .await
            .expect("defaults");
        assert_eq!(defaults.agent.as_deref(), Some("global"));
        assert_eq!(
            defaults.model,
            Some(ModelRef {
                provider_id: "test".into(),
                id: "project".into(),
                variant: Some("high".into()),
            })
        );
    }

    #[gpui::test]
    async fn the_event_stream_yields_every_event_including_an_unterminated_last_one() {
        let client = client_answering(
            "text/event-stream",
            ": heartbeat\n\ndata: {\"id\":\"evt_1\",\"type\":\"a\"}\n\ndata: {\"id\":\"evt_2\",\"type\":\"b\"}",
        );
        let frames: Vec<EventFrame> = client
            .subscribe_to_events()
            .await
            .expect("an event stream")
            .filter_map(|item| async move { item.expect("readable") })
            .collect()
            .await;
        let ids: Vec<&str> = frames.iter().map(|frame| frame.id.as_str()).collect();
        assert_eq!(ids, ["evt_1", "evt_2"]);
    }

    #[gpui::test]
    async fn the_event_stream_must_be_an_event_stream() {
        let client = client_answering("text/plain", "");
        assert!(client.subscribe_to_events().await.is_err());
    }

    #[test]
    fn sse_parser_dispatches_on_blank_line() {
        let mut parser = SseParser::default();
        let mut dispatched = Vec::new();
        for chunk in [
            ": heartbeat\n\ndata: {\"a\":",
            "\ndata: 1}\n\nevent: ignored\ndata:{\"b\"",
            ":2}\r\n\r\n",
        ] {
            dispatched.extend(parser.push(chunk.as_bytes()).expect("small events"));
        }
        assert_eq!(dispatched, vec!["{\"a\":\n1}", "{\"b\":2}"]);
    }

    #[test]
    fn sse_parser_keeps_an_event_the_stream_ended_in() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"data: {\"a\":1}").expect("small").is_empty());
        assert_eq!(parser.finish().as_deref(), Some("{\"a\":1}"));
        assert_eq!(parser.finish(), None);
    }

    #[test]
    fn sse_parser_rejects_an_event_over_16_mb() {
        let mut parser = SseParser::default();
        parser.push(b"data: ").expect("small");
        let chunk = vec![b'x'; 1024 * 1024];
        let mut failed = false;
        for _ in 0..17 {
            if parser.push(&chunk).is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
    }
}
