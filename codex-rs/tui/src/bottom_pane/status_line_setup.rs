//! Fixed built-in status-line items.

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum StatusLineItem {
    ModelWithReasoning,
    CurrentDir,
}
