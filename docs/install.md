# Apple Silicon development baseline

The only supported target is `aarch64-apple-darwin`. Rust 1.95.0, `cargo`,
`rustfmt`, and POSIX shell are sufficient; the workspace configuration fixes
Cargo to four jobs with incremental compilation disabled.

The distributed product binary is `codex`, built by `codex-cli`. Its primary
workflows are interactive launch, `exec`, `login`, `logout`, `resume`, `fork`,
`help`, and `version`. The current parser also exposes `completion`, `doctor`,
`debug`, `apply`, `archive`, `delete`, `unarchive`, experimental `cloud`, and
`features`; this list is intentionally explicit so retained surface is visible.
Other workspace binaries are internal helpers, focused test fixtures, local
diagnostics, or the config-schema generator, not separate distribution products.

## Build and run

```sh
git clone https://github.com/code-akram/ycode.git
cd ycode/codex-rs
cargo metadata --locked --no-deps
cargo build -p codex-cli
cargo run -p codex-cli --bin codex -- --version
cargo run -p codex-cli --bin codex -- --help
cargo run -p codex-cli --bin codex -- exec --help
```

Run the TUI with `cargo run -p codex-cli --bin codex`. Set `CODEX_API_KEY` for
OpenAI API-key authentication, or use `codex login` for ChatGPT subscription
authentication. The API key is environment-only; ChatGPT credentials retain the
existing `CODEX_HOME/auth.json` path and format.

## Focused validation matrix

Run only the affected rows. Do not use a full-workspace, release, TUI, or
nextest sweep as the default development check.

| Surface | Command |
| --- | --- |
| Dependency graph | `cargo metadata --locked --no-deps` |
| Current config schema | `cargo run -p codex-core --bin codex-write-config-schema` |
| Production CLI graph | `cargo check -p codex-cli` |
| Core test harness | `cargo test -p codex-core --lib --no-run` |
| Runtime test harness | `cargo test -p codex-cli-runtime --lib --no-run` |
| CLI parser harness | `cargo test -p codex-cli --bin codex --no-run` |
| TUI test harness | `cargo test -p codex-tui --lib --no-run` |
| Formatting | `cargo fmt --all --check` |
| Installer | `sh -n ../scripts/install/install.sh` |

Select exact tests inside those packages for authentication, Responses,
session/history, execution, skills, web/image, collaboration, and TUI behavior.
Set `RUST_MIN_STACK=8388608` for focused `codex-core` integration-test selectors;
their default test-thread stack is too small for the current debug harness.
The repository `justfile` contains only shortcuts for the same focused Cargo
operations.

## Manual installation

The retained POSIX installer supports Apple Silicon macOS native release archives
only. When a release exists, run it explicitly:

```sh
curl -fsSL https://raw.githubusercontent.com/code-akram/ycode/main/scripts/install/install.sh | sh
```

Set `YCODE_RELEASE` to an explicit release name or `YCODE_INSTALL_DIR` to change
the destination. The installer performs release resolution only while it is
being run; the CLI has no updater.

## Local logs

The TUI keeps bounded local diagnostics. To enable a plaintext log for a run:

```sh
codex -c log_dir=./.codex-log
tail -F ./.codex-log/codex-tui.log
```
