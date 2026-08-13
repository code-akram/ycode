#!/bin/sh

set -eu

REPOSITORY_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
WORKSPACE="$REPOSITORY_ROOT/codex-rs"
TARGET_ROOT="${CARGO_TARGET_DIR:-$WORKSPACE/target}"
CARGO=$(command -v cargo)
RUSTC="$(dirname -- "$CARGO")/rustc"
VERSION=$($RUSTC -vV)

if ! command -v shasum >/dev/null 2>&1; then
  echo "native SDK helper requires shasum" >&2
  exit 1
fi

case "$VERSION" in *"release: 1.95.0"*) ;; *) exit 1 ;; esac
case "$VERSION" in *"commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860"*) ;; *) exit 1 ;; esac
case "$VERSION" in *"host: aarch64-apple-darwin"*) ;; *) exit 1 ;; esac

SDK_DIR="$TARGET_ROOT/native-code-mode-sdk"
SDK="$SDK_DIR/libycode_native_sdk.rlib"
TEMPORARY="$SDK.tmp.$$"
mkdir -p "$SDK_DIR"
trap 'rm -f "$TEMPORARY"' EXIT HUP INT TERM
"$RUSTC" --crate-name ycode_native_sdk --crate-type rlib --edition=2024 \
  --target=aarch64-apple-darwin -Copt-level=0 -Cdebuginfo=0 \
  -Cmetadata=ycode-native-sdk-v1 -o "$TEMPORARY" \
  "$WORKSPACE/native-code-mode-sdk/src/lib.rs"
mv "$TEMPORARY" "$SDK"
trap - EXIT HUP INT TERM
YCODE_NATIVE_SDK_RLIB="$SDK"
YCODE_NATIVE_SDK_HASH=$(shasum -a 256 "$SDK" | awk '{ print $1 }')
export YCODE_NATIVE_SDK_RLIB YCODE_NATIVE_SDK_HASH

V8_CACHE="$TARGET_ROOT/rusty-v8-150.4.0-aarch64-apple-darwin"
V8_ARCHIVE="$V8_CACHE/librusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.a.gz"
V8_BINDING="$V8_CACHE/src_binding_ptrcomp_sandbox_release_aarch64-apple-darwin.rs"
if [ -f "$V8_ARCHIVE" ] && [ -f "$V8_BINDING" ]; then
  RUSTY_V8_ARCHIVE="$V8_ARCHIVE"
  RUSTY_V8_SRC_BINDING_PATH="$V8_BINDING"
  export RUSTY_V8_ARCHIVE RUSTY_V8_SRC_BINDING_PATH
fi

cd "$WORKSPACE"
exec "$CARGO" "$@"
