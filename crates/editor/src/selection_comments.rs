use super::*;
use text::{Anchor as TextAnchor, ToOffset as _};

/// What happened to a comment marker that its owner may need to mirror
/// elsewhere, such as in the agent draft.
pub enum SelectionCommentEvent {
    /// The comment was confirmed or re-confirmed with this text.
    Confirmed(String),
    /// The user deleted the marker of a comment that had been queued.
    Removed,
}

/// Called with the id of the affected comment, which the owner can later pass
/// to [`Editor::sync_selection_comment`]. It runs while the source editor is
/// being updated, so it must not update that editor synchronously.
pub type SelectionCommentCallback = Rc<dyn Fn(usize, SelectionCommentEvent, &mut Window, &mut App)>;

pub(crate) struct SelectionCommentOverlay {
    pub(crate) id: usize,
    pub(crate) anchor_range: Range<Anchor>,
    pub(crate) block_id: CustomBlockId,
    pub(crate) prompt_editor: Entity<Editor>,
    pub(crate) confirmed: bool,
    /// True while the comment's content lives on in the owner's queue. It
    /// stays true while a confirmed comment is being edited, so cancelling
    /// the edit can fall back to the marker instead of orphaning that content.
    pub(crate) queued: bool,
    pub(crate) comment_text: String,
    pub(crate) on_confirmed: SelectionCommentCallback,
    _prompt_subscriptions: Vec<Subscription>,
}

impl Editor {
    /// Resolves the queued content ranges for a selection comment marker.
    /// Both the marker anchor and these ranges derive from the same newest
    /// selection, so the pill always quotes the commented lines.
    pub fn selection_comment_queue_ranges(
        &self,
        anchor_range: &Range<Anchor>,
        cx: &App,
    ) -> Vec<(Entity<Buffer>, Range<TextAnchor>)> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let anchor_range = expand_empty_selection_comment_range(&snapshot, anchor_range.clone());
        let multi_buffer = self.buffer.read(cx);
        let Some((start_buffer, start)) =
            multi_buffer.text_anchor_for_position(anchor_range.start, cx)
        else {
            return Vec::new();
        };
        let Some((end_buffer, end)) = multi_buffer.text_anchor_for_position(anchor_range.end, cx)
        else {
            return Vec::new();
        };
        if start_buffer != end_buffer {
            return Vec::new();
        }
        let buffer_snapshot = start_buffer.read(cx).snapshot();
        if start.to_offset(&buffer_snapshot) == end.to_offset(&buffer_snapshot) {
            return Vec::new();
        }
        vec![(start_buffer, start..end)]
    }

    pub fn show_selection_comment(
        &mut self,
        anchor_range: Range<Anchor>,
        on_confirmed: SelectionCommentCallback,
        use_modifier_to_send: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let anchor_range = expand_empty_selection_comment_range(&snapshot, anchor_range);

        // Re-invoking on an already commented range reopens that comment for
        // editing instead of stacking a duplicate marker and queue entry. An
        // unconfirmed input keeps its draft, so only focus it. Ranges compare
        // by resolved points in this snapshot because two anchors in different
        // excerpts can only match when they address the same visible lines.
        if let Some(existing) = self.selection_comment_overlays.iter().find(|overlay| {
            overlay.anchor_range.start.to_point(&snapshot) == anchor_range.start.to_point(&snapshot)
                && overlay.anchor_range.end.to_point(&snapshot)
                    == anchor_range.end.to_point(&snapshot)
        }) {
            let existing_id = existing.id;
            let confirmed = existing.confirmed;
            if confirmed {
                self.edit_selection_comment(existing_id, window, cx);
            } else {
                let focus_handle = existing.prompt_editor.focus_handle(cx);
                window.focus(&focus_handle, cx);
            }
            return;
        }

        let overlay_id = self.next_selection_comment_id;
        self.next_selection_comment_id += 1;

        let prompt_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 5, window, cx);
            editor.set_placeholder_text("Comment on selection", window, cx);
            editor.selection_comment_input = true;
            editor.selection_comment_modifier_send = use_modifier_to_send;
            editor
        });

        // The block tracks the input height as the comment grows; the editor
        // itself scrolls past its maximum. Confirm and cancel are handled on
        // the prompt entity itself so they work wherever focus sits inside
        // the input, without relying on action bubbling to the parent.
        let parent_editor = cx.entity().downgrade();
        let height_subscription = cx.subscribe_in(
            &prompt_editor,
            window,
            move |parent: &mut Editor,
                  _prompt: &Entity<Editor>,
                  event: &EditorEvent,
                  _window: &mut Window,
                  cx: &mut Context<Editor>| {
                if matches!(event, EditorEvent::BufferEdited) {
                    parent.refresh_selection_comment_overlay(overlay_id, cx);
                }
            },
        );
        let (confirm_subscription, cancel_subscription) = prompt_editor.update(cx, {
            move |prompt_editor, _cx| {
                let confirm_parent = parent_editor.clone();
                let confirm_subscription = prompt_editor.register_action(
                    move |_: &ConfirmSelectionComment, window, cx| {
                        if let Some(parent) = confirm_parent.upgrade() {
                            parent.update(cx, |parent, cx| {
                                parent.confirm_selection_comment(window, cx);
                            });
                        }
                    },
                );
                let cancel_parent = parent_editor;
                let cancel_subscription =
                    prompt_editor.register_action(move |_: &CancelSelectionComment, window, cx| {
                        if let Some(parent) = cancel_parent.upgrade() {
                            parent.update(cx, |parent, cx| {
                                parent.cancel_focused_selection_comment(window, cx);
                            });
                        }
                    });
                (confirm_subscription, cancel_subscription)
            }
        });

        let anchor = anchor_range.end;
        let editor_handle = cx.entity().downgrade();
        let block = BlockProperties {
            style: BlockStyle::Sticky,
            placement: BlockPlacement::Below(anchor),
            height: Some(2),
            render: Arc::new(move |cx| {
                Self::render_selection_comment_overlay(overlay_id, &editor_handle, cx)
            }),
            priority: 0,
        };

        let block_ids = self.insert_blocks([block], None, cx);
        let Some(block_id) = block_ids.into_iter().next() else {
            log::error!("Failed to insert selection comment block");
            return;
        };
        log::debug!("Selection comment overlay shown");

        self.selection_comment_overlays
            .push(SelectionCommentOverlay {
                id: overlay_id,
                anchor_range,
                block_id,
                prompt_editor: prompt_editor.clone(),
                confirmed: false,
                queued: false,
                comment_text: String::new(),
                on_confirmed,
                _prompt_subscriptions: vec![
                    height_subscription,
                    confirm_subscription,
                    cancel_subscription,
                ],
            });
        self.refresh_selection_comment_overlay(overlay_id, cx);

        window.focus(&prompt_editor.focus_handle(cx), cx);
        cx.notify();
    }

    /// Confirms the focused unconfirmed comment, queueing it while keeping
    /// the marker. Driven by the prompt input in production; public for
    /// cross-crate tests.
    pub fn confirm_selection_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let overlay_index = self.selection_comment_overlays.iter().position(|overlay| {
            !overlay.confirmed && overlay.prompt_editor.focus_handle(cx).is_focused(window)
        });
        let Some(overlay_index) = overlay_index else {
            return;
        };
        let overlay_id = self.selection_comment_overlays[overlay_index].id;
        self.confirm_selection_comment_by_id(overlay_id, window, cx);
    }

    pub(super) fn confirm_selection_comment_by_id(
        &mut self,
        overlay_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let overlay_index = self
            .selection_comment_overlays
            .iter()
            .position(|overlay| overlay.id == overlay_id && !overlay.confirmed);
        let Some(overlay_index) = overlay_index else {
            return;
        };

        let comment_text = self.selection_comment_overlays[overlay_index]
            .prompt_editor
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if comment_text.is_empty() {
            return;
        }

        let callback = self.selection_comment_overlays[overlay_index]
            .on_confirmed
            .clone();
        {
            let overlay = &mut self.selection_comment_overlays[overlay_index];
            overlay.confirmed = true;
            overlay.queued = true;
            overlay.comment_text = comment_text.clone();
        }

        self.refresh_selection_comment_overlay(overlay_id, cx);
        window.focus(&self.focus_handle, cx);
        cx.notify();

        // The workspace update below touches different entities than the
        // source editor borrowed here; the draft mutation itself runs
        // deferred, after this update completes.
        callback(
            overlay_id,
            SelectionCommentEvent::Confirmed(comment_text),
            window,
            cx,
        );
    }

    pub(super) fn cancel_selection_comment_by_id(
        &mut self,
        overlay_id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(overlay_index) = self
            .selection_comment_overlays
            .iter()
            .position(|overlay| overlay.id == overlay_id && !overlay.confirmed)
        else {
            return;
        };
        self.dismiss_selection_comment_input(overlay_index, window, cx);
    }

    pub(crate) fn cancel_focused_selection_comment(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let overlay_index = self.selection_comment_overlays.iter().position(|overlay| {
            !overlay.confirmed && overlay.prompt_editor.focus_handle(cx).is_focused(window)
        });
        let Some(overlay_index) = overlay_index else {
            return false;
        };
        self.dismiss_selection_comment_input(overlay_index, window, cx);
        true
    }

    // Cancelling the edit of an already queued comment returns to its marker.
    // Removing the marker instead would leave the queued content with nothing
    // on screen pointing at it.
    fn dismiss_selection_comment_input(
        &mut self,
        overlay_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selection_comment_overlays[overlay_index].queued {
            let overlay = &mut self.selection_comment_overlays[overlay_index];
            overlay.confirmed = true;
            let overlay_id = overlay.id;
            self.refresh_selection_comment_overlay(overlay_id, cx);
        } else {
            let overlay = self.selection_comment_overlays.remove(overlay_index);
            self.remove_blocks(HashSet::from_iter([overlay.block_id]), None, cx);
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Removes a marker. If its comment was queued, the owner is told so it
    /// can take the queued content back out as well.
    pub fn delete_selection_comment(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(overlay_index) = self
            .selection_comment_overlays
            .iter()
            .position(|overlay| overlay.id == id)
        else {
            return false;
        };

        let overlay = self.selection_comment_overlays.remove(overlay_index);
        self.remove_blocks(HashSet::from_iter([overlay.block_id]), None, cx);
        cx.notify();
        if overlay.queued {
            (overlay.on_confirmed)(id, SelectionCommentEvent::Removed, window, cx);
        }
        true
    }

    /// Turns a comment whose owner could not queue it back into an editable
    /// input with the typed text intact, so a marker never claims content was
    /// queued when it was not. Like [`Self::sync_selection_comment`], this
    /// never reports back through the callback.
    pub fn reject_selection_comment(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(overlay) = self
            .selection_comment_overlays
            .iter_mut()
            .find(|overlay| overlay.id == id)
        else {
            return;
        };
        overlay.confirmed = false;
        overlay.queued = false;
        let focus_handle = overlay.prompt_editor.focus_handle(cx);
        self.refresh_selection_comment_overlay(id, cx);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    /// Mirrors a change made to the queued content back onto its marker.
    /// `Some` replaces the marker text; `None` means the queued content is
    /// gone, which removes a confirmed marker. A comment that is mid-edit
    /// keeps its input open so typing is never discarded, but no longer counts
    /// as queued. Never reports back through the callback.
    pub fn sync_selection_comment(
        &mut self,
        id: usize,
        comment_text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(overlay_index) = self
            .selection_comment_overlays
            .iter()
            .position(|overlay| overlay.id == id)
        else {
            return;
        };

        match comment_text {
            Some(comment_text) => {
                let overlay = &mut self.selection_comment_overlays[overlay_index];
                if overlay.comment_text == comment_text {
                    return;
                }
                overlay.comment_text = comment_text;
                let confirmed = overlay.confirmed;
                if confirmed {
                    self.refresh_selection_comment_overlay(id, cx);
                }
                cx.notify();
            }
            None => {
                let overlay = &mut self.selection_comment_overlays[overlay_index];
                overlay.queued = false;
                if overlay.confirmed {
                    let overlay = self.selection_comment_overlays.remove(overlay_index);
                    self.remove_blocks(HashSet::from_iter([overlay.block_id]), None, cx);
                    cx.notify();
                }
            }
        }
    }

    pub(crate) fn edit_selection_comment(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(overlay) = self
            .selection_comment_overlays
            .iter_mut()
            .find(|overlay| overlay.id == id)
        else {
            return false;
        };

        // Restore the queued text explicitly instead of relying on the input
        // never having been cleared. Unconfirmed inputs keep their live
        // draft, so only refocus those.
        if !overlay.confirmed {
            let focus_handle = overlay.prompt_editor.focus_handle(cx);
            window.focus(&focus_handle, cx);
            return true;
        }
        let comment_text = SharedString::from(overlay.comment_text.clone());
        let focus_handle = overlay.prompt_editor.focus_handle(cx);
        overlay.confirmed = false;
        overlay.prompt_editor.update(cx, |prompt_editor, cx| {
            prompt_editor.set_text(comment_text, window, cx);
        });
        self.refresh_selection_comment_overlay(id, cx);
        window.focus(&focus_handle, cx);
        cx.notify();
        true
    }

    pub fn clear_selection_comments(&mut self, cx: &mut Context<Self>) {
        if self.selection_comment_overlays.is_empty() {
            return;
        }
        let block_ids: HashSet<_> = self
            .selection_comment_overlays
            .drain(..)
            .map(|overlay| overlay.block_id)
            .collect();
        self.remove_blocks(block_ids, None, cx);
        cx.notify();
    }

    /// Returns the prompt editor of the first unconfirmed overlay, if any.
    /// This is primarily used for testing.
    pub fn selection_comment_prompt_editor(&self) -> Option<Entity<Editor>> {
        self.selection_comment_overlays
            .iter()
            .find(|overlay| !overlay.confirmed)
            .map(|overlay| overlay.prompt_editor.clone())
    }

    /// Returns the ids of all selection comment markers, in creation order.
    pub fn selection_comment_ids(&self) -> Vec<usize> {
        self.selection_comment_overlays
            .iter()
            .map(|overlay| overlay.id)
            .collect()
    }

    /// Returns the text shown on each confirmed marker, in creation order.
    pub fn confirmed_selection_comment_texts(&self) -> Vec<String> {
        self.selection_comment_overlays
            .iter()
            .filter(|overlay| overlay.confirmed)
            .map(|overlay| overlay.comment_text.clone())
            .collect()
    }

    /// Returns how many selection comment markers are currently visible.
    pub fn selection_comment_count(&self) -> usize {
        self.selection_comment_overlays.len()
    }

    /// Returns how many of the visible markers already queued their comment.
    pub fn confirmed_selection_comment_count(&self) -> usize {
        self.selection_comment_overlays
            .iter()
            .filter(|overlay| overlay.confirmed)
            .count()
    }

    fn refresh_selection_comment_overlay(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(overlay) = self
            .selection_comment_overlays
            .iter()
            .find(|overlay| overlay.id == id)
        else {
            return;
        };
        let block_id = overlay.block_id;
        // Unconfirmed inputs report their live line count so the block grows
        // while typing; confirmed markers use the stored snapshot instead.
        let text_lines = if overlay.confirmed {
            overlay.comment_text.lines().count().max(1) as u32
        } else {
            overlay
                .prompt_editor
                .read(cx)
                .text(cx)
                .lines()
                .count()
                .max(1) as u32
        };
        let new_height = 1 + text_lines.min(5);

        let mut heights = HashMap::default();
        heights.insert(block_id, new_height);
        self.resize_blocks(heights, None, cx);

        let editor_handle = cx.entity().downgrade();
        let render: Arc<dyn Fn(&mut BlockContext) -> AnyElement + Send + Sync> =
            Arc::new(move |cx| Self::render_selection_comment_overlay(id, &editor_handle, cx));
        let mut renderers = HashMap::default();
        renderers.insert(block_id, render);
        self.replace_blocks(renderers, None, cx);
    }

    fn render_selection_comment_overlay(
        id: usize,
        editor_handle: &WeakEntity<Editor>,
        cx: &mut BlockContext,
    ) -> AnyElement {
        let overlay_snapshot = editor_handle.upgrade().map(|editor| {
            let editor = editor.read(cx);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let overlay = editor
                .selection_comment_overlays
                .iter()
                .find(|overlay| overlay.id == id);
            let (confirmed, comment_text, prompt_editor, line_label) = overlay
                .map(|overlay| {
                    let start_point = overlay.anchor_range.start.to_point(&snapshot);
                    let end_point = overlay.anchor_range.end.to_point(&snapshot);
                    let label = if start_point.row == end_point.row {
                        format!("Line {}", start_point.row + 1)
                    } else {
                        format!("Lines {}-{}", start_point.row + 1, end_point.row + 1)
                    };
                    (
                        overlay.confirmed,
                        overlay.comment_text.clone(),
                        Some(overlay.prompt_editor.clone()),
                        label,
                    )
                })
                .unwrap_or((false, String::new(), None, String::new()));
            (confirmed, comment_text, prompt_editor, line_label)
        });

        let Some((confirmed, comment_text, prompt_editor, line_label)) = overlay_snapshot else {
            return gpui::Empty.into_any_element();
        };
        let Some(prompt_editor) = prompt_editor else {
            return gpui::Empty.into_any_element();
        };

        let overlay_id = id;
        let cancel_parent = editor_handle.clone();
        let confirm_parent = editor_handle.clone();
        if !confirmed {
            let actions = h_flex()
                .flex_shrink_0()
                .gap_1()
                .child(
                    IconButton::new(("selection-comment-cancel", overlay_id), IconName::Close)
                        .icon_color(ui::Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Cancel (Esc)"))
                        .on_click(move |_, window, cx| {
                            if let Some(parent) = cancel_parent.upgrade() {
                                parent.update(cx, |parent, cx| {
                                    parent.cancel_selection_comment_by_id(overlay_id, window, cx);
                                });
                            }
                        }),
                )
                .child(
                    IconButton::new(("selection-comment-confirm", overlay_id), IconName::Return)
                        .icon_color(ui::Color::Muted)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Queue comment"))
                        .on_click(move |_, window, cx| {
                            if let Some(parent) = confirm_parent.upgrade() {
                                parent.update(cx, |parent, cx| {
                                    parent.confirm_selection_comment_by_id(overlay_id, window, cx);
                                });
                            }
                        }),
                )
                .into_any_element();
            let body = EditorElement::new(
                &prompt_editor,
                EditorStyle {
                    background: cx.theme().system().transparent,
                    local_player: cx.editor_style.local_player,
                    text: cx.editor_style.text.clone(),
                    scrollbar_width: cx.editor_style.scrollbar_width,
                    syntax: cx.editor_style.syntax.clone(),
                    status: cx.editor_style.status.clone(),
                    ..EditorStyle::default()
                },
            )
            .into_any_element();
            let header = Label::new(line_label)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
            return Self::selection_comment_frame(header, body, actions, cx);
        }

        let edit_parent = editor_handle.clone();
        let delete_parent = editor_handle.clone();
        let header = h_flex()
            .w_full()
            .gap_1()
            .child(
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(ui::Color::Success),
            )
            .child(
                Label::new(line_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new("Queued for agent")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element();
        let body = Label::new(comment_text).into_any_element();
        let actions = h_flex()
            .flex_shrink_0()
            .gap_1()
            .child(
                IconButton::new(("selection-comment-edit", overlay_id), IconName::Pencil)
                    .icon_color(ui::Color::Muted)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Edit"))
                    .on_click(move |_, window, cx| {
                        if let Some(parent) = edit_parent.upgrade() {
                            parent.update(cx, |parent, cx| {
                                parent.edit_selection_comment(overlay_id, window, cx);
                            });
                        }
                    }),
            )
            .child(
                IconButton::new(("selection-comment-delete", overlay_id), IconName::Trash)
                    .icon_color(ui::Color::Muted)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Remove comment"))
                    .on_click(move |_, window, cx| {
                        if let Some(parent) = delete_parent.upgrade() {
                            parent.update(cx, |parent, cx| {
                                parent.delete_selection_comment(overlay_id, window, cx);
                            });
                        }
                    }),
            )
            .into_any_element();
        Self::selection_comment_frame(header, body, actions, cx)
    }

    /// The single shared container for both overlay states, so typing, grown,
    /// and confirmed markers always share one geometry. The width stays
    /// automatic: an explicit full width plus margins would overflow the block
    /// instead of insetting it. The body needs `min_w_0`: GPUI measures text
    /// without a wrap width while sizing flex items, so without it a long
    /// comment keeps its single-line width, spills past the border, and pushes
    /// the action buttons out of view.
    fn selection_comment_frame(
        header: AnyElement,
        body: AnyElement,
        actions: AnyElement,
        cx: &mut BlockContext,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        v_flex()
            .w(gpui::relative(0.6))
            .bg(colors.editor_background)
            .ml(cx.margins.gutter.full_width())
            .mr(px(24.))
            .px_2()
            .pb_2()
            .gap_1()
            .child(h_flex().w_full().px_2().child(header))
            .child(
                h_flex()
                    .w_full()
                    .items_stretch()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .bg(colors.editor_background)
                    .border_1()
                    .border_color(colors.border)
                    .child(div().w(px(3.)).rounded_full().bg(colors.text_accent))
                    .child(div().flex_1().min_w_0().px_2().py_1().child(body))
                    .child(actions),
            )
            .into_any_element()
    }
}

// An empty selection addresses the full line under the cursor, the same
// convention Add Selection To Thread uses when queueing.
fn expand_empty_selection_comment_range(
    snapshot: &MultiBufferSnapshot,
    anchor_range: Range<Anchor>,
) -> Range<Anchor> {
    let start_point = anchor_range.start.to_point(snapshot);
    let end_point = anchor_range.end.to_point(snapshot);
    if start_point == end_point {
        let row = MultiBufferRow(start_point.row);
        snapshot.anchor_before(Point::new(start_point.row, 0))
            ..snapshot.anchor_after(Point::new(start_point.row, snapshot.line_len(row)))
    } else {
        anchor_range
    }
}
