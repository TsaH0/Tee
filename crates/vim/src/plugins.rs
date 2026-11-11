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
    Action, AnyWindowHandle, App, Context, Focusable as _, Keystroke, Subscription, WeakEntity,
    Window, px,
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

    /// Called for `vim::FlashJump`. Return true if the plugin started a
    /// jump.
    fn flash_jump_requested(
        &mut self,
        _vim: &mut Vim,
        _window: &mut Window,
        _cx: &mut Context<Vim>,
    ) -> bool {
        false
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
    || Box::new(FlashJumpPlugin::default()),
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
    completion_dismiss_keys: Vec<String>,
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

        // Keys are parsed when they're used, where the host OS is known, which
        // only matters for `secondary-`.
        let completion_dismiss_keys = completion_dismiss
            .keys
            .unwrap_or_else(|| vec!["j".to_string(), "k".to_string()])
            .into_iter()
            .filter(
                |key| match Keystroke::parse_with_os(key, gpui::OperatingSystem::Linux) {
                    Ok(_) => true,
                    Err(error) => {
                        log::error!(
                            "invalid key {key:?} in vim.plugins.completion_dismiss: {error}"
                        );
                        false
                    }
                },
            )
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

gpui::actions!(
    vim,
    [
        /// Jumps like flash.nvim: type some characters, and labels appear on
        /// their matches on screen. Typing more narrows them, and typing a
        /// label jumps there.
        FlashJump,
    ]
);

pub(crate) fn register(editor: &mut Editor, cx: &mut Context<Vim>) {
    Vim::action(editor, cx, |vim, _: &FlashJump, window, cx| {
        vim.run_plugins(|plugin, vim| plugin.flash_jump_requested(vim, window, cx));
    });
}

impl Vim {
    /// Runs `hook` on each plugin until one returns true.
    fn run_plugins(&mut self, mut hook: impl FnMut(&mut dyn VimPlugin, &mut Vim) -> bool) -> bool {
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
            // Every editor has a Vim, so only the focused one does any work.
            let focused = vim
                .read(cx)
                .editor()
                .is_some_and(|editor| editor.focus_handle(cx).is_focused(window));
            if !focused {
                return;
            }
            let consumed = vim.update(cx, |vim, cx| {
                vim.run_plugins(|plugin, vim| {
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
        let Some(target) = active
            .targets
            .into_iter()
            .find(|target| target.label == key)
        else {
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
        let ([origin_selection], Some(find)) = (origin, FindParameters::from_motion(motion)) else {
            return;
        };
        let label_color = cx.theme().colors().vim_helix_jump_label_foreground;

        let flash = vim.update_editor(cx, |_, editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            let display_snapshot = &snapshot.display_snapshot;
            let buffer = display_snapshot.buffer_snapshot();
            let visible_range = Vim::visible_jump_range(editor, &snapshot, display_snapshot, cx);
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
            let cursor =
                if origin_selection.reversed || origin_selection.start == origin_selection.end {
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

/// A flash.nvim-style jump: while it's active, typed characters build a
/// search whose matches on screen get labels, and typing a label jumps to
/// its match. Labels skip characters that would continue a match, so typing
/// more keeps narrowing the search.
#[derive(Default)]
struct FlashJumpPlugin {
    active: Option<ActiveFlashJump>,
}

struct ActiveFlashJump {
    pattern: String,
    /// Each label and the start of its match, nearest to the cursor first.
    targets: Vec<(char, Anchor)>,
}

enum FlashJumpOverlay {}

const FLASH_JUMP_OVERLAY_KEY: NavigationOverlayKey =
    NavigationOverlayKey::unique::<FlashJumpOverlay>();

impl FlashJumpPlugin {
    fn finish(&mut self, vim: &mut Vim, cx: &mut Context<Vim>) {
        self.active = None;
        vim.status_label = None;
        vim.update_editor(cx, |_, editor, cx| {
            editor.clear_navigation_overlays(FLASH_JUMP_OVERLAY_KEY, cx);
            editor.clear_background_highlights(editor::HighlightKey::VimFlashJump, cx);
        });
        cx.notify();
    }

    /// Finds the pattern's matches on screen and labels them.
    fn update_matches(&mut self, vim: &mut Vim, window: &mut Window, cx: &mut Context<Vim>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        vim.status_label = Some(format!("flash: {}", active.pattern).into());
        let settings = VimPluginSettings::get_global(cx);
        let labels = settings.flash_find_labels.clone();
        let label_color = cx.theme().colors().vim_helix_jump_label_foreground;
        let pattern = active.pattern.clone();
        // Like smartcase: an uppercase letter makes the search case sensitive.
        let case_sensitive = pattern.chars().any(char::is_uppercase);

        let targets = vim.update_editor(cx, |_, editor, cx| {
            editor.clear_navigation_overlays(FLASH_JUMP_OVERLAY_KEY, cx);
            editor.clear_background_highlights(editor::HighlightKey::VimFlashJump, cx);
            if pattern.is_empty() {
                return Vec::new();
            }
            let snapshot = editor.snapshot(window, cx);
            let display_snapshot = &snapshot.display_snapshot;
            let buffer = display_snapshot.buffer_snapshot();
            let visible_range = Vim::visible_jump_range(editor, &snapshot, display_snapshot, cx);
            let visible_start = buffer.point_to_offset(visible_range.start);
            let last_row = visible_range.end.row.min(buffer.max_point().row);
            let visible_end = buffer.point_to_offset(Point::new(
                last_row,
                buffer.line_len(multi_buffer::MultiBufferRow(last_row)),
            ));
            let text = buffer
                .text_for_range(visible_start..visible_end)
                .collect::<String>();
            let cursor = editor
                .selections
                .newest_anchor()
                .head()
                .to_offset(buffer);

            let fold = |character: char| {
                if case_sensitive {
                    character
                } else {
                    character.to_lowercase().next().unwrap_or(character)
                }
            };
            let haystack = text.chars().map(fold).collect::<String>();
            let needle = pattern.chars().map(fold).collect::<String>();
            // Lowercasing can change byte lengths, so map positions back
            // through characters rather than bytes.
            let original_offsets = text
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(text.len()))
                .collect::<Vec<_>>();
            let folded_starts = haystack
                .char_indices()
                .map(|(index, _)| index)
                .collect::<Vec<_>>();

            let mut matches = Vec::new();
            let mut continuations = std::collections::HashSet::new();
            let needle_length = needle.chars().count();
            for (character_index, folded_start) in folded_starts.iter().enumerate() {
                if !haystack[*folded_start..].starts_with(&needle) {
                    continue;
                }
                let (Some(start), Some(end)) = (
                    original_offsets.get(character_index),
                    original_offsets.get(character_index + needle_length),
                ) else {
                    continue;
                };
                let start = visible_start + *start;
                let end = visible_start + *end;
                let next = text[end - visible_start..].chars().next();
                if let Some(next) = next {
                    continuations.insert(fold(next));
                }
                matches.push((start, end, next));
            }
            matches.sort_by_key(|(start, _, _)| start.0.abs_diff(cursor.0));

            let mut labels = labels
                .into_iter()
                .filter(|label| !continuations.contains(&fold(*label)));
            let mut targets = Vec::new();
            let mut overlays = Vec::new();
            let mut highlights = Vec::new();
            for (start, end, next) in matches {
                let range = buffer.anchor_after(start)..buffer.anchor_before(end);
                highlights.push(range.clone());
                let Some(label) = labels.next() else {
                    continue;
                };
                // The label goes just after the match, where the next typed
                // character would be, as in flash.nvim.
                let covered = next.filter(|next| *next != '\n').map(|next| {
                    buffer.anchor_after(end)..buffer.anchor_before(end + next.len_utf8())
                });
                overlays.push(NavigationTargetOverlay {
                    target_range: range.clone(),
                    label: NavigationOverlayLabel {
                        text: label.to_string().into(),
                        text_color: label_color,
                        x_offset: px(0.),
                        scale_factor: 1.,
                    },
                    covered_text_range: covered,
                });
                targets.push((label, range.start));
            }
            if !highlights.is_empty() {
                editor.highlight_background(
                    editor::HighlightKey::VimFlashJump,
                    &highlights,
                    |_, theme| theme.colors().search_match_background,
                    cx,
                );
            }
            if !overlays.is_empty() {
                editor.set_navigation_overlays(FLASH_JUMP_OVERLAY_KEY, overlays, cx);
            }
            targets
        });
        if let Some(active) = self.active.as_mut() {
            active.targets = targets.unwrap_or_default();
        }
        cx.notify();
    }

    fn jump(&mut self, target: Anchor, vim: &mut Vim, window: &mut Window, cx: &mut Context<Vim>) {
        self.finish(vim, cx);
        let extend = vim.mode.is_visual();
        vim.update_editor(cx, |_, editor, cx| {
            editor.change_selections(
                editor::SelectionEffects::scroll(editor::scroll::Autoscroll::fit()),
                window,
                cx,
                |selections| {
                    selections.move_with(&mut |map, selection| {
                        let point = editor::display_map::ToDisplayPoint::to_display_point(
                            &target, map,
                        );
                        if extend {
                            selection.set_head(point, selection.goal);
                        } else {
                            selection.collapse_to(point, selection.goal);
                        }
                    })
                },
            );
        });
    }
}

impl VimPlugin for FlashJumpPlugin {
    fn flash_jump_requested(
        &mut self,
        vim: &mut Vim,
        window: &mut Window,
        cx: &mut Context<Vim>,
    ) -> bool {
        self.active = Some(ActiveFlashJump {
            pattern: String::new(),
            targets: Vec::new(),
        });
        self.update_matches(vim, window, cx);
        true
    }

    fn intercept_keystroke(
        &mut self,
        vim: &mut Vim,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Vim>,
    ) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let modifiers = keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            self.finish(vim, cx);
            return false;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.finish(vim, cx);
                return true;
            }
            "enter" => {
                match active.targets.first().map(|(_, target)| *target) {
                    Some(target) => self.jump(target, vim, window, cx),
                    None => self.finish(vim, cx),
                }
                return true;
            }
            "backspace" => {
                if active.pattern.pop().is_none() {
                    self.finish(vim, cx);
                } else {
                    self.update_matches(vim, window, cx);
                }
                return true;
            }
            _ => {}
        }
        let typed = keystroke
            .key_char
            .clone()
            .or_else(|| (keystroke.key == "space").then(|| " ".to_string()))
            .or_else(|| {
                let mut characters = keystroke.key.chars();
                match (characters.next(), characters.next()) {
                    (Some(character), None) => Some(if modifiers.shift {
                        character.to_uppercase().collect()
                    } else {
                        character.to_string()
                    }),
                    _ => None,
                }
            });
        let Some(typed) = typed else {
            self.finish(vim, cx);
            return false;
        };
        let mut characters = typed.chars();
        if let (Some(character), None) = (characters.next(), characters.next())
            && !active.pattern.is_empty()
            && let Some(target) = active
                .targets
                .iter()
                .find(|(label, _)| *label == character)
                .map(|(_, target)| *target)
        {
            self.jump(target, vim, window, cx);
            return true;
        }
        active.pattern.push_str(&typed);
        self.update_matches(vim, window, cx);
        true
    }

    fn focus_changed(
        &mut self,
        vim: &mut Vim,
        focused: bool,
        _window: &mut Window,
        cx: &mut Context<Vim>,
    ) {
        if !focused && self.active.is_some() {
            self.finish(vim, cx);
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
        let os = cx.platform_info().os;
        let matches = settings.completion_dismiss_keys.iter().any(|key| {
            Keystroke::parse_with_os(key, os).is_ok_and(|key| {
                key.modifiers == keystroke.modifiers && key.key.eq_ignore_ascii_case(&keystroke.key)
            })
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
        let Some(command) =
            command.filter(|_| VimPluginSettings::get_global(cx).substitute_preview_enabled)
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
                let range =
                    buffer.anchor_before(match_start)..buffer.anchor_after(start + found.end());
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
    async fn test_flash_jump(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;

        // Matches are labeled nearest first, skipping letters that continue
        // a match (`o` and `b` here).
        cx.set_state("ˇfoo bar foobar baz fob", Mode::Normal);
        cx.simulate_keystrokes("enter f o s");
        cx.assert_state("foo bar ˇfoobar baz fob", Mode::Normal);

        // Typing more narrows the matches, and enter takes the nearest.
        cx.set_state("ˇfoo bar foobar baz fob", Mode::Normal);
        cx.simulate_keystrokes("enter f o b enter");
        cx.assert_state("foo bar foobar baz ˇfob", Mode::Normal);

        // Backspace widens the search again.
        cx.set_state("ˇfoo bar foobar baz fob", Mode::Normal);
        cx.simulate_keystrokes("enter f o b backspace d");
        cx.assert_state("foo bar foobar baz ˇfob", Mode::Normal);

        // Escape cancels, and keys work as usual afterwards.
        cx.set_state("ˇfoo bar", Mode::Normal);
        cx.simulate_keystrokes("enter b a escape l");
        cx.assert_state("fˇoo bar", Mode::Normal);

        // An uppercase letter makes the search case sensitive.
        cx.set_state("ˇxx Ab ab", Mode::Normal);
        cx.simulate_keystrokes("enter A enter");
        cx.assert_state("xx ˇAb ab", Mode::Normal);
        cx.set_state("ˇxx Ab ab", Mode::Normal);
        cx.simulate_keystrokes("enter a s");
        cx.assert_state("xx Ab ˇab", Mode::Normal);
    }

    #[gpui::test]
    async fn test_flash_jump_in_helix_mode(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.enable_helix();
        cx.set_state("ˇone two three", Mode::HelixNormal);
        cx.simulate_keystrokes("enter t h enter");
        cx.assert_state("one two ˇthree", Mode::HelixNormal);
    }

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
    async fn test_flash_find_backward_labels(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;

        // `F` labels the occurrences before the cursor, nearest first.
        cx.set_state("abc abc abc abˇc", Mode::Normal);
        cx.simulate_keystrokes("shift-f c");
        cx.assert_state("abc abc abˇc abc", Mode::Normal);
        cx.simulate_keystrokes("s");
        cx.assert_state("abˇc abc abc abc", Mode::Normal);

        // `T` stops after the occurrence, like `{count}T{char}`.
        cx.set_state("abc abc abc abˇc", Mode::Normal);
        cx.simulate_keystrokes("shift-t a a");
        cx.assert_state("abc abc aˇbc abc", Mode::Normal);

        // Occurrences after the cursor get labels after the ones before it.
        cx.set_state("abc abˇc abc", Mode::Normal);
        cx.simulate_keystrokes("shift-f c a");
        cx.assert_state("abc abc abˇc", Mode::Normal);
    }

    #[gpui::test]
    async fn test_flash_find_in_helix_mode(cx: &mut gpui::TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.enable_helix();

        cx.set_state("ˇabc abc abc", Mode::HelixNormal);
        cx.simulate_keystrokes("f c a");
        cx.assert_state("«abc abcˇ» abc", Mode::HelixNormal);

        cx.set_state("abc abc abˇc", Mode::HelixNormal);
        cx.simulate_keystrokes("shift-f a a");
        cx.assert_state("abc «ˇabc abc»", Mode::HelixNormal);

        cx.set_state("ˇabc abc abc", Mode::HelixNormal);
        cx.simulate_keystrokes("t c a");
        cx.assert_state("«abc abˇ»c abc", Mode::HelixNormal);
    }
}

#[cfg(test)]
mod completion_test {
    use editor::test::editor_lsp_test_context::EditorLspTestContext;

    use crate::{state::Mode, test::VimTestContext};

    async fn context_with_completions(
        cx: &mut gpui::TestAppContext,
        helix: bool,
    ) -> VimTestContext {
        VimTestContext::init(cx);
        let mut cx = VimTestContext::new_with_lsp(
            EditorLspTestContext::new_rust(
                lsp::ServerCapabilities {
                    completion_provider: Some(lsp::CompletionOptions {
                        trigger_characters: Some(vec![".".to_string()]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
            )
            .await,
            true,
        );
        if helix {
            cx.enable_helix();
        }
        // The Linux keymap binds `ctrl-c` differently from the macOS one
        // the test context loads.
        cx.update(|_, cx| {
            cx.clear_key_bindings();
            for path in ["keymaps/default-linux.json", "keymaps/vim.json"] {
                let bindings =
                    settings::KeymapFile::load_asset_allow_partial_failure(path, cx).unwrap();
                cx.bind_keys(bindings);
            }
        });
        cx.set_request_handler::<lsp::request::Completion, _, _>(move |_, _, _| async move {
            Ok(Some(lsp::CompletionResponse::Array(vec![
                lsp::CompletionItem {
                    label: "first".to_string(),
                    ..Default::default()
                },
            ])))
        });
        cx
    }

    /// Opens the completions menu in insert mode, leaves insert mode with
    /// `exit`, and checks that `j` and `k` move right away instead of
    /// waiting for a pending key sequence or the menu.
    async fn assert_keys_move_after_exiting(
        cx: &mut VimTestContext,
        exit: &str,
        normal_mode: Mode,
    ) {
        cx.set_state(
            "fn main() {\n    let a = 1;\n    a.ˇ\n    let b = 2;\n}",
            Mode::Insert,
        );
        cx.simulate_keystroke("f");
        cx.condition(|editor, _| editor.context_menu_visible())
            .await;

        cx.simulate_keystroke(exit);
        cx.update_editor(|editor, _, _| assert!(!editor.context_menu_visible()));
        assert_eq!(cx.mode(), normal_mode);

        // Single keystrokes with no timer advanced: a pending sequence or a
        // menu in the way would leave the cursor where it was.
        let row = |cx: &mut VimTestContext| {
            cx.update_editor(|editor, _, cx| {
                editor
                    .selections
                    .newest::<language::Point>(&editor.display_snapshot(cx))
                    .head()
                    .row
            })
        };
        let start = row(cx);
        cx.simulate_keystroke("j");
        assert_eq!(row(cx), start + 1, "j should move down immediately");
        cx.simulate_keystroke("k");
        assert_eq!(row(cx), start, "k should move up immediately");
        cx.simulate_keystroke("k");
        assert_eq!(row(cx), start - 1, "k should keep moving immediately");
    }

    #[gpui::test]
    async fn test_jk_after_exiting_insert_with_completions(cx: &mut gpui::TestAppContext) {
        let mut cx = context_with_completions(cx, false).await;
        assert_keys_move_after_exiting(&mut cx, "ctrl-c", Mode::Normal).await;
        assert_keys_move_after_exiting(&mut cx, "escape", Mode::Normal).await;
    }

    #[gpui::test]
    async fn test_slow_completions_dont_reopen_after_exiting_insert(cx: &mut gpui::TestAppContext) {
        let mut cx = context_with_completions(cx, false).await;
        let executor = cx.executor();
        cx.set_request_handler::<lsp::request::Completion, _, _>(move |_, _, _| {
            let executor = executor.clone();
            async move {
                executor.timer(std::time::Duration::from_millis(500)).await;
                Ok(Some(lsp::CompletionResponse::List(lsp::CompletionList {
                    // An incomplete list makes every keystroke ask again.
                    is_incomplete: true,
                    items: vec![lsp::CompletionItem {
                        label: "first".to_string(),
                        ..Default::default()
                    }],
                    item_defaults: None,
                })))
            }
        });
        cx.set_state(
            "fn main() {\n    let a = 1;\n    a.ˇ\n    let b = 2;\n}",
            Mode::Insert,
        );
        cx.simulate_keystroke("f");
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        cx.update_editor(|editor, _, _| assert!(editor.context_menu_visible()));

        // Another request is in flight when insert mode ends.
        cx.simulate_keystroke("i");
        cx.simulate_keystroke("ctrl-c");
        assert_eq!(cx.mode(), Mode::Normal);
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        cx.update_editor(|editor, _, _| {
            assert!(
                !editor.context_menu_visible(),
                "the menu reopened after leaving insert mode"
            )
        });
    }

    #[gpui::test]
    async fn test_switch_to_normal_mode_closes_completions(cx: &mut gpui::TestAppContext) {
        let mut cx = context_with_completions(cx, false).await;
        // A user keymap can bind `ctrl-c` to switch modes without the
        // dismissing that `vim::NormalBefore` does.
        cx.update(|_, cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-c",
                crate::SwitchToNormalMode,
                Some("Editor && vim_mode == insert"),
            )])
        });
        cx.set_state(
            "fn main() {\n    let a = 1;\n    a.ˇ\n    let b = 2;\n}",
            Mode::Insert,
        );
        cx.simulate_keystroke("f");
        cx.condition(|editor, _| editor.context_menu_visible())
            .await;
        cx.simulate_keystroke("ctrl-c");
        assert_eq!(cx.mode(), Mode::Normal);
        cx.update_editor(|editor, _, _| assert!(!editor.context_menu_visible()));
        cx.simulate_keystroke("k");
        cx.assert_state(
            "fn main() {\n    letˇ a = 1;\n    a.f\n    let b = 2;\n}",
            Mode::Normal,
        );
    }

    #[gpui::test]
    async fn test_jk_after_exiting_insert_with_completions_in_helix(cx: &mut gpui::TestAppContext) {
        let mut cx = context_with_completions(cx, true).await;
        assert_keys_move_after_exiting(&mut cx, "ctrl-c", Mode::HelixNormal).await;
        assert_keys_move_after_exiting(&mut cx, "escape", Mode::HelixNormal).await;
    }
}
