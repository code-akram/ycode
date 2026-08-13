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

resolve_cargo() {
  if cargo_path=$(command -v cargo 2>/dev/null) && [ -x "$cargo_path" ]; then
    CARGO="$cargo_path"
    return
  fi

  if command -v rustup >/dev/null 2>&1; then
    if cargo_path=$(cd "$WORKSPACE" && rustup which cargo 2>/dev/null) &&
      [ -x "$cargo_path" ]; then
      CARGO="$cargo_path"
      PATH="$(dirname -- "$cargo_path"):$PATH"
      export PATH
      return
    fi
    echo "Unable to resolve Cargo: rustup could not find cargo for $WORKSPACE/rust-toolchain.toml." >&2
    exit 1
  fi

  echo "Unable to resolve Cargo: neither cargo nor rustup is available on PATH." >&2
  exit 1
}

resolve_cargo

if ! command -v shasum >/dev/null 2>&1; then
  echo "shasum is required to verify build dependencies." >&2
  exit 1
fi

RUSTC="$(dirname -- "$CARGO")/rustc"
if [ ! -x "$RUSTC" ]; then
  echo "Unable to resolve rustc adjacent to the repository-pinned Cargo." >&2
  exit 1
fi
if ! RUSTC_VERSION=$($RUSTC -vV 2>&1); then
  echo "Unable to probe repository-pinned rustc at $RUSTC." >&2
  exit 1
fi
case "$RUSTC_VERSION" in
  *"release: 1.95.0"*) ;;
  *)
    echo "Repository-pinned rustc must report release 1.95.0." >&2
    exit 1
    ;;
esac
case "$RUSTC_VERSION" in
  *"commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860"*) ;;
  *)
    echo "Repository-pinned rustc must report commit 59807616e1fa2540724bfbac14d7976d7e4a3860." >&2
    exit 1
    ;;
esac
case "$RUSTC_VERSION" in
  *"host: aarch64-apple-darwin"*) ;;
  *)
    echo "Repository-pinned rustc must target host aarch64-apple-darwin." >&2
    exit 1
    ;;
esac

CARGO_TARGET_ROOT="${CARGO_TARGET_DIR:-$WORKSPACE/target}"
NATIVE_SDK_DIR="$CARGO_TARGET_ROOT/native-code-mode-sdk"
NATIVE_SDK="$NATIVE_SDK_DIR/libycode_native_sdk.rlib"
NATIVE_SDK_TMP="$NATIVE_SDK.tmp.$$"
mkdir -p "$NATIVE_SDK_DIR"
trap 'rm -f "$NATIVE_SDK_TMP"' EXIT HUP INT TERM
"$RUSTC" \
  --crate-name ycode_native_sdk \
  --crate-type rlib \
  --edition=2024 \
  --target=aarch64-apple-darwin \
  -Copt-level=0 \
  -Cdebuginfo=0 \
  -Cmetadata=ycode-native-sdk-v1 \
  -o "$NATIVE_SDK_TMP" \
  "$WORKSPACE/native-code-mode-sdk/src/lib.rs"
mv "$NATIVE_SDK_TMP" "$NATIVE_SDK"
trap - EXIT HUP INT TERM
YCODE_NATIVE_SDK_RLIB="$NATIVE_SDK"
YCODE_NATIVE_SDK_HASH=$(shasum -a 256 "$NATIVE_SDK" | awk '{ print $1 }')
export YCODE_NATIVE_SDK_RLIB YCODE_NATIVE_SDK_HASH

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required to fetch the pinned sandboxed V8 build dependency." >&2
  exit 1
fi

if [ -n "${RUSTY_V8_ARCHIVE:-}" ] || [ -n "${RUSTY_V8_SRC_BINDING_PATH:-}" ]; then
  if [ -z "${RUSTY_V8_ARCHIVE:-}" ] || [ -z "${RUSTY_V8_SRC_BINDING_PATH:-}" ]; then
    echo "RUSTY_V8_ARCHIVE and RUSTY_V8_SRC_BINDING_PATH must be set together." >&2
    exit 1
  fi
  cd "$WORKSPACE"
  exec "$CARGO" "$CARGO_COMMAND" "$@"
fi

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
  exec "$CARGO" "$CARGO_COMMAND" "$@"
