#!/bin/sh
# Installs the latest mailidx release into ~/.local/bin (override with MAILIDX_BIN_DIR).
set -eu

repo="jx-grxf/mailidx"
asset="mailidx-macos-universal.tar.gz"
bin_dir="${MAILIDX_BIN_DIR:-$HOME/.local/bin}"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "mailidx reads Apple Mail and only runs on macOS." >&2
  exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

base="https://github.com/$repo/releases/latest/download"
echo "Downloading $asset ..."
curl -fsSL "$base/$asset" -o "$tmp/$asset"
curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256"

expected="$(awk '{print $1}' "$tmp/$asset.sha256")"
actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
if [ "$expected" != "$actual" ]; then
  echo "Checksum mismatch, nothing installed." >&2
  exit 1
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$bin_dir"
install -m 755 "$tmp/mailidx" "$bin_dir/mailidx"
echo "Installed $("$bin_dir/mailidx" --version) to $bin_dir/mailidx"
echo

"$bin_dir/mailidx" doctor || true

cat <<MSG

Register it with Claude Code:
  claude mcp add --scope user mail -- $bin_dir/mailidx

Other MCP clients: run $bin_dir/mailidx as a stdio server with no arguments.
MSG

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "Note: $bin_dir is not on your PATH." ;;
esac
