set working-directory := "codex-rs"
set positional-arguments := true
set shell := ["sh", "-eu", "-c"]

# Display help
help:
    just -l

# `codex`

alias c := codex

# Build the complete product pair: the CLI and its adjacent Code Mode host.
build:
    ../scripts/build/build-product.sh build

codex *args: build
    ./target/debug/codex {args}

# `codex exec`
exec *args: build
    ./target/debug/codex exec {args}

# Check the complete product graph.
check:
    ../scripts/build/build-product.sh check

# Run the CLI version of the file-search crate.
file-search *args:
    cargo run --bin codex-file-search -- {args}

# Run the separately built code-mode host.
code-mode-host *args: build
    ./target/debug/codex-code-mode-host {args}

# Format Rust source.
fmt:
    cargo fmt --all

# Check formatting without modifying files.
fmt-check:
    cargo fmt --all --check

# Regenerate the json schema for config.toml from the current config types.
write-config-schema:
    cargo run -p codex-core --bin codex-write-config-schema

# Tail logs from the state SQLite database
log *args:
    if [ "${1:-}" = "--" ]; then shift; fi; cargo run -p codex-cli --bin logs_client -- "$@"
