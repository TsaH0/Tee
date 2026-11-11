//! Multiple cursors the way Neovim 0.13 has them (`:h multicursor`): `Q`
//! places a cursor and leaves it behind while the primary cursor moves on,
//! `zq{motion}` places cursors wherever a motion leads, and then every
//! command, operator and insertion acts at each cursor.
//!
//! Every cursor is one of the editor's own selections, so edits apply to all
//! of them natively. The difference from plain multiple selections is that,
//! outside follow mode (`q=`), a motion in normal mode moves only the primary
//! cursor, the newest selection, and the others stay where they were placed.

use std::ops::Range;

use editor::{
    Anchor, Editor, HighlightKey, MultiBufferOffset, SelectionEffects,
    display_map::{DisplayPoint, DisplaySnapshot, ToDisplayPoint as _},
};
use gpui::{Context, Window, actions};
use language::{Point, Selection, SelectionGoal};
use multi_buffer::{ToOffset as _, ToPoint as _};
use settings::Settings as _;

use crate::{Vim, motion::Motion, state::Mode, state::Operator};

actions!(
    vim,
    [
        /// Toggles a cursor at the cursor's position (`Q`), which stays there
        /// when the cursor moves on. A count of 1 adds it and 2 removes it.
        ToggleCursor,
        /// Starts `zq{motion}`, which places a cursor wherever the motion
        /// leads, repeated to the end of the buffer or of the selection.
        PushPlaceCursors,
        /// Places a cursor at every match of the word under the cursor
        /// (`zq*`).
        PlaceCursorsAtWordMatches,
        /// Places a cursor at every match of the last search (`zqn`).
        PlaceCursorsAtSearchMatches,
        /// Places a cursor on each line of the selection, at the cursor's
        /// column (`{Visual}Q`), and turns on follow mode.
        CursorsOnSelectedLines,
        /// Toggles follow mode (`q=`), in which motions move every cursor.
        ToggleCursorFollow,
        /// Removes every cursor but the primary one (`CTRL-L`).
        ClearCursors,
        /// Brings back the cursors removed last (`gQ`).
        RestoreCursors,
        /// Moves the primary cursor to the next cursor (`]C`).
        NextCursor,
        /// Moves the primary cursor to the previous cursor (`[C`).
        PreviousCursor,
        /// Inserts 1, 2, 3, … at the cursors, in order (`g CTRL-A`).
        InsertCursorCounter,
    ]
);

#[derive(Default)]
pub(crate) struct MulticursorState {
    /// Whether motions move every cursor.
    follow: bool,
    /// Whether a cursor stays behind when the primary cursor moves away.
    pinned: bool,
    /// Where `zq{motion}` stops, when it started from a selection.
    limit: Option<Range<Anchor>>,
    /// The cursors `ClearCursors` removed, and the primary cursor, for
    /// `RestoreCursors`.
    previous: Option<(Vec<Anchor>, Anchor)>,
    /// Whether the editor has more than one cursor, kept up to date from its
    /// selection events, for the key context.
    many_cursors: bool,
    /// Whether these commands placed the cursors. Cursors added any other
    /// way, like `editor::SelectNext`, keep moving together as before.
    session: bool,
}

impl MulticursorState {
    /// Whether keys like `CTRL-L` and `]C` act on cursors.
    pub(crate) fn in_session(&self) -> bool {
        self.session && (self.pinned || self.many_cursors)
    }
}

/// Places cursors at no more positions than this, so that a motion that
/// keeps going can't place millions.
const MAX_CURSORS: usize = 10_000;

pub(crate) fn register(editor: &mut Editor, cx: &mut Context<Vim>) {
    Vim::action(editor, cx, |vim, _: &ToggleCursor, _, cx| {
        let count = Vim::take_count(cx);
        vim.multicursor.pinned = match count {
            Some(1) => true,
            Some(2) => false,
            _ => !vim.multicursor.pinned,
        };
        vim.multicursor.follow = false;
        vim.multicursor.session = true;
        vim.refresh_pinned_highlight(cx);
        cx.notify();
    });
    Vim::action(editor, cx, |vim, _: &ToggleCursorFollow, _, cx| {
        vim.toggle_cursor_follow(cx);
    });
    Vim::action(editor, cx, |vim, _: &PushPlaceCursors, window, cx| {
        vim.push_place_cursors(window, cx);
    });
    Vim::action(
        editor,
        cx,
        |vim, _: &PlaceCursorsAtWordMatches, window, cx| {
            vim.clear_operator(window, cx);
            vim.place_cursors_at_matches(MatchKind::Word, window, cx);
        },
    );
    Vim::action(
        editor,
        cx,
        |vim, _: &PlaceCursorsAtSearchMatches, window, cx| {
            vim.clear_operator(window, cx);
            vim.place_cursors_at_matches(MatchKind::Search, window, cx);
        },
    );
    Vim::action(editor, cx, |vim, _: &CursorsOnSelectedLines, window, cx| {
        vim.cursors_on_selected_lines(window, cx);
    });
    Vim::action(editor, cx, |vim, _: &ClearCursors, window, cx| {
        vim.clear_cursors(window, cx);
    });
    Vim::action(editor, cx, |vim, _: &RestoreCursors, window, cx| {
        vim.restore_cursors(window, cx);
    });
    Vim::action(editor, cx, |vim, _: &NextCursor, window, cx| {
        let count = Vim::take_count(cx).unwrap_or(1);
        vim.jump_to_cursor(true, count, window, cx);
    });
    Vim::action(editor, cx, |vim, _: &PreviousCursor, window, cx| {
        let count = Vim::take_count(cx).unwrap_or(1);
        vim.jump_to_cursor(false, count, window, cx);
    });
    Vim::action(editor, cx, |vim, _: &InsertCursorCounter, window, cx| {
        let start = Vim::take_count(cx).unwrap_or(1);
        vim.insert_cursor_counter(start, window, cx);
    });
}

enum MatchKind {
    Word,
    Search,
}

/// A collapsed selection, a cursor, at `point`.
fn cursor(id: usize, point: Point, goal: SelectionGoal) -> Selection<Point> {
    Selection {
        id,
        start: point,
        end: point,
        reversed: false,
        goal,
    }
}

impl Vim {
    /// `q=`: toggles whether motions move every cursor.
    pub(crate) fn toggle_cursor_follow(&mut self, cx: &mut Context<Self>) {
        let count = Vim::take_count(cx);
        self.multicursor.follow = match count {
            Some(1) => true,
            Some(2) => false,
            _ => !self.multicursor.follow,
        };
        let label = if self.multicursor.follow {
            "follow on"
        } else {
            "follow off"
        };
        self.status_label = Some(label.into());
        cx.notify();
    }

    /// Whether there are cursors these commands placed, besides the primary
    /// one or at it.
    fn multicursor_active(&self, editor: &Editor) -> bool {
        self.multicursor.session && (self.multicursor.pinned || editor.selections.count() > 1)
    }

    pub(crate) fn multicursor_selections_changed(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor() else {
            return;
        };
        let many_cursors = editor.read(cx).selections.count() > 1;
        if many_cursors != self.multicursor.many_cursors {
            self.multicursor.many_cursors = many_cursors;
            cx.notify();
        }
        if !many_cursors && !self.multicursor.pinned {
            self.multicursor.session = false;
        }
        if self.multicursor.pinned {
            self.refresh_pinned_highlight(cx);
        }
    }

    /// Moves only the primary cursor with `motion`, leaving the others where
    /// they are, unless follow mode is on. Returns whether it did.
    pub(crate) fn move_primary_cursor(
        &mut self,
        motion: &Motion,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.mode != Mode::Normal || self.multicursor.follow || self.temp_mode {
            return false;
        }
        let active = self
            .editor()
            .is_some_and(|editor| self.multicursor_active(editor.read(cx)));
        if !active {
            return false;
        }
        let effects = SelectionEffects::default().nav_history(motion.push_to_jump_list());
        self.move_primary(
            effects,
            |map, primary, _, text_layout_details| {
                motion.move_point(
                    map,
                    primary.head(),
                    primary.goal,
                    times,
                    text_layout_details,
                )
            },
            window,
            cx,
        );
        true
    }

    /// Moves the primary cursor to where `target` says, leaving a cursor
    /// behind if one was placed there, and taking over a cursor it lands on.
    fn move_primary(
        &mut self,
        effects: SelectionEffects,
        target: impl FnOnce(
            &DisplaySnapshot,
            &Selection<DisplayPoint>,
            &[DisplayPoint],
            &editor::movement::TextLayoutDetails,
        ) -> Option<(DisplayPoint, SelectionGoal)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pinned = self.multicursor.pinned;
        let landed_on_cursor = self.update_editor(cx, |_, editor, cx| {
            let text_layout_details = editor.text_layout_details(window, cx);
            let map = editor.display_snapshot(cx);
            let selections = editor.selections.all_display(&map);
            let Some(primary) = selections
                .iter()
                .max_by_key(|selection| selection.id)
                .cloned()
            else {
                return false;
            };
            let other_heads = selections
                .iter()
                .filter(|selection| selection.id != primary.id)
                .map(|selection| selection.head())
                .collect::<Vec<_>>();
            let Some((point, goal)) = target(&map, &primary, &other_heads, &text_layout_details)
            else {
                return false;
            };
            let mut landed_on_cursor = false;
            let mut others = Vec::new();
            for selection in selections {
                if selection.id == primary.id {
                    continue;
                }
                if selection.head() == point {
                    landed_on_cursor = true;
                    continue;
                }
                others.push(selection.map(|point| point.to_point(&map)));
            }
            let old_head = primary.head().to_point(&map);
            let new_head = point.to_point(&map);
            editor.change_selections(effects, window, cx, |selections| {
                if pinned && old_head != new_head {
                    others.push(cursor(primary.id, old_head, primary.goal));
                }
                others.push(cursor(selections.new_selection_id(), new_head, goal));
                selections.select(others);
            });
            landed_on_cursor
        });
        if let Some(landed_on_cursor) = landed_on_cursor {
            self.multicursor.pinned = landed_on_cursor;
        }
        self.refresh_pinned_highlight(cx);
    }

    /// Replaces every cursor: `others` and the primary one last.
    fn set_cursors(
        &mut self,
        others: Vec<Point>,
        primary: Point,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_editor(cx, |_, editor, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                let mut cursors = others
                    .into_iter()
                    .filter(|point| *point != primary)
                    .take(MAX_CURSORS)
                    .map(|point| cursor(selections.new_selection_id(), point, SelectionGoal::None))
                    .collect::<Vec<_>>();
                cursors.push(cursor(
                    selections.new_selection_id(),
                    primary,
                    SelectionGoal::None,
                ));
                selections.select(cursors);
            });
        });
        self.multicursor.pinned = false;
        self.multicursor.session = true;
        self.refresh_pinned_highlight(cx);
    }

    /// Where `zq{motion}` starts and stops: the selection in Visual mode,
    /// else the primary cursor to the end of the buffer.
    fn push_place_cursors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.multicursor.limit = None;
        if self.mode.is_visual() {
            let range = self.update_editor(cx, |_, editor, cx| {
                let map = editor.display_snapshot(cx);
                let buffer = map.buffer_snapshot();
                let selections = editor.selections.all::<Point>(&map);
                let start = selections.iter().map(|selection| selection.start).min()?;
                let end = selections.iter().map(|selection| selection.end).max()?;
                Some(buffer.anchor_before(start)..buffer.anchor_after(end))
            });
            let Some(Some(range)) = range else {
                return;
            };
            self.switch_mode(Mode::Normal, true, window, cx);
            let start = range.start;
            self.update_editor(cx, |_, editor, cx| {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_anchor_ranges([start..start]);
                });
            });
            self.multicursor.limit = Some(range);
        }
        self.push_operator(Operator::PlaceCursors, window, cx);
    }

    /// `zq{motion}`: a cursor wherever `motion` leads, again and again.
    pub(crate) fn place_cursors_motion(
        &mut self,
        motion: Motion,
        times: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let limit = self.multicursor.limit.take();
        let placed = self.update_editor(cx, |_, editor, cx| {
            let text_layout_details = editor.text_layout_details(window, cx);
            let map = editor.display_snapshot(cx);
            let primary = editor.selections.newest_display(&map);
            let end = limit
                .as_ref()
                .map(|limit| limit.end.to_display_point(&map))
                .unwrap_or_else(|| map.max_point());
            let mut current = primary.head();
            let mut goal = primary.goal;
            let mut points = Vec::new();
            while points.len() < MAX_CURSORS {
                let Some((next, next_goal)) =
                    motion.move_point(&map, current, goal, times, &text_layout_details)
                else {
                    break;
                };
                if next <= current || next > end {
                    break;
                }
                points.push(next.to_point(&map));
                current = next;
                goal = next_goal;
            }
            (points, primary.head().to_point(&map))
        });
        if let Some((points, primary)) = placed {
            self.set_cursors(points, primary, window, cx);
        }
    }

    /// `zq*` and `zqn`: a cursor at the start of every match, wrapping
    /// around the buffer, or within the selection `zq` started from.
    fn place_cursors_at_matches(
        &mut self,
        kind: MatchKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let limit = self.multicursor.limit.take();
        let smartcase = editor::EditorSettings::get_global(cx).use_smartcase_search;
        let placed = self.update_editor(cx, |_, editor, cx| {
            let map = editor.display_snapshot(cx);
            let buffer = map.buffer_snapshot();
            let head = editor.selections.newest::<MultiBufferOffset>(&map).head();
            let ranges: Vec<Range<MultiBufferOffset>> = match kind {
                MatchKind::Search => editor
                    .background_highlight_ranges(HighlightKey::BufferSearchHighlights)
                    .iter()
                    .map(|range| range.start.to_offset(buffer)..range.end.to_offset(buffer))
                    .collect(),
                MatchKind::Word => {
                    let text = buffer.text();
                    let Some((word_start, word_end)) = word_at(&text, head.0) else {
                        return None;
                    };
                    let word = &text[word_start..word_end];
                    let case_sensitive = smartcase && word.chars().any(char::is_uppercase);
                    word_matches(&text, word, case_sensitive)
                        .into_iter()
                        .map(|(start, end)| MultiBufferOffset(start)..MultiBufferOffset(end))
                        .collect()
                }
            };
            let limit =
                limit.map(|limit| limit.start.to_offset(buffer)..limit.end.to_offset(buffer));
            let mut ranges = ranges
                .into_iter()
                .filter(|range| {
                    limit
                        .as_ref()
                        .is_none_or(|limit| limit.start <= range.start && range.end <= limit.end)
                })
                .collect::<Vec<_>>();
            ranges.sort_by_key(|range| range.start);
            // The primary cursor moves to the match it's in, or the next one.
            let primary = ranges
                .iter()
                .find(|range| range.start <= head && head < range.end)
                .or_else(|| ranges.iter().find(|range| range.start >= head))
                .or_else(|| ranges.first())?
                .start;
            let points = ranges
                .iter()
                .map(|range| range.start.to_point(buffer))
                .collect::<Vec<_>>();
            Some((points, primary.to_point(buffer)))
        });
        if let Some(Some((points, primary))) = placed {
            self.set_cursors(points, primary, window, cx);
        }
    }

    /// `{Visual}Q`: a cursor on each line of the selection.
    fn cursors_on_selected_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placed = self.update_editor(cx, |_, editor, cx| {
            let map = editor.display_snapshot(cx);
            let buffer = map.buffer_snapshot();
            let selections = editor.selections.all::<Point>(&map);
            let first_row = selections
                .iter()
                .map(|selection| selection.start.row)
                .min()?;
            let last_row = selections.iter().map(|selection| selection.end.row).max()?;
            let newest = editor.selections.newest::<Point>(&map);
            // In Visual mode the cursor is on the character before the head
            // of a forward selection.
            let mut head = newest.head();
            if !newest.reversed && newest.start != newest.end && head.column > 0 {
                head.column -= 1;
            }
            let points = (first_row..=last_row)
                .map(|row| {
                    let length = buffer.line_len(multi_buffer::MultiBufferRow(row));
                    Point::new(row, head.column.min(length.saturating_sub(1)))
                })
                .collect::<Vec<_>>();
            let primary = Point::new(
                head.row,
                head.column.min(
                    buffer
                        .line_len(multi_buffer::MultiBufferRow(head.row))
                        .saturating_sub(1),
                ),
            );
            Some((points, primary))
        });
        self.switch_mode(Mode::Normal, true, window, cx);
        if let Some(Some((points, primary))) = placed {
            self.set_cursors(points, primary, window, cx);
        }
        self.multicursor.follow = true;
    }

    fn clear_cursors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.update_editor(cx, |_, editor, cx| {
            let map = editor.display_snapshot(cx);
            let primary = editor.selections.newest_anchor().clone();
            let others = editor
                .selections
                .disjoint_anchors()
                .iter()
                .filter(|selection| selection.id != primary.id)
                .map(|selection| selection.head())
                .collect::<Vec<_>>();
            let head = primary.head().to_display_point(&map);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_display_ranges([head..head]);
            });
            (others, primary.head())
        });
        if let Some(previous) = previous
            && !previous.0.is_empty()
        {
            self.multicursor.previous = Some(previous);
        }
        self.multicursor.pinned = false;
        self.multicursor.follow = false;
        self.multicursor.session = false;
        self.refresh_pinned_highlight(cx);
    }

    fn restore_cursors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self
            .editor()
            .is_some_and(|editor| editor.read(cx).selections.count() > 1);
        if active {
            return;
        }
        let Some((others, primary)) = self.multicursor.previous.clone() else {
            return;
        };
        let points = self.update_editor(cx, |_, editor, cx| {
            let buffer = editor.buffer().read(cx).snapshot(cx);
            (
                others
                    .iter()
                    .map(|anchor| anchor.to_point(&buffer))
                    .collect::<Vec<_>>(),
                primary.to_point(&buffer),
            )
        });
        if let Some((others, primary)) = points {
            self.set_cursors(others, primary, window, cx);
        }
    }

    fn jump_to_cursor(
        &mut self,
        forward: bool,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_primary(
            SelectionEffects::default(),
            |_, primary, other_heads, _| {
                let head = primary.head();
                let mut others = other_heads.to_vec();
                others.sort();
                others.dedup();
                if others.is_empty() {
                    return None;
                }
                let index = if forward {
                    let after = others.iter().position(|point| *point > head).unwrap_or(0);
                    (after + count - 1) % others.len()
                } else {
                    let length = others.len();
                    let before = others
                        .iter()
                        .rposition(|point| *point < head)
                        .unwrap_or(length - 1);
                    (before + length * count - (count - 1)) % length
                };
                others.get(index).map(|point| (*point, SelectionGoal::None))
            },
            window,
            cx,
        );
    }

    fn insert_cursor_counter(&mut self, start: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.update_editor(cx, |_, editor, cx| {
            let map = editor.display_snapshot(cx);
            let mut heads = editor
                .selections
                .all::<MultiBufferOffset>(&map)
                .into_iter()
                .map(|selection| selection.head())
                .collect::<Vec<_>>();
            heads.sort();
            let edits = heads
                .into_iter()
                .enumerate()
                .map(|(index, head)| (head..head, (start + index).to_string()))
                .collect::<Vec<_>>();
            editor.transact(window, cx, |editor, _, cx| {
                editor.edit(edits, cx);
            });
        });
    }

    /// Marks the character under the primary cursor while a cursor is
    /// placed there.
    pub(crate) fn refresh_pinned_highlight(&mut self, cx: &mut Context<Self>) {
        let pinned = self.multicursor.pinned;
        self.update_editor(cx, |_, editor, cx| {
            if !pinned {
                editor.clear_background_highlights(HighlightKey::VimMulticursor, cx);
                return;
            }
            let buffer = editor.buffer().read(cx).snapshot(cx);
            let head = editor.selections.newest_anchor().head();
            let offset = head.to_offset(&buffer);
            let next = buffer
                .chars_at(offset)
                .next()
                .map_or(0, |character| character.len_utf8());
            let range = buffer.anchor_before(offset)..buffer.anchor_after(offset + next);
            editor.highlight_background(
                HighlightKey::VimMulticursor,
                &[range],
                |_, theme| theme.colors().vim_helix_jump_label_foreground.opacity(0.35),
                cx,
            );
        });
    }
}

/// The word at `offset`, or the next one on its line, as `*` finds it.
fn word_at(text: &str, offset: usize) -> Option<(usize, usize)> {
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    let line_end = text[offset.min(text.len())..]
        .find('\n')
        .map_or(text.len(), |index| offset + index);
    let start = text[offset.min(text.len())..line_end]
        .char_indices()
        .find(|(_, character)| is_word(*character))
        .map(|(index, _)| offset + index)?;
    let mut word_start = start;
    while let Some(previous) = text[..word_start].chars().next_back() {
        if !is_word(previous) {
            break;
        }
        word_start -= previous.len_utf8();
    }
    let word_end = text[start..]
        .char_indices()
        .find(|(_, character)| !is_word(*character))
        .map_or(text.len(), |(index, _)| start + index);
    Some((word_start, word_end))
}

/// Every whole-word occurrence of `word`.
fn word_matches(text: &str, word: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    let (haystack, needle) = if case_sensitive {
        (text.to_string(), word.to_string())
    } else {
        (text.to_lowercase(), word.to_lowercase())
    };
    // Lowercasing can change lengths; only use it when it doesn't.
    let (haystack, needle) = if haystack.len() == text.len() && needle.len() == word.len() {
        (haystack, needle)
    } else {
        (text.to_string(), word.to_string())
    };
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(index) = haystack[from..].find(&needle) {
        let start = from + index;
        let end = start + needle.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        if !before.is_some_and(is_word) && !after.is_some_and(is_word) {
            found.push((start, end));
        }
        from = start + needle.len().max(1);
    }
    found
}

#[cfg(test)]
mod test {
    use indoc::indoc;

    use crate::{state::Mode, test::VimTestContext};

    #[gpui::test]
    async fn test_q_leaves_cursors_behind(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.set_state("ˇone two three", Mode::Normal);
        cx.simulate_keystrokes("shift-q w shift-q w");
        cx.assert_state("ˇone ˇtwo ˇthree", Mode::Normal);

        // Motions move only the primary cursor.
        cx.simulate_keystrokes("l");
        cx.assert_state("ˇone ˇtwo tˇhree", Mode::Normal);

        // Edits act at every cursor.
        cx.simulate_keystrokes("r x");
        cx.assert_state("ˇxne ˇxwo tˇxree", Mode::Normal);
        cx.simulate_keystrokes("i - escape");
        cx.assert_state("ˇ-xne ˇ-xwo tˇ-xree", Mode::Normal);

        // CTRL-L clears them, and gQ brings them back.
        cx.simulate_keystrokes("ctrl-l");
        cx.assert_state("-xne -xwo tˇ-xree", Mode::Normal);
        cx.simulate_keystrokes("g shift-q");
        cx.assert_state("ˇ-xne ˇ-xwo tˇ-xree", Mode::Normal);

        // Escape clears them too.
        cx.simulate_keystrokes("escape");
        cx.assert_state("-xne -xwo tˇ-xree", Mode::Normal);
        cx.simulate_keystrokes("0 w");
        cx.assert_state("-ˇxne -xwo t-xree", Mode::Normal);
    }

    #[gpui::test]
    async fn test_q_toggles_and_counts(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.set_state("ˇa b c", Mode::Normal);
        // Q twice removes the cursor it placed.
        cx.simulate_keystrokes("shift-q shift-q w");
        cx.assert_state("a ˇb c", Mode::Normal);
        // 1Q always adds, and moving onto a cursor takes it over, so Q there
        // removes it.
        cx.simulate_keystrokes("1 shift-q 1 shift-q w");
        cx.assert_state("a ˇb ˇc", Mode::Normal);
        cx.simulate_keystrokes("b shift-q w");
        cx.assert_state("a b ˇc", Mode::Normal);
    }

    #[gpui::test]
    async fn test_zq_motions(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.set_state("ˇa b c", Mode::Normal);
        cx.simulate_keystrokes("z q w r x");
        cx.assert_state("ˇx ˇx ˇx", Mode::Normal);

        cx.set_state(
            indoc! {"
                ˇone
                two
                three"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("z q j r -");
        cx.assert_state(
            indoc! {"
                ˇ-ne
                ˇ-wo
                ˇ-hree"},
            Mode::Normal,
        );

        // In Visual mode, only within the selection.
        cx.set_state("ˇa b c d", Mode::Normal);
        cx.simulate_keystrokes("v e z q w r x");
        cx.assert_state("ˇx ˇx c d", Mode::Normal);
    }

    #[gpui::test]
    async fn test_zq_matches(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.set_state("fˇoo bar foo foobar", Mode::Normal);
        cx.simulate_keystrokes("z q * r z");
        cx.assert_state("ˇzoo bar ˇzoo foobar", Mode::Normal);
    }

    #[gpui::test]
    async fn test_visual_q_and_follow(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.set_state(
            indoc! {"
                ˇab
                cd
                ef"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("v j j shift-q");
        cx.assert_state(
            indoc! {"
                ˇab
                ˇcd
                ˇef"},
            Mode::Normal,
        );
        // Visual Q turns on follow mode, so motions move every cursor.
        cx.simulate_keystrokes("l");
        cx.assert_state(
            indoc! {"
                aˇb
                cˇd
                eˇf"},
            Mode::Normal,
        );
        // q= turns it off again.
        cx.simulate_keystrokes("q = h");
        cx.assert_state(
            indoc! {"
                aˇb
                cˇd
                ˇef"},
            Mode::Normal,
        );
    }

    #[gpui::test]
    async fn test_jumping_between_cursors_and_counters(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.set_state(
            indoc! {"
                ˇa
                b
                c"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("z q j");
        cx.assert_state(
            indoc! {"
                ˇa
                ˇb
                ˇc"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("g ctrl-a");
        cx.assert_state(
            indoc! {"
                1ˇa
                2ˇb
                3ˇc"},
            Mode::Normal,
        );
        // ]C takes over the next cursor; Q there removes it.
        cx.simulate_keystrokes("] shift-c shift-q k");
        cx.assert_state(
            indoc! {"
                1ˇa
                2b
                3ˇc"},
            Mode::Normal,
        );
    }
}
