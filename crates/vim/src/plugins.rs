//! Plugins change how Vim and Helix modes behave without touching the core
//! modal engine. Each plugin implements [`VimPlugin`], gets its own instance
//! per editor, and is turned on and configured under `vim.plugins` in the
//! settings.
//!
//! To add a plugin, implement [`VimPlugin`] and add its constructor to
//! [`BUILTIN_PLUGINS`]. Settings for it go in `VimPluginsSettingsContent`.

use editor::{
    Anchor, Editor, MultiBufferOffset, NavigationOverlayKey, NavigationOverlayLabel,
    NavigationTargetOverlay,
};
use gpui::{
    Action, AnyWindowHandle, App, Context, Focusable as _, Keystroke, Subscription,
    WeakEntity, Window, px,
};
use language::{Point, Selection};
use multi_buffer::ToOffset as _;
use settings::{RegisterSetting, Settings};
use theme::ActiveTheme as _;
use workspace::Workspace;

use crate::{
    Vim, VimAddon,
    motion::{self, Motion},
    normal::search::ReplaceCommand,
};
use editor::{EditorSettings, movement::FindRange};

/// Hooks a plugin can implement. Every hook gets the [`Vim`] state of the
/// editor the plugin instance belongs to.
pub(crate) trait VimPlugin: 'static {
    /// Called before a keystroke in this editor is dispatched. Return true to
    /// consume it, so that no binding or text input sees it.
    fn intercept_keystroke(
        &mut self,
        _vim: &mut Vim,
        _keystroke: &Keystroke,
        _window: &mut Window,
        _cx: &mut Context<Vim>,
    ) -> bool {
        false
    }

    /// Called after `f`, `F`, `t` or `T` (without an operator) moved the
    /// cursor. `origin` holds the selections from before the motion.
    fn find_motion_finished(
        &mut self,
        _vim: &mut Vim,
        _motion: &Motion,
        _origin: &[Selection<Anchor>],
        _window: &mut Window,
        _cx: &mut Context<Vim>,
    ) {
    }

    /// Called while a command is typed in the command palette (`:`), with the
    /// command it parses to so far, and with `None` once it doesn't parse or
    /// the palette is closed.
    fn command_changed(
        &mut self,
        _vim: &mut Vim,
        _command: Option<&dyn Action>,
        _window: &mut Window,
        _cx: &mut Context<Vim>,
    ) {
    }

    /// Called when the editor gains or loses focus.
    fn focus_changed(
        &mut self,
        _vim: &mut Vim,
        _focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Vim>,
    ) {
    }
}

const BUILTIN_PLUGINS: &[fn() -> Box<dyn VimPlugin>] = &[
    || Box::new(FlashFind::default()),
    || Box::new(CompletionDismiss),
    || Box::new(SubstitutePreview::default()),
];

pub(crate) fn create_plugins() -> Vec<Box<dyn VimPlugin>> {
    BUILTIN_PLUGINS.iter().map(|create| create()).collect()
}

#[derive(RegisterSetting, Clone, Debug, PartialEq)]
pub(crate) struct VimPluginSettings {
    flash_find_enabled: bool,
    flash_find_labels: Vec<char>,
    completion_dismiss_enabled: bool,
    completion_dismiss_keys: Vec<Keystroke>,
    substitute_preview_enabled: bool,
}

impl Settings for VimPluginSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let plugins = content
            .vim
            .as_ref()
            .and_then(|vim| vim.plugins.clone())
            .unwrap_or_default();
        let flash_find = plugins.flash_find.unwrap_or_default();
        let completion_dismiss = plugins.completion_dismiss.unwrap_or_default();
        let substitute_preview = plugins.substitute_preview.unwrap_or_default();

        let mut flash_find_labels = Vec::new();
        for label in flash_find
            .labels
            .unwrap_or_else(|| "asdfgqwertyuiopzxcvbnm".to_string())
            .chars()
        {
            if !label.is_whitespace() && !flash_find_labels.contains(&label) {
                flash_find_labels.push(label);
            }
        }

        let completion_dismiss_keys = completion_dismiss
            .keys
            .unwrap_or_else(|| vec!["j".to_string(), "k".to_string()])
            .iter()
            .filter_map(|key| match Keystroke::parse(key) {
                Ok(keystroke) => Some(keystroke),
                Err(error) => {
                    log::error!("invalid key {key:?} in vim.plugins.completion_dismiss: {error}");
                    None
                }
            })
            .collect();

        Self {
            flash_find_enabled: flash_find.enabled.unwrap_or(true),
            flash_find_labels,
            completion_dismiss_enabled: completion_dismiss.enabled.unwrap_or(true),
            completion_dismiss_keys,
            substitute_preview_enabled: substitute_preview.enabled.unwrap_or(true),
        }
    }
}

impl Vim {
    /// Runs `hook` on each plugin until one returns true.
    fn run_plugins(
        &mut self,
        mut hook: impl FnMut(&mut dyn VimPlugin, &mut Vim) -> bool,
    ) -> bool {
        // Plugins are taken out while they run so that they can borrow `self`.
        let mut plugins = std::mem::take(&mut self.plugins);
        let handled = plugins.iter_mut().any(|plugin| hook(plugin.as_mut(), self));
        self.plugins = plugins;
        handled
    }

    pub(crate) fn subscribe_plugin_keystrokes(cx: &mut Context<Vim>) -> Subscription {
        let vim = cx.weak_entity();
        cx.intercept_keystrokes(move |event, window, cx| {
            let Some(vim) = vim.upgrade() else {
                return;
            };
            let consumed = vim.update(cx, |vim, cx| {
                let focused = vim
                    .editor()
                    .is_some_and(|editor| editor.focus_handle(cx).is_focused(window));
                focused
                    && vim.run_plugins(|plugin, vim| {
                        plugin.intercept_keystroke(vim, &event.keystroke, window, cx)
                    })
            });
            if consumed {
                cx.stop_propagation();
            }
        })
    }

    pub(crate) fn notify_plugins_find_motion(
        &mut self,
        motion: &Motion,
        origin: &[Selection<Anchor>],
        window: &mut Window,
        cx: &mut Context<Vim>,
    ) {
        self.run_plugins(|plugin, vim| {
            plugin.find_motion_finished(vim, motion, origin, window, cx);
            false
        });
    }

    pub(crate) fn notify_plugins_focus(
        &mut self,
        focused: bool,
        window: &mut Window,
        cx: &mut Context<Vim>,
    ) {
        self.run_plugins(|plugin, vim| {
            plugin.focus_changed(vim, focused, window, cx);
            false
        });
    }

    fn notify_plugins_command(
        &mut self,
        command: Option<&dyn Action>,
        window: &mut Window,
        cx: &mut Context<Vim>,
    ) {
        self.run_plugins(|plugin, vim| {
            plugin.command_changed(vim, command, window, cx);
            false
        });
    }
}

/// Tells the plugins of the workspace's active editor about the command being
/// typed. This runs after the command palette finishes updating, because the
/// palette calls it in the middle of its own update.
pub(crate) fn schedule_command_changed(
    command: Option<Box<dyn Action>>,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    let Some(window) = cx.active_window() else {
        return;
    };
    cx.defer(move |cx| notify_command_changed(command, workspace, window, cx));
}

fn notify_command_changed(
    command: Option<Box<dyn Action>>,
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let Some(editor) = workspace.read(cx).active_item_as::<Editor>(cx) else {
        return;
    };
    let Some(vim) = editor
        .read(cx)
        .addon::<VimAddon>()
        .map(|addon| addon.entity.clone())
    else {
        return;
    };
    window
        .update(cx, |_, window, cx| {
            vim.update(cx, |vim, cx| {
                vim.notify_plugins_command(command.as_deref(), window, cx)
            })
        })
        .ok();
}

enum FlashFindOverlay {}

const FLASH_FIND_OVERLAY_KEY: NavigationOverlayKey =
    NavigationOverlayKey::unique::<FlashFindOverlay>();

struct FlashTarget {
    label: char,
    motion: Motion,
    count: usize,
}

struct ActiveFlash {
    targets: Vec<FlashTarget>,
    origin: Vec<Selection<Anchor>>,
}

/// After a character find, labels the other visible occurrences of the
/// character. Typing a label redoes the find with the count that reaches
/// that occurrence, so it behaves exactly like `{count}f{char}` in every
/// mode; any other key removes the labels and is handled as usual.
#[derive(Default)]
struct FlashFind {
    active: Option<ActiveFlash>,
}

impl FlashFind {
    fn clear(&mut self, vim: &mut Vim, cx: &mut Context<Vim>) {
        if self.active.take().is_some() {
            vim.update_editor(cx, |_, editor, cx| {
                editor.clear_navigation_overlays(FLASH_FIND_OVERLAY_KEY, cx);
            });
        }
    }
}

/// The parts of a find motion that decide which characters it stops at.
struct FindParameters {
    target: char,
    forward: bool,
    /// `t` or `T` rather than `f` or `F`.
    till: bool,
    smartcase: bool,
}

impl FindParameters {
    fn from_motion(motion: &Motion) -> Option<Self> {
        match *motion {
            Motion::FindForward {
                before,
                char,
                smartcase,
                ..
            } => Some(Self {
                target: char,
                forward: true,
                till: before,
                smartcase,
            }),
            Motion::FindBackward {
                after,
                char,
                smartcase,
                ..
            } => Some(Self {
                target: char,
                forward: false,
                till: after,
                smartcase,
            }),
            _ => None,
        }
    }

    fn motion(&self, forward: bool) -> Motion {
        // Labels may be on other lines, so the jump searches across lines
        // even when the find that showed them didn't.
        if forward {
            Motion::FindForward {
                before: self.till,
                char: self.target,
                mode: FindRange::MultiLine,
                smartcase: self.smartcase,
            }
        } else {
            Motion::FindBackward {
                after: self.till,
                char: self.target,
                mode: FindRange::MultiLine,
                smartcase: self.smartcase,
            }
        }
    }
}

impl VimPlugin for FlashFind {
    fn intercept_keystroke(
        &mut self,
        vim: &mut Vim,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Vim>,
    ) -> bool {
        let Some(active) = self.active.take() else {
            return false;
        };
        vim.update_editor(cx, |_, editor, cx| {
            editor.clear_navigation_overlays(FLASH_FIND_OVERLAY_KEY, cx);
        });
        if keystroke.modifiers.modified() {
            return false;
        }
        let mut key = keystroke.key.chars();
        let (Some(key), None) = (key.next(), key.next()) else {
            return false;
        };
        let Some(target) = active.targets.into_iter().find(|target| target.label == key) else {
            return false;
        };
        let origin = active.origin;
        vim.update_editor(cx, |_, editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchors(origin)
            });
        });
        let globals = Vim::globals(cx);
        globals.pre_count = Some(target.count);
        globals.post_count = None;
        vim.motion(target.motion, window, cx);
        true
    }

    fn find_motion_finished(
        &mut self,
        vim: &mut Vim,
        motion: &Motion,
        origin: &[Selection<Anchor>],
        window: &mut Window,
        cx: &mut Context<Vim>,
    ) {
        self.clear(vim, cx);
        let settings = VimPluginSettings::get_global(cx);
        if !settings.flash_find_enabled || settings.flash_find_labels.is_empty() {
            return;
        }
        let labels = settings.flash_find_labels.clone();
        let ([origin_selection], Some(find)) = (origin, FindParameters::from_motion(motion))
        else {
            return;
        };
        let label_color = cx.theme().colors().vim_helix_jump_label_foreground;

        let flash = vim.update_editor(cx, |_, editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            let display_snapshot = &snapshot.display_snapshot;
            let buffer = display_snapshot.buffer_snapshot();
            let visible_range =
                Vim::visible_jump_range(editor, &snapshot, display_snapshot, cx);
            let visible_start = buffer.point_to_offset(visible_range.start);
            // The visible range ends at the start of the last visible line.
            let last_row = visible_range.end.row.min(buffer.max_point().row);
            let visible_end = buffer.point_to_offset(Point::new(
                last_row,
                buffer.line_len(multi_buffer::MultiBufferRow(last_row)),
            ));

            // Motions start from the character under the cursor, which in a
            // forward selection is the one before the head.
            let head = origin_selection.head().to_offset(buffer);
            let cursor = if origin_selection.reversed
                || origin_selection.start == origin_selection.end
            {
                head
            } else {
                head - buffer
                    .reversed_chars_at(head)
                    .next()
                    .map_or(0, |character| character.len_utf8())
            };

            let mut forward = Vec::new();
            let mut backward = Vec::new();
            let mut offset = visible_start;
            for chunk in buffer.text_for_range(visible_start..visible_end) {
                for (index, character) in chunk.char_indices() {
                    let character_offset = offset + index;
                    if character_offset != cursor
                        && motion::is_character_match(find.target, character, find.smartcase)
                    {
                        let range = character_offset..character_offset + character.len_utf8();
                        if character_offset > cursor {
                            forward.push(range);
                        } else {
                            backward.push(range);
                        }
                    }
                }
                offset += chunk.len();
            }
            backward.reverse();

            // The find already went to the nearest occurrence in its
            // direction, so that one gets no label.
            let (same_direction, other_direction) = if find.forward {
                (forward, backward)
            } else {
                (backward, forward)
            };
            let candidates = same_direction
                .into_iter()
                .enumerate()
                .skip(1)
                .map(|(index, range)| (find.forward, index + 1, range))
                .chain(
                    other_direction
                        .into_iter()
                        .enumerate()
                        .map(|(index, range)| (!find.forward, index + 1, range)),
                );

            let mut targets = Vec::new();
            let mut overlays = Vec::new();
            for (label, (forward, count, range)) in labels.into_iter().zip(candidates) {
                let range: std::ops::Range<MultiBufferOffset> = range;
                let range = buffer.anchor_after(range.start)..buffer.anchor_before(range.end);
                overlays.push(NavigationTargetOverlay {
                    target_range: range.clone(),
                    label: NavigationOverlayLabel {
                        text: label.to_string().into(),
                        text_color: label_color,
                        x_offset: px(0.),
                        scale_factor: 1.,
                    },
                    covered_text_range: Some(range),
                });
                targets.push(FlashTarget {
                    label,
                    motion: find.motion(forward),
                    count,
                });
            }
            if !overlays.is_empty() {
                editor.set_navigation_overlays(FLASH_FIND_OVERLAY_KEY, overlays, cx);
            }
            targets
        });

        if let Some(targets) = flash
            && !targets.is_empty()
        {
            self.active = Some(ActiveFlash {
                targets,
                origin: origin.to_vec(),
            });
        }
    }

    fn focus_changed(
        &mut self,
        vim: &mut Vim,
        focused: bool,
        _window: &mut Window,
        cx: &mut Context<Vim>,
    ) {
        if !focused {
            self.clear(vim, cx);
        }
    }
}

/// Hides the completions menu when one of the configured keys is typed.
struct CompletionDismiss;

impl VimPlugin for CompletionDismiss {
    fn intercept_keystroke(
        &mut self,
        vim: &mut Vim,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Vim>,
    ) -> bool {
        let settings = VimPluginSettings::get_global(cx);
        if !settings.completion_dismiss_enabled {
            return false;
        }
        let matches = settings.completion_dismiss_keys.iter().any(|key| {
            key.modifiers == keystroke.modifiers && key.key.eq_ignore_ascii_case(&keystroke.key)
        });
        if matches {
            vim.update_editor(cx, |_, editor, cx| {
                editor.dismiss_completions_for_current_word(window, cx);
            });
        }
        // The key still does what it would have done.
        false
    }
}

/// Shows what `:s` would change while it is being typed: matches are struck
/// through and followed by their replacement.
#[derive(Default)]
struct SubstitutePreview {
    showing: bool,
}

impl SubstitutePreview {
    fn clear(&mut self, vim: &mut Vim, cx: &mut Context<Vim>) {
        if std::mem::take(&mut self.showing) {
            vim.update_editor(cx, |_, editor, cx| editor.clear_substitute_preview(cx));
        }
    }
}

impl VimPlugin for SubstitutePreview {
    fn command_changed(
        &mut self,
        vim: &mut Vim,
        command: Option<&dyn Action>,
        window: &mut Window,
        cx: &mut Context<Vim>,
    ) {
        let command = command.and_then(|command| command.as_any().downcast_ref::<ReplaceCommand>());
        let Some(command) = command.filter(|_| VimPluginSettings::get_global(cx).substitute_preview_enabled)
        else {
            self.clear(vim, cx);
            return;
        };
        let replacement = &command.replacement;
        if replacement.search().is_empty() {
            self.clear(vim, cx);
            return;
        }

        let case_sensitive = replacement.case_sensitive().unwrap_or_else(|| {
            let editor_settings = EditorSettings::get_global(cx);
            if editor_settings.use_smartcase_search {
                replacement.search().chars().any(char::is_uppercase)
            } else {
                editor_settings.search.case_sensitive
            }
        });
        let regex = match fancy_regex::RegexBuilder::new(replacement.search())
            .case_insensitive(!case_sensitive)
            .multi_line(true)
            .build()
        {
            Ok(regex) => regex,
            Err(_) => {
                // The pattern is usually incomplete while it's being typed.
                self.clear(vim, cx);
                return;
            }
        };
        let replace_all_in_line = replacement.replaces_all_in_line(cx);
        let replacement_text = unescape_replacement(replacement.replacement());

        let shown = vim.update_editor(cx, |vim, editor, cx| {
            let Ok(rows) = command.range.buffer_range(vim, editor, window, cx) else {
                editor.clear_substitute_preview(cx);
                return false;
            };
            let snapshot = editor.snapshot(window, cx);
            let display_snapshot = &snapshot.display_snapshot;
            let buffer = display_snapshot.buffer_snapshot();
            // Only what's on screen can be seen while the palette is open.
            let visible = Vim::visible_jump_range(editor, &snapshot, display_snapshot, cx);
            let first_row = rows.start.0.max(visible.start.row);
            let last_row = rows.end.0.min(visible.end.row);
            if first_row > last_row {
                editor.clear_substitute_preview(cx);
                return false;
            }
            let start = buffer.point_to_offset(Point::new(first_row, 0));
            let end = buffer.point_to_offset(Point::new(
                last_row,
                buffer.line_len(multi_buffer::MultiBufferRow(last_row)),
            ));
            let text = buffer.text_for_range(start..end).collect::<String>();

            let mut replacements = Vec::new();
            let mut last_replaced_row = None;
            for captures in regex.captures_iter(text.as_str()) {
                let Ok(captures) = captures else {
                    break;
                };
                let Some(found) = captures.get(0) else {
                    continue;
                };
                let match_start = start + found.start();
                let row = buffer.offset_to_point(match_start).row;
                if !replace_all_in_line && last_replaced_row == Some(row) {
                    continue;
                }
                last_replaced_row = Some(row);
                let mut replaced = String::new();
                captures.expand(&replacement_text, &mut replaced);
                let range = buffer.anchor_before(match_start)
                    ..buffer.anchor_after(start + found.end());
                replacements.push((range, replaced));
                if replacements.len() >= 2000 {
                    break;
                }
            }
            let shown = !replacements.is_empty();
            editor.set_substitute_preview(replacements, cx);
            shown
        });
        self.showing = shown.unwrap_or(false);
    }

    fn focus_changed(
        &mut self,
        vim: &mut Vim,
        focused: bool,
        _window: &mut Window,
        cx: &mut Context<Vim>,
    ) {
        // The editor gets focus back when the command palette closes.
        if focused {
            self.clear(vim, cx);
        }
    }
}

/// Applies the escapes Zed's search applies to a replacement before it
/// expands capture groups.
fn unescape_replacement(replacement: &str) -> String {
    let mut output = String::new();
    let mut chars = replacement.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => output.push('\n'),
            Some('t') => output.push('\t'),
            Some('\\') => output.push('\\'),
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }
    output
}

#[cfg(test)]
mod test {
    use indoc::indoc;

    use crate::{state::Mode, test::VimTestContext};

    #[gpui::test]
    async fn test_flash_find_labels(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;

        cx.set_state("ˇabc abc abc abc", Mode::Normal);
        cx.simulate_keystrokes("f c");
        cx.assert_state("abˇc abc abc abc", Mode::Normal);
        // The second occurrence after the cursor gets the first label.
        cx.simulate_keystrokes("s");
        cx.assert_state("abc abc abˇc abc", Mode::Normal);

        cx.set_state("ˇabc abc abc", Mode::Normal);
        cx.simulate_keystrokes("t c a");
        cx.assert_state("abc aˇbc abc", Mode::Normal);

        // Any other key removes the labels and works as usual.
        cx.set_state("ˇabc abc abc", Mode::Normal);
        cx.simulate_keystrokes("f c l");
        cx.assert_state("abcˇ abc abc", Mode::Normal);

        // Labels reach other lines, and occurrences behind the cursor.
        cx.set_state(
            indoc! {"
                xc ˇc c
                c"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("f c");
        cx.assert_state(
            indoc! {"
                xc c ˇc
                c"},
            Mode::Normal,
        );
        cx.simulate_keystrokes("s");
        cx.assert_state(
            indoc! {"
                xˇc c c
                c"},
            Mode::Normal,
        );
    }

    #[gpui::test]
    async fn test_flash_find_in_helix_mode(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.enable_helix();

        cx.set_state("ˇabc abc abc", Mode::HelixNormal);
        cx.simulate_keystrokes("f c a");
        cx.assert_state("«abc abcˇ» abc", Mode::HelixNormal);
    }
}
