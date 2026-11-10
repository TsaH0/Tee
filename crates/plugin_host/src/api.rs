//! What plugins can ask the editor to do. A request with an `id` gets a
//! `response` with the same `id`, holding its `result` or an `error`.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use editor::{Editor, MultiBufferOffset, MultiBufferSnapshot};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, IntoElement, KeyBinding, KeyContext,
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
    },
    /// The active editor's text, or the text between `start` and `end`.
    GetText {
        #[serde(default)]
        start: Option<Position>,
        #[serde(default)]
        end: Option<Position>,
    },
    /// Replaces the active editor's selections. Each `end` is where the
    /// cursor goes, so an `end` before its `start` selects backwards.
    SetSelections { selections: Vec<RangeJson> },
    /// Replaces ranges of the active editor's text, as one undoable edit.
    Edit { edits: Vec<EditJson> },
    /// Sets the active editor's mode, which key bindings can match with
    /// `fool_mode == <mode>` in their context, and optionally its cursor.
    /// `null` leaves the mode.
    SetMode {
        mode: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Shows text in the status bar, or removes it when `null`.
    SetStatus { text: Option<String> },
    /// Asks the user to pick one of `items`. The result is its index, or
    /// `null` when the picker is dismissed.
    Pick {
        items: Vec<String>,
        #[serde(default)]
        placeholder: Option<String>,
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
    GetDiagnostics {},
    /// The names of every action, for `dispatch` and key bindings.
    GetActions {},
    /// Adds key bindings, in the same format as `keymap.json`.
    BindKeys { keymap: Value },
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

/// The `fool_mode` a plugin gave an editor, for key bindings to match.
pub(crate) struct PluginModeAddon {
    pub mode: Option<String>,
}

impl editor::Addon for PluginModeAddon {
    fn extend_key_context(&self, key_context: &mut KeyContext, _: &App) {
        if let Some(mode) = &self.mode {
            key_context.set("fool_mode", mode.clone());
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

fn active_editor(workspace: &Workspace, cx: &App) -> Result<Entity<Editor>> {
    workspace
        .active_item_as::<Editor>(cx)
        .context("no editor is active")
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
        PluginRequest::Pick { items, placeholder } => {
            open_pick(items, placeholder, responder, workspace, window, cx);
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
        PluginRequest::Show {
            title,
            text,
            language,
        } => show_text(plugin, title, text, language, workspace, window, cx),
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
        PluginRequest::GetEditor { text } => {
            let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
                return Ok(Value::Null);
            };
            return Ok(editor.update(cx, |editor, cx| editor_context(editor, text, cx)));
        }
        PluginRequest::GetText { start, end } => {
            let editor = active_editor(workspace, cx)?;
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            let start = start.map_or(Point::zero(), |start| point_of(&snapshot, start));
            let end = end.map_or(snapshot.max_point(), |end| point_of(&snapshot, end));
            let (start, end) = (start.min(end), start.max(end));
            return Ok(Value::String(
                snapshot.text_for_range(start..end).collect::<String>(),
            ));
        }
        PluginRequest::SetSelections { selections } => {
            anyhow::ensure!(!selections.is_empty(), "there must be at least one selection");
            let editor = active_editor(workspace, cx)?;
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
        PluginRequest::Edit { edits } => {
            let editor = active_editor(workspace, cx)?;
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
        PluginRequest::SetMode { mode, cursor } => {
            let editor = active_editor(workspace, cx)?;
            let shape = cursor.as_deref().map(cursor_shape).transpose()?;
            editor.update(cx, |editor, cx| {
                editor.register_addon(PluginModeAddon { mode });
                if let Some(shape) = shape {
                    editor.set_cursor_shape(shape, cx);
                }
                cx.notify();
            });
        }
        PluginRequest::SetStatus { text } => {
            set_status(plugin, text, cx);
        }
        PluginRequest::Pick { .. } | PluginRequest::Prompt { .. } => {
            unreachable!("handled by apply_request")
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
        PluginRequest::GetDiagnostics {} => {
            let editor = active_editor(workspace, cx)?;
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
        PluginRequest::GetActions {} => {
            return Ok(json!(cx.all_action_names()));
        }
        PluginRequest::BindKeys { keymap } => {
            let source = serde_json::to_string(&keymap)?;
            let bindings = parse_keymap(&source, cx)?;
            cx.global_mut::<PluginHost>()
                .keymaps
                .push((plugin.to_string(), source));
            cx.bind_keys(bindings);
        }
    }
    Ok(Value::Null)
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
    items: Vec<String>,
    placeholder: Option<String>,
    responder: Responder,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, move |window, cx| {
        let candidates = items
            .iter()
            .enumerate()
            .map(|(index, item)| StringMatchCandidate::new(index, item))
            .collect();
        Picker::uniform_list(
            PickDelegate {
                placeholder: placeholder.unwrap_or_else(|| "Pick…".to_string()).into(),
                candidates,
                matches: Vec::new(),
                selected_index: 0,
                responder: Some(responder),
            },
            window,
            cx,
        )
        .initial_width(rems(40.))
    });
}

struct PickDelegate {
    placeholder: Arc<str>,
    candidates: Arc<[StringMatchCandidate]>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    /// Taken when the answer is sent, so that it's sent once.
    responder: Option<Responder>,
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
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(index)?;
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    found.string.clone(),
                    found.positions.clone(),
                )),
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
