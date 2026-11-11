# Writing Fool plugins

These instructions are for anyone, person or coding agent, asked to write a
plugin for Fool, this fork of Zed. A plugin changes what the editor does
without rebuilding it: it adds commands, key bindings, modes, decorations,
pickers and more. `docs/src/fool.md` is the full reference; this file is the
working summary, with the traps to avoid.

## What a plugin is

A plugin is a folder holding a manifest and a program in any language. Fool
runs the program and exchanges one JSON object per line with it: the editor
writes to the program's stdin and reads its stdout. Python 3 with only the
standard library is the usual choice, because every example uses it and
nothing needs installing.

```
~/.config/zed/plugins/<name>/
  plugin.json      the manifest
  <name>.py        the program
  keymap.json      key bindings that load with the plugin (optional)
  test_<name>.py   unit tests of the plugin's logic (recommended)
```

Users install a plugin by copying its folder there and running
`plugins: reload`. Plugins in this repository live in `docs/plugins/`, and
`script/install-fool-plugin.sh <name>` installs one from GitHub.

Study the examples before writing a new plugin:

- `hello/`: the smallest plugin, with a command and an ex command.
- `multicursor/`: commands that read the selections from their `context`
  and answer with requests, without waiting for responses.
- `ki-mode/`: a whole modal mode, with `set_mode`, mode-specific key
  bindings and requests that wait for their responses.

## The manifest

```json
{
  "name": "example",
  "description": "What it does",
  "run": ["python3", "example.py"],
  "start": "on_command",
  "events": ["saved"],
  "keymap": "keymap.json",
  "commands": [
    { "name": "do_it", "description": "Shown in the command picker", "vim": "DoIt", "wants_text": true }
  ]
}
```

- `name` must match the folder name and is what key bindings refer to.
- `run` runs in the plugin's folder.
- `start` is `"on_command"` (the default; the plugin starts when one of its
  commands is first used) or `"startup"` (it starts with the editor, which
  event listeners and background work need).
- `events` lists the events the plugin receives. Listing none costs nothing.
- `commands` entries take only `name`, `description`, `vim` and `wants_text`;
  any other field makes the manifest invalid. `vim` makes the command an ex
  command (`:DoIt args`), and `wants_text` adds the buffer's text to the
  command's `context`.
- Comments and trailing commas are allowed.

## Key bindings

`keymap.json` uses Zed's keymap format. Bind keys to
`["plugins::RunCommand", { "plugin": "<name>", "command": "<command>", "args": "..." }]`.

Useful contexts:

- Vim and Helix modes: `vim_mode == normal`, `visual`, `insert`,
  `helix_normal`, `helix_select`. Add `&& !menu` so keys still work in
  completion menus.
- A plugin's own mode, set with `set_mode`: `fool_mode == <mode>`.
- A minor mode, set with `set_minor_mode`: the mode's name, as in
  `Editor && my_mode`.

Plugin bindings load after the built-in Vim and Helix keys, so a plugin
binding wins over a built-in one in the same context. The user's own
`keymap.json` loads after plugins.

Before taking a key, check what already uses it in `assets/keymaps/vim.json`.
A sequence like `space m x` makes an existing `space m` binding wait for the
key timeout. Add an explicit binding (like `space m m`) if the existing key
needs to stay instant.

## The protocol

Each line is one JSON object. The editor sends:

| Message | Fields |
|---------|--------|
| `initialize` | `protocol_version` (3), `editor`, `plugin_dir`, `config_dir`, `data_dir`, `settings` |
| `command` | `id`, `command`, `args`, `context` |
| `event` | `event`, `data` (`file`, `language`; `action` and `key` for `action`) |
| `response` | `id`, then `result` or `error` |
| `key` | `key`, `mode`, for keys a plugin captures with `capture_keys` |
| `settings` | `settings`, when the user changes the plugin's settings |
| `pick_query` | `pick`, `query`, while the user types in a `live` picker |

A command's `context` has `file`, `language`, `cursor`, `selections`, `mode`,
`line_count` and `worktrees`, plus `text` with `wants_text`. Each selection
has `start`, `end` and `text`, and `end` is where the cursor is.

The plugin sends requests: `{"type": "<request>", ...fields}`. Give a request
an `id` to get a `response`. Without an `id`, failures only show as
notifications. See the request table in `docs/src/fool.md`. The ones most
plugins use:

| Request | Use |
|---------|-----|
| `get_editor`, `get_text` | Read the editor and its text |
| `set_selections` | Move cursors or select; several selections are several cursors |
| `edit`, `insert`, `set_text` | Change text, as one undoable edit |
| `set_mode`, `set_minor_mode` | Give an editor a mode its key bindings can match |
| `dispatch` | Run any action, like `editor::SelectAll` or `vim::SwitchToInsertMode` |
| `set_status`, `notify`, `pick`, `prompt`, `show` | Talk to the user |
| `set_decorations` | Highlight text and show virtual text |
| `bind_keys`, `override_action`, `capture_keys` | Decide what keys do at run time |
| `lsp_request` | Ask the file's language server anything |

## Traps

- **Positions are 1-based lines and columns, and columns count characters,
  not bytes.** Write `to_offset` and `to_position` helpers, like the
  examples', and unit-test that they round-trip.
- **Only JSON goes to stdout.** A stray `print` breaks the protocol. Log to
  stderr instead; it appears in `zed: open log`.
- **Flush after every message** (`flush=True` or `sys.stdout.flush()`), or
  the editor waits forever.
- **Messages arrive while you wait for a response.** If a plugin sends a
  request with an `id` and waits, commands, events and keys can come first.
  Keep them in a queue and handle them after the response, like the
  `Editor` class in `ki-mode/ki_mode.py`. Dropping them loses user input.
  A live picker's `pick_query` must be answered while you wait, or the
  picker stays empty.
- **Avoid round trips where you can.** A command's `context` already holds
  the selections (and the text, with `wants_text`). Requests that need no
  answer can be sent without an `id`, as `multicursor/` does.
- **A plugin that stops reading stdin misses events.** Fool keeps at most
  1024 queued, so read stdin continuously.
- **Use `data_dir`** from `initialize` for anything the plugin stores, not
  the plugin's own folder.
- **Requests act on the active editor** unless they take a `file`: a path,
  or the title of a buffer the plugin showed.

## Testing

1. Put the plugin's logic in plain functions and unit-test them with
   `python3 -m unittest` (see `multicursor/test_multicursor.py`).
2. For plugins in `docs/plugins/`, add an end-to-end test to
   `crates/plugin_host/src/plugin_host.rs` that loads the examples folder,
   runs the plugin's commands with `run_command` and checks the editor (see
   `test_example_multicursor_plugin`). Update the binding count in
   `test_example_ki_mode_plugin`, which counts every example's bindings.
3. Run `cargo test -p plugin_host`.

Run Python with `-B` in tests so they leave no `__pycache__` in the
repository.
