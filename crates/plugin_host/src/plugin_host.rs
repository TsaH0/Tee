//! Runtime plugins: programs in any language that add commands to the
//! editor without rebuilding it.
//!
//! Each plugin is a directory in `<config dir>/plugins/` with a
//! `plugin.json` manifest naming the program to run and the commands it
//! provides. The program is started when one of its commands is first used
//! (or at startup) and talks to the editor with newline-delimited JSON over
//! stdin and stdout. See `docs/src/plugins.md` for the protocol.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use editor::{Editor, MultiBufferOffset};
use futures::{
    AsyncBufReadExt as _, AsyncWriteExt as _, StreamExt as _,
    channel::mpsc::{self, UnboundedSender},
    io::BufReader,
};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Entity, Global, Subscription, Task,
    WeakEntity, Window, actions,
};
use language::{Buffer, Capability, Point};
use multi_buffer::MultiBuffer;
use picker::{Picker, PickerDelegate};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smol::process::{Child, Command};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, Toggleable, prelude::*, rems};
use util::ResultExt as _;
use workspace::{
    MultiWorkspace, OpenOptions, Workspace,
    notifications::{NotificationId, show_app_notification},
};

const PROTOCOL_VERSION: u32 = 1;

actions!(
    plugins,
    [
        /// Picks a command from the installed plugins and runs it.
        OpenCommandPicker,
        /// Stops all plugins and loads the plugin manifests again.
        Reload,
        /// Opens the folder that plugins are installed in.
        OpenPluginsFolder,
    ]
);

/// Runs a command of a runtime plugin.
#[derive(Clone, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = plugins)]
#[serde(deny_unknown_fields)]
pub struct RunCommand {
    /// The plugin's name from its `plugin.json`.
    pub plugin: String,
    /// The command's name from the plugin's `plugin.json`.
    pub command: String,
    /// Text passed to the command, like the arguments of an ex command.
    #[serde(default)]
    pub args: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StartWhen {
    #[default]
    OnCommand,
    Startup,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    name: String,
    #[serde(default)]
    description: String,
    /// The program and its arguments, run in the plugin's directory.
    run: Vec<String>,
    #[serde(default)]
    start: StartWhen,
    #[serde(default)]
    commands: Vec<ManifestCommand>,
    /// Editor events to send: "saved" and "opened".
    #[serde(default)]
    events: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestCommand {
    name: String,
    #[serde(default)]
    description: String,
    /// The name of an ex command (`:Name`) for Vim and Helix modes.
    #[serde(default)]
    vim: Option<String>,
    /// Whether to send the whole text of the active buffer.
    #[serde(default)]
    wants_text: bool,
}

struct Plugin {
    manifest: Manifest,
    dir: PathBuf,
    running: Option<RunningPlugin>,
}

struct RunningPlugin {
    messages: UnboundedSender<String>,
    _child: Child,
    _tasks: Vec<Task<()>>,
}

#[derive(Default)]
struct PluginHost {
    plugins: Vec<Plugin>,
    /// Read-only buffers plugins show text in, by plugin and title.
    shown_buffers: HashMap<(String, String), WeakEntity<Buffer>>,
    next_request_id: u64,
    _editor_subscriptions: Vec<Subscription>,
}

impl Global for PluginHost {}

pub fn plugins_dir() -> PathBuf {
    paths::config_dir().join("plugins")
}

pub fn init(cx: &mut App) {
    cx.set_global(PluginHost::default());
    load_plugins(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &OpenCommandPicker, window, cx| {
                open_command_picker(workspace, window, cx);
            })
            .register_action(|_, _: &Reload, _, cx| {
                load_plugins(cx);
                let count = cx.global::<PluginHost>().plugins.len();
                notify(format!("Loaded {count} plugin(s) from {}", plugins_dir().display()), false, cx);
            })
            .register_action(|workspace, _: &OpenPluginsFolder, window, cx| {
                let dir = plugins_dir();
                if let Err(error) = std::fs::create_dir_all(&dir) {
                    notify(format!("Failed to create {}: {error}", dir.display()), true, cx);
                    return;
                }
                workspace
                    .open_abs_path(dir, OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            })
            .register_action(|workspace, action: &RunCommand, window, cx| {
                run_command(workspace, action, window, cx);
            });
    })
    .detach();
    cx.observe_new(|editor: &mut Editor, _, cx| {
        cx.subscribe(&cx.entity(), |editor, _, event: &editor::EditorEvent, cx| {
            if matches!(event, editor::EditorEvent::Saved) {
                send_event("saved", editor, cx);
            }
        })
        .detach();
        if let Some(path) = editor_path(editor, cx)
            && cx.has_global::<PluginHost>()
        {
            broadcast_event("opened", json!({ "file": path }), cx);
        }
    })
    .detach();
}

fn notify(message: String, is_error: bool, cx: &mut App) {
    struct PluginNotification;
    if is_error {
        log::error!("plugins: {message}");
    }
    show_app_notification(NotificationId::unique::<PluginNotification>(), cx, move |cx| {
        let message = message.clone();
        cx.new(|cx| {
            workspace::notifications::simple_message_notification::MessageNotification::new(
                message, cx,
            )
        })
    });
}

fn read_manifests(dir: &Path) -> Vec<(Manifest, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut manifests = Vec::new();
    for entry in entries.flatten() {
        let plugin_dir = entry.path();
        let manifest_path = plugin_dir.join("plugin.json");
        if !manifest_path.is_file() {
            continue;
        }
        let manifest = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))
            .and_then(|text| {
                serde_json_lenient::from_str::<Manifest>(&text)
                    .with_context(|| format!("invalid {}", manifest_path.display()))
            });
        match manifest {
            Ok(manifest) if manifest.run.is_empty() => {
                log::error!("plugins: {} has an empty \"run\"", manifest_path.display())
            }
            Ok(manifest) => manifests.push((manifest, plugin_dir)),
            Err(error) => log::error!("plugins: {error:#}"),
        }
    }
    manifests.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    manifests
}

fn load_plugins(cx: &mut App) {
    let manifests = read_manifests(&plugins_dir());
    let host = cx.global_mut::<PluginHost>();
    // Dropping the running plugins kills their processes.
    host.plugins = manifests
        .into_iter()
        .map(|(manifest, dir)| Plugin {
            manifest,
            dir,
            running: None,
        })
        .collect();
    let startup = host
        .plugins
        .iter()
        .filter(|plugin| plugin.manifest.start == StartWhen::Startup)
        .map(|plugin| plugin.manifest.name.clone())
        .collect::<Vec<_>>();
    for name in startup {
        if let Err(error) = ensure_running(&name, cx) {
            notify(format!("{error:#}"), true, cx);
        }
    }
}

/// The plugin and command an ex command runs, for Vim and Helix modes.
pub fn vim_command(name: &str, cx: &App) -> Option<(String, String)> {
    let host = cx.try_global::<PluginHost>()?;
    host.plugins.iter().find_map(|plugin| {
        plugin.manifest.commands.iter().find_map(|command| {
            (command.vim.as_deref() == Some(name))
                .then(|| (plugin.manifest.name.clone(), command.name.clone()))
        })
    })
}

fn ensure_running(name: &str, cx: &mut App) -> Result<UnboundedSender<String>> {
    let plugin = cx
        .global::<PluginHost>()
        .plugins
        .iter()
        .find(|plugin| plugin.manifest.name == name)
        .ok_or_else(|| anyhow!("no plugin is named {name}"))?;
    if let Some(running) = &plugin.running {
        return Ok(running.messages.clone());
    }
    let plugin_dir = plugin.dir.clone();
    let run = plugin.manifest.run.clone();

    let (program, arguments) = run
        .split_first()
        .ok_or_else(|| anyhow!("plugin {name} has an empty \"run\""))?;
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(&plugin_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start plugin {name} ({program})"))?;
    let stdin = child.stdin.take().context("plugin has no stdin")?;
    let stdout = child.stdout.take().context("plugin has no stdout")?;
    let stderr = child.stderr.take().context("plugin has no stderr")?;
    let (messages, mut outgoing) = mpsc::unbounded::<String>();

    let plugin_name = name.to_string();
    let writer = cx.background_spawn({
        let plugin_name = plugin_name.clone();
        async move {
            let mut stdin = stdin;
            while let Some(mut message) = outgoing.next().await {
                message.push('\n');
                if let Err(error) = stdin.write_all(message.as_bytes()).await {
                    log::error!("plugins: failed to write to {plugin_name}: {error}");
                    break;
                }
                stdin.flush().await.log_err();
            }
        }
    });
    let logger = cx.background_spawn({
        let plugin_name = plugin_name.clone();
        async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(Ok(line)) = lines.next().await {
                log::info!("plugin {plugin_name}: {line}");
            }
        }
    });
    let reader = cx.spawn({
        let plugin_name = plugin_name.clone();
        async move |cx| {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(Ok(line)) = lines.next().await {
                if line.trim().is_empty() {
                    continue;
                }
                let plugin_name = plugin_name.clone();
                cx.update(|cx| handle_plugin_message(&plugin_name, &line, cx));
            }
            cx.update(|cx| plugin_exited(&plugin_name, cx));
        }
    });

    messages
        .unbounded_send(
            json!({
                "type": "initialize",
                "protocol_version": PROTOCOL_VERSION,
                "editor": "fool",
                "plugin_dir": plugin_dir,
                "config_dir": paths::config_dir(),
            })
            .to_string(),
        )
        .ok();
    if let Some(plugin) = cx
        .global_mut::<PluginHost>()
        .plugins
        .iter_mut()
        .find(|plugin| plugin.manifest.name == name)
    {
        plugin.running = Some(RunningPlugin {
            messages: messages.clone(),
            _child: child,
            _tasks: vec![writer, logger, reader],
        });
    }
    Ok(messages)
}

fn plugin_exited(name: &str, cx: &mut App) {
    let host = cx.global_mut::<PluginHost>();
    if let Some(plugin) = host
        .plugins
        .iter_mut()
        .find(|plugin| plugin.manifest.name == name)
    {
        plugin.running = None;
    }
    log::info!("plugins: {name} exited");
}

fn editor_path(editor: &Editor, cx: &App) -> Option<PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?.as_local()?;
    Some(file.abs_path(cx))
}

fn position(point: Point) -> Value {
    json!({ "line": point.row + 1, "column": point.column + 1 })
}

/// What a command knows about the editor it was run from.
fn editor_context(editor: &Entity<Editor>, wants_text: bool, cx: &mut App) -> Value {
    editor.update(cx, |editor, cx| context_of(editor, wants_text, cx))
}

fn context_of(editor: &mut Editor, wants_text: bool, cx: &mut Context<Editor>) -> Value {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let display_snapshot = editor.display_snapshot(cx);
    let selections = editor
        .selections
        .all::<Point>(&display_snapshot)
        .into_iter()
        .map(|selection| {
            json!({
                "start": position(selection.start),
                "end": position(selection.end),
                "text": snapshot.text_for_range(selection.start..selection.end).collect::<String>(),
            })
        })
        .collect::<Vec<_>>();
    let cursor = editor
        .selections
        .newest::<Point>(&display_snapshot)
        .head();
    let language = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| buffer.read(cx).language().map(|language| language.name().to_string()));
    let mut context = json!({
        "file": editor_path(editor, cx),
        "language": language,
        "cursor": position(cursor),
        "selections": selections,
    });
    if wants_text {
        context["text"] = Value::String(snapshot.text());
    }
    context
}

fn run_command(
    workspace: &mut Workspace,
    action: &RunCommand,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let host = cx.global::<PluginHost>();
    let command = host
        .plugins
        .iter()
        .find(|plugin| plugin.manifest.name == action.plugin)
        .and_then(|plugin| {
            plugin
                .manifest
                .commands
                .iter()
                .find(|command| command.name == action.command)
        })
        .cloned();
    let Some(command) = command else {
        notify(
            format!(
                "Plugin {} has no command {} (run `plugins: reload` after installing plugins)",
                action.plugin, action.command
            ),
            true,
            cx,
        );
        return;
    };
    let mut context = workspace
        .active_item_as::<Editor>(cx)
        .map(|editor| editor_context(&editor, command.wants_text, cx))
        .unwrap_or_else(|| json!({}));
    context["worktrees"] = Value::Array(
        workspace
            .root_paths(cx)
            .iter()
            .map(|path| Value::String(path.to_string_lossy().into_owned()))
            .collect(),
    );
    let messages = match ensure_running(&action.plugin, cx) {
        Ok(messages) => messages,
        Err(error) => {
            notify(format!("{error:#}"), true, cx);
            return;
        }
    };
    let host = cx.global_mut::<PluginHost>();
    host.next_request_id += 1;
    let message = json!({
        "type": "command",
        "id": host.next_request_id,
        "command": action.command,
        "args": action.args.clone().unwrap_or_default(),
        "context": context,
    });
    messages.unbounded_send(message.to_string()).ok();
}

fn send_event(event: &str, editor: &Editor, cx: &mut App) {
    let Some(path) = editor_path(editor, cx) else {
        return;
    };
    broadcast_event(event, json!({ "file": path }), cx);
}

/// Sends an event to the running plugins that asked for it in their manifest.
fn broadcast_event(event: &str, data: Value, cx: &mut App) {
    let Some(host) = cx.try_global::<PluginHost>() else {
        return;
    };
    let message = json!({ "type": "event", "event": event, "data": data }).to_string();
    for plugin in &host.plugins {
        if let Some(running) = &plugin.running
            && plugin.manifest.events.iter().any(|wanted| wanted == event)
        {
            running.messages.unbounded_send(message.clone()).ok();
        }
    }
}

/// Requests a plugin can send.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PluginRequest {
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
}

fn handle_plugin_message(plugin: &str, line: &str, cx: &mut App) {
    let request = match serde_json::from_str::<PluginRequest>(line) {
        Ok(request) => request,
        Err(error) => {
            log::error!("plugins: {plugin} sent an invalid message ({error}): {line}");
            return;
        }
    };
    if let PluginRequest::Notify { message, level } = &request {
        notify(
            format!("{plugin}: {message}"),
            level.as_deref() == Some("error"),
            cx,
        );
        return;
    }
    let plugin = plugin.to_string();
    with_any_workspace(cx, move |workspace, window, cx| {
        if let Err(error) = apply_request(&plugin, request, workspace, window, cx) {
            notify(format!("{plugin}: {error:#}"), true, cx);
        }
    });
}

fn with_any_workspace(
    cx: &mut App,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    let window = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<MultiWorkspace>())
        });
    let Some(window) = window else {
        log::error!("plugins: no window to handle a plugin request in");
        return;
    };
    cx.defer(move |cx| {
        window
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| f(workspace, window, cx));
            })
            .log_err();
    });
}

fn apply_request(
    plugin: &str,
    request: PluginRequest,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<()> {
    match request {
        PluginRequest::Notify { .. } => {}
        PluginRequest::Open { path, line, column } => {
            let open = workspace.open_abs_path(path, OpenOptions::default(), window, cx);
            cx.spawn_in(window, async move |_, cx| {
                let item = open.await?;
                if let Some(line) = line
                    && let Some(editor) = cx.update(|_, cx| item.act_as::<Editor>(cx))?
                {
                    let point = Point::new(
                        line.saturating_sub(1),
                        column.unwrap_or(1).saturating_sub(1),
                    );
                    editor.update_in(cx, |editor, window, cx| {
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
            let editor = workspace
                .active_item_as::<Editor>(cx)
                .context("no editor is active")?;
            editor.update(cx, |editor, cx| editor.insert(&text, window, cx));
        }
        PluginRequest::SetText { text } => {
            let editor = workspace
                .active_item_as::<Editor>(cx)
                .context("no editor is active")?;
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
    }
    Ok(())
}

fn show_text(
    plugin: &str,
    title: String,
    text: String,
    language: Option<String>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let key = (plugin.to_string(), title.clone());
    let existing = cx
        .global::<PluginHost>()
        .shown_buffers
        .get(&key)
        .and_then(|buffer| buffer.upgrade());
    if let Some(buffer) = existing {
        set_read_only_text(&buffer, text.clone(), cx);
        let editor = workspace.items_of_type::<Editor>(cx).find(|editor| {
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .is_some_and(|singleton| singleton == buffer)
        });
        if let Some(editor) = editor {
            workspace.activate_item(&editor, true, false, window, cx);
            return;
        }
    }
    let language = language.map(|language| {
        workspace
            .app_state()
            .languages
            .language_for_name(&language)
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let language = match language {
            Some(language) => language.await.log_err(),
            None => None,
        };
        let buffer = workspace
            .update(cx, |workspace, cx| {
                workspace
                    .project()
                    .update(cx, |project, cx| project.create_buffer(language, false, cx))
            })?
            .await?;
        workspace.update_in(cx, |workspace, window, cx| {
            set_read_only_text(&buffer, text, cx);
            cx.global_mut::<PluginHost>()
                .shown_buffers
                .insert(key, buffer.downgrade());
            let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(
                    multi_buffer,
                    Some(workspace.project().clone()),
                    window,
                    cx,
                );
                editor.set_read_only(true);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        })
    })
    .detach_and_log_err(cx);
}

fn set_read_only_text(buffer: &Entity<Buffer>, text: String, cx: &mut App) {
    buffer.update(cx, |buffer, cx| {
        buffer.set_capability(Capability::ReadWrite, cx);
        buffer.set_text(text, cx);
        buffer.set_capability(Capability::ReadOnly, cx);
    });
}

struct CommandEntry {
    label: String,
    action: RunCommand,
}

fn open_command_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let entries = cx
        .global::<PluginHost>()
        .plugins
        .iter()
        .flat_map(|plugin| {
            plugin.manifest.commands.iter().map(|command| {
                let description = if command.description.is_empty() {
                    plugin.manifest.description.clone()
                } else {
                    command.description.clone()
                };
                CommandEntry {
                    label: format!("{}: {}  {description}", plugin.manifest.name, command.name),
                    action: RunCommand {
                        plugin: plugin.manifest.name.clone(),
                        command: command.name.clone(),
                        args: None,
                    },
                }
            })
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        notify(
            format!(
                "No plugin commands. Add plugins to {} and run `plugins: reload`.",
                plugins_dir().display()
            ),
            true,
            cx,
        );
        return;
    }
    let handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        let delegate = CommandPickerDelegate::new(handle, entries);
        Picker::uniform_list(delegate, window, cx).initial_width(rems(40.))
    });
}

struct CommandPickerDelegate {
    workspace: WeakEntity<Workspace>,
    entries: Vec<CommandEntry>,
    candidates: Arc<[StringMatchCandidate]>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl CommandPickerDelegate {
    fn new(workspace: WeakEntity<Workspace>, entries: Vec<CommandEntry>) -> Self {
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

impl PickerDelegate for CommandPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "plugin command picker"
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

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Run a plugin command…".into()
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
                    false,
                    true,
                    candidates.len(),
                    &Default::default(),
                    background,
                )
                .await;
                matches.sort_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then(a.candidate_id.cmp(&b.candidate_id))
                });
                matches
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

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected_index)
            .and_then(|found| self.entries.get(found.candidate_id))
        else {
            return;
        };
        let action = entry.action.clone();
        cx.emit(DismissEvent);
        self.workspace
            .update(cx, |workspace, cx| run_command(workspace, &action, window, cx))
            .log_err();
    }

    fn dismissed(&mut self, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_manifests_and_requests_parse() -> Result<()> {
        let plugins = tempfile::tempdir()?;
        let dir = plugins.path().join("hello");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("plugin.json"),
            r#"{
                // Comments and trailing commas are allowed.
                "name": "hello",
                "run": ["python3", "hello.py"],
                "commands": [{ "name": "greet", "vim": "Greet" }],
            }"#,
        )?;
        let manifests = read_manifests(plugins.path());
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].0.commands[0].vim.as_deref(), Some("Greet"));

        let request: PluginRequest =
            serde_json::from_str(r#"{"type":"open","path":"/tmp/a.rs","line":3}"#)?;
        assert!(matches!(request, PluginRequest::Open { line: Some(3), .. }));
        let request: PluginRequest = serde_json::from_str(
            r#"{"type":"dispatch","action":"editor::SelectAll"}"#,
        )?;
        assert!(matches!(request, PluginRequest::Dispatch { .. }));
        Ok(())
    }
}
