#!/usr/bin/env sh
# Installs one of the example plugins in docs/plugins into
# ~/.config/zed/plugins, then `plugins: reload` (or a restart) loads it.
#
#   curl -fsSL https://raw.githubusercontent.com/TsaH0/Tee/main/script/install-fool-plugin.sh | sh -s multicursor
#
# Set FOOL_VERSION to a release tag to install the plugin as of that release.
set -eu

name="${1:?usage: install-fool-plugin.sh <plugin name, like multicursor>}"
ref="${FOOL_VERSION:-main}"
base="https://raw.githubusercontent.com/TsaH0/Tee/$ref/docs/plugins/$name"
target="${XDG_CONFIG_HOME:-$HOME/.config}/zed/plugins/$name"

mkdir -p "$target"
curl -fsSL "$base/plugin.json" -o "$target/plugin.json"
# The manifest names the other files: the keymap and the program's arguments.
files="$(python3 - "$target/plugin.json" <<'PY'
import json, re, sys
text = open(sys.argv[1]).read()
# Manifests may have comments and trailing commas.
text = re.sub(r"^\s*//.*$", "", text, flags=re.M)
text = re.sub(r",(\s*[}\]])", r"\1", text)
manifest = json.loads(text)
names = [manifest.get("keymap")] + manifest.get("run", [])[1:]
print("\n".join(name for name in names if name and "/" not in name and "." in name))
PY
)"
for file in $files; do
    curl -fsSL "$base/$file" -o "$target/$file"
done
echo "Installed the $name plugin into $target. Run \`plugins: reload\` in Fool."
