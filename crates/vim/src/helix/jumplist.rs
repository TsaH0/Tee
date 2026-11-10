use std::{any::Any, sync::Arc};

use collections::HashSet;
use editor::{Editor, ToPoint as _, navigation_data_cursor};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EntityId, ParentElement as _, Task, TaskExt as _,
    WeakEntity, Window, actions,
};
use language::Point;
use multi_buffer::MultiBufferRow;
use picker::{Picker, PickerDelegate};
use project::ProjectPath;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, Toggleable, rems};
use util::ResultExt as _;
use workspace::{Workspace, item::WeakItemHandle};

actions!(
    vim,
    [
        /// Opens a picker over the jumplist, the locations that `ctrl-o` and
        /// `ctrl-i` move through, like Helix's `space j`.
        ToggleJumplistView,
    ]
);

pub(crate) fn register(workspace: &mut Workspace, _window: Option<&mut Window>) {
    workspace.register_action(|workspace, _: &ToggleJumplistView, window, cx| {
        let entries = jumplist_entries(workspace, cx);
        let handle = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let delegate = JumplistDelegate::new(handle, entries);
            Picker::uniform_list(delegate, window, cx).initial_width(rems(48.))
        });
    });
}

struct JumplistEntry {
    item: Arc<dyn WeakItemHandle + Send + Sync>,
    project_path: Option<ProjectPath>,
    data: Option<Arc<dyn Any + Send + Sync>>,
    label: String,
}

/// Collects the jumplists of all panes, newest first, like Helix collects the
/// jumplists of all views.
fn jumplist_entries(workspace: &Workspace, cx: &App) -> Vec<JumplistEntry> {
    let path_style = workspace.path_style(cx);
    let mut entries = Vec::new();
    for pane in workspace.panes() {
        let nav_history = pane.read(cx).nav_history();
        for entry in nav_history.jump_entries() {
            let project_path = nav_history
                .path_for_item(entry.item.id())
                .map(|(project_path, _)| project_path)
                .or_else(|| entry.item.upgrade()?.project_path(cx));
            let item = entry.item.upgrade();
            // Items without a path (e.g. untitled buffers) can't be reopened.
            let name = match (&project_path, &item) {
                (Some(project_path), _) => project_path.path.display(path_style).into_owned(),
                (None, Some(item)) => item.tab_content_text(0, cx).to_string(),
                (None, None) => continue,
            };
            let editor = item.and_then(|item| item.act_as::<Editor>(cx));
            let (position, line) = entry
                .data
                .as_deref()
                .and_then(|data| navigation_data_cursor(data))
                .map(|(anchor, stored_position)| {
                    location_in_editor(editor.as_ref(), anchor, stored_position, cx)
                })
                .unwrap_or_default();
            entries.push((
                entry.timestamp,
                JumplistEntry {
                    item: entry.item.clone(),
                    label: format!(
                        "{}:{}:{}  {}",
                        name,
                        position.row + 1,
                        position.column + 1,
                        line.trim()
                    ),
                    project_path,
                    data: entry.data.clone(),
                },
                position.row,
            ));
        }
    }

    entries.sort_by_key(|(timestamp, _, _)| std::cmp::Reverse(*timestamp));
    let mut seen: HashSet<(EntityId, u32)> = HashSet::default();
    entries
        .into_iter()
        .filter(|(_, entry, row)| seen.insert((entry.item.id(), *row)))
        .map(|(_, entry, _)| entry)
        .collect()
}

/// Resolves a jump's position against the editor's current text, falling back
/// to the position recorded at the time of the jump if the editor is closed.
fn location_in_editor(
    editor: Option<&Entity<Editor>>,
    anchor: multi_buffer::Anchor,
    stored_position: Point,
    cx: &App,
) -> (Point, String) {
    let Some(editor) = editor else {
        return (stored_position, String::new());
    };
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let position = if snapshot.can_resolve(&anchor) {
        anchor.to_point(&snapshot)
    } else {
        snapshot.clip_point(stored_position, text::Bias::Left)
    };
    let line_end = Point::new(
        position.row,
        snapshot.line_len(MultiBufferRow(position.row)),
    );
    let line = snapshot
        .text_for_range(Point::new(position.row, 0)..line_end)
        .collect();
    (position, line)
}

struct JumplistDelegate {
    workspace: WeakEntity<Workspace>,
    entries: Vec<JumplistEntry>,
    candidates: Arc<[StringMatchCandidate]>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl JumplistDelegate {
    fn new(workspace: WeakEntity<Workspace>, entries: Vec<JumplistEntry>) -> Self {
        let candidates = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| StringMatchCandidate::new(index, &entry.label))
            .collect();
        Self {
            workspace,
            entries,
            candidates,
            matches: Vec::new(),
            selected_index: 0,
        }
    }
}

impl PickerDelegate for JumplistDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "jumplist view"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search the jumplist…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string.clone(),
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                let mut matches = match_strings(
                    &candidates,
                    &query,
                    true,
                    false,
                    candidates.len(),
                    &Default::default(),
                    background,
                )
                .await;
                // Keep the newest-first order among equally good matches.
                matches.sort_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then(a.candidate_id.cmp(&b.candidate_id))
                });
                matches
            };
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches;
                    delegate.selected_index = 0;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected_index)
            .and_then(|found| self.entries.get(found.candidate_id))
        else {
            return;
        };
        let data = entry.data.clone();
        let project_path = entry.project_path.clone();
        let item = entry.item.upgrade();
        self.workspace
            .update(cx, |workspace, cx| {
                if let Some(item) = item
                    && workspace.activate_item(item.as_ref(), true, true, window, cx)
                {
                    if let Some(data) = data {
                        item.navigate(data, window, cx);
                    }
                    return;
                }
                let Some(project_path) = project_path else {
                    return;
                };
                let open = workspace.open_path(project_path, None, true, window, cx);
                cx.spawn_in(window, async move |_, cx| {
                    let item = open.await?;
                    if let Some(data) = data {
                        cx.update(|window, cx| item.navigate(data, window, cx))?;
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    found.string.clone(),
                    found.positions.clone(),
                )),
        )
    }
}

#[cfg(test)]
mod test {
    use super::JumplistDelegate;
    use crate::{state::Mode, test::VimTestContext};
    use picker::Picker;

    #[gpui::test]
    async fn test_jumplist_picker(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.enable_helix();
        // Zed only records jumps that move the cursor far enough.
        let filler = "\n".repeat(20);
        cx.set_state(
            &format!("fn ˇone() {{}}{filler}fn two() {{}}"),
            Mode::HelixNormal,
        );
        cx.simulate_keystrokes("g e");
        cx.assert_state(
            &format!("fn one() {{}}{filler}ˇfn two() {{}}"),
            Mode::HelixNormal,
        );

        cx.simulate_keystrokes("space j");
        cx.run_until_parked();
        let labels = cx.workspace(|workspace, _, cx| {
            let picker = workspace
                .active_modal::<Picker<JumplistDelegate>>(cx)
                .expect("jumplist picker should be open");
            picker
                .read(cx)
                .delegate
                .matches
                .iter()
                .map(|found| found.string.clone())
                .collect::<Vec<_>>()
        });
        // The newest jump comes first.
        assert!(
            labels
                .first()
                .is_some_and(|label| label.ends_with("file.rs:1:4  fn one() {}")),
            "unexpected jumplist: {labels:?}"
        );

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.assert_state(
            &format!("fn ˇone() {{}}{filler}fn two() {{}}"),
            Mode::HelixNormal,
        );
    }
}
