#!/bin/sh

set -eu

REPOSITORY="code-akram/ycode"
TARGET="aarch64-apple-darwin"
ASSET="codex-package-$TARGET.tar.gz"
INSTALL_DIR="${YCODE_INSTALL_DIR:-$HOME/.local/bin}"
RELEASE="${YCODE_RELEASE:-latest}"

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "ycode supports only $TARGET (Apple Silicon macOS)." >&2
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required to install ycode." >&2
  exit 1
fi

if ! command -v tar >/dev/null 2>&1; then
  echo "tar is required to install ycode." >&2
  exit 1
fi

case "$RELEASE" in
  latest)
    DOWNLOAD_URL="https://github.com/$REPOSITORY/releases/latest/download/$ASSET"
    ;;
  *)
    DOWNLOAD_URL="https://github.com/$REPOSITORY/releases/download/$RELEASE/$ASSET"
    ;;
esac

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT HUP INT TERM

ARCHIVE="$TMP_DIR/$ASSET"
EXTRACTED="$TMP_DIR/extracted"
mkdir -p "$EXTRACTED" "$INSTALL_DIR"

printf 'Downloading ycode for %s...\n' "$TARGET"
curl --fail --location --proto '=https' --tlsv1.2 "$DOWNLOAD_URL" --output "$ARCHIVE"
tar -xzf "$ARCHIVE" -C "$EXTRACTED"

if [ -x "$EXTRACTED/bin/codex" ]; then
  BINARY="$EXTRACTED/bin/codex"
elif [ -x "$EXTRACTED/codex" ]; then
  BINARY="$EXTRACTED/codex"
else
  echo "The downloaded archive does not contain a codex binary." >&2
  exit 1
fi

cp "$BINARY" "$INSTALL_DIR/codex"
chmod 0755 "$INSTALL_DIR/codex"

printf 'Installed ycode CLI at %s\n' "$INSTALL_DIR/codex"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf 'Add %s to PATH before running codex.\n' "$INSTALL_DIR" ;;
esac
