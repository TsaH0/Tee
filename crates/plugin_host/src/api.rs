//! What plugins can ask the editor to do. A request with an `id` gets a
//! `response` with the same `id`, holding its `result` or an `error`.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use editor::{Editor, MultiBufferOffset, MultiBufferSnapshot};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Focusable as _, Context, DismissEvent, Entity, IntoElement, KeyBinding, KeyContext,
    ParentElement as _, Render, Task, WeakEntity, Window,
};
use language::{CursorShape, DiagnosticSeverity, Point};
use picker::{Picker, PickerDelegate};
use serde::Deserialize;
use serde_json::{Value, json};
use settings::{KeymapFile, KeymapFileLoadResult, MergeFromTrait as _, SettingsStore};
use ui::{HighlightedLabel, Label, ListItem, ListItemSpacing, Toggleable, prelude::*, rems};
use util::ResultExt as _;
use workspace::{ItemHandle, OpenOptions, StatusItemView, Workspace};

use crate::{PluginHost, notify, show_text};

/// A position the way plugins see it: 1-based lines and columns, where
/// columns count characters rather than bytes.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Position {
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub(crate) struct RangeJson {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EditJson {
    pub start: Position,
    pub end: Position,
    pub text: String,
}

/// How decorated text looks. Colors are `#rrggbb` or `#rrggbbaa`, or a
/// theme color: error, warning, info, hint, success, created, deleted,
/// modified, accent or muted.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct StyleJson {
    pub color: Option<String>,
    pub background: Option<String>,
    pub underline: Option<String>,
    pub wavy: bool,
    pub strikethrough: bool,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HighlightJson {
    pub start: Position,
    pub end: Position,
    #[serde(flatten)]
    pub style: StyleJson,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VirtualTextJson {
    pub position: Position,
    pub text: String,
    #[serde(flatten)]
    pub style: StyleJson,
}

fn color(name: &str, cx: &App) -> Result<gpui::Hsla> {
    let status = cx.theme().status();
    let colors = cx.theme().colors();
    Ok(match name {
        "error" => status.error,
        "warning" => status.warning,
        "info" => status.info,
        "hint" => status.hint,
        "success" => status.success,
        "created" => status.created,
        "deleted" => status.deleted,
        "modified" => status.modified,
        "accent" => colors.text_accent,
        "muted" => colors.text_muted,
        hex => gpui::Rgba::try_from(hex)
            .map(gpui::Hsla::from)
            .map_err(|_| anyhow!("unknown color {hex:?}"))?,
    })
}

fn highlight_style(style: &StyleJson, cx: &App) -> Result<gpui::HighlightStyle> {
    let mut highlight = gpui::HighlightStyle::default();
    if let Some(name) = &style.color {
        highlight.color = Some(color(name, cx)?);
    }
    if let Some(name) = &style.background {
        highlight.background_color = Some(color(name, cx)?);
    }
    if let Some(name) = &style.underline {
        highlight.underline = Some(gpui::UnderlineStyle {
            thickness: gpui::px(1.),
            color: Some(color(name, cx)?),
            wavy: style.wavy,
        });
    }
    if style.strikethrough {
        highlight.strikethrough = Some(gpui::StrikethroughStyle {
            thickness: gpui::px(1.),
            color: None,
        });
    }
    if style.bold {
        highlight.font_weight = Some(gpui::FontWeight::BOLD);
    }
    if style.italic {
        highlight.font_style = Some(gpui::FontStyle::Italic);
    }
    Ok(highlight)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum PickItem {
    Label(String),
    Described {
        label: String,
        #[serde(default)]
        description: Option<String>,
    },
}

impl PickItem {
    fn label(&self) -> &str {
        match self {
            PickItem::Label(label) | PickItem::Described { label, .. } => label,
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            PickItem::Label(_) => None,
            PickItem::Described { description, .. } => description.as_deref(),
        }
    }
}

/// Requests a plugin can send.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum PluginRequest {
    /// Shows a notification.
    Notify {
        message: String,
        #[serde(default)]
        level: Option<String>,
    },
    /// Opens a file, optionally at a 1-based line and column.
    Open {
        path: PathBuf,
        #[serde(default)]
        line: Option<u32>,
        #[serde(default)]
        column: Option<u32>,
    },
    /// Shows text in a read-only tab, replacing what an earlier `show` with
    /// the same title showed.
    Show {
        title: String,
        text: String,
        #[serde(default)]
        language: Option<String>,
        #[serde(flatten)]
        options: crate::ShowOptions,
    },
    /// Replaces every selection (or inserts at every cursor) in the active
    /// editor.
    Insert { text: String },
    /// Replaces the whole text of the active editor.
    SetText { text: String },
    /// Dispatches any action by name, like a key binding would.
    Dispatch {
        action: String,
        #[serde(default)]
        args: Option<Value>,
    },
    /// The active editor's file, language, cursor and selections, and its
    /// text when `text` is true. `null` when no editor is active.
    GetEditor {
        #[serde(default)]
        text: bool,
        #[serde(default)]
        file: Option<PathBuf>,
    },
    /// The active editor's text, or the text between `start` and `end`.
    GetText {
        #[serde(default)]
        file: Option<PathBuf>,
        #[serde(default)]
        start: Option<Position>,
        #[serde(default)]
        end: Option<Position>,
    },
    /// Replaces the active editor's selections. Each `end` is where the
    /// cursor goes, so an `end` before its `start` selects backwards.
    SetSelections {
        selections: Vec<RangeJson>,
        #[serde(default)]
        file: Option<PathBuf>,
    },
    /// Replaces ranges of the active editor's text, as one undoable edit.
    Edit {
        edits: Vec<EditJson>,
        #[serde(default)]
        file: Option<PathBuf>,
    },
    /// Sets the active editor's mode, which key bindings can match with
    /// `fool_mode == <mode>` in their context, and optionally its cursor.
    /// `null` leaves the mode.
    SetMode {
        mode: Option<String>,
        #[serde(default)]
        file: Option<PathBuf>,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Shows text in the status bar, or removes it when `null`.
    SetStatus { text: Option<String> },
    /// Asks the user to pick one of `items`, each a string or a `label`
    /// with a `description`. The result is the chosen item's index, or
    /// `null` when the picker is dismissed. A `live` picker sends the plugin
    /// a `pick_query` message as the user types, and shows the items the
    /// plugin answers with `update_pick`, instead of filtering them itself.
    Pick {
        items: Vec<PickItem>,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        live: bool,
    },
    /// Replaces the items of the live picker opened by the `pick` request
    /// with id `pick`.
    UpdatePick { pick: Value, items: Vec<PickItem> },
    /// Adds a command to the plugin, or replaces one with the same name. It
    /// shows up in `plugins: open command picker` and as `:<vim>`.
    RegisterCommand {
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        vim: Option<String>,
    },
    /// Turns a minor mode on or off in an editor. Key bindings match a
    /// minor mode by its name in their context, like `Editor && my_mode`,
    /// and any number can be on at once, alongside `fool_mode`.
    SetMinorMode {
        name: String,
        enabled: bool,
        #[serde(default)]
        file: Option<PathBuf>,
    },
    /// Runs a shell command in the terminal panel. The result is its exit
    /// code once it finishes, or `null` if it couldn't run.
    RunInTerminal {
        command: String,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    /// Asks the user to type text. The result is the text, or `null` when
    /// the prompt is dismissed.
    Prompt {
        #[serde(default)]
        placeholder: Option<String>,
    },
    /// The user's settings, as in `settings.json`.
    GetSettings {},
    /// Merges `settings` into the user's `settings.json`. This changes
    /// anything settings control, from the theme and panels to language
    /// servers (`lsp`) and languages.
    UpdateSettings { settings: Value },
    /// The active editor's diagnostics.
    GetDiagnostics {
        #[serde(default)]
        file: Option<PathBuf>,
    },
    /// Highlights text and shows virtual text, replacing what the plugin's
    /// `namespace` showed in that editor before. Both move with edits.
    SetDecorations {
        namespace: String,
        #[serde(default)]
        file: Option<PathBuf>,
        #[serde(default)]
        highlights: Vec<HighlightJson>,
        #[serde(default)]
        virtual_text: Vec<VirtualTextJson>,
    },
    /// Removes what the plugin's `namespace` shows in an editor.
    ClearDecorations {
        namespace: String,
        #[serde(default)]
        file: Option<PathBuf>,
    },
    /// The open editors: each one's `file`, `title`, `language`, whether it
    /// has unsaved changes (`dirty`) and whether it's the active one.
    GetBuffers {},
    /// The names of every action, for `dispatch` and key bindings.
    GetActions {},
    /// Adds key bindings, in the same format as `keymap.json`.
    BindKeys { keymap: Value },
    /// Every key binding, or those of `action`: each binding's `keys`,
    /// `context` and `action`, from lowest to highest precedence.
    GetBindings {
        #[serde(default)]
        action: Option<String>,
    },
    /// Makes every key bound to `action` run the plugin's `command` instead,
    /// in the same contexts. The command can still `dispatch` the action, to
    /// add behavior before or after it.
    OverrideAction {
        action: String,
        command: String,
        #[serde(default)]
        args: Option<String>,
    },
    /// Sends every keystroke in editors whose mode is `mode` to the plugin as
    /// a `key` message instead of handling it, except the keys in `except`.
    CaptureKeys {
        mode: String,
        #[serde(default)]
        except: Vec<String>,
    },
    /// Stops capturing the keystrokes of `mode`.
    ReleaseKeys { mode: String },
}

/// A mode whose keystrokes go to a plugin.
pub(crate) struct KeyCapture {
    pub plugin: String,
    pub except: Vec<String>,
}

/// Where the response to a request goes.
#[derive(Clone)]
pub(crate) struct Responder {
    pub plugin: String,
    pub id: Option<Value>,
}

impl Responder {
    pub(crate) fn respond(&self, result: Result<Value>, cx: &mut App) {
        let Some(id) = &self.id else {
            // Without an id there's no one to tell about an error but the
            // user.
            if let Err(error) = result {
                notify(format!("{}: {error:#}", self.plugin), true, cx);
            }
            return;
        };
        let message = match result {
            Ok(result) => json!({ "type": "response", "id": id, "result": result }),
            Err(error) => json!({ "type": "response", "id": id, "error": format!("{error:#}") }),
        };
        crate::send_to_plugin(&self.plugin, message.to_string(), cx);
    }
}

/// Counts characters, not bytes, so that plugins in any language agree with
/// the editor about columns.
pub(crate) fn position_of(snapshot: &MultiBufferSnapshot, point: Point) -> Value {
    let line_start = Point::new(point.row, 0);
    let column = snapshot
        .text_for_range(line_start..point)
        .map(|chunk| chunk.chars().count())
        .sum::<usize>();
    json!({ "line": point.row + 1, "column": column + 1 })
}

pub(crate) fn point_of(snapshot: &MultiBufferSnapshot, position: Position) -> Point {
    let row = position.line.saturating_sub(1).min(snapshot.max_point().row);
    let line_end = Point::new(row, snapshot.line_len(multi_buffer::MultiBufferRow(row)));
    let mut remaining = position.column.saturating_sub(1) as usize;
    let mut column = 0;
    'chunks: for chunk in snapshot.text_for_range(Point::new(row, 0)..line_end) {
        for character in chunk.chars() {
            if remaining == 0 {
                break 'chunks;
            }
            remaining -= 1;
            column += character.len_utf8() as u32;
        }
    }
    Point::new(row, column)
}

/// What a command or `get_editor` knows about an editor.
pub(crate) fn editor_context(editor: &mut Editor, text: bool, cx: &mut Context<Editor>) -> Value {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let display_snapshot = editor.display_snapshot(cx);
    let selections = editor
        .selections
        .all::<Point>(&display_snapshot)
        .into_iter()
        .map(|selection| {
            let (start, end) = if selection.reversed {
                (selection.end, selection.start)
            } else {
                (selection.start, selection.end)
            };
            json!({
                "start": position_of(&snapshot, start),
                "end": position_of(&snapshot, end),
                "text": snapshot
                    .text_for_range(selection.start..selection.end)
                    .collect::<String>(),
            })
        })
        .collect::<Vec<_>>();
    let cursor = editor.selections.newest::<Point>(&display_snapshot).head();
    let language = editor.buffer().read(cx).as_singleton().and_then(|buffer| {
        buffer
            .read(cx)
            .language()
            .map(|language| language.name().to_string())
    });
    let mode = editor
        .addon::<PluginModeAddon>()
        .and_then(|addon| addon.mode.clone());
    let mut context = json!({
        "file": crate::editor_path(editor, cx),
        "language": language,
        "cursor": position_of(&snapshot, cursor),
        "selections": selections,
        "mode": mode,
        "line_count": snapshot.max_point().row + 1,
    });
    if text {
        context["text"] = Value::String(snapshot.text());
    }
    context
}

/// The `fool_mode` a plugin gave an editor, and the minor modes it turned
/// on, for key bindings to match.
#[derive(Clone, Default)]
pub(crate) struct PluginModeAddon {
    pub mode: Option<String>,
    pub minor_modes: std::collections::BTreeSet<String>,
}

impl PluginModeAddon {
    pub(crate) fn new(mode: Option<String>) -> Self {
        Self {
            mode,
            minor_modes: Default::default(),
        }
    }
}

impl editor::Addon for PluginModeAddon {
    fn extend_key_context(&self, key_context: &mut KeyContext, _: &App) {
        if let Some(mode) = &self.mode {
            key_context.set("fool_mode", mode.clone());
        }
        for minor_mode in &self.minor_modes {
            key_context.add(minor_mode.clone());
        }
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn cursor_shape(name: &str) -> Result<CursorShape> {
    Ok(match name {
        "bar" => CursorShape::Bar,
        "block" => CursorShape::Block,
        "underline" => CursorShape::Underline,
        "hollow" => CursorShape::Hollow,
        _ => anyhow::bail!("unknown cursor {name:?}; use bar, block, underline or hollow"),
    })
}

/// The open editor of `file`, or the active editor.
fn target_editor(
    workspace: &Workspace,
    file: Option<&std::path::Path>,
    cx: &App,
) -> Result<Entity<Editor>> {
    let Some(file) = file else {
        return workspace
            .active_item_as::<Editor>(cx)
            .context("no editor is active");
    };
    // A plugin's own `show` buffers have no file, so they go by title.
    let title = file.to_string_lossy();
    workspace
        .items_of_type::<Editor>(cx)
        .find(|editor| crate::editor_path(editor.read(cx), cx).as_deref() == Some(file))
        .or_else(|| {
            workspace.items_of_type::<Editor>(cx).find(|editor| {
                let editor = editor.read(cx);
                editor_path_is_none(editor, cx) && editor.buffer().read(cx).title(cx) == title
            })
        })
        .with_context(|| format!("{} isn't open; `open` it first", file.display()))
}

fn editor_path_is_none(editor: &Editor, cx: &App) -> bool {
    crate::editor_path(editor, cx).is_none()
}

fn active_editor(workspace: &Workspace, cx: &App) -> Result<Entity<Editor>> {
    target_editor(workspace, None, cx)
}

/// A number for each plugin's decoration namespace, the same in every
/// editor.
fn namespace_id(plugin: &str, namespace: &str, cx: &mut App) -> u32 {
    let host = cx.global_mut::<PluginHost>();
    let next = host.namespaces.len() as u32;
    *host
        .namespaces
        .entry((plugin.to_string(), namespace.to_string()))
        .or_insert(next)
}

/// Handles a request in `workspace`, answering through `responder` either
/// right away or, for requests that wait on the user, later.
pub(crate) fn apply_request(
    request: PluginRequest,
    responder: Responder,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let plugin = responder.plugin.clone();
    let result = match request {
        PluginRequest::Pick {
            items,
            placeholder,
            live,
        } => {
            if live && responder.id.is_none() {
                responder.respond(Err(anyhow!("a live pick needs an id")), cx);
                return;
            }
            open_pick(items, placeholder, live, responder, workspace, window, cx);
            return;
        }
        PluginRequest::Show {
            title,
            text,
            language,
            options,
        } => {
            // Answer once the buffer exists, so that the plugin can use it.
            let shown = show_text(&plugin, title, text, language, options, workspace, window, cx);
            cx.spawn(async move |_, cx| {
                let result = shown.await.map(|()| Value::Null);
                cx.update(|cx| responder.respond(result, cx));
            })
            .detach();
            return;
        }
        PluginRequest::RunInTerminal { command, cwd } => {
            let cwd = cwd.or_else(|| {
                workspace
                    .root_paths(cx)
                    .first()
                    .map(|path| path.to_path_buf())
            });
            let spawn = task::SpawnInTerminal {
                id: task::TaskId(format!("plugin-{plugin}")),
                full_label: command.clone(),
                label: command.clone(),
                command: Some(command.clone()),
                args: Vec::new(),
                command_label: command,
                cwd,
                env: Default::default(),
                use_new_terminal: true,
                allow_concurrent_runs: true,
                reveal: task::RevealStrategy::Always,
                reveal_target: task::RevealTarget::Dock,
                hide: task::HideStrategy::Never,
                shell: task::Shell::System,
                show_summary: true,
                show_command: true,
                show_rerun: true,
                save: task::SaveStrategy::default(),
            };
            let status = workspace.spawn_in_terminal(spawn, window, cx);
            cx.spawn(async move |_, cx| {
                let result = match status.await {
                    Some(Ok(status)) => Ok(json!(status.code())),
                    Some(Err(error)) => Err(error),
                    None => Ok(Value::Null),
                };
                cx.update(|cx| responder.respond(result, cx));
            })
            .detach();
            return;
        }
        PluginRequest::Prompt { placeholder } => {
            open_prompt(placeholder, responder, workspace, window, cx);
            return;
        }
        request => apply_immediate(&plugin, request, workspace, window, cx),
    };
    responder.respond(result, cx);
}

fn apply_immediate(
    plugin: &str,
    request: PluginRequest,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<Value> {
    match request {
        PluginRequest::Notify { message, level } => {
            notify(format!("{plugin}: {message}"), level.as_deref() == Some("error"), cx);
        }
        PluginRequest::Open { path, line, column } => {
            let open = workspace.open_abs_path(path, OpenOptions::default(), window, cx);
            cx.spawn_in(window, async move |_, cx| {
                let item = open.await?;
                if let Some(line) = line
                    && let Some(editor) = cx.update(|_, cx| item.act_as::<Editor>(cx))?
                {
                    editor.update_in(cx, |editor, window, cx| {
                        let snapshot = editor.buffer().read(cx).snapshot(cx);
                        let point = point_of(
                            &snapshot,
                            Position {
                                line,
                                column: column.unwrap_or(1),
                            },
                        );
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select_ranges([point..point])
                        });
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }
        PluginRequest::Insert { text } => {
            let editor = active_editor(workspace, cx)?;
            editor.update(cx, |editor, cx| editor.insert(&text, window, cx));
        }
        PluginRequest::SetText { text } => {
            let editor = active_editor(workspace, cx)?;
            editor.update(cx, |editor, cx| {
                let end = editor.buffer().read(cx).snapshot(cx).len();
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([MultiBufferOffset(0)..end])
                });
                editor.insert(&text, window, cx);
            });
        }
        PluginRequest::Dispatch { action, args } => {
            let action = cx
                .build_action(&action, args)
                .map_err(|error| anyhow!("{error}"))?;
            window.dispatch_action(action, cx);
        }
        PluginRequest::GetEditor { text, file } => {
            let editor = match file {
                Some(file) => target_editor(workspace, Some(&file), cx)?,
                None => {
                    let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
                        return Ok(Value::Null);
                    };
                    editor
                }
            };
            return Ok(editor.update(cx, |editor, cx| editor_context(editor, text, cx)));
        }
        PluginRequest::GetText { file, start, end } => {
            let editor = target_editor(workspace, file.as_deref(), cx)?;
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            let start = start.map_or(Point::zero(), |start| point_of(&snapshot, start));
            let end = end.map_or(snapshot.max_point(), |end| point_of(&snapshot, end));
            let (start, end) = (start.min(end), start.max(end));
            return Ok(Value::String(
                snapshot.text_for_range(start..end).collect::<String>(),
            ));
        }
        PluginRequest::SetSelections { selections, file } => {
            anyhow::ensure!(!selections.is_empty(), "there must be at least one selection");
            let editor = target_editor(workspace, file.as_deref(), cx)?;
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let ranges = selections
                    .iter()
                    .map(|range| point_of(&snapshot, range.start)..point_of(&snapshot, range.end))
                    .collect::<Vec<_>>();
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges(ranges)
                });
            });
        }
        PluginRequest::Edit { edits, file } => {
            let editor = target_editor(workspace, file.as_deref(), cx)?;
            anyhow::ensure!(
                !editor.read(cx).read_only(cx),
                "the active editor is read-only"
            );
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let mut edits = edits
                    .into_iter()
                    .map(|edit| {
                        let start = point_of(&snapshot, edit.start);
                        let end = point_of(&snapshot, edit.end);
                        (start.min(end)..start.max(end), edit.text)
                    })
                    .collect::<Vec<_>>();
                edits.sort_by_key(|(range, _)| range.start);
                editor.edit(edits, cx);
            });
        }
        PluginRequest::SetMode { mode, file, cursor } => {
            let editor = target_editor(workspace, file.as_deref(), cx)?;
            let shape = cursor.as_deref().map(cursor_shape).transpose()?;
            editor.update(cx, |editor, cx| {
                let mut addon = editor.addon::<PluginModeAddon>().cloned().unwrap_or_default();
                addon.mode = mode;
                editor.register_addon(addon);
                if let Some(shape) = shape {
                    editor.set_cursor_shape(shape, cx);
                }
                cx.notify();
            });
        }
        PluginRequest::SetStatus { text } => {
            set_status(plugin, text, cx);
        }
        PluginRequest::Pick { .. }
        | PluginRequest::Prompt { .. }
        | PluginRequest::Show { .. }
        | PluginRequest::RunInTerminal { .. } => {
            unreachable!("handled by apply_request")
        }
        PluginRequest::UpdatePick { pick, items } => {
            let picker = cx
                .global::<PluginHost>()
                .live_picks
                .get(&pick.to_string())
                .and_then(|picker| picker.upgrade())
                .context("that picker is closed")?;
            picker.update(cx, |picker, cx| {
                picker.delegate.set_items(items);
                cx.notify();
            });
        }
        PluginRequest::RegisterCommand {
            name,
            description,
            vim,
        } => {
            let host = cx.global_mut::<PluginHost>();
            let manifest = &mut host
                .plugins
                .iter_mut()
                .find(|candidate| candidate.manifest.name == plugin)
                .context("the plugin isn't loaded")?
                .manifest;
            manifest.commands.retain(|command| command.name != name);
            manifest.commands.push(crate::ManifestCommand {
                name,
                description,
                vim,
                wants_text: false,
            });
        }
        PluginRequest::SetMinorMode {
            name,
            enabled,
            file,
        } => {
            anyhow::ensure!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
                    && !name.starts_with(|character: char| character.is_ascii_digit()),
                "a minor mode's name must be letters, digits and underscores, like my_mode"
            );
            let editor = target_editor(workspace, file.as_deref(), cx)?;
            editor.update(cx, |editor, cx| {
                let mut addon = editor.addon::<PluginModeAddon>().cloned().unwrap_or_default();
                if enabled {
                    addon.minor_modes.insert(name);
                } else {
                    addon.minor_modes.remove(&name);
                }
                editor.register_addon(addon);
                cx.notify();
            });
        }
        PluginRequest::GetSettings {} => {
            let settings = cx
                .global::<SettingsStore>()
                .raw_user_settings()
                .map(|settings| serde_json::to_value(&settings.content))
                .transpose()?;
            return Ok(settings.unwrap_or(Value::Object(Default::default())));
        }
        PluginRequest::UpdateSettings { settings } => {
            let patch: settings::SettingsContent = serde_json::from_value(settings)
                .context("the settings aren't valid")?;
            let fs = workspace.app_state().fs.clone();
            settings::update_settings_file(fs, cx, move |settings, _| {
                settings.merge_from(&patch);
            });
        }
        PluginRequest::GetDiagnostics { file } => {
            let editor = target_editor(workspace, file.as_deref(), cx)?;
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            let diagnostics = snapshot
                .diagnostics_in_range::<Point>(Point::zero()..snapshot.max_point())
                .map(|entry| {
                    let severity = match entry.diagnostic.severity {
                        DiagnosticSeverity::ERROR => "error",
                        DiagnosticSeverity::WARNING => "warning",
                        DiagnosticSeverity::INFORMATION => "information",
                        _ => "hint",
                    };
                    json!({
                        "start": position_of(&snapshot, entry.range.start),
                        "end": position_of(&snapshot, entry.range.end),
                        "severity": severity,
                        "message": entry.diagnostic.message,
                        "source": entry.diagnostic.source,
                    })
                })
                .collect::<Vec<_>>();
            return Ok(Value::Array(diagnostics));
        }
        PluginRequest::SetDecorations {
            namespace,
            file,
            highlights,
            virtual_text,
        } => {
            let editor = target_editor(workspace, file.as_deref(), cx)?;
            let namespace = namespace_id(plugin, &namespace, cx);
            let highlight_styles = highlights
                .iter()
                .map(|highlight| highlight_style(&highlight.style, cx))
                .collect::<Result<Vec<_>>>()?;
            let virtual_text_styles = virtual_text
                .iter()
                .map(|text| highlight_style(&text.style, cx))
                .collect::<Result<Vec<_>>>()?;
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let highlights = highlights
                    .iter()
                    .zip(highlight_styles)
                    .map(|(highlight, style)| {
                        let start = point_of(&snapshot, highlight.start);
                        let end = point_of(&snapshot, highlight.end);
                        let (start, end) = (start.min(end), start.max(end));
                        (
                            snapshot.anchor_after(start)..snapshot.anchor_before(end),
                            style,
                        )
                    })
                    .collect();
                let virtual_text = virtual_text
                    .into_iter()
                    .zip(virtual_text_styles)
                    .map(|(text, style)| {
                        let position = snapshot.anchor_after(point_of(&snapshot, text.position));
                        (position, text.text, style)
                    })
                    .collect();
                editor.set_plugin_decorations(namespace, highlights, virtual_text, cx);
            });
        }
        PluginRequest::ClearDecorations { namespace, file } => {
            let editor = target_editor(workspace, file.as_deref(), cx)?;
            let namespace = namespace_id(plugin, &namespace, cx);
            editor.update(cx, |editor, cx| editor.clear_plugin_decorations(namespace, cx));
        }
        PluginRequest::GetBuffers {} => {
            let active = workspace.active_item_as::<Editor>(cx);
            let buffers = workspace
                .items_of_type::<Editor>(cx)
                .map(|editor| {
                    let is_active = active.as_ref() == Some(&editor);
                    let editor = editor.read(cx);
                    let buffer = editor.buffer().read(cx);
                    let language = buffer.as_singleton().and_then(|buffer| {
                        buffer
                            .read(cx)
                            .language()
                            .map(|language| language.name().to_string())
                    });
                    json!({
                        "file": crate::editor_path(editor, cx),
                        "title": buffer.title(cx).to_string(),
                        "language": language,
                        "dirty": buffer.is_dirty(cx),
                        "active": is_active,
                    })
                })
                .collect::<Vec<_>>();
            return Ok(Value::Array(buffers));
        }
        PluginRequest::GetActions {} => {
            return Ok(json!(cx.all_action_names()));
        }
        PluginRequest::BindKeys { keymap } => {
            bind_plugin_keys(plugin, &keymap, cx)?;
        }
        PluginRequest::GetBindings { action } => {
            return Ok(Value::Array(
                bindings_of(action.as_deref(), cx)
                    .into_iter()
                    .map(|binding| {
                        json!({
                            "keys": binding.keys,
                            "context": binding.context,
                            "action": binding.action,
                        })
                    })
                    .collect(),
            ));
        }
        PluginRequest::OverrideAction {
            action,
            command,
            args,
        } => {
            let bindings = bindings_of(Some(&action), cx);
            anyhow::ensure!(!bindings.is_empty(), "no keys are bound to {action}");
            let mut blocks: BTreeMap<Option<String>, serde_json::Map<String, Value>> =
                BTreeMap::new();
            for binding in &bindings {
                blocks.entry(binding.context.clone()).or_default().insert(
                    binding.keys.clone(),
                    json!([
                        "plugins::RunCommand",
                        { "plugin": plugin, "command": command, "args": args }
                    ]),
                );
            }
            let keymap = blocks
                .into_iter()
                .map(|(context, bindings)| match context {
                    Some(context) => json!({ "context": context, "bindings": bindings }),
                    None => json!({ "bindings": bindings }),
                })
                .collect::<Vec<_>>();
            bind_plugin_keys(plugin, &Value::Array(keymap), cx)?;
            return Ok(json!({ "rebound": bindings.len() }));
        }
        PluginRequest::CaptureKeys { mode, except } => {
            cx.global_mut::<PluginHost>().captures.insert(
                mode,
                KeyCapture {
                    plugin: plugin.to_string(),
                    except,
                },
            );
        }
        PluginRequest::ReleaseKeys { mode } => {
            cx.global_mut::<PluginHost>().captures.remove(&mode);
        }
    }
    Ok(Value::Null)
}

fn bind_plugin_keys(plugin: &str, keymap: &Value, cx: &mut App) -> Result<()> {
    let source = serde_json::to_string(keymap)?;
    let bindings = parse_keymap(&source, cx)?;
    cx.global_mut::<PluginHost>()
        .keymaps
        .push((plugin.to_string(), source));
    cx.bind_keys(bindings);
    Ok(())
}

pub(crate) struct BindingInfo {
    pub keys: String,
    pub context: Option<String>,
    pub action: String,
}

/// The key bindings, or those of `action`, lowest precedence first.
pub(crate) fn bindings_of(action: Option<&str>, cx: &App) -> Vec<BindingInfo> {
    let os = cx.platform_info().os;
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    keymap
        .bindings()
        .filter(|binding| action.is_none_or(|action| binding.action().name() == action))
        .map(|binding| BindingInfo {
            keys: binding
                .keystrokes()
                .iter()
                .map(|keystroke| keystroke.unparse(os))
                .collect::<Vec<_>>()
                .join(" "),
            context: binding.predicate().map(|predicate| predicate.to_string()),
            action: binding.action().name().to_string(),
        })
        .collect()
}

/// Sends `keystroke` to the plugin capturing the focused editor's mode,
/// returning whether it was captured.
pub(crate) fn capture_keystroke(
    keystroke: &gpui::Keystroke,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if cx.global::<PluginHost>().captures.is_empty() {
        return false;
    }
    let Some(Some(multi_workspace)) = window.root::<workspace::MultiWorkspace>() else {
        return false;
    };
    let workspace = multi_workspace.read(cx).workspace().clone();
    let Some(editor) = workspace.read(cx).active_item_as::<Editor>(cx) else {
        return false;
    };
    if !editor.focus_handle(cx).is_focused(window) {
        return false;
    }
    let Some(mode) = editor
        .read(cx)
        .addon::<PluginModeAddon>()
        .and_then(|addon| addon.mode.clone())
    else {
        return false;
    };
    let os = cx.platform_info().os;
    let key = keystroke.unparse(os);
    let Some(capture) = cx.global::<PluginHost>().captures.get(&mode) else {
        return false;
    };
    if capture.except.iter().any(|except| *except == key) {
        return false;
    }
    let plugin = capture.plugin.clone();
    let message = json!({ "type": "key", "key": key, "mode": mode }).to_string();
    crate::send_to_plugin(&plugin, message, cx);
    true
}

pub(crate) fn parse_keymap(source: &str, cx: &App) -> Result<Vec<KeyBinding>> {
    match KeymapFile::load(source, cx) {
        KeymapFileLoadResult::Success { key_bindings } => Ok(key_bindings),
        KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
            Err(anyhow!("{}", error_message.0))
        }
        KeymapFileLoadResult::JsonParseFailure { error } => Err(error),
    }
}

/// Key bindings from plugins' `keymap` files and `bind_keys` requests, for
/// the editor to load after the built-in modal keymap and before
/// `keymap.json`, so that the user's own bindings still win.
pub fn plugin_key_bindings(cx: &App) -> Vec<KeyBinding> {
    let Some(host) = cx.try_global::<PluginHost>() else {
        return Vec::new();
    };
    host.keymaps
        .iter()
        .flat_map(|(plugin, source)| match parse_keymap(source, cx) {
            Ok(bindings) => bindings,
            Err(error) => {
                log::error!("plugins: {plugin}'s key bindings: {error:#}");
                Vec::new()
            }
        })
        .collect()
}

fn set_status(plugin: &str, text: Option<String>, cx: &mut App) {
    let host = cx.global_mut::<PluginHost>();
    match text {
        Some(text) => {
            host.statuses.insert(plugin.to_string(), text);
        }
        None => {
            host.statuses.remove(plugin);
        }
    }
    host.status_items.retain(|item| item.upgrade().is_some());
    let items = host.status_items.clone();
    for item in items {
        item.update(cx, |_, cx| cx.notify()).ok();
    }
}

/// Shows the plugins' `set_status` texts in the status bar.
pub(crate) struct PluginStatusItem;

impl Render for PluginStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let statuses: &BTreeMap<String, String> = &cx.global::<PluginHost>().statuses;
        let text = statuses.values().cloned().collect::<Vec<_>>().join("  ");
        h_flex().when(!text.is_empty(), |this| {
            this.child(Label::new(text).size(LabelSize::Small))
        })
    }
}

impl StatusItemView for PluginStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // Empty unless a plugin shows something; plugins remove their own text.
        None
    }
}

fn open_pick(
    items: Vec<PickItem>,
    placeholder: Option<String>,
    live: bool,
    responder: Responder,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let live_pick = live.then(|| {
        (
            responder.plugin.clone(),
            responder.id.clone().unwrap_or(Value::Null),
        )
    });
    workspace.toggle_modal(window, cx, move |window, cx| {
        let mut delegate = PickDelegate {
            placeholder: placeholder.unwrap_or_else(|| "Pick…".to_string()).into(),
            items: Vec::new(),
            candidates: Arc::from([]),
            matches: Vec::new(),
            selected_index: 0,
            live: live_pick,
            responder: Some(responder),
        };
        delegate.set_items(items);
        Picker::uniform_list(delegate, window, cx).initial_width(rems(48.))
    });
    if live {
        let picker = workspace.active_modal::<Picker<PickDelegate>>(cx);
        if let Some(picker) = picker {
            let pick = picker
                .read(cx)
                .delegate
                .live
                .as_ref()
                .map(|(_, pick)| pick.to_string());
            if let Some(pick) = pick {
                cx.global_mut::<PluginHost>()
                    .live_picks
                    .insert(pick, picker.downgrade());
            }
        }
    }
}

pub(crate) struct PickDelegate {
    placeholder: Arc<str>,
    items: Vec<PickItem>,
    candidates: Arc<[StringMatchCandidate]>,
    pub(crate) matches: Vec<StringMatch>,
    selected_index: usize,
    /// The plugin and pick id of a live picker, which the plugin filters.
    live: Option<(String, Value)>,
    /// Taken when the answer is sent, so that it's sent once.
    responder: Option<Responder>,
}

impl PickDelegate {
    fn set_items(&mut self, items: Vec<PickItem>) {
        self.candidates = items
            .iter()
            .enumerate()
            .map(|(index, item)| StringMatchCandidate::new(index, item.label()))
            .collect();
        self.items = items;
        self.matches = self
            .candidates
            .iter()
            .map(|candidate| StringMatch {
                candidate_id: candidate.id,
                string: candidate.string.clone(),
                positions: Vec::new(),
                score: 0.0,
            })
            .collect();
        self.selected_index = 0;
    }
}

impl PickerDelegate for PickDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "plugin pick"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = index;
        cx.notify();
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        self.placeholder.clone()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if let Some((plugin, pick)) = &self.live {
            // The plugin chooses the items for the query.
            let message = json!({ "type": "pick_query", "pick": pick, "query": query });
            crate::send_to_plugin(plugin, message.to_string(), cx);
            return Task::ready(());
        }
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
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    candidates.len(),
                    &Default::default(),
                    background,
                )
                .await
            };
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(found) = self.matches.get(self.selected_index) else {
            return;
        };
        if let Some(responder) = self.responder.take() {
            responder.respond(Ok(json!(found.candidate_id)), cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(responder) = self.responder.take() {
            responder.respond(Ok(Value::Null), cx);
        }
        if let Some((_, pick)) = &self.live {
            cx.global_mut::<PluginHost>()
                .live_picks
                .remove(&pick.to_string());
        }
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(index)?;
        let description = self
            .items
            .get(found.candidate_id)
            .and_then(PickItem::description)
            .map(str::to_string);
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .gap_2()
                        .child(HighlightedLabel::new(
                            found.string.clone(),
                            found.positions.clone(),
                        ))
                        .when_some(description, |this, description| {
                            this.child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                ),
        )
    }
}

fn open_prompt(
    placeholder: Option<String>,
    responder: Responder,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, move |window, cx| {
        Picker::uniform_list(
            PromptDelegate {
                placeholder: placeholder.unwrap_or_else(|| "Type…".to_string()).into(),
                query: String::new(),
                responder: Some(responder),
            },
            window,
            cx,
        )
        .initial_width(rems(40.))
    });
}

struct PromptDelegate {
    placeholder: Arc<str>,
    query: String,
    responder: Option<Responder>,
}

impl PickerDelegate for PromptDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "plugin prompt"
    }

    fn match_count(&self) -> usize {
        usize::from(!self.query.is_empty())
    }

    fn selected_index(&self) -> usize {
        0
    }

    fn set_selected_index(&mut self, _: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        self.placeholder.clone()
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query;
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(responder) = self.responder.take() {
            responder.respond(Ok(Value::String(self.query.clone())), cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(responder) = self.responder.take() {
            responder.respond(Ok(Value::Null), cx);
        }
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .child(Label::new(self.query.clone())),
        )
    }
}

/// Keeps a weak handle so that closed workspaces' items can be dropped.
pub(crate) fn status_item_handle(item: &Entity<PluginStatusItem>) -> WeakEntity<PluginStatusItem> {
    item.downgrade()
}
