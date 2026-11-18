//! Keeps Zed's thread history in step with the OpenCode service, without the
//! user importing anything and whether or not they use OpenCode D.
//!
//! Archive state is shared in both directions. Running sessions return to the
//! sidebar; which ones run comes from the connection, which asks the server
//! on every connect.

use crate::client::OpenCodeClient;
use crate::connection::AGENT_ID;
use crate::types::SessionInfo;
use acp_thread::AgentSessionInfo;
use agent_client_protocol::v2::schema as acp;
use agent_ui::thread_metadata_store::{
    ThreadId, ThreadMetadata, ThreadMetadataStore, ThreadMetadataStoreEvent, WorktreePaths,
};
use anyhow::Result;
use chrono::Utc;
use gpui::{App, AppContext as _, AsyncApp, SharedString, TaskExt as _};
use project::AgentId;
use std::path::PathBuf;
use util::ResultExt as _;
use util::path_list::PathList;

const MAX_SYNCED_SESSIONS: usize = 2000;

pub(crate) fn start(client: OpenCodeClient, cx: &mut App) {
    let store = ThreadMetadataStore::global(cx);
    cx.subscribe(&store, {
        let client = client.clone();
        move |store, event, cx| {
            let (thread_id, archived) = match event {
                ThreadMetadataStoreEvent::ThreadArchived(thread_id) => (thread_id, true),
                ThreadMetadataStoreEvent::ThreadUnarchived(thread_id) => (thread_id, false),
            };
            let session_id = store
                .read(cx)
                .entry(*thread_id)
                .filter(|entry| entry.agent_id == AgentId::new(AGENT_ID))
                .and_then(|entry| entry.session_id.clone());
            if let Some(session_id) = session_id {
                let client = client.clone();
                cx.background_spawn(
                    async move { client.set_archived(&session_id.0, archived).await },
                )
                .detach_and_log_err(cx);
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| sync_all(&client, cx).await.log_err())
        .detach();
}

async fn sync_all(client: &OpenCodeClient, cx: &mut AsyncApp) -> Result<()> {
    let store = cx.update(|cx| ThreadMetadataStore::global(cx));
    let reload = cx.update(|cx| store.read(cx).reload_task());
    reload.await;

    let sessions = client.list_all_sessions(MAX_SYNCED_SESSIONS).await?;

    cx.update(|cx| {
        let (rows, unused_rows): (Vec<ThreadMetadata>, Vec<ThreadId>) = {
            let store = store.read(cx);
            // Rows earlier syncs added before unused sessions were skipped.
            let unused_rows = sessions
                .iter()
                .filter(|session| is_unused(session))
                .filter_map(|session| row_for(store, &session.id))
                .filter(|entry| entry.archived && entry.title_override.is_none())
                .map(|entry| entry.thread_id)
                .collect();
            let rows = sessions
                .iter()
                .filter(|session| !is_unused(session))
                .filter(|session| row_for(store, &session.id).is_none())
                .filter_map(|session| row(session, session.time.archived.is_some()))
                .collect();
            (rows, unused_rows)
        };
        log::info!(
            "synced {} OpenCode sessions into thread history",
            rows.len()
        );
        // Rows added before their session had a title (or renamed or moved
        // while Zed was closed) catch up here.
        for session in &sessions {
            if !is_unused(session) {
                if session.time.archived.is_some() {
                    archive_history_row(&session.id, cx);
                } else {
                    unarchive_history_row(&session.id, cx);
                }
            }
            if let Some(title) = &session.title {
                rename_history_row(&session.id, title, cx);
            }
            if let Some(location) = &session.location {
                move_history_row(&session.id, &location.directory, cx);
            }
        }
        store.update(cx, |store, cx| {
            store.delete_all(unused_rows, cx);
            store.save_all(rows, cx);
        });
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
pub(crate) fn rename_history_row(session_id: &str, title: &str, cx: &mut App) {
    let store = ThreadMetadataStore::global(cx);
    let thread_id = row_for(store.read(cx), session_id)
        .filter(|entry| entry.title_override.is_none())
        .filter(|entry| entry.title.as_deref() != Some(title))
        .map(|entry| entry.thread_id);
    if let Some(thread_id) = thread_id {
        store.update(cx, |store, cx| {
            store.set_generated_title(thread_id, title.to_string().into(), cx)
        });
    }
}

/// Moves an OpenCode D row to the folder its session moved to, so the sidebar
/// lists it under that project.
pub(crate) fn move_history_row(session_id: &str, directory: &str, cx: &mut App) {
    let store = ThreadMetadataStore::global(cx);
    let folder_paths = PathList::new(&[PathBuf::from(directory)]);
    let moved = row_for(store.read(cx), session_id)
        .filter(|entry| entry.folder_paths() != &folder_paths)
        .map(|entry| ThreadMetadata {
            worktree_paths: WorktreePaths::from_folder_paths(&folder_paths),
            ..entry.clone()
        });
    if let Some(moved) = moved {
        store.update(cx, |store, cx| store.save(moved, cx));
    }
}

pub(crate) fn unarchive_history_row(session_id: &str, cx: &mut App) {
    let store = ThreadMetadataStore::global(cx);
    let thread_id = row_for(store.read(cx), session_id)
        .filter(|entry| entry.archived)
        .map(|entry| entry.thread_id);
    if let Some(thread_id) = thread_id {
        store.update(cx, |store, cx| store.unarchive(thread_id, cx));
    }
}

pub(crate) fn archive_history_row(session_id: &str, cx: &mut App) {
    let store = ThreadMetadataStore::global(cx);
    let thread_id = row_for(store.read(cx), session_id)
        .filter(|entry| !entry.archived)
        .map(|entry| entry.thread_id);
    if let Some(thread_id) = thread_id {
        store.update(cx, |store, cx| store.archive(thread_id, None, cx));
    }
}

pub(crate) fn delete_history_row(session_id: &str, cx: &mut App) {
    let store = ThreadMetadataStore::global(cx);
    let thread_id = row_for(store.read(cx), session_id).map(|entry| entry.thread_id);
    if let Some(thread_id) = thread_id {
        store.update(cx, |store, cx| store.delete(thread_id, cx));
    }
}

pub(crate) fn mark_session_running(
    client: &OpenCodeClient,
    session_id: &str,
    running: bool,
    cx: &mut App,
) {
    let store = ThreadMetadataStore::global(cx);
    store.update(cx, |store, cx| {
        store.set_externally_running(acp::SessionId::new(session_id), running, cx)
    });
    if !running {
        return;
    }
    let known = row_for(store.read(cx), session_id).is_some();
    if known {
        unarchive_history_row(session_id, cx);
        return;
    }
    // A session started elsewhere (e.g. in the TUI): add it, if it's a
    // top-level session.
    let client = client.clone();
    let session_id = session_id.to_string();
    cx.spawn(async move |cx| {
        let session = client.fetch_session(&session_id).await?;
        if session.parent_id.is_some() {
            return anyhow::Ok(());
        }
        cx.update(|cx| {
            let store = ThreadMetadataStore::global(cx);
            // A sync may have filed the session away meanwhile.
            if row_for(store.read(cx), &session_id).is_some() {
                unarchive_history_row(&session_id, cx);
            } else if let Some(row) = row(&session, false) {
                store.update(cx, |store, cx| store.save(row, cx));
            }
        });
        Ok(())
    })
    .detach_and_log_err(cx);
}

/// The OpenCode D row for `session_id`. Another agent's thread can have the
/// same session, through OpenCode's own ACP agent.
fn row_for<'a>(store: &'a ThreadMetadataStore, session_id: &str) -> Option<&'a ThreadMetadata> {
    store
        .entry_by_session(&acp::SessionId::new(session_id))
        .filter(|entry| entry.agent_id == AgentId::new(AGENT_ID))
}

pub(crate) fn session_list_info(session: &SessionInfo) -> AgentSessionInfo {
    let mut info = AgentSessionInfo::new(acp::SessionId::new(session.id.as_str()));
    info.title = session.title.clone().map(SharedString::from);
    info.work_dirs = session
        .location
        .as_ref()
        .map(|location| PathList::new(&[PathBuf::from(&location.directory)]));
    info.created_at = session
        .time
        .created
        .and_then(chrono::DateTime::from_timestamp_millis);
    info.updated_at = session
        .time
        .updated
        .or(session.time.created)
        .and_then(chrono::DateTime::from_timestamp_millis);
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::thread_sync_tests::Harness;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn initial_sync_matches_server_archive_state(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve("ses_archived", |session| session.archived = Some(1));
        harness.serve("ses_listed", |_| {});
        harness.serve("ses_unused", |session| session.unused = true);
        let client = harness.connection.client().clone();
        cx.spawn(async move |mut cx| sync_all(&client, &mut cx).await)
            .await
            .expect("history sync");

        cx.update(|cx| {
            let store = ThreadMetadataStore::global(cx);
            let store = store.read(cx);
            assert!(
                row_for(store, "ses_archived")
                    .expect("archived row")
                    .archived
            );
            assert!(!row_for(store, "ses_listed").expect("listed row").archived);
            assert!(row_for(store, "ses_unused").is_none());
        });
    }

    #[gpui::test]
    async fn sync_reconciles_existing_rows_in_both_directions(cx: &mut TestAppContext) {
        let harness = Harness::new(cx).await;
        harness.serve("ses_archived", |session| session.archived = Some(1));
        harness.serve("ses_listed", |_| {});
        let client = harness.connection.client().clone();
        cx.spawn(async move |mut cx| sync_all(&client, &mut cx).await)
            .await
            .expect("history sync");
        cx.update(|cx| {
            let store = ThreadMetadataStore::global(cx);
            let rows = store
                .read(cx)
                .entries()
                .cloned()
                .map(|row| ThreadMetadata {
                    archived: !row.archived,
                    ..row
                })
                .collect();
            store.update(cx, |store, cx| store.save_all(rows, cx));
        });
        let client = harness.connection.client().clone();
        cx.spawn(async move |mut cx| sync_all(&client, &mut cx).await)
            .await
            .expect("history sync");

        cx.update(|cx| {
            let store = ThreadMetadataStore::global(cx);
            let store = store.read(cx);
            assert_eq!(store.entries().count(), 2);
            assert!(
                row_for(store, "ses_archived")
                    .expect("archived row")
                    .archived
            );
            assert!(!row_for(store, "ses_listed").expect("listed row").archived);
        });
    }

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
