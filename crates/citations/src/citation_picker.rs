use std::sync::Arc;

use editor::Editor;
use fuzzy::{StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity, Window, actions, rems,
};
use picker::{Picker, PickerDelegate};
use ui::{Color, Label, LabelSize, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{BibEntry, Bibliography};

actions!(
    citations,
    [
        /// Search project bibliography files and insert a citation.
        InsertCitation,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action_renderer(|div, workspace, _window, cx| {
            let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
                return div;
            };
            let is_markdown = editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .is_some_and(|buffer| {
                    buffer
                        .read(cx)
                        .language()
                        .is_some_and(|language| language.name().as_ref() == "Markdown")
                });
            if !is_markdown {
                return div;
            }
            div.on_action(cx.listener(|workspace, _: &InsertCitation, window, cx| {
                CitationPicker::open(workspace, window, cx);
            }))
        });
    })
    .detach();
}

struct CitationPicker {
    picker: Entity<Picker<CitationPickerDelegate>>,
    _subscription: Subscription,
}

impl CitationPicker {
    fn open(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<()> {
        let editor = workspace.active_item_as::<Editor>(cx)?.downgrade();
        let project = workspace.project().clone();
        let bibliography = Bibliography::global(cx);
        Bibliography::ensure_project(&bibliography, &project, cx);
        workspace.toggle_modal(window, cx, move |window, cx| {
            let mut delegate = CitationPickerDelegate {
                modal: cx.entity().downgrade(),
                editor,
                entries: Vec::new(),
                candidates: Vec::new(),
                matches: Vec::new(),
                selected_index: 0,
                chosen: Vec::new(),
            };
            delegate.reload(bibliography.read(cx));
            let picker =
                cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(40.)));
            let subscription = cx.observe_in(
                &bibliography,
                window,
                |this: &mut Self, bibliography, window, cx| {
                    this.picker.update(cx, |picker, cx| {
                        picker.delegate.reload(bibliography.read(cx));
                        picker.refresh(window, cx);
                    });
                },
            );
            Self {
                picker,
                _subscription: subscription,
            }
        });
        Some(())
    }
}

impl Render for CitationPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CitationPicker")
            .child(self.picker.clone())
    }
}
impl Focusable for CitationPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}
impl EventEmitter<DismissEvent> for CitationPicker {}
impl ModalView for CitationPicker {}

struct CitationPickerDelegate {
    modal: WeakEntity<CitationPicker>,
    editor: WeakEntity<Editor>,
    entries: Vec<BibEntry>,
    candidates: Vec<StringMatchCandidate>,
    matches: Vec<usize>,
    selected_index: usize,
    chosen: Vec<SharedString>,
}

impl CitationPickerDelegate {
    fn reload(&mut self, bibliography: &Bibliography) {
        let mut entries: Vec<_> = bibliography.entries().cloned().collect();
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        entries.dedup_by(|left, right| left.key == right.key);
        self.candidates = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let search = [
                    entry.title.as_deref(),
                    entry.authors.as_deref(),
                    entry.year.as_deref(),
                    Some(entry.key.as_ref()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
                StringMatchCandidate::new(index, &search)
            })
            .collect();
        self.entries = entries;
    }

    fn insert(
        &mut self,
        keys: Vec<SharedString>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        if keys.is_empty() {
            return;
        }
        let text = citation_text(&keys);
        self.editor
            .update(cx, |editor, cx| editor.insert(&text, window, cx))
            .log_err();
        self.dismissed(window, cx);
    }
}

fn citation_text(keys: &[SharedString]) -> String {
    format!(
        "[{}]",
        keys.iter()
            .map(|key| format!("@{key}"))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_multiple_project_citations_in_selection_order() {
        assert_eq!(
            citation_text(&["first".into(), "second".into()]),
            "[@first; @second]"
        );
    }
}

impl PickerDelegate for CitationPickerDelegate {
    type ListItem = ListItem;
    fn name() -> &'static str {
        "citation picker"
    }
    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Search project citations…".into()
    }
    fn match_count(&self) -> usize {
        self.matches.len()
    }
    fn selected_index(&self) -> usize {
        self.selected_index
    }
    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }
    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self.candidates.clone();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |this, cx| {
            let indices = if query.trim().is_empty() {
                (0..candidates.len().min(50)).collect()
            } else {
                match_strings(
                    &candidates,
                    query.trim(),
                    false,
                    true,
                    50,
                    &Default::default(),
                    executor,
                )
                .await
                .into_iter()
                .map(|found| found.candidate_id)
                .collect()
            };
            this.update_in(cx, |picker, window, cx| {
                picker.delegate.matches = indices;
                picker.set_selected_index(0, None, false, window, cx);
                cx.notify();
            })
            .log_err();
        })
    }
    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected_index)
            .and_then(|index| self.entries.get(*index))
        else {
            return;
        };
        self.insert(vec![entry.key.clone()], window, cx);
    }
    fn supports_multi_select(&self) -> bool {
        true
    }
    fn is_item_selected(&self, index: usize) -> bool {
        self.matches
            .get(index)
            .and_then(|index| self.entries.get(*index))
            .is_some_and(|entry| self.chosen.contains(&entry.key))
    }
    fn toggle_item_selected(
        &mut self,
        index: usize,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(entry) = self
            .matches
            .get(index)
            .and_then(|index| self.entries.get(*index))
        else {
            return;
        };
        if let Some(position) = self.chosen.iter().position(|key| *key == entry.key) {
            self.chosen.remove(position);
        } else {
            self.chosen.push(entry.key.clone());
        }
        cx.notify();
    }
    fn selected_item_count(&self) -> usize {
        self.chosen.len()
    }
    fn clear_selection(&mut self, _: &mut Context<Picker<Self>>) {
        self.chosen.clear();
    }
    fn confirm_multi(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let chosen = std::mem::take(&mut self.chosen);
        self.insert(chosen, window, cx);
    }
    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }
    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.entries.get(*self.matches.get(index)?)?;
        let title = entry.title.clone().unwrap_or_else(|| entry.key.clone());
        let detail = [
            entry.authors.as_deref(),
            entry.year.as_deref(),
            Some(entry.key.as_ref()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex().child(Label::new(title)).child(
                        Label::new(detail)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                ),
        )
    }
}
