//! Read-path helpers for Codex memories.
//!
//! This crate owns memory injection and memory citation parsing. It intentionally
//! does not depend on the memory write pipeline.

pub mod citations;

use codex_utils_absolute_path::AbsolutePathBuf;

pub fn memory_root(codex_home: &AbsolutePathBuf) -> AbsolutePathBuf {
    codex_home.join("memories")
}
