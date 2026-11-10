#!/usr/bin/env sh
# Installs or updates Tee (this fork of Zed) from its GitHub releases into
# ~/.local, making it available as ~/.local/bin/zed.
#
#   curl -fsSL https://raw.githubusercontent.com/TsaH0/Tee/main/script/install-tee.sh | sh
#
# Set TEE_VERSION to a release tag (e.g. tee-v0.1.0) to install that release
# instead of the latest one.
set -eu

repo="TsaH0/Tee"
version="${TEE_VERSION:-latest}"

if [ "$(uname -s)" != "Linux" ]; then
    echo "Tee releases are only built for Linux. Build from source with script/install-linux instead."
    exit 1
fi

case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    aarch64 | arm64) arch="aarch64" ;;
    *)
        echo "Unsupported architecture $(uname -m)"
        exit 1
        ;;
esac

if [ "$version" = "latest" ]; then
    release_url="https://github.com/$repo/releases/latest/download"
    script_ref="main"
else
    release_url="https://github.com/$repo/releases/download/$version"
    script_ref="$version"
fi

temp="$(mktemp -d "${TMPDIR:-/tmp}/tee-XXXXXX")"
trap 'rm -rf "$temp"' EXIT

echo "Downloading Tee ($version) for $arch"
curl -fL "$release_url/zed-linux-$arch.tar.gz" -o "$temp/zed-linux-$arch.tar.gz"
curl -fsSL "https://raw.githubusercontent.com/$repo/$script_ref/script/install.sh" -o "$temp/install.sh"

# Releases are built on the dev channel, so they install as zed-dev.app with
# their own desktop entry next to any official Zed install, though the `zed`
# command in ~/.local/bin is pointed at Tee.
ZED_CHANNEL=dev ZED_BUNDLE_PATH="$temp/zed-linux-$arch.tar.gz" sh "$temp/install.sh"
