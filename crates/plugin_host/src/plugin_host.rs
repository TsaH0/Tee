//! Runtime plugins: programs in any language that add commands to the
//! editor without rebuilding it.
//!
//! Each plugin is a directory in `<config dir>/plugins/` with a
//! `plugin.json` manifest naming the program to run and the commands it
//! provides. The program is started when one of its commands is first used
//! (or at startup) and talks to the editor with newline-delimited JSON over
//! stdin and stdout. See `docs/src/fool.md` for the protocol.

mod api;

pub use api::plugin_key_bindings;

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use editor::Editor;
use futures::{
    AsyncBufReadExt as _, AsyncWriteExt as _, StreamExt as _,
    channel::mpsc::{self, UnboundedSender},
    io::BufReader,
};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Entity, Global, Task,
    WeakEntity, Window, actions,
};
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use picker::{Picker, PickerDelegate};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use smol::process::{Child, Command};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, Toggleable, prelude::*, rems};
use util::ResultExt as _;
use workspace::{
    MultiWorkspace, OpenOptions, SplitDirection, Workspace,
    notifications::{NotificationId, show_app_notification},
};

const PROTOCOL_VERSION: u32 = 3;

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
    /// Editor events to send: "saved", "opened", "edited",
    /// "selection_changed" and "focused".
    #[serde(default)]
    events: Vec<String>,
    /// A file in `keymap.json` format, relative to the plugin's directory,
    /// whose bindings load with the plugin.
    #[serde(default)]
    keymap: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The name of an ex command (`:Name`) for Vim and Helix modes.
    #[serde(default)]
    pub vim: Option<String>,
    /// Whether to send the whole text of the active buffer.
    #[serde(default)]
    pub wants_text: bool,
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

struct PluginHost {
    dir: PathBuf,
    plugins: Vec<Plugin>,
    /// Read-only buffers plugins show text in, by plugin and title.
    shown_buffers: HashMap<(String, String), WeakEntity<Buffer>>,
    next_request_id: u64,
    /// Key bindings in `keymap.json` format, by plugin.
    keymaps: Vec<(String, String)>,
    /// Status bar texts, by plugin.
    statuses: std::collections::BTreeMap<String, String>,
    status_items: Vec<WeakEntity<api::PluginStatusItem>>,
    /// Modes whose keystrokes go to a plugin.
    captures: HashMap<String, api::KeyCapture>,
    /// Decoration namespaces, by plugin and name.
    namespaces: HashMap<(String, String), u32>,
    /// Open live pickers, by the id of the `pick` request that opened them.
    live_picks: HashMap<String, WeakEntity<Picker<api::PickDelegate>>>,
    /// The settings each running plugin was last sent.
    sent_settings: HashMap<String, Value>,
}

/// The `plugins` settings, by plugin name.
#[derive(settings::RegisterSetting, Clone, Debug, Default, PartialEq)]
struct PluginSettings(HashMap<String, Value>);

impl settings::Settings for PluginSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(
            content
                .plugins
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        )
    }
}

fn settings_for(plugin: &str, cx: &App) -> Value {
    use settings::Settings as _;
    PluginSettings::get_global(cx)
        .0
        .get(plugin)
        .cloned()
        .unwrap_or(Value::Object(Default::default()))
}

/// A folder each plugin can keep its own files in.
fn data_dir_for(plugin: &str) -> PathBuf {
    paths::data_dir().join("plugins").join(plugin)
}

impl Global for PluginHost {}

pub fn plugins_dir() -> PathBuf {
    paths::config_dir().join("plugins")
}

pub fn init(cx: &mut App) {
    init_with_dir(plugins_dir(), cx);
}

/// Like [`init`], loading plugins from `dir`.
pub fn init_with_dir(dir: PathBuf, cx: &mut App) {
    cx.set_global(PluginHost {
        dir,
        plugins: Vec::new(),
        shown_buffers: HashMap::default(),
        next_request_id: 0,
        keymaps: Vec::new(),
        statuses: Default::default(),
        status_items: Vec::new(),
        captures: HashMap::default(),
        namespaces: HashMap::default(),
        live_picks: HashMap::default(),
        sent_settings: HashMap::default(),
    });
    // Tells running plugins when their settings change.
    cx.observe_global::<settings::SettingsStore>(|cx| {
        let running = cx
            .global::<PluginHost>()
            .plugins
            .iter()
            .filter(|plugin| plugin.running.is_some())
            .map(|plugin| plugin.manifest.name.clone())
            .collect::<Vec<_>>();
        for plugin in running {
            let settings = settings_for(&plugin, cx);
            let host = cx.global_mut::<PluginHost>();
            if host.sent_settings.get(&plugin) == Some(&settings) {
                continue;
            }
            host.sent_settings.insert(plugin.clone(), settings.clone());
            send_to_plugin(
                &plugin,
                json!({ "type": "settings", "settings": settings }).to_string(),
                cx,
            );
        }
    })
    .detach();
    load_plugins(cx);
    cx.intercept_keystrokes(|event, window, cx| {
        if api::capture_keystroke(&event.keystroke, window, cx) {
            cx.stop_propagation();
        }
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if let Some(window) = window {
            let item = cx.new(|_| api::PluginStatusItem);
            cx.global_mut::<PluginHost>()
                .status_items
                .push(api::status_item_handle(&item));
            workspace.status_bar().update(cx, |status_bar, cx| {
                status_bar.add_right_item(item, window, cx);
            });
        }
        workspace
            .register_action(|workspace, _: &OpenCommandPicker, window, cx| {
                open_command_picker(workspace, window, cx);
            })
            .register_action(|_, _: &Reload, _, cx| {
                load_plugins(cx);
                let host = cx.global::<PluginHost>();
                let message = format!(
                    "Loaded {} plugin(s) from {}",
                    host.plugins.len(),
                    host.dir.display()
                );
                notify(message, false, cx);
            })
            .register_action(|workspace, _: &OpenPluginsFolder, window, cx| {
                let dir = cx.global::<PluginHost>().dir.clone();
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
            let event = match event {
                editor::EditorEvent::Saved => "saved",
                editor::EditorEvent::Edited { .. } => "edited",
                editor::EditorEvent::SelectionsChanged { local: true } => "selection_changed",
                editor::EditorEvent::Focused => "focused",
                _ => return,
            };
            send_event(event, editor, cx);
        })
        .detach();
        if cx.has_global::<PluginHost>() {
            send_event("opened", editor, cx);
            cx.on_release(|editor, cx| send_event("closed", editor, cx))
                .detach();
        }
    })
    .detach();
    // Like Emacs's `post-command-hook`: every action run from a key.
    cx.observe_keystrokes(|event, _, cx| {
        let Some(action) = &event.action else {
            return;
        };
        let os = cx.platform_info().os;
        broadcast_event(
            "action",
            json!({ "action": action.name(), "key": event.keystroke.unparse(os) }),
            cx,
        );
    })
    .detach();
}

pub(crate) fn notify(message: String, is_error: bool, cx: &mut App) {
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
    let manifests = read_manifests(&cx.global::<PluginHost>().dir);
    let mut keymaps = Vec::new();
    for (manifest, dir) in &manifests {
        let Some(keymap) = &manifest.keymap else {
            continue;
        };
        let path = dir.join(keymap);
        match std::fs::read_to_string(&path) {
            Ok(source) => keymaps.push((manifest.name.clone(), source)),
            Err(error) => log::error!("plugins: failed to read {}: {error}", path.display()),
        }
    }
    let mut bindings = Vec::new();
    for (plugin, source) in &keymaps {
        match api::parse_keymap(source, cx) {
            Ok(plugin_bindings) => bindings.extend(plugin_bindings),
            Err(error) => notify(format!("{plugin}'s key bindings: {error:#}"), true, cx),
        }
    }
    cx.bind_keys(bindings);
    let host = cx.global_mut::<PluginHost>();
    host.keymaps = keymaps;
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

    let settings = settings_for(name, cx);
    cx.global_mut::<PluginHost>()
        .sent_settings
        .insert(name.to_string(), settings.clone());
    let data_dir = data_dir_for(name);
    std::fs::create_dir_all(&data_dir).log_err();
    messages
        .unbounded_send(
            json!({
                "type": "initialize",
                "protocol_version": PROTOCOL_VERSION,
                "editor": "fool",
                "plugin_dir": plugin_dir,
                "config_dir": paths::config_dir(),
                "data_dir": data_dir,
                "settings": settings,
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

pub(crate) fn editor_path(editor: &Editor, cx: &App) -> Option<PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?.as_local()?;
    Some(file.abs_path(cx))
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
        .map(|editor| {
            editor.update(cx, |editor, cx| api::editor_context(editor, command.wants_text, cx))
        })
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
    let language = editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| buffer.read(cx).language().map(|language| language.name().to_string()));
    broadcast_event(event, json!({ "file": path, "language": language }), cx);
}

/// Sends a message to a running plugin.
pub(crate) fn send_to_plugin(plugin: &str, message: String, cx: &App) {
    let Some(host) = cx.try_global::<PluginHost>() else {
        return;
    };
    if let Some(running) = host
        .plugins
        .iter()
        .find(|candidate| candidate.manifest.name == plugin)
        .and_then(|plugin| plugin.running.as_ref())
    {
        running.messages.unbounded_send(message).ok();
    }
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

/// A request from a plugin, with the `id` its response goes back with.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    id: Option<Value>,
    #[serde(flatten)]
    request: api::PluginRequest,
}

fn handle_plugin_message(plugin: &str, line: &str, cx: &mut App) {
    let id = serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|value| value.get("id").cloned());
    let envelope = match serde_json::from_str::<Envelope>(line) {
        Ok(envelope) => envelope,
        Err(error) => {
            log::error!("plugins: {plugin} sent an invalid message ({error}): {line}");
            api::Responder {
                plugin: plugin.to_string(),
                id,
            }
            .respond(Err(anyhow!("invalid request: {error}")), cx);
            return;
        }
    };
    let responder = api::Responder {
        plugin: plugin.to_string(),
        id: envelope.id,
    };
    if let api::PluginRequest::Notify { message, level } = &envelope.request {
        notify(
            format!("{plugin}: {message}"),
            level.as_deref() == Some("error"),
            cx,
        );
        responder.respond(Ok(Value::Null), cx);
        return;
    }
    let request = envelope.request;
    with_any_workspace(cx, move |workspace, window, cx| {
        api::apply_request(request, responder, workspace, window, cx);
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

/// How a plugin's `show` buffer behaves.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ShowOptions {
    /// Lets the user edit the text.
    pub editable: bool,
    /// The mode the buffer's editor starts in, for its own key bindings.
    pub mode: Option<String>,
    /// Opens the buffer in a split to the "right" or "down" instead of the
    /// active pane.
    pub split: Option<String>,
}

pub(crate) fn show_text(
    plugin: &str,
    title: String,
    text: String,
    language: Option<String>,
    options: ShowOptions,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let key = (plugin.to_string(), title.clone());
    let existing = cx
        .global::<PluginHost>()
        .shown_buffers
        .get(&key)
        .and_then(|buffer| buffer.upgrade());
    if let Some(buffer) = existing {
        set_buffer_text(&buffer, text.clone(), options.editable, cx);
        let editor = workspace.items_of_type::<Editor>(cx).find(|editor| {
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .is_some_and(|singleton| singleton == buffer)
        });
        if let Some(editor) = editor {
            editor.update(cx, |editor, cx| {
                editor.set_read_only(!options.editable);
                if let Some(mode) = &options.mode {
                    editor.register_addon(api::PluginModeAddon::new(Some(mode.clone())));
                    cx.notify();
                }
            });
            workspace.activate_item(&editor, true, true, window, cx);
            return Task::ready(Ok(()));
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
            set_buffer_text(&buffer, text, options.editable, cx);
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
                editor.set_read_only(!options.editable);
                if let Some(mode) = options.mode {
                    editor.register_addon(api::PluginModeAddon::new(Some(mode)));
                }
                editor
            });
            match options.split.as_deref() {
                Some("right") => {
                    workspace.split_item(SplitDirection::Right, Box::new(editor), window, cx)
                }
                Some("down") => {
                    workspace.split_item(SplitDirection::Down, Box::new(editor), window, cx)
                }
                _ => workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx),
            }
        })
    })
}

fn set_buffer_text(buffer: &Entity<Buffer>, text: String, editable: bool, cx: &mut App) {
    buffer.update(cx, |buffer, cx| {
        buffer.set_capability(Capability::ReadWrite, cx);
        buffer.set_text(text, cx);
        if !editable {
            buffer.set_capability(Capability::ReadOnly, cx);
        }
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
    use crate::api::{PluginRequest, bindings_of};
    use gpui::Focusable as _;

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

    const PROBE_PLUGIN: &str = r#"
import json, sys

next_id = 0
# Keys and commands that arrive while waiting for a response.
pending = []

def request(message):
    global next_id
    next_id += 1
    message["id"] = next_id
    print(json.dumps(message), flush=True)
    while True:
        line = sys.stdin.readline()
        if not line:
            raise SystemExit("the editor closed")
        reply = json.loads(line)
        if reply.get("type") == "response" and reply.get("id") == next_id:
            return reply
        pending.append(reply)

def messages():
    while True:
        if pending:
            yield pending.pop(0)
            continue
        line = sys.stdin.readline()
        if not line:
            return
        yield json.loads(line)

def check(results):
    errors = []
    text = request({"type": "get_text"})["result"]
    if text != "hello wörld\nsecond line\n":
        errors.append(f"get_text gave {text!r}")
    # Columns count characters, so `ö` is one column.
    request({"type": "edit", "edits": [
        {"start": {"line": 1, "column": 7}, "end": {"line": 1, "column": 12}, "text": "there"}]})
    request({"type": "set_selections", "selections": [
        {"start": {"line": 2, "column": 1}, "end": {"line": 2, "column": 7}}]})
    request({"type": "set_mode", "mode": "probe", "cursor": "block"})
    request({"type": "set_status", "text": "probe ready"})
    editor = request({"type": "get_editor"})["result"]
    if editor["mode"] != "probe":
        errors.append(f"mode is {editor['mode']!r}")
    if editor["selections"][0]["text"] != "second":
        errors.append(f"selection is {editor['selections'][0]!r}")
    request({"type": "bind_keys", "keymap": [
        {"context": "Editor", "bindings": {"ctrl-alt-q": "editor::SelectAll"}}]})
    bad = request({"type": "set_mode", "mode": "x", "cursor": "triangle"})
    if "error" not in bad:
        errors.append("an invalid cursor should be an error")
    actions = request({"type": "get_actions"})["result"]
    if "editor::SelectAll" not in actions:
        errors.append("get_actions is missing editor::SelectAll")
    settings = request({"type": "get_settings"})["result"]
    if not isinstance(settings, dict):
        errors.append(f"get_settings gave {settings!r}")
    # Take over every key bound to an action, like Emacs advice.
    rebound = request({"type": "override_action", "action": "editor::SelectAll",
                       "command": "select_all"})
    if rebound.get("result", {}).get("rebound") != 1:
        errors.append(f"override_action gave {rebound!r}")
    keys = [binding["keys"] for binding in request(
        {"type": "get_bindings", "action": "plugins::RunCommand"})["result"]]
    if "ctrl-alt-q" not in keys:
        errors.append(f"ctrl-alt-q isn't bound to the plugin: {keys!r}")
    missing = request({"type": "override_action", "action": "editor::Nothing", "command": "x"})
    if "error" not in missing:
        errors.append("overriding an unbound action should be an error")
    # From now on the plugin decides what every key does in its mode.
    request({"type": "capture_keys", "mode": "probe", "except": ["ctrl-alt-p"]})
    with open("result.json", "w") as result:
        json.dump(errors, result)

for message in messages():
    if message.get("type") == "command" and message["command"] == "go":
        check(message)
    elif message.get("type") == "key":
        request({"type": "insert", "text": f"[{message['key']}]"})
"#;

    /// Runs a real plugin that uses the request API on an editor, and checks
    /// what it did.
    #[gpui::test]
    async fn test_plugin_api(cx: &mut gpui::TestAppContext) {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 isn't installed");
            return;
        }
        let plugins = tempfile::tempdir().expect("a temporary directory");
        let plugin_dir = plugins.path().join("probe");
        std::fs::create_dir_all(&plugin_dir).expect("the plugin directory");
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name": "probe", "run": ["python3", "probe.py"],
                "commands": [{"name": "go"}, {"name": "select_all"}], "keymap": "keymap.json"}"#,
        )
        .expect("the manifest");
        std::fs::write(plugin_dir.join("probe.py"), PROBE_PLUGIN).expect("the plugin");
        std::fs::write(
            plugin_dir.join("keymap.json"),
            r#"[{"context": "Editor && fool_mode == probe",
                 "bindings": {"ctrl-alt-p": ["plugins::RunCommand", {"plugin": "probe", "command": "go"}]}}]"#,
        )
        .expect("the keymap");

        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/project"),
            json!({ "a.txt": "hello wörld\nsecond line\n" }),
        )
        .await;
        let project = project::Project::test(fs, [util::path!("/project").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the workspace");
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.update(|_, cx| init_with_dir(plugins.path().to_path_buf(), cx));

        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(util::path!("/project/a.txt")),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("the file should open")
            .downcast::<Editor>()
            .expect("the file should open in an editor");

        workspace.update_in(cx, |workspace, window, cx| {
            run_command(
                workspace,
                &RunCommand {
                    plugin: "probe".into(),
                    command: "go".into(),
                    args: None,
                },
                window,
                cx,
            )
        });

        let result_path = plugin_dir.join("result.json");
        let started = std::time::Instant::now();
        while !result_path.exists() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(60),
                "the plugin didn't finish"
            );
            let (sender, receiver) = futures::channel::oneshot::channel::<()>();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                sender.send(()).ok();
            });
            receiver.await.ok();
        }
        let errors: Vec<String> = serde_json::from_str(
            &std::fs::read_to_string(&result_path).expect("the plugin's result"),
        )
        .expect("the plugin's result should be JSON");
        assert!(errors.is_empty(), "the plugin saw: {errors:?}");

        // Keys in the captured mode go to the plugin, which decides what
        // they do.
        editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.simulate_keystrokes("x shift-y");
        let started = std::time::Instant::now();
        while editor.update(cx, |editor, cx| editor.text(cx)) != "hello there\n[x][shift-y] line\n" {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(60),
                "the plugin didn't handle the keys: {:?}",
                editor.update(cx, |editor, cx| editor.text(cx))
            );
            let (sender, receiver) = futures::channel::oneshot::channel::<()>();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                sender.send(()).ok();
            });
            receiver.await.ok();
        }

        editor.update(cx, |editor, _| {
            let mode = editor
                .addon::<api::PluginModeAddon>()
                .and_then(|addon| addon.mode.clone());
            assert_eq!(mode.as_deref(), Some("probe"));
        });
        cx.update(|_, cx| {
            let host = cx.global::<PluginHost>();
            assert_eq!(
                host.statuses.get("probe").map(String::as_str),
                Some("probe ready")
            );
            // The manifest's keymap, `bind_keys` and `override_action`.
            assert_eq!(host.keymaps.len(), 3);
            let bindings = plugin_key_bindings(cx);
            assert!(bindings.iter().any(|binding| {
                binding.action().name() == "plugins::RunCommand"
                    && binding.predicate().is_some_and(|predicate| {
                        predicate.to_string().contains("fool_mode == probe")
                    })
            }));
            assert!(bindings
                .iter()
                .any(|binding| binding.action().name() == "editor::SelectAll"));
            // The last binding of `ctrl-alt-q` now runs the plugin.
            let last = bindings_of(None, cx)
                .into_iter()
                .filter(|binding| binding.keys == "ctrl-alt-q")
                .last()
                .map(|binding| binding.action);
            assert_eq!(last.as_deref(), Some("plugins::RunCommand"));
        });
    }

    /// Resolves after `duration` of real time, for waiting on a plugin.
    async fn sleep(duration: std::time::Duration) {
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            sender.send(()).ok();
        });
        receiver.await.ok();
    }

    /// The example plugins in `docs/plugins` load, and Ki mode drives an
    /// editor through its commands.
    #[gpui::test]
    async fn test_example_ki_mode_plugin(cx: &mut gpui::TestAppContext) {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 isn't installed");
            return;
        }
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/plugins");
        let unit_tests = std::process::Command::new("python3")
            .args(["-m", "unittest", "test_ki_mode"])
            .current_dir(examples.join("ki-mode"))
            .output()
            .expect("python3 should run");
        assert!(
            unit_tests.status.success(),
            "the Ki mode plugin's tests failed: {}",
            String::from_utf8_lossy(&unit_tests.stderr)
        );

        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/project"),
            json!({ "a.py": "let total = price * 2\n" }),
        )
        .await;
        let project = project::Project::test(fs, [util::path!("/project").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the workspace");
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.update(|_, cx| init_with_dir(examples.clone(), cx));
        cx.update(|_, cx| {
            let host = cx.global::<PluginHost>();
            let names = host
                .plugins
                .iter()
                .map(|plugin| plugin.manifest.name.as_str())
                .collect::<Vec<_>>();
            assert!(names.contains(&"ki-mode") && names.contains(&"hello"), "{names:?}");
            let ki_bindings = plugin_key_bindings(cx)
                .into_iter()
                .filter(|binding| binding.action().name() == "plugins::RunCommand")
                .count();
            assert_eq!(ki_bindings, 11, "every Ki mode binding should load");
        });

        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(util::path!("/project/a.py")),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("the file should open")
            .downcast::<Editor>()
            .expect("the file should open in an editor");

        let run = |command: &str, args: Option<&str>, cx: &mut gpui::VisualTestContext| {
            let action = RunCommand {
                plugin: "ki-mode".into(),
                command: command.into(),
                args: args.map(str::to_string),
            };
            workspace.update_in(cx, |workspace, window, cx| {
                run_command(workspace, &action, window, cx)
            });
        };
        let selected = |cx: &mut gpui::VisualTestContext| {
            editor.update(cx, |editor, cx| {
                let display_snapshot = editor.display_snapshot(cx);
                let selection = editor
                    .selections
                    .newest::<language::Point>(&display_snapshot);
                editor
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .text_for_range(selection.start..selection.end)
                    .collect::<String>()
            })
        };
        async fn wait_until(
            cx: &mut gpui::VisualTestContext,
            mut condition: impl FnMut(&mut gpui::VisualTestContext) -> bool,
            what: &str,
        ) {
            let started = std::time::Instant::now();
            while !condition(cx) {
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(60),
                    "timed out waiting for {what}"
                );
                sleep(std::time::Duration::from_millis(50)).await;
            }
        }

        run("enter", None, cx);
        wait_until(cx, |cx| selected(cx) == "let", "Ki mode to select the first word").await;
        editor.update(cx, |editor, _| {
            let mode = editor
                .addon::<api::PluginModeAddon>()
                .and_then(|addon| addon.mode.clone());
            assert_eq!(mode.as_deref(), Some("ki"));
        });
        // Ki mode's keys resolve in the editor's real key context.
        let ki_key = editor.update_in(cx, |editor, window, cx| {
            let key_context = editor.key_context(window, cx);
            assert_eq!(key_context.get("fool_mode").map(|mode| mode.as_ref()), Some("ki"));
            let keystroke = gpui::Keystroke::parse_with_os("l", gpui::OperatingSystem::Linux)
                .expect("a keystroke");
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let (bindings, _) = keymap.bindings_for_input(&[keystroke], &[key_context]);
            bindings
                .first()
                .and_then(|binding| binding.action().as_any().downcast_ref::<RunCommand>().cloned())
        });
        assert_eq!(
            ki_key.map(|action| action.command).as_deref(),
            Some("next"),
            "`l` should select the next unit in Ki mode"
        );

        run("next", None, cx);
        wait_until(cx, |cx| selected(cx) == "total", "the next word").await;
        run("extend_next", None, cx);
        wait_until(cx, |cx| selected(cx) == "total =", "extending the selection").await;
        run("delete", None, cx);
        wait_until(
            cx,
            |cx| editor.update(cx, |editor, cx| editor.text(cx)) == "let  price * 2\n",
            "deleting the selection",
        )
        .await;
        run("unit", Some("line"), cx);
        wait_until(cx, |cx| selected(cx) == "let  price * 2", "selecting the line").await;
        run("leave", None, cx);
        wait_until(
            cx,
            |cx| {
                editor.update(cx, |editor, _| {
                    editor
                        .addon::<api::PluginModeAddon>()
                        .is_some_and(|addon| addon.mode.is_none())
                })
            },
            "leaving Ki mode",
        )
        .await;
    }

    const EMACS_PROBE: &str = r##"
import json, os, sys

next_id = 0
pending = []
log = []
initialize = json.loads(sys.stdin.readline())

def record(entry):
    log.append(entry)
    with open("log.json.tmp", "w") as file:
        json.dump(log, file)
    os.replace("log.json.tmp", "log.json")

def answer_query(message):
    # A live picker asks for items while the plugin waits for its answer.
    query = message["query"]
    print(json.dumps({"type": "update_pick", "pick": message["pick"], "items": [
        {"label": query + "-result", "description": "found"}, "other"]}), flush=True)

def request(message):
    global next_id
    next_id += 1
    message["id"] = next_id
    print(json.dumps(message), flush=True)
    while True:
        line = sys.stdin.readline()
        if not line:
            raise SystemExit(0)
        reply = json.loads(line)
        if reply.get("type") == "response" and reply.get("id") == next_id:
            return reply
        if reply.get("type") == "pick_query":
            answer_query(reply)
        else:
            pending.append(reply)

def messages():
    while True:
        if pending:
            yield pending.pop(0)
            continue
        line = sys.stdin.readline()
        if not line:
            return
        yield json.loads(line)

record("settings " + json.dumps(initialize["settings"]))
record("data_dir " + str(os.path.isdir(initialize["data_dir"])))

def go():
    a, b = "A_FILE", "B_FILE"
    record("decorate " + json.dumps(request({"type": "set_decorations", "namespace": "lint",
        "highlights": [{"start": {"line": 1, "column": 1}, "end": {"line": 1, "column": 6},
                        "color": "error", "background": "#ff000033", "underline": "warning",
                        "wavy": True}],
        "virtual_text": [{"position": {"line": 1, "column": 12}, "text": "  <- note",
                          "color": "muted"}]})))
    record("bad color " + str("error" in request({"type": "set_decorations", "namespace": "x",
        "highlights": [{"start": {"line": 1, "column": 1}, "end": {"line": 1, "column": 2},
                        "color": "not-a-color"}]})))
    buffers = request({"type": "get_buffers"})["result"]
    record("buffers " + json.dumps(sorted(
        (os.path.basename(buffer["file"] or ""), buffer["active"]) for buffer in buffers)))
    request({"type": "edit", "file": b, "edits": [
        {"start": {"line": 1, "column": 1}, "end": {"line": 1, "column": 1}, "text": "edited "}]})
    record("b text " + request({"type": "get_text", "file": b})["result"])
    request({"type": "set_minor_mode", "name": "lint_on", "enabled": True})
    record("bad minor mode " + str("error" in request(
        {"type": "set_minor_mode", "name": "bad name", "enabled": True})))
    request({"type": "register_command", "name": "late", "vim": "Late"})
    request({"type": "bind_keys", "keymap": [
        {"context": "Editor", "bindings": {"ctrl-alt-u": "editor::SelectAll"}}]})
    terminal = request({"type": "run_in_terminal", "command": "true"})
    record("terminal " + json.dumps(terminal.get("result", "error")))
    request({"type": "show", "title": "Scratch", "text": "scratch text\n",
             "editable": True, "mode": "scratch_mode"})
    request({"type": "edit", "file": "Scratch", "edits": [
        {"start": {"line": 1, "column": 1}, "end": {"line": 1, "column": 8}, "text": "plugin"}]})
    record("scratch " + request({"type": "get_text", "file": "Scratch"})["result"])
    chosen = request({"type": "pick", "items": ["first"], "live": True})
    record("picked " + json.dumps(chosen.get("result")))

for message in messages():
    kind = message.get("type")
    if kind == "command" and message["command"] == "go":
        go()
    elif kind == "event":
        record("event " + message["event"] + " " + os.path.basename(message["data"].get("file") or "")
               + " " + str(message["data"].get("action") or message["data"].get("language")))
    elif kind == "settings":
        record("new settings " + json.dumps(message["settings"]))
    elif kind == "pick_query":
        answer_query(message)
"##;

    /// Reads the probe plugin's log.
    fn probe_log(plugin_dir: &Path) -> Vec<String> {
        std::fs::read_to_string(plugin_dir.join("log.json"))
            .ok()
            .and_then(|log| serde_json::from_str(&log).ok())
            .unwrap_or_default()
    }

    async fn wait_for_log(plugin_dir: &Path, entry: &str) {
        let started = std::time::Instant::now();
        while !probe_log(plugin_dir).iter().any(|logged| logged == entry) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(60),
                "the plugin never logged {entry:?}; it logged {:?}",
                probe_log(plugin_dir)
            );
            sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// The Emacs-style parts of the API: decorations, other buffers, plugin
    /// buffers with their own mode, minor modes, runtime commands, settings,
    /// hooks and live pickers.
    #[gpui::test]
    async fn test_emacs_style_api(cx: &mut gpui::TestAppContext) {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 isn't installed");
            return;
        }
        let plugins = tempfile::tempdir().expect("a temporary directory");
        let plugin_dir = plugins.path().join("probe2");
        std::fs::create_dir_all(&plugin_dir).expect("the plugin directory");
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name": "probe2", "run": ["python3", "probe.py"], "start": "startup",
                "commands": [{"name": "go"}], "events": ["closed", "action", "opened"]}"#,
        )
        .expect("the manifest");
        let a_file = util::path!("/project/a.txt");
        let b_file = util::path!("/project/b.txt");
        std::fs::write(
            plugin_dir.join("probe.py"),
            EMACS_PROBE.replace("A_FILE", a_file).replace("B_FILE", b_file),
        )
        .expect("the plugin");

        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            gpui::UpdateGlobal::update_global(cx, |store: &mut settings::SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.plugins = Some(
                        [("probe2".to_string(), json!({ "greeting": "hi" }))]
                            .into_iter()
                            .collect(),
                    );
                });
            });
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/project"),
            json!({ "a.txt": "first line\nsecond\n", "b.txt": "bee\n" }),
        )
        .await;
        let project = project::Project::test(fs, [util::path!("/project").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the workspace");
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cx.update(|_, cx| init_with_dir(plugins.path().to_path_buf(), cx));

        wait_for_log(&plugin_dir, r#"settings {"greeting": "hi"}"#).await;
        wait_for_log(&plugin_dir, "data_dir True").await;

        let open = |path: &'static str, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(PathBuf::from(path), OpenOptions::default(), window, cx)
            })
        };
        let b_editor = open(b_file, cx)
            .await
            .expect("b.txt should open")
            .downcast::<Editor>()
            .expect("an editor");
        let a_editor = open(a_file, cx)
            .await
            .expect("a.txt should open")
            .downcast::<Editor>()
            .expect("an editor");
        wait_for_log(&plugin_dir, "event opened a.txt Plain Text").await;

        workspace.update_in(cx, |workspace, window, cx| {
            run_command(
                workspace,
                &RunCommand {
                    plugin: "probe2".into(),
                    command: "go".into(),
                    args: None,
                },
                window,
                cx,
            )
        });

        wait_for_log(&plugin_dir, r#"decorate {"type": "response", "id": 1, "result": null}"#)
            .await;
        wait_for_log(&plugin_dir, "bad color True").await;
        wait_for_log(
            &plugin_dir,
            r#"buffers [["a.txt", true], ["b.txt", false]]"#,
        )
        .await;
        wait_for_log(&plugin_dir, "b text edited bee\n").await;
        wait_for_log(&plugin_dir, "bad minor mode True").await;
        // There is no terminal in tests, so nothing runs.
        wait_for_log(&plugin_dir, "terminal null").await;
        wait_for_log(&plugin_dir, "scratch plugin text\n").await;

        // Decorations and minor modes are on a.txt, which stayed active.
        let namespace = cx.update(|_, cx| {
            cx.global::<PluginHost>()
                .namespaces
                .get(&("probe2".to_string(), "lint".to_string()))
                .copied()
                .expect("the namespace")
        });
        a_editor.update_in(cx, |editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let ranges = editor
                .plugin_highlight_ranges(namespace, cx)
                .into_iter()
                .map(|range| {
                    snapshot
                        .text_for_range(range.start..range.end)
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            assert_eq!(ranges, vec!["first"]);
            assert_eq!(editor.plugin_virtual_text(namespace, cx), vec!["  <- note"]);
            assert!(editor.key_context(window, cx).contains("lint_on"));
        });
        assert_eq!(
            b_editor.update(cx, |editor, cx| editor.text(cx)),
            "edited bee\n"
        );
        cx.update(|_, cx| {
            assert_eq!(
                vim_command("Late", cx),
                Some(("probe2".to_string(), "late".to_string()))
            );
        });

        // The scratch buffer is editable and has the plugin's mode.
        let scratch = workspace.update(cx, |workspace, cx| {
            workspace.items_of_type::<Editor>(cx).find(|editor| {
                editor.read(cx).buffer().read(cx).title(cx) == "Scratch"
            })
        });
        let scratch = scratch.expect("the scratch buffer");
        scratch.update(cx, |editor, cx| {
            assert!(!editor.read_only(cx));
            assert_eq!(
                editor
                    .addon::<api::PluginModeAddon>()
                    .and_then(|addon| addon.mode.clone())
                    .as_deref(),
                Some("scratch_mode")
            );
        });

        // A live picker shows what the plugin answers for the query.
        let picker = {
            let started = std::time::Instant::now();
            loop {
                let picker = workspace.update(cx, |workspace, cx| {
                    workspace.active_modal::<Picker<api::PickDelegate>>(cx)
                });
                if let Some(picker) = picker {
                    break picker;
                }
                assert!(started.elapsed() < std::time::Duration::from_secs(60));
                sleep(std::time::Duration::from_millis(50)).await;
            }
        };
        picker.update_in(cx, |picker, window, cx| picker.set_query("ab", window, cx));
        let started = std::time::Instant::now();
        while picker.update(cx, |picker, _| picker.delegate.match_count()) != 2
            || picker.update(cx, |picker, _| {
                picker
                    .delegate
                    .matches
                    .first()
                    .map(|found| found.string.clone())
            }) != Some("ab-result".to_string())
        {
            assert!(started.elapsed() < std::time::Duration::from_secs(60));
            sleep(std::time::Duration::from_millis(50)).await;
        }
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx)
        });
        wait_for_log(&plugin_dir, "picked 0").await;

        // Hooks: every action run from a key, and closing an editor.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&a_editor, true, true, window, cx);
        });
        cx.simulate_keystrokes("ctrl-alt-u");
        wait_for_log(&plugin_dir, "event action  editor::SelectAll").await;
        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            pane.update(cx, |pane, cx| {
                pane.close_item_by_id(b_editor.entity_id(), workspace::SaveIntent::Skip, window, cx)
            })
            .detach_and_log_err(cx);
        });
        drop(b_editor);
        cx.run_until_parked();
        wait_for_log(&plugin_dir, "event closed b.txt Plain Text").await;

        // Settings changes reach the plugin.
        cx.update(|_, cx| {
            gpui::UpdateGlobal::update_global(cx, |store: &mut settings::SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.plugins = Some(
                        [("probe2".to_string(), json!({ "greeting": "hello" }))]
                            .into_iter()
                            .collect(),
                    );
                });
            });
        });
        wait_for_log(&plugin_dir, r#"new settings {"greeting": "hello"}"#).await;
    }
}
