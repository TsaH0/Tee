#!/usr/bin/env python3
"""A selection-first modal mode in the style of the Ki editor, as a Fool plugin.

Copy this folder to ~/.config/zed/plugins/ki-mode, run `plugins: reload`, then
press `space k` in normal mode (or run `:Ki`). In Ki mode, `c`, `w` and `e`
choose whether the selection is a character, a word or a line, `l` and `h`
select the next and previous one, `shift-l` extends the selection, `d`
deletes it, `i` and `a` insert before and after it, and `escape` leaves.
"""

import json
import re
import sys

WORD = re.compile(r"\w+|[^\w\s]+")


def units(text, unit):
    """The (start, end) offsets of every unit in `text`."""
    if unit == "char":
        return [(index, index + 1) for index, character in enumerate(text) if character != "\n"]
    if unit == "line":
        spans, start = [], 0
        for line in text.split("\n"):
            if line:
                spans.append((start, start + len(line)))
            start += len(line) + 1
        return spans
    return [(found.start(), found.end()) for found in WORD.finditer(text)]


def next_unit(text, unit, selection, forward):
    """The unit after (or before) `selection`, a (start, end) pair."""
    start, end = selection
    spans = units(text, unit)
    if forward:
        later = [span for span in spans if span[0] >= end and span != selection]
        return later[0] if later else None
    earlier = [span for span in spans if span[1] <= start and span != selection]
    return earlier[-1] if earlier else None


def unit_at(text, unit, offset):
    """The unit containing `offset`, else the next one, else the last one."""
    spans = units(text, unit)
    for span in spans:
        if span[0] <= offset < span[1]:
            return span
    later = [span for span in spans if span[0] >= offset]
    if later:
        return later[0]
    return spans[-1] if spans else None


def to_offset(text, position):
    lines = text.split("\n")
    line = min(position["line"], len(lines)) - 1
    return sum(len(previous) + 1 for previous in lines[:line]) + position["column"] - 1


def to_position(text, offset):
    before = text[:offset]
    line = before.count("\n")
    return {"line": line + 1, "column": offset - (before.rfind("\n") + 1) + 1}


class Editor:
    """Talks to Fool over stdin and stdout."""

    def __init__(self, stdin, stdout):
        self.stdin, self.stdout, self.next_id = stdin, stdout, 0
        # Commands and events that arrive while waiting for a response.
        self.pending = []

    def messages(self):
        """Every command and event, including those that came in early."""
        while True:
            if self.pending:
                yield self.pending.pop(0)
                continue
            line = self.stdin.readline()
            if not line:
                return
            yield json.loads(line)

    def request(self, message):
        self.next_id += 1
        message["id"] = self.next_id
        self.stdout.write(json.dumps(message) + "\n")
        self.stdout.flush()
        while True:
            line = self.stdin.readline()
            if not line:
                raise SystemExit(0)
            reply = json.loads(line)
            if reply.get("type") == "response" and reply.get("id") == self.next_id:
                if "error" in reply:
                    raise RuntimeError(reply["error"])
                return reply.get("result")
            self.pending.append(reply)


class KiMode:
    def __init__(self, editor):
        self.editor, self.unit = editor, "word"

    def state(self):
        context = self.editor.request({"type": "get_editor", "text": True})
        text = context["text"]
        selection = context["selections"][0]
        start, end = to_offset(text, selection["start"]), to_offset(text, selection["end"])
        return text, (min(start, end), max(start, end))

    def select(self, text, span):
        self.editor.request({"type": "set_selections", "selections": [
            {"start": to_position(text, span[0]), "end": to_position(text, span[1])}]})

    def status(self):
        self.editor.request({"type": "set_status", "text": f"KI {self.unit.upper()}"})

    def run(self, command, args):
        if command == "enter":
            self.editor.request({"type": "set_mode", "mode": "ki", "cursor": "block"})
            self.select_unit_at_cursor()
        elif command == "leave":
            self.editor.request({"type": "set_mode", "mode": None})
            self.editor.request({"type": "set_status", "text": None})
        elif command == "unit":
            self.unit = args if args in ("char", "word", "line") else "word"
            self.select_unit_at_cursor()
        elif command in ("next", "previous", "extend_next"):
            text, selection = self.state()
            span = next_unit(text, self.unit, selection, command != "previous")
            if span:
                if command == "extend_next":
                    span = (selection[0], span[1])
                self.select(text, span)
        elif command == "delete":
            text, (start, end) = self.state()
            self.editor.request({"type": "edit", "edits": [{
                "start": to_position(text, start), "end": to_position(text, end), "text": ""}]})
        elif command in ("insert_before", "insert_after"):
            text, (start, end) = self.state()
            offset = start if command == "insert_before" else end
            self.select(text, (offset, offset))
            self.editor.request({"type": "set_mode", "mode": None})
            self.editor.request({"type": "set_status", "text": None})
            self.editor.request({"type": "dispatch", "action": "vim::SwitchToInsertMode"})

    def select_unit_at_cursor(self):
        text, (start, _) = self.state()
        span = unit_at(text, self.unit, start)
        if span:
            self.select(text, span)
        self.status()


def main():
    editor = Editor(sys.stdin, sys.stdout)
    mode = KiMode(editor)
    for message in editor.messages():
        if message.get("type") == "command":
            try:
                mode.run(message["command"], message.get("args", ""))
            except RuntimeError as error:
                print(f"ki-mode: {error}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    main()
