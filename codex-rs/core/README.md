# codex-core

This crate implements ycode's Rust CLI business logic on `aarch64-apple-darwin`.

## Helper dispatch

Expects the binary containing `codex-core` to simulate the virtual
`apply_patch` CLI when `arg1` is `--codex-run-as-apply-patch`. See the
`codex-arg0` crate for details.
