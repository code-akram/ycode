#!/bin/sh

set -eu

REPOSITORY="code-akram/ycode"
TARGET="aarch64-apple-darwin"
ASSET="codex-package-$TARGET.tar.gz"
INSTALL_DIR="${YCODE_INSTALL_DIR:-$HOME/.local/bin}"
RELEASE="${YCODE_RELEASE:-latest}"
CODEX_BINARY_NAME="codex"
CODE_MODE_HOST_BINARY_NAME="codex-code-mode-host"

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

if [ -x "$EXTRACTED/bin/$CODEX_BINARY_NAME" ] &&
  [ -x "$EXTRACTED/bin/$CODE_MODE_HOST_BINARY_NAME" ]; then
  PACKAGE_BIN_DIR="$EXTRACTED/bin"
elif [ -x "$EXTRACTED/$CODEX_BINARY_NAME" ] &&
  [ -x "$EXTRACTED/$CODE_MODE_HOST_BINARY_NAME" ]; then
  PACKAGE_BIN_DIR="$EXTRACTED"
else
  echo "The downloaded archive must contain adjacent $CODEX_BINARY_NAME and $CODE_MODE_HOST_BINARY_NAME executables." >&2
  exit 1
fi

cp "$PACKAGE_BIN_DIR/$CODEX_BINARY_NAME" "$INSTALL_DIR/$CODEX_BINARY_NAME"
cp "$PACKAGE_BIN_DIR/$CODE_MODE_HOST_BINARY_NAME" "$INSTALL_DIR/$CODE_MODE_HOST_BINARY_NAME"
chmod 0755 "$INSTALL_DIR/$CODEX_BINARY_NAME" "$INSTALL_DIR/$CODE_MODE_HOST_BINARY_NAME"

printf 'Installed ycode CLI and Code Mode host at %s\n' "$INSTALL_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf 'Add %s to PATH before running codex.\n' "$INSTALL_DIR" ;;
esac
