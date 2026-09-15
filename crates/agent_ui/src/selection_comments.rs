use std::cell::RefCell;
use std::ops::Range;
use std::rc::{Rc, Weak};

use editor::{
    Anchor, Editor, EditorEvent, MultiBufferOffset, SelectionCommentCallback,
    SelectionCommentEvent, ToOffset as _,
};
use feature_flags::{FeatureFlagAppExt as _, SelectionCommentFeatureFlag};
use gpui::{App, Context, Entity, Subscription, WeakEntity, Window};
use settings::Settings as _;
use workspace::Workspace;
use zed_actions::agent::AddSelectionCommentToThread;

use crate::agent_panel::AgentPanel;
use crate::completion_provider::AgentContextSelection;
use crate::conversation_view::ConversationView;

pub fn selection_comment(
    workspace: &mut Workspace,
    _: &AddSelectionCommentToThread,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    log::debug!("AddSelectionCommentToThread invoked");
    if !cx.has_flag::<SelectionCommentFeatureFlag>() {
        log::warn!("AddSelectionCommentToThread ignored: selection-comment flag is off");
        return;
    }

    let Some(active_editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        log::debug!("AddSelectionCommentToThread ignored: no active editor");
        return;
    };

    // Both the visible marker and the queued pill derive from this one
    // newest selection, snapshotted now so later cursor moves cannot
    // redirect the queued content.
    let newest_selection = active_editor.read(cx).selections.newest_anchor();
    let anchor_range: Range<Anchor> = newest_selection.start..newest_selection.end;
    let queued_ranges = active_editor
        .read(cx)
        .selection_comment_queue_ranges(&anchor_range, cx);
    if queued_ranges.is_empty() {
        log::debug!("AddSelectionCommentToThread ignored: selection resolves to no quotable lines");
        return;
    }
    let queued_selection = AgentContextSelection::Editor(queued_ranges);
    let use_modifier_to_send = agent_settings::AgentSettings::get_global(cx).use_modifier_to_send;

    let callback = comment_callback(
        cx.entity().downgrade(),
        active_editor.downgrade(),
        queued_selection,
        |workspace, cx| {
            workspace
                .panel::<AgentPanel>(cx)
                .and_then(|panel| panel.read(cx).active_conversation_view().cloned())
        },
    );

    active_editor.update(cx, |editor, cx| {
        editor.show_selection_comment(anchor_range, callback, use_modifier_to_send, window, cx);
    });
}

/// Where a queued comment sits in the thread draft. The draft stays the
/// editable source of truth, and the marker in the source editor mirrors it:
/// removing the marker takes this range out of the draft, and editing or
/// deleting the comment in the draft updates or removes the marker.
struct DraftEntry {
    draft_editor: WeakEntity<Editor>,
    /// Covers the selection pill and the prose, so removing the marker removes
    /// both. The start sticks to the right so text typed right before the
    /// pill stays out, and the end sticks to the left so text typed after the
    /// comment does too.
    range: Range<Anchor>,
    /// Starts after the pill. The marker shows only the text from here on.
    comment_start: Anchor,
    _draft_subscription: Subscription,
}

type SharedDraftEntry = Rc<RefCell<Option<DraftEntry>>>;

/// Anchors for a comment that was just written into a draft, before it is
/// linked to a marker.
pub(crate) struct QueuedComment {
    draft_editor: Entity<Editor>,
    range: Range<Anchor>,
    comment_start: Anchor,
}

impl DraftEntry {
    /// The comment as it currently reads in the draft, or `None` once the
    /// entry no longer matches what its marker claims: either no prose is left
    /// or the selection pill was deleted, so the agent would no longer get the
    /// quoted code. Any prose that remains stays in the draft as plain text.
    fn comment_text(&self, cx: &App) -> Option<String> {
        let draft_editor = self.draft_editor.upgrade()?;
        let snapshot = draft_editor.read(cx).buffer().read(cx).snapshot(cx);
        let pill_start = self.range.start.to_offset(&snapshot);
        let start = self.comment_start.to_offset(&snapshot);
        let end = self.range.end.to_offset(&snapshot);
        if end <= start {
            return None;
        }
        let pill_removed = start <= pill_start
            || snapshot
                .text_for_range(pill_start..start)
                .all(|chunk| chunk.trim().is_empty());
        if pill_removed {
            return None;
        }
        let text: String = snapshot.text_for_range(start..end).collect();
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    /// Rewrites the comment prose in place. Returns false when the draft is
    /// gone, so the caller can queue the comment again somewhere live.
    fn replace_comment(&mut self, comment_text: &str, window: &mut Window, cx: &mut App) -> bool {
        let Some(draft_editor) = self.draft_editor.upgrade() else {
            return false;
        };
        let replacement = format!("\n{comment_text}\n");
        let new_bounds = draft_editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let start = self.comment_start.to_offset(&snapshot);
            let end = self.range.end.to_offset(&snapshot).max(start);
            editor.transact(window, cx, |editor, _window, cx| {
                editor.edit([(start..end, replacement.clone())], cx);
            });
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            (
                snapshot.anchor_before(start),
                snapshot.anchor_before(MultiBufferOffset(start.0 + replacement.len())),
            )
        });
        self.comment_start = new_bounds.0;
        self.range.end = new_bounds.1;
        true
    }

    fn remove_from_draft(&self, window: &mut Window, cx: &mut App) {
        let Some(draft_editor) = self.draft_editor.upgrade() else {
            return;
        };
        draft_editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let start = self.range.start.to_offset(&snapshot);
            let end = self.range.end.to_offset(&snapshot);
            if start < end {
                editor.transact(window, cx, |editor, _window, cx| {
                    editor.edit([(start..end, "")], cx);
                });
            }
        });
    }
}

struct NoThreadToast;

/// Builds the callback for one marker. It owns the link between that marker
/// and its place in the draft; when the marker goes away the callback is
/// dropped with it, which also ends the draft subscription.
pub(crate) fn comment_callback(
    workspace: WeakEntity<Workspace>,
    source_editor: WeakEntity<Editor>,
    queued_selection: AgentContextSelection,
    resolve_conversation_view: impl Fn(&Workspace, &App) -> Option<Entity<ConversationView>>
    + Clone
    + 'static,
) -> SelectionCommentCallback {
    let draft_entry: SharedDraftEntry = Rc::default();
    Rc::new(
        move |overlay_id: usize,
              event: SelectionCommentEvent,
              window: &mut Window,
              cx: &mut App| match event {
            SelectionCommentEvent::Confirmed(comment_text) => {
                let Some(workspace) = workspace.upgrade() else {
                    log::warn!(
                        "Selection comment confirmed but the workspace is gone; nothing queued"
                    );
                    return;
                };
                let draft_entry = draft_entry.clone();
                let source_editor = source_editor.clone();
                let queued_selection = queued_selection.clone();
                let resolve_conversation_view = resolve_conversation_view.clone();
                // The draft mutation runs deferred: this callback fires while
                // the source editor is mid-update, and the draft sync that
                // follows the mutation updates that editor again.
                workspace.update(cx, |_workspace, cx| {
                    cx.defer_in(window, move |workspace, window, cx| {
                        if replace_queued_comment(&draft_entry, &comment_text, window, cx) {
                            return;
                        }
                        let queued = resolve_conversation_view(workspace, cx).and_then(
                            |conversation_view| {
                                queue_selection_comment(
                                    &conversation_view,
                                    queued_selection,
                                    comment_text,
                                    window,
                                    cx,
                                )
                            },
                        );
                        match queued {
                            Some(queued) => {
                                link_to_draft(&draft_entry, queued, source_editor, overlay_id, cx);
                            }
                            None => {
                                workspace.show_toast(
                                    workspace::Toast::new(
                                        workspace::notifications::NotificationId::unique::<
                                            NoThreadToast,
                                        >(),
                                        "Open an agent thread to queue this comment. Your text is kept.",
                                    )
                                    .autohide(),
                                    cx,
                                );
                                source_editor
                                    .update(cx, |editor, cx| {
                                        editor.reject_selection_comment(overlay_id, window, cx);
                                    })
                                    .ok();
                            }
                        }
                    });
                });
            }
            SelectionCommentEvent::Removed => {
                let entry = draft_entry.borrow_mut().take();
                if let Some(entry) = entry {
                    entry.remove_from_draft(window, cx);
                }
            }
        },
    )
}

/// Rewrites an already queued comment in the draft instead of appending a
/// second copy when a marker is edited and confirmed again.
fn replace_queued_comment(
    draft_entry: &SharedDraftEntry,
    comment_text: &str,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let mut slot = draft_entry.borrow_mut();
    let Some(entry) = slot.as_mut() else {
        return false;
    };
    if entry.replace_comment(comment_text, window, cx) {
        return true;
    }
    *slot = None;
    false
}

fn link_to_draft(
    draft_entry: &SharedDraftEntry,
    queued: QueuedComment,
    source_editor: WeakEntity<Editor>,
    overlay_id: usize,
    cx: &mut App,
) {
    // Weak so the entry, which owns this subscription, can be dropped by the
    // marker's callback without the subscription keeping it alive.
    let weak_entry = Rc::downgrade(draft_entry);
    let draft_subscription = cx.subscribe(
        &queued.draft_editor,
        move |_draft_editor: Entity<Editor>, event: &EditorEvent, cx: &mut App| {
            if matches!(event, EditorEvent::BufferEdited) {
                sync_marker_from_draft(&weak_entry, &source_editor, overlay_id, cx);
            }
        },
    );
    *draft_entry.borrow_mut() = Some(DraftEntry {
        draft_editor: queued.draft_editor.downgrade(),
        range: queued.range,
        comment_start: queued.comment_start,
        _draft_subscription: draft_subscription,
    });
}

fn sync_marker_from_draft(
    draft_entry: &Weak<RefCell<Option<DraftEntry>>>,
    source_editor: &WeakEntity<Editor>,
    overlay_id: usize,
    cx: &mut App,
) {
    let Some(draft_entry) = draft_entry.upgrade() else {
        return;
    };
    let comment_text = match draft_entry.borrow().as_ref() {
        Some(entry) => entry.comment_text(cx),
        None => return,
    };
    if comment_text.is_none() {
        draft_entry.borrow_mut().take();
    }
    source_editor
        .update(cx, |editor, cx| {
            editor.sync_selection_comment(overlay_id, comment_text, cx);
        })
        .ok();
}

/// Appends one queued selection pill plus the comment prose to the thread
/// draft. Kept panel-independent so the bridging is directly testable; the
/// action handler above only resolves which view to target. Returns where the
/// comment landed, or `None` when it could not be written into a draft.
pub(crate) fn queue_selection_comment(
    conversation_view: &Entity<ConversationView>,
    selection: AgentContextSelection,
    comment_text: String,
    window: &mut Window,
    cx: &mut App,
) -> Option<QueuedComment> {
    conversation_view.update(cx, |conversation_view, cx| {
        // Without a live thread `insert_selection` would park the pill for
        // later and drop the comment text, so refuse before touching anything.
        let message_editor = conversation_view
            .active_thread()
            .map(|thread| thread.read(cx).active_editor(cx))?;
        let draft_editor = message_editor.read(cx).editor().clone();
        let start = draft_cursor_offset(&draft_editor, cx);

        conversation_view.insert_selection(selection, window, cx);

        let comment = format!("\n{comment_text}\n");
        message_editor.update(cx, |message_editor, cx| {
            message_editor.insert_text(&comment, window, cx);
        });

        let end = draft_cursor_offset(&draft_editor, cx);
        let comment_start = MultiBufferOffset(end.0.checked_sub(comment.len())?);
        if comment_start < start {
            return None;
        }
        let snapshot = draft_editor.read(cx).buffer().read(cx).snapshot(cx);
        Some(QueuedComment {
            range: snapshot.anchor_after(start)..snapshot.anchor_before(end),
            comment_start: snapshot.anchor_before(comment_start),
            draft_editor,
        })
    })
}

fn draft_cursor_offset(draft_editor: &Entity<Editor>, cx: &App) -> MultiBufferOffset {
    let editor = draft_editor.read(cx);
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    editor
        .selections
        .newest_anchor()
        .head()
        .to_offset(&snapshot)
}
