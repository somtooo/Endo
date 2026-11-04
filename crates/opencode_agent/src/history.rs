//! Keeps Zed's thread history in step with the OpenCode service, without the
//! user importing anything and whether or not they use OpenCode D.
//!
//! Sessions the server is running show in the sidebar (unarchived, with a
//! running status); the rest go to history, as manual import does.

use crate::connection::{AGENT_ID, Hub, session_list_info};
use crate::client::ListSessions;
use crate::session_sync::Activity;
use crate::types::SessionInfo;
use agent_client_protocol::v2::schema as acp;
use agent_ui::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore, WorktreePaths};
use anyhow::Result;
use chrono::Utc;
use gpui::{App, AsyncApp, TaskExt as _};
use project::AgentId;
use std::rc::Rc;
use util::ResultExt as _;

const MAX_SYNCED_SESSIONS: usize = 2000;
const PAGE_SIZE: usize = 200;

pub(crate) fn start(hub: Rc<Hub>, cx: &mut App) {
    {
        let hub_for_events = Rc::downgrade(&hub);
        hub.observe_activity(move |session_id, activity, cx| {
            let Some(hub) = hub_for_events.upgrade() else {
                return;
            };
            on_activity(hub, session_id.to_string(), activity, cx);
        });
    }
    hub.observe_renames(|session_id, title, cx| set_title(session_id, title, cx));
    cx.spawn(async move |cx| sync_all(&hub, cx).await.log_err())
        .detach();
}

async fn sync_all(hub: &Rc<Hub>, cx: &mut AsyncApp) -> Result<()> {
    let store = cx.update(|cx| ThreadMetadataStore::global(cx));
    let reload = cx.update(|cx| store.read(cx).reload_task());
    reload.await;

    let client = hub.client().clone();
    let running = client.active_sessions().await?;
    let mut sessions = Vec::new();
    let mut cursor: Option<String> = None;
    while sessions.len() < MAX_SYNCED_SESSIONS {
        let page = client
            .list_sessions(ListSessions {
                directory: None,
                roots_only: true,
                cursor: cursor.as_deref(),
                limit: Some(PAGE_SIZE),
            })
            .await?;
        let page_len = page.data.len();
        sessions.extend(page.data);
        match page.cursor.next {
            Some(next) if page_len == PAGE_SIZE => cursor = Some(next),
            _ => break,
        }
    }

    cx.update(|cx| {
        let (rows, unused_rows): (Vec<ThreadMetadata>, Vec<ThreadId>) = {
            let store = store.read(cx);
            // Rows earlier syncs added before unused sessions were skipped.
            let unused_rows = sessions
                .iter()
                .filter(|session| is_unused(session))
                .filter_map(|session| {
                    store.entry_by_session(&acp::SessionId::new(session.id.as_str()))
                })
                .filter(|entry| {
                    entry.archived
                        && entry.title_override.is_none()
                        && entry.agent_id == AgentId::new(AGENT_ID)
                })
                .map(|entry| entry.thread_id)
                .collect();
            let rows = sessions
                .iter()
                .filter(|session| session.parent_id.is_none())
                .filter(|session| !is_unused(session))
                .filter(|session| {
                    store
                        .entry_by_session(&acp::SessionId::new(session.id.as_str()))
                        .is_none()
                })
                .filter_map(|session| row(session, !running.contains_key(&session.id)))
                .collect();
            (rows, unused_rows)
        };
        log::info!("synced {} OpenCode sessions into thread history", rows.len());
        // Rows added before their session had a title (or renamed while Zed
        // was closed) catch up here.
        for session in &sessions {
            if let Some(title) = &session.title {
                set_title(&session.id, title, cx);
            }
        }
        store.update(cx, |store, cx| {
            store.delete_all(unused_rows, cx);
            store.save_all(rows, cx);
            for session_id in running.keys() {
                store.set_externally_running(acp::SessionId::new(session_id.as_str()), true, cx);
            }
        });
        for session_id in running.keys() {
            unarchive(session_id, cx);
        }
    });
    Ok(())
}

/// Opening a thread creates its session, so sessions nobody sent anything to
/// pile up on the server. Like the native agent's empty drafts, they stay out
/// of history. Old sessions can lack `busy`, but they have a title.
fn is_unused(session: &SessionInfo) -> bool {
    session.title.as_deref().is_none_or(str::is_empty) && session.time.busy.is_none()
}

fn row(session: &SessionInfo, archived: bool) -> Option<ThreadMetadata> {
    let info = session_list_info(session);
    let work_dirs = info.work_dirs?;
    Some(ThreadMetadata {
        thread_id: ThreadId::new(),
        session_id: Some(info.session_id),
        agent_id: AgentId::new(AGENT_ID),
        title: info.title,
        title_override: None,
        updated_at: info.updated_at.unwrap_or_else(Utc::now),
        created_at: info.created_at,
        interacted_at: None,
        worktree_paths: WorktreePaths::from_folder_paths(&work_dirs),
        remote_connection: None,
        archived,
    })
}

/// Sessions get their title after the first prompt, often after the row was
/// added, so renames are applied to rows whose thread isn't open. Open
/// threads update their row themselves. A title the user set in Zed wins.
fn set_title(session_id: &str, title: &str, cx: &mut App) {
    let store = ThreadMetadataStore::global(cx);
    let thread_id = store
        .read(cx)
        .entry_by_session(&acp::SessionId::new(session_id))
        .filter(|entry| entry.title_override.is_none())
        .filter(|entry| entry.title.as_deref() != Some(title))
        .map(|entry| entry.thread_id);
    if let Some(thread_id) = thread_id {
        store.update(cx, |store, cx| {
            store.set_generated_title(thread_id, title.to_string().into(), cx)
        });
    }
}

fn unarchive(session_id: &str, cx: &mut App) {
    let store = ThreadMetadataStore::global(cx);
    let thread_id = store
        .read(cx)
        .entry_by_session(&acp::SessionId::new(session_id))
        .filter(|entry| entry.archived)
        .map(|entry| entry.thread_id);
    if let Some(thread_id) = thread_id {
        store.update(cx, |store, cx| store.unarchive(thread_id, cx));
    }
}

fn on_activity(hub: Rc<Hub>, session_id: String, activity: Activity, cx: &mut App) {
    let store = ThreadMetadataStore::global(cx);
    let running = activity == Activity::Running;
    store.update(cx, |store, cx| {
        store.set_externally_running(acp::SessionId::new(session_id.as_str()), running, cx)
    });
    if !running {
        return;
    }
    let known = store
        .read(cx)
        .entry_by_session(&acp::SessionId::new(session_id.as_str()))
        .is_some();
    if known {
        unarchive(&session_id, cx);
        return;
    }
    // A session started elsewhere (e.g. in the TUI): add it, if it's a
    // top-level session.
    let client = hub.client().clone();
    cx.spawn(async move |cx| {
        let session = client.session(&session_id).await?;
        if session.parent_id.is_some() {
            return anyhow::Ok(());
        }
        cx.update(|cx| {
            let store = ThreadMetadataStore::global(cx);
            if store
                .read(cx)
                .entry_by_session(&acp::SessionId::new(session_id.as_str()))
                .is_none()
                && let Some(row) = row(&session, false)
            {
                store.update(cx, |store, cx| store.save(row, cx));
            }
        });
        Ok(())
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_sessions_nobody_used() {
        let session = |value: serde_json::Value| {
            serde_json::from_value::<SessionInfo>(value).expect("valid session")
        };
        assert!(is_unused(&session(serde_json::json!({
            "id": "ses_1", "time": {"created": 1, "updated": 1}
        }))));
        // Prompted, before its title arrived.
        assert!(!is_unused(&session(serde_json::json!({
            "id": "ses_2", "time": {"created": 1, "busy": 2}
        }))));
        // Old sessions have no `busy` time.
        assert!(!is_unused(&session(serde_json::json!({
            "id": "ses_3", "title": "Greeting", "time": {"created": 1}
        }))));
    }
}
