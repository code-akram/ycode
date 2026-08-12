#!/bin/sh

set -eu

REPOSITORY_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
WORKSPACE="$REPOSITORY_ROOT/codex-rs"
TARGET="aarch64-apple-darwin"
V8_VERSION="150.4.0"
V8_PROFILE="ptrcomp_sandbox_release"
V8_RELEASE_BASE="https://github.com/openai/codex/releases/download/rusty-v8-v$V8_VERSION"
ARCHIVE_NAME="librusty_v8_${V8_PROFILE}_${TARGET}.a.gz"
BINDING_NAME="src_binding_${V8_PROFILE}_${TARGET}.rs"
CHECKSUMS_NAME="rusty_v8_${V8_PROFILE}_${TARGET}.sha256"
CARGO_COMMAND="${1:-build}"

case "$CARGO_COMMAND" in
  build | check | test) shift || true ;;
  *)
    echo "usage: $0 [build|check|test] [cargo arguments...]" >&2
    exit 2
    ;;
esac

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "ycode builds support only $TARGET (Apple Silicon macOS)." >&2
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required to fetch the pinned sandboxed V8 build dependency." >&2
  exit 1
fi

if ! command -v shasum >/dev/null 2>&1; then
  echo "shasum is required to verify the pinned sandboxed V8 build dependency." >&2
  exit 1
fi

if [ -n "${RUSTY_V8_ARCHIVE:-}" ] || [ -n "${RUSTY_V8_SRC_BINDING_PATH:-}" ]; then
  if [ -z "${RUSTY_V8_ARCHIVE:-}" ] || [ -z "${RUSTY_V8_SRC_BINDING_PATH:-}" ]; then
    echo "RUSTY_V8_ARCHIVE and RUSTY_V8_SRC_BINDING_PATH must be set together." >&2
    exit 1
  fi
  cd "$WORKSPACE"
  exec cargo "$CARGO_COMMAND" "$@"
fi

CARGO_TARGET_ROOT="${CARGO_TARGET_DIR:-$WORKSPACE/target}"
V8_CACHE="$CARGO_TARGET_ROOT/rusty-v8-$V8_VERSION-$TARGET"
ARCHIVE="$V8_CACHE/$ARCHIVE_NAME"
BINDING="$V8_CACHE/$BINDING_NAME"
CHECKSUMS="$V8_CACHE/$CHECKSUMS_NAME"

mkdir -p "$V8_CACHE"

download() {
  url=$1
  destination=$2
  temporary="$destination.tmp.$$"
  trap 'rm -f "$temporary"' EXIT HUP INT TERM
  curl --fail --location --proto '=https' --tlsv1.2 "$url" --output "$temporary"
  mv "$temporary" "$destination"
  trap - EXIT HUP INT TERM
}

if [ ! -f "$CHECKSUMS" ]; then
  download "$V8_RELEASE_BASE/$CHECKSUMS_NAME" "$CHECKSUMS"
fi

expected_checksum() {
  artifact_name=$1
  awk -v name="$artifact_name" '$2 == name { print $1 }' "$CHECKSUMS"
}

verify() {
  artifact=$1
  artifact_name=$2
  expected=$(expected_checksum "$artifact_name")
  [ -n "$expected" ] || return 1
  actual=$(shasum -a 256 "$artifact" | awk '{ print $1 }')
  [ "$actual" = "$expected" ]
}

if ! verify "$ARCHIVE" "$ARCHIVE_NAME" 2>/dev/null; then
  download "$V8_RELEASE_BASE/$ARCHIVE_NAME" "$ARCHIVE"
fi
verify "$ARCHIVE" "$ARCHIVE_NAME" || {
  echo "Sandboxed V8 archive checksum verification failed." >&2
  exit 1
}

if ! verify "$BINDING" "$BINDING_NAME" 2>/dev/null; then
  download "$V8_RELEASE_BASE/$BINDING_NAME" "$BINDING"
fi
verify "$BINDING" "$BINDING_NAME" || {
  echo "Sandboxed V8 binding checksum verification failed." >&2
  exit 1
}

cd "$WORKSPACE"
RUSTY_V8_ARCHIVE="$ARCHIVE" \
  RUSTY_V8_SRC_BINDING_PATH="$BINDING" \
  exec cargo "$CARGO_COMMAND" "$@"
