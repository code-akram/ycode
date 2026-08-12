#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
INSTALLER="$SCRIPT_DIR/install.sh"

fail() {
  printf 'installer contract failure: %s\n' "$1" >&2
  exit 1
}

assert_contains() {
  grep -F -- "$1" "$INSTALLER" >/dev/null || fail "missing $1"
}

assert_absent() {
  if grep -F -- "$1" "$INSTALLER" >/dev/null; then
    fail "unsupported surface present: $1"
  fi
}

sh -n "$INSTALLER"

assert_contains 'REPOSITORY="code-akram/ycode"'
assert_contains 'TARGET="aarch64-apple-darwin"'
# These assertions intentionally match unexpanded shell expressions.
# shellcheck disable=SC2016
assert_contains 'ASSET="codex-package-$TARGET.tar.gz"'
# shellcheck disable=SC2016
assert_contains 'RELEASE="${YCODE_RELEASE:-latest}"'
assert_contains 'CODEX_BINARY_NAME="codex"'
assert_contains 'CODE_MODE_HOST_BINARY_NAME="codex-code-mode-host"'
# shellcheck disable=SC2016
assert_contains 'https://github.com/$REPOSITORY/releases/latest/download/$ASSET'
# shellcheck disable=SC2016
assert_contains 'https://github.com/$REPOSITORY/releases/download/$RELEASE/$ASSET'
assert_contains "curl --fail --location --proto '=https' --tlsv1.2"
# shellcheck disable=SC2016
assert_contains 'cp "$PACKAGE_BIN_DIR/$CODEX_BINARY_NAME" "$INSTALL_DIR/$CODEX_BINARY_NAME"'
# shellcheck disable=SC2016
assert_contains 'cp "$PACKAGE_BIN_DIR/$CODE_MODE_HOST_BINARY_NAME" "$INSTALL_DIR/$CODE_MODE_HOST_BINARY_NAME"'
# shellcheck disable=SC2016
assert_contains 'chmod 0755 "$INSTALL_DIR/$CODEX_BINARY_NAME" "$INSTALL_DIR/$CODE_MODE_HOST_BINARY_NAME"'

assert_absent 'npm'
assert_absent 'Homebrew'
assert_absent 'brew install'
assert_absent 'x86_64'
assert_absent 'Linux'

printf 'installer contract: PASS\n'
