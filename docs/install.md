# Apple Silicon development baseline

The only supported target is `aarch64-apple-darwin`. Rust 1.95.0, `cargo`,
`rustfmt`, `just`, `curl`, `shasum`, and POSIX shell are sufficient; the
workspace configuration fixes Cargo to four jobs with incremental compilation
disabled.

The distributed product is the `codex` CLI plus the adjacent
`codex-code-mode-host` companion process. The host is required for default-on
Code Mode and remains a separate executable. The CLI primary workflows are
interactive launch, `exec`, `login`, `logout`, `resume`, `fork`, `help`, and
`version`. The current parser also exposes `completion`, `doctor`, `debug`,
`apply`, `archive`, `delete`, `unarchive`, experimental `cloud`, and `features`;
this list is intentionally explicit so retained surface is visible. Other
workspace binaries are internal helpers, focused test fixtures, local
diagnostics, or the config-schema generator, not distribution products.

## Build and run

```sh
git clone https://github.com/code-akram/ycode.git
cd ycode/codex-rs
cargo metadata --locked --no-deps
just build
./target/debug/codex --version
./target/debug/codex --help
./target/debug/codex exec --help
./target/debug/codex-code-mode-host --help
```

`just build` is the canonical developer product build. Workspace default
members select both required executables, while the recipe downloads and
verifies the exact sandbox-enabled V8 archive and generated binding published
by OpenAI for the locked `v8` crate before invoking Cargo. It produces both
binaries in `target/debug`. Do not use `cargo build -p codex-cli` as a product
build because it omits the host.

Run the TUI with `just codex`. Set `CODEX_API_KEY` for
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
| Complete product graph | `just check` |
| Core test harness | `../scripts/build/build-product.sh test -p codex-core --lib --no-run` |
| Runtime test harness | `../scripts/build/build-product.sh test -p codex-cli-runtime --lib --no-run` |
| CLI parser harness | `../scripts/build/build-product.sh test -p codex-cli --bin codex --no-run` |
| TUI test harness | `../scripts/build/build-product.sh test -p codex-tui --lib --no-run` |
| Formatting | `cargo fmt --all --check` |
| Installer | `sh ../scripts/install/install_test.sh` |

Select exact tests inside those packages for authentication, Responses,
session/history, execution, skills, web/image, collaboration, and TUI behavior.
Set `RUST_MIN_STACK=8388608` for focused `codex-core` integration-test selectors;
their default test-thread stack is too small for the current debug harness.
The repository `justfile` contains only shortcuts for the same focused Cargo
operations.

## Manual installation

The retained POSIX installer supports Apple Silicon macOS native release
archives only. Each `codex-package-aarch64-apple-darwin.tar.gz` archive must
contain both `codex` and `codex-code-mode-host`; the installer places them
adjacent in the selected bin directory. When a release exists, run it explicitly:

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
