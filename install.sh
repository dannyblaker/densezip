#!/usr/bin/env bash
# densezip installer for Linux/macOS: downloads the latest stable dnz binary
# from GitHub releases, verifies its checksum, and installs it. Re-run any
# time to update.
#
#   curl -fsSL https://raw.githubusercontent.com/dannyblaker/densezip/master/install.sh | bash
#
# Environment overrides:
#   DNZ_INSTALL_DIR   install directory (default: ~/.local/bin)
#   DNZ_VERSION       tag to install, e.g. v0.1.0 (default: latest stable)
set -euo pipefail

REPO="dannyblaker/densezip"
INSTALL_DIR="${DNZ_INSTALL_DIR:-$HOME/.local/bin}"

err() { echo "densezip install: $*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *) err "unsupported OS: $(uname -s). Build from source: cargo build --release" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) err "unsupported architecture: $(uname -m). Build from source: cargo build --release" ;;
esac
target="$arch-$os"

if [ -n "${DNZ_VERSION:-}" ]; then
  base="https://github.com/$REPO/releases/download/$DNZ_VERSION"
else
  base="https://github.com/$REPO/releases/latest/download"
fi
asset="dnz-$target.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading $asset ..."
curl -fsSL "$base/$asset" -o "$tmp/$asset" ||
  err "download failed — is there a release with a $target binary? See https://github.com/$REPO/releases"
curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256" || err "checksum download failed"
(cd "$tmp" && { sha256sum -c "$asset.sha256" >/dev/null 2>&1 || shasum -a 256 -c "$asset.sha256" >/dev/null; }) ||
  err "checksum verification failed"

tar xzf "$tmp/$asset" -C "$tmp" dnz
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/dnz" "$INSTALL_DIR/dnz"

echo "installed $("$INSTALL_DIR/dnz" --version) to $INSTALL_DIR/dnz"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo "note: $INSTALL_DIR is not on your PATH. Add it with e.g.:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac
