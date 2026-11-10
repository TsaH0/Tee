---
title: Fool Plugins and Customization
description: Runtime plugins, built-in Vim/Helix plugins, custom modal keymaps, :substitute, the file explorer and competitive programming in Fool.
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
  // Editor events to receive: "saved" and "opened".
  "events": ["saved"],
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
| `initialize` | `protocol_version` (1), `editor` ("fool"), `plugin_dir`, `config_dir` |
| `command` | `id`, `command`, `args` (text after the ex command), `context` |
| `event` | `event` ("saved" or "opened"), `data.file` |

`context` has `file`, `language`, `worktrees`, `cursor` and `selections` (lines
and columns start at 1, and each selection has its `text`), plus `text` for
commands with `wants_text`.

The plugin can send these at any time, not only in reply to a command, so a
plugin can also run its own servers or watchers:

| Request | Fields | Effect |
|---------|--------|--------|
| `notify` | `message`, `level` ("info" or "error") | Shows a notification |
| `open` | `path`, `line`, `column` | Opens a file, optionally at a position |
| `show` | `title`, `text`, `language` | Shows text in a read-only tab, replacing an earlier `show` with the same title |
| `insert` | `text` | Replaces the selections of the active editor (or inserts at the cursors) |
| `set_text` | `text` | Replaces the whole text of the active editor |
| `dispatch` | `action`, `args` | Runs any action, e.g. `"editor::SelectAll"` or `"workspace::NewTerminal"` |

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
