set working-directory := "codex-rs"
set positional-arguments := true

set shell := ["sh", "-eu", "-c"]

rust_min_stack := "8388608"
# Display help
help:
    just -l

# `codex`

alias c := codex

codex *args:
    cargo run --bin codex -- {args}

# `codex exec`
exec *args:
    cargo run --bin codex -- exec {args}

# Run the CLI version of the file-search crate.
file-search *args:
    cargo run --bin codex-file-search -- {args}

# Run the standalone code-mode host from source.
code-mode-host *args:
    cargo run --bin codex-code-mode-host -- {args}

# Format Rust source.
fmt:
    cargo fmt --all

# Check formatting without modifying files.
fmt-check:
    cargo fmt --all --check

fix *args:
    cargo clippy --fix --tests --allow-dirty "$@"

clippy *args:
    cargo clippy --tests "$@"

install:
    rustup show active-toolchain
    cargo fetch

# Run nextest with --no-fail-fast so all tests are run.
#
# Run `cargo install --locked cargo-nextest` if you don't have it installed.
# Prefer this for routine local runs. Workspace crate features are banned, so

# there should be no need to add `--all-features`.
test *args:
    RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=local cargo nextest run --no-fail-fast "$@"

# Run explicit workspace benchmark targets.
bench *args:
    cargo bench --workspace --bench '*' {args}

# Run benchmark targets once to ensure they start successfully.
bench-smoke:
    just bench -- --test

# Regenerate the json schema for config.toml from the current config types.
write-config-schema:
    cargo run -p codex-core --bin codex-write-config-schema

[no-cd]
write-hooks-schema:
    cargo run --manifest-path {{ justfile_directory() }}/codex-rs/Cargo.toml -p codex-hooks --bin write_hooks_schema_fixtures

# Tail logs from the state SQLite database
log *args:
    if [ "${1:-}" = "--" ]; then shift; fi; cargo run -p codex-cli --bin logs_client -- "$@"
