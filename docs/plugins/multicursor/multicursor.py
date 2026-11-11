#!/usr/bin/env python3
"""Multiple cursors, as a Fool plugin.

Copy this folder to ~/.config/zed/plugins/multicursor and run `plugins: reload`.
In normal or visual mode (Vim or Helix):

  space m n   add a cursor at the next match of the word under the cursor, or
              of the selection (the first press selects the word itself)
  space m p   the same, backwards
  space m s   skip: move the last added cursor to the next match instead
  space m a   select every match
  space m j   add a cursor on the line below (space m k: above)
  space m c   remove every cursor but the first

Then edit as usual: `c`, `d`, `i`, `a` and the rest act on every cursor.
"""

import json
import re
import sys

WORD_CHARACTER = re.compile(r"\w")


def to_offset(text, position):
    """A 1-based line and column, counting characters, as an offset in `text`."""
    lines = text.split("\n")
    line = min(max(position["line"], 1), len(lines)) - 1
    column = min(max(position["column"], 1) - 1, len(lines[line]))
    return sum(len(previous) + 1 for previous in lines[:line]) + column


def to_position(text, offset):
    before = text[:offset]
    return {
        "line": before.count("\n") + 1,
        "column": offset - (before.rfind("\n") + 1) + 1,
    }


def word_at(text, offset):
    """The (start, end) of the word at or just before `offset`, or None."""
    if offset < len(text) and WORD_CHARACTER.match(text[offset]):
        start = offset
    elif offset > 0 and WORD_CHARACTER.match(text[offset - 1]):
        start = offset - 1
    else:
        return None
    end = start
    while start > 0 and WORD_CHARACTER.match(text[start - 1]):
        start -= 1
    while end < len(text) and WORD_CHARACTER.match(text[end]):
        end += 1
    return start, end


def matches(text, needle, whole_word):
    """Every (start, end) where `needle` occurs."""
    if not needle:
        return []
    pattern = re.escape(needle)
    if whole_word:
        pattern = rf"(?<!\w){pattern}(?!\w)"
    return [(found.start(), found.end()) for found in re.finditer(pattern, text)]


class Selections:
    """The editor's selections as sorted (start, end) offsets."""

    def __init__(self, text, selections):
        self.text = text
        self.spans = sorted(
            tuple(sorted((to_offset(text, s["start"]), to_offset(text, s["end"]))))
            for s in selections
        )

    def needle(self):
        """What to search for: the selected text, or the word at the cursor.

        Returns (needle, whole_word, spans), where spans are the selections
        with an empty first one widened to its word.
        """
        spans = list(self.spans)
        start, end = spans[0]
        if start == end:
            word = word_at(self.text, start)
            if word is None:
                return None, False, spans
            spans[0] = word
            return self.text[word[0]:word[1]], True, spans
        # A selection that is exactly a word, as the first press leaves it,
        # matches whole words, like a search for the word under the cursor.
        return self.text[start:end], word_at(self.text, start) == (start, end), spans


def next_match(found, spans, after, forward):
    """The nearest match not already selected, wrapping around the text."""
    free = [span for span in found if span not in spans]
    if not free:
        return None
    if forward:
        later = [span for span in free if span[0] >= after]
        return later[0] if later else free[0]
    earlier = [span for span in free if span[1] <= after]
    return earlier[-1] if earlier else free[-1]


def add_match(text, selections, forward):
    current = Selections(text, selections)
    needle, whole_word, spans = current.needle()
    if needle is None:
        return None
    # The first press only selects the word under the cursor.
    if spans != current.spans and len(spans) == 1:
        return spans
    found = matches(text, needle, whole_word)
    after = spans[-1][1] if forward else spans[0][0]
    span = next_match(found, spans, after, forward)
    return sorted(spans + [span]) if span else spans


def skip_match(text, selections):
    current = Selections(text, selections)
    needle, whole_word, spans = current.needle()
    if needle is None or len(spans) < 2:
        return add_match(text, selections, True)
    last = spans.pop()
    found = matches(text, needle, whole_word)
    span = next_match(found, spans + [last], last[1], True)
    return sorted(spans + [span]) if span else spans + [last]


def all_matches(text, selections):
    needle, whole_word, _ = Selections(text, selections).needle()
    if needle is None:
        return None
    return matches(text, needle, whole_word)


def add_line(text, selections, below):
    """A cursor on the line below the last cursor, or above the first one."""
    spans = Selections(text, selections).spans
    head = spans[-1][1] if below else spans[0][0]
    position = to_position(text, head)
    line = position["line"] + (1 if below else -1)
    lines = text.split("\n")
    if line < 1 or line > len(lines):
        return spans
    column = min(position["column"], len(lines[line - 1]) + 1)
    offset = to_offset(text, {"line": line, "column": column})
    cursors = [(start, start) for start, _ in spans] + [(offset, offset)]
    return sorted(set(cursors))


def clear(text, selections):
    first = Selections(text, selections).spans[0][0]
    return [(first, first)]


COMMANDS = {
    "add_next": lambda text, selections: add_match(text, selections, True),
    "add_previous": lambda text, selections: add_match(text, selections, False),
    "skip": skip_match,
    "all": all_matches,
    "add_below": lambda text, selections: add_line(text, selections, True),
    "add_above": lambda text, selections: add_line(text, selections, False),
    "clear": clear,
}


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def main():
    # Initialize, then one command at a time. Each command only sends
    # requests without waiting for answers, so nothing else can arrive in
    # between that would need keeping.
    for line in sys.stdin:
        message = json.loads(line)
        if message.get("type") != "command":
            continue
        context = message.get("context") or {}
        text, selections = context.get("text"), context.get("selections")
        command = COMMANDS.get(message.get("command"))
        if command is None or text is None or not selections:
            continue
        spans = command(text, selections)
        if spans:
            send({"type": "set_selections", "selections": [
                {"start": to_position(text, start), "end": to_position(text, end)}
                for start, end in spans
            ]})
            send({"type": "set_status", "text": f"{len(spans)} cursors" if len(spans) > 1 else None})


if __name__ == "__main__":
    main()
