#!/usr/bin/env python3
"""An example Tee plugin. Copy this folder to ~/.config/zed/plugins/hello."""

import json
import sys


def send(message):
    print(json.dumps(message), flush=True)


for line in sys.stdin:
    message = json.loads(line)
    kind = message.get("type")
    if kind == "initialize":
        continue
    if kind == "event":
        print(f"saved {message['data']['file']}", file=sys.stderr, flush=True)
        continue

    context = message.get("context", {})
    command = message.get("command")
    if command == "greet":
        name = message.get("args") or "world"
        send({"type": "notify", "message": f"Hello, {name}! You are in {context.get('file')}"})
    elif command == "count":
        words = len(context.get("text", "").split())
        send({"type": "show", "title": "Word count", "text": f"{words} words\n"})
    elif command == "upper":
        selections = context.get("selections", [])
        if len(selections) == 1:
            send({"type": "insert", "text": selections[0]["text"].upper()})
        else:
            send({"type": "notify", "message": "Select one range first", "level": "error"})
