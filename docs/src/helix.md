---
title: Helix Mode - Zed
description: Helix-style keybindings and modal editing in Zed. Selection-first editing built on top of Vim mode.
---

# Helix Mode

_Work in progress. Not all Helix keybindings are implemented yet._

Zed's Helix mode is an emulation layer that brings Helix-style keybindings and modal editing to Zed. It builds upon Zed's [Vim mode](./vim.md), so much of the core functionality is shared. Enabling `helix_mode` will also enable `vim_mode`.

For a guide on Vim-related features that are also available in Helix mode, please refer to our [Vim mode documentation](./vim.md).

To check the current status of Helix mode, or to request a missing Helix feature, see the ["Are we Helix yet?" discussion](https://github.com/zed-industries/zed/discussions/33580).

For a detailed list of Helix's default keybindings, please visit the [official Helix documentation](https://docs.helix-editor.com/keymap.html).

## Core differences

Any text object that works with `m i` or `m a` also works with `]` and `[`, so for example `] (` selects the next pair of parentheses after the cursor.

In Helix mode, some text object keys follow Helix's meanings instead of Vim's: `m i m` and `m a m` select the closest surrounding pair of any kind (brackets, quotes, backticks, or vertical bars), `t` is the type/class text object, `c` is the comment text object, and `x` is the (X)HTML element text object.

Text objects for a specific pair, such as `m i (` or `m a "`, also work when the cursor is not inside that pair: they select the pair enclosing the cursor if there is one, otherwise the next pair on the cursor's line, then the previous pair on that line, then the nearest pair elsewhere. Quotes are paired from the start of the cursor's line and backslash-escaped quotes are skipped, so the closing quote of one string is never paired with the opening quote of the next one.

Some keys deliberately differ from Helix's defaults:

| Key | Helix | Zed's Helix mode |
| --- | --- | --- |
| `enter` | not bound | jump labels, like `g w` |
| `alt-m` | not bound | select inside the next pair of brackets or quotes that opens to the right of the cursor on the same line (a pair the cursor is inside is skipped) |
| `m i b` / `m a b` | not a text object | the nearest `()`, `[]` or `{}` |
| `m i q` / `m a q` | not a text object | the nearest `"`, `'` or `` ` `` quotes |
| `m i A` / `m a A` | not a text object | `<>` |
| `space e` / `space E` | file explorer in the project root / current file's folder | the project panel / the current file revealed in it |
| `space g` | changed-file picker | the git panel |
| `space G` | debug menu | the uncommitted changes (the debug menu is on `space B`) |
| `space x` | not bound | the command palette with `:! ` typed, to run a shell command |
| `space X` | not bound | show or hide the terminal panel with the shell command's output |

`space j` opens a searchable picker over the jumplist (the locations that `ctrl-o` and `ctrl-i`/`tab` move through), newest first.

In insert mode, Helix mode adds readline-style movement on top of Helix's own insert bindings: `ctrl-p`/`ctrl-n` move up/down a line, `ctrl-b`/`ctrl-f` move left/right a character, `ctrl-a`/`ctrl-e` go to the start/end of the line, and `alt-b`/`alt-f` move to the previous word start/next word end. While the completion menu is open, `ctrl-n`/`ctrl-p` still select completions. Helix's `ctrl-k`, `ctrl-d`, `ctrl-h`, `ctrl-j`, `alt-d` and `alt-backspace` work as in Helix. `ctrl-x` keeps Zed's `ctrl-x` chords (for example `ctrl-x ctrl-o` for completions) instead of Helix's completion.

Like Helix, the closest surrounding pair (`m i m`, `m a m`, `m d m`, `m r m`) is matched using the language's syntax tree, so delimiters inside string literals or comments don't count as pairs. In buffers without a language, no pair is matched.
