#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPOSITORY_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)
WORKSPACE="$REPOSITORY_ROOT/codex-rs"
WRAPPER="$SCRIPT_DIR/build-product.sh"
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/ycode-build-product-test.XXXXXX")
trap 'find "$TEST_ROOT" -depth -delete' EXIT HUP INT TERM

FAKE_BIN="$TEST_ROOT/bin"
RESOLVED_BIN="$TEST_ROOT/resolved"
UTILITY_BIN="$TEST_ROOT/utilities"
LOG="$TEST_ROOT/launcher.log"
mkdir -p "$FAKE_BIN" "$RESOLVED_BIN" "$UTILITY_BIN"
for utility in awk curl dirname mkdir mv rm shasum uname; do
  utility_path=$(command -v "$utility")
  ln -s "$utility_path" "$UTILITY_BIN/$utility"
done
TEST_PATH="$FAKE_BIN:$UTILITY_BIN"

# The generated mock script must retain these expressions for its own runtime.
# shellcheck disable=SC2016
write_fake_cargo() {
  destination=$1
  printf '%s\n' \
    '#!/bin/sh' \
    '[ -z "${BUILD_PRODUCT_EXPECTED_TOOLCHAIN_BIN:-}" ] || case ":$PATH:" in *":$BUILD_PRODUCT_EXPECTED_TOOLCHAIN_BIN:"*) ;; *) exit 72 ;; esac' \
    'printf "cargo|%s|%s|%s|%s|%s|%s\n" "$PWD" "$*" "${RUSTY_V8_ARCHIVE:-}" "${RUSTY_V8_SRC_BINDING_PATH:-}" "${YCODE_NATIVE_SDK_RLIB:-}" "${YCODE_NATIVE_SDK_HASH:-}" >> "$BUILD_PRODUCT_TEST_LOG"' \
    >"$destination"
  chmod 0755 "$destination"
}

# The generated mock accepts only the pinned probe and the one direct SDK rlib build.
# shellcheck disable=SC2016
write_fake_rustc() {
  destination=$1
  printf '%s\n' \
    '#!/bin/sh' \
    'if [ "$#" -eq 1 ] && [ "$1" = "-vV" ]; then' \
    '  if [ "${BUILD_PRODUCT_FAKE_RUSTC_VERSION:-}" = wrong ]; then printf "%s\n" "release: 1.94.0"; exit 0; fi' \
    '  printf "%s\n" "rustc 1.95.0 (59807616e 2026-08-03)" "release: 1.95.0" "commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860" "host: aarch64-apple-darwin"' \
    '  exit 0' \
    'fi' \
    'output=' \
    'previous=' \
    'for argument in "$@"; do' \
    '  if [ "$previous" = "-o" ]; then output=$argument; fi' \
    '  previous=$argument' \
    'done' \
    '[ -n "$output" ] || exit 73' \
    'printf "fake native SDK rlib\n" > "$output"' \
    'printf "rustc|%s\n" "$*" >> "$BUILD_PRODUCT_TEST_LOG"' \
    >"$destination"
  chmod 0755 "$destination"
}

# The generated mock script must retain these expressions for its own runtime.
# shellcheck disable=SC2016
write_fake_rustup() {
  destination=$1
  printf '%s\n' \
    '#!/bin/sh' \
    'printf "rustup|%s|%s\n" "$PWD" "$*" >> "$BUILD_PRODUCT_TEST_LOG"' \
    '[ "$PWD" = "$BUILD_PRODUCT_EXPECTED_WORKSPACE" ] || exit 70' \
    '[ "$1" = "which" ] && [ "$2" = "cargo" ] || exit 71' \
    'printf "%s\n" "$BUILD_PRODUCT_RESOLVED_CARGO"' \
    >"$destination"
  chmod 0755 "$destination"
}

fail() {
  printf 'build product contract failure: %s\n' "$1" >&2
  exit 1
}

write_fake_cargo "$FAKE_BIN/cargo"
write_fake_cargo "$RESOLVED_BIN/cargo"
write_fake_rustc "$FAKE_BIN/rustc"
write_fake_rustc "$RESOLVED_BIN/rustc"
write_fake_rustup "$FAKE_BIN/rustup"

# A mismatched adjacent compiler fails with an actionable pinned-release diagnostic.
ERROR_LOG="$TEST_ROOT/wrong-rustc.log"
if PATH="$TEST_PATH" \
  BUILD_PRODUCT_TEST_LOG="$LOG" \
  BUILD_PRODUCT_FAKE_RUSTC_VERSION=wrong \
  RUSTY_V8_ARCHIVE="$TEST_ROOT/explicit-archive" \
  RUSTY_V8_SRC_BINDING_PATH="$TEST_ROOT/explicit-binding" \
  "$WRAPPER" check -p wrong-rustc 2>"$ERROR_LOG"; then
  fail "wrapper accepted the wrong repository-pinned rustc"
fi
grep -F 'Repository-pinned rustc must report release 1.95.0.' "$ERROR_LOG" >/dev/null ||
  fail "wrong-rustc failure was not actionable"

# An ordinary cargo on PATH wins, including in the explicit V8 override branch.
: >"$LOG"
PATH="$TEST_PATH" \
  BUILD_PRODUCT_TEST_LOG="$LOG" \
  BUILD_PRODUCT_EXPECTED_WORKSPACE="$WORKSPACE" \
  BUILD_PRODUCT_RESOLVED_CARGO="$RESOLVED_BIN/cargo" \
  RUSTY_V8_ARCHIVE="$TEST_ROOT/explicit-archive" \
  RUSTY_V8_SRC_BINDING_PATH="$TEST_ROOT/explicit-binding" \
  "$WRAPPER" check -p cargo-precedence
grep -F "cargo|$WORKSPACE|check -p cargo-precedence|$TEST_ROOT/explicit-archive|$TEST_ROOT/explicit-binding|" "$LOG" >/dev/null ||
  fail "cargo on PATH was not invoked"
grep -F 'rustc|--crate-name ycode_native_sdk --crate-type rlib --edition=2024 --target=aarch64-apple-darwin -Copt-level=0 -Cdebuginfo=0 -Cmetadata=ycode-native-sdk-v1 -o ' "$LOG" >/dev/null ||
  fail "native SDK was not direct-compiled with the canonical inputs"
if grep -F 'rustup|' "$LOG" >/dev/null; then
  fail "rustup was invoked even though cargo was on PATH"
fi

# With no cargo on PATH, rustup resolves cargo from the pinned workspace.
rm "$FAKE_BIN/cargo"
: >"$LOG"
PATH="$TEST_PATH" \
  BUILD_PRODUCT_TEST_LOG="$LOG" \
  BUILD_PRODUCT_EXPECTED_WORKSPACE="$WORKSPACE" \
  BUILD_PRODUCT_RESOLVED_CARGO="$RESOLVED_BIN/cargo" \
  BUILD_PRODUCT_EXPECTED_TOOLCHAIN_BIN="$RESOLVED_BIN" \
  RUSTY_V8_ARCHIVE="$TEST_ROOT/explicit-archive" \
  RUSTY_V8_SRC_BINDING_PATH="$TEST_ROOT/explicit-binding" \
  "$WRAPPER" test -p rustup-fallback
grep -F "rustup|$WORKSPACE|which cargo" "$LOG" >/dev/null ||
  fail "rustup did not resolve cargo from the workspace"
grep -F "cargo|$WORKSPACE|test -p rustup-fallback|$TEST_ROOT/explicit-archive|$TEST_ROOT/explicit-binding|" "$LOG" >/dev/null ||
  fail "the cargo resolved by rustup was not invoked"

# The verified V8 branch uses the same rustup-resolved cargo launcher.
TARGET_ROOT="$TEST_ROOT/target"
V8_CACHE="$TARGET_ROOT/rusty-v8-150.4.0-aarch64-apple-darwin"
ARCHIVE_NAME="librusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.a.gz"
BINDING_NAME="src_binding_ptrcomp_sandbox_release_aarch64-apple-darwin.rs"
CHECKSUMS_NAME="rusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.sha256"
mkdir -p "$V8_CACHE"
printf 'archive fixture\n' >"$V8_CACHE/$ARCHIVE_NAME"
printf 'binding fixture\n' >"$V8_CACHE/$BINDING_NAME"
archive_checksum=$(shasum -a 256 "$V8_CACHE/$ARCHIVE_NAME" | awk '{ print $1 }')
binding_checksum=$(shasum -a 256 "$V8_CACHE/$BINDING_NAME" | awk '{ print $1 }')
printf '%s  %s\n%s  %s\n' \
  "$archive_checksum" "$ARCHIVE_NAME" \
  "$binding_checksum" "$BINDING_NAME" \
  >"$V8_CACHE/$CHECKSUMS_NAME"
: >"$LOG"
PATH="$TEST_PATH" \
  BUILD_PRODUCT_TEST_LOG="$LOG" \
  BUILD_PRODUCT_EXPECTED_WORKSPACE="$WORKSPACE" \
  BUILD_PRODUCT_RESOLVED_CARGO="$RESOLVED_BIN/cargo" \
  BUILD_PRODUCT_EXPECTED_TOOLCHAIN_BIN="$RESOLVED_BIN" \
  CARGO_TARGET_DIR="$TARGET_ROOT" \
  "$WRAPPER" build
grep -F "cargo|$WORKSPACE|build|$V8_CACHE/$ARCHIVE_NAME|$V8_CACHE/$BINDING_NAME|$TARGET_ROOT/native-code-mode-sdk/libycode_native_sdk.rlib|" "$LOG" >/dev/null ||
  fail "verified V8 branch did not use rustup-resolved cargo with paired artifacts"

# A shell with neither launcher receives a direct, useful failure.
ERROR_LOG="$TEST_ROOT/neither-found.log"
if PATH="$UTILITY_BIN" \
  RUSTY_V8_ARCHIVE="$TEST_ROOT/explicit-archive" \
  RUSTY_V8_SRC_BINDING_PATH="$TEST_ROOT/explicit-binding" \
  "$WRAPPER" build 2>"$ERROR_LOG"; then
  fail "wrapper succeeded without cargo or rustup"
fi
grep -F 'Unable to resolve Cargo: neither cargo nor rustup is available on PATH.' "$ERROR_LOG" >/dev/null ||
  fail "neither-found failure was not useful"

printf 'build product Cargo launcher contract: PASS\n'
