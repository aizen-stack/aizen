#!/bin/sh
# Aizen installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/dawnofcd/aizen/main/install.sh | sh
#
# Downloads the latest optimized `aizen` binary from GitHub Releases into
# ~/.aizen/bin (override with $AIZEN_INSTALL) and makes it executable.
# Pure static binary — no toolchain, no Node/Python.
set -eu

repo="dawnofcd/aizen"
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux) plat="linux-x86_64" ;;
  Darwin)
    case "$arch" in
      arm64 | aarch64) plat="macos-aarch64" ;;
      x86_64) echo "aizen: Intel macs (x86_64) are no longer supported -- Apple Silicon (arm64) only." >&2; exit 1 ;;
      *) echo "aizen: unsupported macOS arch: $arch" >&2; exit 1 ;;
    esac ;;
  *) echo "aizen: unsupported OS: $os (use install.ps1 on Windows)" >&2; exit 1 ;;
esac

api="https://api.github.com/repos/$repo/releases/latest"
url="$(curl -fsSL "$api" | grep -o "https://github.com/[^\"]*aizen-[^\"]*${plat}" | head -1)"
if [ -z "$url" ]; then
  echo "aizen: no release asset for '$plat' yet (still building?) -- see $api" >&2
  exit 1
fi

dir="${AIZEN_INSTALL:-$HOME/.aizen}/bin"
mkdir -p "$dir"
dest="$dir/aizen"

echo "Downloading $(basename "$url") ..."
curl -fsSL "$url" -o "$dest"
chmod +x "$dest"
echo "aizen installed -> $dest"

# PATH hint (don't edit the user's profile silently).
case ":$PATH:" in
  *":$dir:"*) : ;;
  *)
    echo ""
    echo "Add aizen to your PATH (append to ~/.bashrc or ~/.zshrc):"
    echo "    export PATH=\"$dir:\$PATH\""
    ;;
esac
echo "Then run:  aizen config"
