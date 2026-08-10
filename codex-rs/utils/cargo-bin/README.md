# codex-utils-cargo-bin

This crate provides helpers for locating workspace binaries and test resources
in Cargo builds.

Function behavior:
- `cargo_bin`: reads Cargo's `CARGO_BIN_EXE_*` environment variables and accepts
  absolute binary paths.
- `find_resource!`: locates fixtures relative to `CARGO_MANIFEST_DIR` in Cargo
  runs.
