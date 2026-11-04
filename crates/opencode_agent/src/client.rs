use crate::service::ServiceRegistration;
use crate::types::{
    AgentInfo, Event, Message, ModelInfo, ModelRef, NamedItem, ProviderInfo, ServerInfo, SessionInfo, Skill,
    SessionPage,
};
use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use collections::{HashMap, HashSet};
use futures::io::BufReader;
use futures::stream::BoxStream;
use futures::{AsyncBufReadExt as _, AsyncReadExt as _, StreamExt as _};
use http_client::{AsyncBody, HttpClient, Method, Request, Url};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

/// A client for one OpenCode server. Cheap to clone.
#[derive(Clone)]
pub struct OpenCodeClient {
    http: Arc<dyn HttpClient>,
    base_url: Url,
    authorization: String,
}

/// One page of a shell command's captured output.
#[derive(Debug, Deserialize)]
pub struct ShellOutput {
    pub output: String,
    /// Byte position after this page.
    pub cursor: u64,
    /// Bytes captured so far.
    pub size: u64,
}

fn skill_attachments(skills: &[String]) -> Vec<Value> {
    skills
        .iter()
        .map(|id| serde_json::json!({ "id": id }))
        .collect()
}

pub struct ListSessions<'a> {
    pub directory: Option<&'a str>,
    pub roots_only: bool,
    pub cursor: Option<&'a str>,
    pub limit: Option<usize>,
}

#[derive(Clone, Copy)]
pub enum Order {
    Ascending,
    Descending,
}

pub struct MessagePage {
    pub messages: Vec<Message>,
    pub next_cursor: Option<String>,
}

impl OpenCodeClient {
    pub fn new(http: Arc<dyn HttpClient>, registration: &ServiceRegistration) -> Result<Self> {
        let base_url = Url::parse(&registration.url).context("invalid OpenCode service URL")?;
        let credentials = base64::engine::general_purpose::STANDARD
            .encode(format!("opencode:{}", registration.password));
        Ok(Self {
            http,
            base_url,
            authorization: format!("Basic {credentials}"),
        })
    }

    pub async fn info(&self) -> Result<ServerInfo> {
        self.get_json(self.url("/api/info", &[])?).await
    }

    pub async fn list_sessions(&self, request: ListSessions<'_>) -> Result<SessionPage> {
        let limit = request.limit.map(|limit| limit.to_string());
        let mut query = Vec::new();
        if request.roots_only {
            query.push(("parentID", "null"));
        }
        if let Some(directory) = request.directory {
            query.push(("directory", directory));
        }
        if let Some(cursor) = request.cursor {
            query.push(("cursor", cursor));
        }
        if let Some(limit) = limit.as_deref() {
            query.push(("limit", limit));
        }
        self.get_json(self.url("/api/session", &query)?).await
    }

    /// Sessions the server is executing right now, keyed by session ID.
    pub async fn active_sessions(&self) -> Result<HashMap<String, Value>> {
        #[derive(Deserialize)]
        struct Response {
            data: HashMap<String, Value>,
        }
        let response: Response = self.get_json(self.url("/api/session/active", &[])?).await?;
        Ok(response.data)
    }

    pub async fn session(&self, session_id: &str) -> Result<SessionInfo> {
        #[derive(Deserialize)]
        struct Response {
            data: SessionInfo,
        }
        let path = format!("/api/session/{session_id}");
        let response: Response = self.get_json(self.url(&path, &[])?).await?;
        Ok(response.data)
    }

    pub async fn messages(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
        order: Order,
    ) -> Result<MessagePage> {
        #[derive(Deserialize)]
        struct Response {
            data: Vec<Value>,
            #[serde(default)]
            cursor: crate::types::PageCursor,
        }
        let limit = limit.to_string();
        let order = match order {
            Order::Ascending => "asc",
            Order::Descending => "desc",
        };
        let mut query = vec![("limit", limit.as_str())];
        // The cursor already encodes the order; the server rejects both.
        match cursor {
            Some(cursor) => query.push(("cursor", cursor)),
            None => query.push(("order", order)),
        }
        let path = format!("/api/session/{session_id}/message");
        let response: Response = self.get_json(self.url(&path, &query)?).await?;
        let messages = response
            .data
            .into_iter()
            .map(Message::from_value)
            .collect::<Result<Vec<_>>>()?;
        Ok(MessagePage {
            messages,
            next_cursor: response.cursor.next,
        })
    }

    /// Every message in the session, oldest first.
    pub async fn all_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        const PAGE_SIZE: usize = 200;
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .messages(session_id, cursor.as_deref(), PAGE_SIZE, Order::Ascending)
                .await?;
            let page_len = page.messages.len();
            messages.extend(page.messages);
            match page.next_cursor {
                Some(next) if page_len == PAGE_SIZE && Some(&next) != cursor.as_ref() => {
                    cursor = Some(next)
                }
                _ => break,
            }
        }
        Ok(messages)
    }

    pub async fn create_session(&self, directory: &str) -> Result<SessionInfo> {
        #[derive(Deserialize)]
        struct Response {
            data: SessionInfo,
        }
        let body = serde_json::json!({ "location": { "directory": directory } });
        let response: Response = self
            .post_json(self.url("/api/session", &[])?, &body)
            .await?;
        Ok(response.data)
    }

    /// Admits a prompt and returns once the server has accepted it; the turn
    /// itself is reported through `session.execution.*` events. `message_id`
    /// lets the client know the ID of the user message before it exists.
    pub async fn prompt(
        &self,
        session_id: &str,
        message_id: &str,
        text: &str,
        files: &[Value],
        skills: &[String],
        metadata: Value,
    ) -> Result<()> {
        let body = serde_json::json!({
            "id": message_id,
            "text": text,
            "files": files,
            "skills": skill_attachments(skills),
            "metadata": metadata,
        });
        let path = format!("/api/session/{session_id}/prompt");
        let _: Value = self.post_json(self.url(&path, &[])?, &body).await?;
        Ok(())
    }

    pub async fn interrupt(&self, session_id: &str) -> Result<()> {
        let path = format!("/api/session/{session_id}/interrupt");
        let _: Value = self
            .post_json(self.url(&path, &[])?, &serde_json::json!({}))
            .await?;
        Ok(())
    }

    pub async fn models(&self, directory: &str) -> Result<Vec<ModelInfo>> {
        self.located_data("/api/model", directory).await
    }

    pub async fn providers(&self, directory: &str) -> Result<Vec<ProviderInfo>> {
        self.located_data("/api/provider", directory).await
    }

    pub async fn default_model(&self, directory: &str) -> Result<ModelInfo> {
        self.located_data("/api/model/default", directory).await
    }

    pub async fn agents(&self, directory: &str) -> Result<Vec<AgentInfo>> {
        self.located_data("/api/agent", directory).await
    }

    pub async fn commands(&self, directory: &str) -> Result<Vec<NamedItem>> {
        self.located_data("/api/command", directory).await
    }

    pub async fn skills(&self, directory: &str) -> Result<Vec<Skill>> {
        self.located_data("/api/skill", directory).await
    }

    pub async fn set_model(&self, session_id: &str, model: &ModelRef) -> Result<()> {
        let mut model_json = serde_json::json!({ "providerID": model.provider_id, "id": model.id });
        if let Some(variant) = &model.variant {
            model_json["variant"] = Value::String(variant.clone());
        }
        let path = format!("/api/session/{session_id}/model");
        let _: Value = self
            .post_json(
                self.url(&path, &[])?,
                &serde_json::json!({ "model": model_json }),
            )
            .await?;
        Ok(())
    }

    pub async fn set_agent(&self, session_id: &str, agent: &str) -> Result<()> {
        let path = format!("/api/session/{session_id}/agent");
        let _: Value = self
            .post_json(self.url(&path, &[])?, &serde_json::json!({ "agent": agent }))
            .await?;
        Ok(())
    }

    pub async fn set_title(&self, session_id: &str, title: &str) -> Result<()> {
        let path = format!("/api/session/{session_id}");
        let request = self
            .request(Method::PATCH, self.url(&path, &[])?)
            .header("Content-Type", "application/json")
            .body(AsyncBody::from(
                serde_json::json!({ "title": title }).to_string(),
            ))?;
        let _: Value = self.send_json(request, &path).await?;
        Ok(())
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let path = format!("/api/session/{session_id}");
        let request = self
            .request(Method::DELETE, self.url(&path, &[])?)
            .body(AsyncBody::empty())?;
        let _: Value = self.send_json(request, &path).await?;
        Ok(())
    }

    pub async fn pending_permissions(&self, session_id: &str) -> Result<Vec<Value>> {
        #[derive(Deserialize)]
        struct Response {
            data: Vec<Value>,
        }
        let path = format!("/api/session/{session_id}/permission");
        let response: Response = self.get_json(self.url(&path, &[])?).await?;
        Ok(response.data)
    }

    /// Adds or replaces an MCP server for every session in `directory`.
    pub async fn add_mcp(&self, directory: &str, name: &str, config: &Value) -> Result<()> {
        let path = format!("/api/experimental/mcp/{name}");
        let request = self
            .request(
                Method::PUT,
                self.url(&path, &[("location[directory]", directory)])?,
            )
            .header("Content-Type", "application/json")
            .body(AsyncBody::from(serde_json::to_string(
                &serde_json::json!({ "config": config }),
            )?))?;
        let _: Value = self.send_json(request, &path).await?;
        Ok(())
    }

    /// Removes an MCP server from every session in `directory`. A server the
    /// service doesn't have (it restarted since) counts as removed.
    pub async fn remove_mcp(&self, directory: &str, name: &str) -> Result<()> {
        let path = format!("/api/experimental/mcp/{name}");
        let request = self
            .request(
                Method::DELETE,
                self.url(&path, &[("location[directory]", directory)])?,
            )
            .body(AsyncBody::empty())?;
        let mut response = self.http.send(request).await?;
        let status = response.status();
        if status.is_success() || status == http_client::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        Err(anyhow!(
            "OpenCode request {path} failed with status {status}: {}",
            String::from_utf8_lossy(&body)
        ))
    }

    /// Names of the MCP servers in OpenCode's own config files for
    /// `directory`, such as `opencode.json`. Servers added through the API
    /// aren't included.
    pub async fn configured_mcp_names(&self, directory: &str) -> Result<HashSet<String>> {
        let path = "/api/config";
        let documents: Vec<Value> = self
            .get_json(self.url(path, &[("location[directory]", directory)])?)
            .await?;
        Ok(documents
            .iter()
            .filter_map(|document| document.pointer("/info/mcp/servers")?.as_object())
            .flat_map(|servers| servers.keys().cloned())
            .collect())
    }

    /// Forms in the session still waiting for an answer.
    pub async fn pending_forms(&self, session_id: &str) -> Result<Vec<Value>> {
        #[derive(Deserialize)]
        struct Response {
            data: Vec<Value>,
        }
        let path = format!("/api/session/{session_id}/form");
        let response: Response = self.get_json(self.url(&path, &[])?).await?;
        Ok(response.data)
    }

    pub async fn reply_form(&self, session_id: &str, form_id: &str, answer: &Value) -> Result<()> {
        let path = format!("/api/session/{session_id}/form/{form_id}/reply");
        let _: Value = self
            .post_json(
                self.url(&path, &[])?,
                &serde_json::json!({ "answer": answer }),
            )
            .await?;
        Ok(())
    }

    /// Cancels a form. `message` tells the asker why nobody answered.
    pub async fn cancel_form(
        &self,
        session_id: &str,
        form_id: &str,
        message: Option<&str>,
    ) -> Result<()> {
        let path = format!("/api/session/{session_id}/form/{form_id}");
        let query: Vec<(&str, &str)> = message.map(|message| ("message", message)).into_iter().collect();
        let request = self
            .request(Method::DELETE, self.url(&path, &query)?)
            .body(AsyncBody::empty())?;
        let _: Value = self.send_json(request, &path).await?;
        Ok(())
    }

    /// Blocks until the session stops executing.
    pub async fn wait_until_idle(&self, session_id: &str) -> Result<()> {
        let path = format!("/api/experimental/session/{session_id}/wait");
        let _: Value = self
            .post_json(self.url(&path, &[])?, &serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Hides `message_id` and everything after it, and restores the files
    /// those messages changed. The next prompt makes it permanent.
    pub async fn stage_revert(&self, session_id: &str, message_id: &str) -> Result<()> {
        let path = format!("/api/session/{session_id}/revert/stage");
        let _: Value = self
            .post_json(
                self.url(&path, &[])?,
                &serde_json::json!({ "messageID": message_id }),
            )
            .await?;
        Ok(())
    }

    pub async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: &str,
    ) -> Result<()> {
        let path = format!("/api/session/{session_id}/permission/{request_id}/reply");
        let _: Value = self
            .post_json(
                self.url(&path, &[])?,
                &serde_json::json!({ "decision": decision }),
            )
            .await?;
        Ok(())
    }

    pub async fn command(
        &self,
        session_id: &str,
        name: &str,
        arguments: &str,
        skills: &[String],
    ) -> Result<()> {
        let path = format!("/api/session/{session_id}/command");
        let body = serde_json::json!({
            "name": name,
            "text": arguments,
            "skills": skill_attachments(skills),
        });
        let _: Value = self.post_json(self.url(&path, &[])?, &body).await?;
        Ok(())
    }

    /// A shell command's status, or `None` once OpenCode has dropped it.
    pub async fn shell_status(&self, directory: &str, shell_id: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Response {
            data: Info,
        }
        #[derive(Deserialize)]
        struct Info {
            status: String,
        }
        let url = self.url(
            &format!("/api/shell/{shell_id}"),
            &[("location[directory]", directory)],
        )?;
        let response: Option<Response> = self.get_json_unless_missing(url).await?;
        Ok(response.map(|response| response.data.status))
    }

    /// Output a shell command wrote from byte `cursor` on, or `None` once
    /// OpenCode has dropped it.
    pub async fn shell_output(
        &self,
        directory: &str,
        shell_id: &str,
        cursor: u64,
    ) -> Result<Option<ShellOutput>> {
        #[derive(Deserialize)]
        struct Response {
            data: ShellOutput,
        }
        let cursor = cursor.to_string();
        let url = self.url(
            &format!("/api/shell/{shell_id}/output"),
            &[("location[directory]", directory), ("cursor", &cursor)],
        )?;
        let response: Option<Response> = self.get_json_unless_missing(url).await?;
        Ok(response.map(|response| response.data))
    }

    /// Text from a session's context, without adding to its history.
    pub async fn generate(&self, session_id: &str, prompt: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Response {
            data: Generated,
        }
        #[derive(Deserialize)]
        struct Generated {
            text: String,
        }
        let path = format!("/api/session/{session_id}/generate");
        let response: Response = self
            .post_json(
                self.url(&path, &[])?,
                &serde_json::json!({ "prompt": prompt }),
            )
            .await?;
        Ok(response.data.text)
    }

    pub async fn compact(&self, session_id: &str) -> Result<()> {
        let path = format!("/api/session/{session_id}/compact");
        let _: Value = self
            .post_json(self.url(&path, &[])?, &serde_json::json!({}))
            .await?;
        Ok(())
    }

    async fn located_data<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        directory: &str,
    ) -> Result<T> {
        #[derive(Deserialize)]
        struct Response<T> {
            data: T,
        }
        let response: Response<T> = self
            .get_json(self.url(path, &[("location[directory]", directory)])?)
            .await?;
        Ok(response.data)
    }

    /// Subscribes to every event on the server. The server drops a subscriber
    /// that falls behind, and never replays missed events, so callers must
    /// resync their state after the stream ends.
    pub async fn events(&self) -> Result<BoxStream<'static, Result<Event>>> {
        let request = self
            .request(Method::GET, self.url("/api/event", &[])?)
            .header("Accept", "text/event-stream")
            .body(AsyncBody::empty())?;
        let response = self.http.send(request).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("OpenCode event stream failed with status {status}"));
        }
        let lines = BufReader::new(response.into_body()).lines();
        let events = lines
            .scan(SseParser::default(), |parser, line| {
                let output = match line {
                    Ok(line) => parser.push_line(&line).map(|data| {
                        serde_json::from_str::<Event>(&data).context("decoding OpenCode event")
                    }),
                    Err(error) => Some(Err(anyhow::Error::from(error))),
                };
                futures::future::ready(Some(output))
            })
            .filter_map(futures::future::ready);
        Ok(events.boxed())
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let mut url = self.base_url.join(path)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    fn request(&self, method: Method, url: Url) -> http_client::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(url.as_str())
            .header("Authorization", &self.authorization)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        let request = self
            .request(Method::GET, url.clone())
            .body(AsyncBody::empty())?;
        self.send_json(request, url.path()).await
    }

    async fn get_json_unless_missing<T: serde::de::DeserializeOwned>(
        &self,
        url: Url,
    ) -> Result<Option<T>> {
        let request = self
            .request(Method::GET, url.clone())
            .body(AsyncBody::empty())?;
        let mut response = self.http.send(request).await?;
        if response.status() == http_client::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!(
                "OpenCode request {} failed with status {status}: {}",
                url.path(),
                String::from_utf8_lossy(&body)
            ));
        }
        serde_json::from_slice(&body)
            .map(Some)
            .with_context(|| format!("decoding OpenCode response {}", url.path()))
    }

    async fn post_json<T: serde::de::DeserializeOwned>(&self, url: Url, body: &Value) -> Result<T> {
        let request = self
            .request(Method::POST, url.clone())
            .header("Content-Type", "application/json")
            .body(AsyncBody::from(serde_json::to_string(body)?))?;
        self.send_json(request, url.path()).await
    }

    async fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        request: Request<AsyncBody>,
        path: &str,
    ) -> Result<T> {
        let mut response = self.http.send(request).await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!(
                "OpenCode request {path} failed with status {status}: {}",
                String::from_utf8_lossy(&body)
            ));
        }
        if body.is_empty() {
            body.extend_from_slice(b"null");
        }
        serde_json::from_slice(&body).with_context(|| format!("decoding OpenCode response {path}"))
    }
}

/// Server-sent events framing: `data:` lines accumulate until a blank line
/// dispatches them; lines starting with `:` (the server's heartbeats) are
/// comments.
#[derive(Default)]
pub struct SseParser {
    data: Vec<String>,
}

impl SseParser {
    pub fn push_line(&mut self, line: &str) -> Option<String> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if self.data.is_empty() {
                return None;
            }
            let data = self.data.join("\n");
            self.data.clear();
            return Some(data);
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            self.data
                .push(value.strip_prefix(' ').unwrap_or(value).to_string());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_dispatches_on_blank_line_and_skips_heartbeats() {
        let mut parser = SseParser::default();
        let mut dispatched = Vec::new();
        for line in [
            ": heartbeat",
            "",
            "data: {\"a\":",
            "data: 1}",
            "",
            "event: ignored",
            "data:{\"b\":2}\r",
            "\r",
        ] {
            dispatched.extend(parser.push_line(line));
        }
        assert_eq!(dispatched, vec!["{\"a\":\n1}", "{\"b\":2}"]);
    }
}
