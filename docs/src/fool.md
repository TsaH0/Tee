---
title: Fool Plugins and Customization
description: Runtime plugins, built-in Vim/Helix plugins, custom modal keymaps, :substitute, the file explorer, Jupyter notebooks with Colab and competitive programming in Fool.
---

# Fool Plugins and Customization

Fool is Zed with an extended Vim and Helix mode. Everything on this page can be
changed while the editor runs: settings and keymaps reload when you save them,
and runtime plugins load from a folder. None of it needs a rebuild.

## Runtime plugins

A runtime plugin is a program in any language that adds commands to the editor.
Put each plugin in its own folder under `~/.config/zed/plugins/` with a
`plugin.json`, then run `plugins: reload` (or restart).

```json
{
  "name": "hello",
  "description": "Example plugin",
  // The program, run in the plugin's folder.
  "run": ["python3", "hello.py"],
  // "on_command" (default) starts it when one of its commands is first used.
  "start": "on_command",
  // Editor events to receive: "saved", "opened", "closed", "edited",
  // "selection_changed", "focused" and "action".
  "events": ["saved"],
  // Key bindings that load with the plugin, in keymap.json format.
  "keymap": "keymap.json",
  "commands": [
    // `vim` makes it an ex command: `:Hello world` in Vim and Helix modes.
    { "name": "greet", "description": "Say hello", "vim": "Hello" },
    // `wants_text` also sends the whole buffer.
    { "name": "count", "description": "Count words", "vim": "Words", "wants_text": true }
  ]
}
```

Run a plugin command:

- `plugins: open command picker` lists every plugin command.
- `:Name args` in Vim and Helix modes, for commands with `vim`.
- Key bindings in `keymap.json`:

  ```json
  {
    "context": "Editor && vim_mode == helix_normal",
    "bindings": {
      "space x": ["plugins::RunCommand", { "plugin": "hello", "command": "greet" }]
    }
  }
  ```

`plugins: open plugins folder` opens the folder, and `plugins: reload` restarts
every plugin and reads the manifests again.

### Protocol

The editor and the plugin exchange one JSON object per line: the editor writes
to the plugin's stdin and reads its stdout. Anything the plugin prints to stderr
goes to the log (`zed: open log`).

The editor sends:

| Message | Fields |
|---------|--------|
| `initialize` | `protocol_version` (3), `editor` ("fool"), `plugin_dir`, `config_dir`, `data_dir` (a folder of the plugin's own for anything it keeps), `settings` |
| `command` | `id`, `command`, `args` (text after the ex command, or from the binding), `context` |
| `event` | `event`, `data` (`file` and `language`; `action` and `key` for `action` events) |
| `settings` | `settings`, sent when the plugin's settings change |
| `pick_query` | `pick`, `query`, as the user types in a `live` picker |
| `response` | `id`, and `result` or `error`, answering a request that had an `id` |
| `key` | `key` (like `"j"` or `"ctrl-shift-k"`), `mode` | A keystroke in a mode the plugin captures |

Events are only sent to plugins that list them under `events` in
`plugin.json`: `saved`, `opened`, `closed`, `edited`, `selection_changed`,
`focused`, and `action`, sent for every action run from a key, like Emacs's
`post-command-hook`.

A plugin's settings are what `settings.json` has under its name in `"plugins"`,
like Emacs's `defcustom`:

```json
{ "plugins": { "hello": { "greeting": "hi" } } }
```

`context` (also the result of `get_editor`) has `file`, `language`, `cursor`,
`selections` (each with `start`, `end` and `text`; `end` is where the cursor
is), `mode`, `line_count` and `worktrees`, plus `text` when asked for. Lines and
columns start at 1, and columns count characters, not bytes.

A plugin can send these requests at any time, not only in reply to a command,
so it can also run its own servers or watchers. Give a request an `id` to get a
`response` with its result; without one, errors show as notifications. While a
plugin waits for a response, commands and events can still arrive, so keep
them to handle afterwards (as `docs/plugins/ki-mode` does).

Requests marked † take an optional `file`: a path, or the title of a buffer
the plugin showed, to work on that open editor instead of the active one.

| Request | Fields | Effect and result |
|---------|--------|-------------------|
| `get_editor` † | `text` (bool) | The editor's `context`, or `null` |
| `get_text` † | `start`, `end` (optional positions) | The text |
| `set_selections` † | `selections`: `[{start, end}]` | Selects; `end` before `start` selects backwards |
| `edit` † | `edits`: `[{start, end, text}]` | Replaces the ranges as one undoable edit |
| `insert` | `text` | Replaces the selections (or inserts at the cursors) |
| `set_text` | `text` | Replaces the whole text |
| `set_mode` † | `mode` (string or `null`), `cursor` (`block`, `bar`, `underline`, `hollow`) | Gives the active editor a mode that key bindings can match with `fool_mode == <mode>` |
| `bind_keys` | `keymap` (`keymap.json` format) | Adds key bindings |
| `get_bindings` | `action` (optional) | Every binding (or `action`'s): `keys`, `context`, `action`, lowest precedence first |
| `override_action` | `action`, `command`, `args` | Every key bound to `action` runs the plugin's `command` instead, in the same contexts |
| `capture_keys` | `mode`, `except` (keys) | Every keystroke in editors in `mode` goes to the plugin as a `key` message instead |
| `release_keys` | `mode` | Stops capturing `mode`'s keystrokes |
| `dispatch` | `action`, `args` | Runs any action, such as `"editor::SelectAll"` or `"vim::SwitchToInsertMode"` |
| `get_actions` | | The names of every action |
| `set_status` | `text` (string or `null`) | Shows text in the status bar |
| `pick` | `items` (strings, or `{label, description}`), `placeholder`, `live` | Asks the user to pick an item; the result is its index, or `null`. A `live` picker asks the plugin for items as the user types (`pick_query`) |
| `update_pick` | `pick`, `items` | Answers a `pick_query`: replaces a live picker's items |
| `prompt` | `placeholder` | Asks the user to type; the result is the text, or `null` |
| `notify` | `message`, `level` ("info" or "error") | Shows a notification |
| `open` | `path`, `line`, `column` | Opens a file, optionally at a position |
| `show` | `title`, `text`, `language`, `editable`, `mode`, `split` (`right` or `down`) | Shows text in a tab, replacing an earlier `show` with the same title. With `editable` and `mode`, it's an interactive buffer with its own keys, like an Emacs special buffer |
| `get_buffers` | | The open editors: `file`, `title`, `language`, `dirty`, `active` |
| `set_decorations` † | `namespace`, `highlights`: `[{start, end, color, background, underline, wavy, strikethrough, bold, italic}]`, `virtual_text`: `[{position, text, color, …}]` | Highlights text and shows virtual text, replacing what `namespace` showed before; both follow edits. Colors are `#rrggbb[aa]` or `error`, `warning`, `info`, `hint`, `success`, `created`, `deleted`, `modified`, `accent`, `muted` |
| `clear_decorations` † | `namespace` | Removes them |
| `register_command` | `name`, `description`, `vim` | Adds a command while the plugin runs |
| `set_minor_mode` † | `name`, `enabled` | Turns on a minor mode, which key bindings match like a context: `"Editor && my_mode"` |
| `run_in_terminal` | `command`, `cwd` | Runs a shell command in the terminal panel; the result is its exit code, like Emacs's `compile` |
| `lsp_request` † | `method`, `params`, `server` | Sends any request, including a server's own extensions, to the file's language server; the result is the server's |
| `get_settings` | | Your `settings.json` |
| `update_settings` | `settings` | Merges into `settings.json`, for anything settings control |
| `get_diagnostics` † | | The active editor's diagnostics: `start`, `end`, `severity`, `message`, `source` |

### Deciding what keys do

Plugins control keys at three levels, from simplest to most flexible:

1. **Bind keys to plugin commands.** A plugin's `keymap` file (or `bind_keys`)
   binds any key, sequence or chord, in any context such as
   `vim_mode == helix_normal`, to `plugins::RunCommand`. Your `keymap.json` can do
   the same, and `args` passes text to the command.
2. **Take over a command everywhere.** `override_action` rebinds every key that
   runs an action, say `vim::NextWordStart` or `editor::Newline`, to the
   plugin's command, keeping each binding's context. The command can still
   `dispatch` the original action, so it can do something before or after it,
   or only in some cases, like Emacs advice.
3. **Decide every key yourself.** After `set_mode`, `capture_keys` sends every
   keystroke typed in that mode to the plugin, which can keep any state it
   likes: counts, operators, chords like Emacs's `C-x C-f`, or a different
   meaning for each key depending on the selection. Keys in `except` (such as
   `"ctrl-s"`) still do what they usually do, and `release_keys` or leaving the
   mode hands keys back.

`get_bindings` shows what keys do, like Emacs's `describe-key` and `where-is`.

### What plugins can change

- **New modal keymaps**: `set_mode` plus key bindings in a `fool_mode == <mode>`
  context make a whole new editing model. Plugin bindings load after the
  built-in Vim and Helix keys, so they win while the mode is on, and keys they
  don't bind keep working. `docs/plugins/ki-mode` is a selection-first mode in
  the style of the [Ki editor](https://ki-editor.github.io/ki-editor/): copy it
  to `~/.config/zed/plugins/ki-mode`, run `plugins: reload`, and press `space k`.
- **Text objects and motions**: read the text, compute ranges in any language,
  and `set_selections`, bound to keys like built-in objects.
- **The UI**: `set_status`, `pick`, `prompt` and `show`, plus every action,
  including those that open, close and move panels and docks (`dispatch`), and
  every UI setting (`update_settings`, such as `"tab_bar"`, `"toolbar"` or
  `"theme"`).
- **Language servers**: `update_settings` with `"lsp"` and `"languages"`
  configures which servers run and how; `get_diagnostics` reads their results,
  `lsp_request` asks them anything, and actions such as
  `editor::GoToDefinition`, `editor::Hover` and `editor::Format` drive them.

### Compared to Emacs

| Emacs | Fool plugins |
|-------|--------------|
| Keymaps, major modes, `define-key` | Key bindings, `set_mode`, `bind_keys` |
| Minor modes | `set_minor_mode` |
| Advice | `override_action`, then `dispatch` the original |
| Reading every key (`read-key`) | `capture_keys` |
| Hooks (`after-save-hook`, `post-command-hook`, …) | `events` |
| Overlays and text properties | `set_decorations` |
| Special buffers (`*compilation*`, Magit) | `show` with `editable` and `mode` |
| `completing-read`, Consult | `pick`, live pickers |
| `M-x` commands defined at runtime | `register_command` |
| `defcustom` | `"plugins"` settings |
| `compile`, `shell-command` | `run_in_terminal` |
| Talking to LSP servers (Eglot) | `lsp_request` |

What plugins still can't do: draw arbitrary UI of their own (only the status
bar, pickers, prompts, notifications, buffers and decorations), add syntax
grammars or language servers (that's what Zed extensions do), or change the
editor's own code: they run as separate programs, not inside the editor like
Emacs Lisp.

A plugin's `keymap` file (named in `plugin.json`) loads with it. Your own
`keymap.json` loads after plugins, so it can still override any plugin binding.

A complete example is in `docs/plugins/hello`. Copy it to
`~/.config/zed/plugins/hello` to try it.

## Built-in Vim and Helix plugins

These are configured under `vim.plugins` in `settings.json`:

```json
{
  "vim": {
    "plugins": {
      "flash_find": { "enabled": true, "labels": "asdfgqwertyuiopzxcvbnm" },
      "completion_dismiss": { "enabled": true, "keys": ["j", "k"] },
      "substitute_preview": { "enabled": true }
    }
  }
}
```

- **flash_find**: after `f`, `F`, `t` or `T` jumps to a character, the other
  visible occurrences of it get a label. Typing a label jumps there, the same as
  `{count}f{char}` would, so it works in Vim normal and visual mode and in
  Helix's normal and select modes. Any other key removes the labels and does
  what it normally does, and `;` and `,` still repeat the find.
- **completion_dismiss**: typing one of `keys` hides the completions menu. The
  key still types, and the menu stays hidden until the cursor leaves the word,
  or until you ask for completions. Keys use keymap syntax, such as `"ctrl-k"`.
- **substitute_preview**: while you type `:s` or `:%s`, every match in range is
  highlighted and struck through, with its replacement shown in green after it.

Helix's `m i` and `m a` pick the closest pair. To make them work like Vim's
`i` and `a` (where, for example, `a"` also takes the space after the quote),
set:

```json
{ "vim": { "helix_text_objects": "vim" } }
```

## `:substitute`

`:s` follows Vim's regex syntax, including the magic levels `\v` (very magic),
`\m`, `\M` and `\V`, `\c` and `\C`, `\<` and `\>`, `\{n,m}` and `\{-}`, `\zs`
and `\ze`, the `\@=`, `\@!`, `\@<=` and `\@<!` assertions, back-references, and
`&`, `\0`-`\9`, `\r` and `\n` in the replacement. For example:

```
:%s/\v(\w+) (\w+)/\2 \1/g
:'<,'>s/\<foo\>/bar/gc
```

Case conversion in replacements (`\u`, `\U`, `\l`, `\L`) and `\=` expressions
aren't supported.

`space m` (or `:marks`) opens a searchable list of marks: type a mark's name
or part of its line, and press `enter` to jump there.

`:!command` runs a command in the terminal panel. Press `escape` or `ctrl-c` in
the editor afterwards to close it again.

## Your own modal keymap

To change some Vim or Helix keys, override them in `keymap.json` with the mode
in the context, such as `vim_mode == normal`, `vim_mode == helix_normal`,
`vim_mode == visual` or `vim_mode == insert`.

To design a modal keymap from scratch, turn the built-in one off and load your
own files:

```json
{
  "vim": {
    "keymap": {
      "load_builtin": false,
      "files": ["keymaps/my-modal.json"]
    }
  }
}
```

The files use the `keymap.json` format, relative paths are relative to the
config folder, and they load before `keymap.json`. Start from
`assets/keymaps/vim.json` in this repository to see every built-in binding.
Changes apply when you save `keymap.json` or change these settings.

## File explorer

`space e` opens the project panel from Vim or Helix normal mode, and its keys
follow Yazi:

| Key | Action |
|-----|--------|
| `j` / `k`, `g g` / `shift-g` | move, first, last |
| `h` / `l` | collapse or go to the parent / expand, or open a file |
| `enter`, `o` | open a file and close the explorer, or expand a directory |
| `q` | close the explorer |
| `escape` | go back to the editor |
| `y` / `x` / `p` | copy / cut / paste |
| `d` / `shift-d` | move to trash / delete |
| `a` / `shift-a` | new file (end the name with `/` for a directory) / new directory |
| `r` | rename |
| `.` | show or hide hidden files |
| `s` / `/` | find a file / search in a directory |
| `c c` / `c r` | copy the path / relative path |
| `shift-o` | open with the system |

## Jupyter notebooks and Google Colab

`.ipynb` files open as notebooks. Pick a kernel from the kernel picker in the
notebook's toolbar (`space n s`): local Jupyter kernels and Python environments
with `ipykernel` are listed, along with the kernels of any remote Jupyter server
you connect to.

### Running on Colab's GPUs and CPUs

Fool talks to Google's [Colab API](https://developers.google.com/colab/api), as
Google's Colab extension for VS Code does: sign in with Google, pick a GPU, TPU
or CPU, and cells run there. No browser tab needs to stay open.

**Once:** Google only lets apps it knows sign in, so make Fool an OAuth client of
your own. It's free:

1. In the [Google Cloud console](https://console.cloud.google.com), create a
   project (or pick one).
2. Enable the **Colab API** for it: *APIs & Services → Library*, search for
   "Colab API" and click *Enable*.
3. Under *Google Auth Platform*, set up the consent screen: user type
   *External*, and add your Google account under *Audience → Test users*.
4. Under *Clients*, create a client of type **Desktop app**, and put its ID and
   secret in your settings:

   ```json
   {
     "jupyter": {
       "colab": {
         "client_id": "123-abc.apps.googleusercontent.com",
         "client_secret": "GOCSPX-…"
       }
     }
   }
   ```

**Each time:**

1. Run `repl: new colab runtime` (`space n g` in a notebook). The first time,
   your browser opens to sign in with Google; Google warns that it hasn't
   verified the app, since it's your own, so continue. Fool keeps the sign-in in
   your system keychain.
2. Pick a runtime, like `T4 GPU` or `CPU`. Only those your account can use are
   listed, and Colab Pro adds more.
3. When Fool says it's ready, pick the runtime's kernel (`Colab T4 GPU …`) in
   the notebook's kernel picker (`space n s`). `!nvidia-smi` or
   `torch.cuda.is_available()` now see the GPU.

Runtimes you already have reconnect when Fool starts, and Fool refreshes their
short-lived tokens. Colab still stops idle runtimes and limits how long they
run, as in the browser. `repl: delete colab runtime` (`space n d`) frees one,
and `repl: sign out of colab` forgets the sign-in. The runtime's files are its
own, so upload data to it rather than expecting your local files there.

Without an OAuth client, `repl: connect to colab` (`space n t`) still works
through a Colab tab: it copies a cell that starts a Jupyter server behind a
Cloudflare tunnel; run it in a Colab notebook and paste the URL it prints.

`repl: connect to jupyter server` (`space n j`) connects to any other Jupyter
server the same way, given the URL with its token that `jupyter server` prints.
Servers are saved in your settings:

```json
{
  "jupyter": {
    "servers": [{ "name": "Lab", "url": "https://lab.example.com/?token=…" }]
  }
}
```

The token is stored in plain text in `settings.json`.

### Notebook keys

A notebook has a command mode for working with cells (press `escape` in a
cell's normal mode to get there) and an edit mode for typing in a cell (`i`,
`a` or `enter`).

| Action | Vim and Helix | Jupyter style |
|--------|---------------|---------------|
| Next / previous cell | `j` / `k` | `j` / `k`, `down` / `up` |
| First / last cell | `g g` / `shift-g` | |
| Edit the cell | `i`, `a`, `enter` | `enter` |
| Back to command mode | `escape` (in normal mode) | `escape` |
| Run the cell and go to the next | `shift-enter` | `shift-enter` |
| Run the cell | `ctrl-enter` | `ctrl-enter` |
| Run the cell and insert one below | `alt-enter` | `alt-enter` |
| Run all | `space n r` | `ctrl-shift-enter` |
| Run all above | `space n a` | |
| Run the cell and all below | `space n b` | |
| Insert a cell below / above | `o` / `shift-o` | `b` / `a` |
| Delete the cell | `d d` (also copies it) | `d d` |
| Undo deleting a cell | `u` | `z` |
| Copy / cut the cell | `y y` / `d d` | `c` / `x` |
| Paste below / above | `p` / `shift-p` | `v` / `shift-v` |
| Move the cell down / up | `shift-j` / `shift-k` | `alt-down` / `alt-up` |
| Turn into markdown / code | `space n m` / `space n y` | `m` / `y` |
| Clear the cell's output / all outputs | `space n x` / `space n c` | |
| Interrupt the kernel | `ctrl-c`, `space n i` | `i i` |
| Restart the kernel | `space n k` | `0 0`, `ctrl-shift-r` |
| Restart and run all | `space n shift-r` | |
| Pick a kernel | `space n s` | |
| New / delete Colab runtime | `space n g` / `space n d` | |
| Connect to Colab through a tab / a Jupyter server | `space n t` / `space n j` | |

The `space n` keys also work inside a cell in normal mode, and every action is
in the command palette under `notebook:`.

## Competitive programming

Fool receives problems from
[Competitive Companion](https://github.com/jmerle/competitive-companion) and
runs their tests, using the same workspace layout as
[cptui](https://github.com/TsaH0/cptui), so problems move freely between Fool,
the cptui TUI and its Neovim plugin. Its browser extension's "Fetch top 3
solutions" also works.

| Command | `:Cp` | Action |
|---------|-------|--------|
| `cp: start receiver` | `:Cp receive [start\|stop\|status]` | listen on `127.0.0.1:27121` |
| `cp: run tests` | `:Cp run [n]`, or `:Cp` | save, compile and run all tests (or test `n`) |
| `cp: show results` | `:Cp show` | reopen the results |
| `cp: add testcase` | `:Cp add` | add a testcase and open it |
| `cp: edit testcase` | `:Cp edit [n]` | open a testcase's input and expected output |
| `cp: delete testcase` | `:Cp delete n` | delete a testcase |
| `cp: accept output` | `:Cp accept [n]` | save the last output as expected |
| `cp: next problem` / `cp: previous problem` | `:Cp next` / `:Cp prev` | move through the contest |
| `cp: open problem` | `:Cp open` | pick any problem |
| `cp: open solutions` | `:Cp solutions` | open a fetched solution |
| `cp: open problem url` | `:Cp url` | open the problem page |

Settings are under `competitive_programming`: `workspace` (default `~/cp`),
`host`, `port`, `autostart`, `open_on_receive`, `source`, `template_file`,
`default_timeout_ms`, `timeout_multiplier` and `languages`. For each source file
extension, `languages` lists a `compile` command and a `run` command; in them,
`$(FNAME)` is the source, `$(BIN)` the binary and `$(DIR)` the problem folder.
C++, C, Rust, Python and Java are set up by default.
