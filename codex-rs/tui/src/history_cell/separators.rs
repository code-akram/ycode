//! Turn separators for transcript history.

use super::*;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;

#[derive(Debug)]
/// Compact metadata committed after each completed assistant turn.
pub struct FinalMessageSeparator {
    model: String,
    elapsed_seconds: Option<u64>,
}
impl FinalMessageSeparator {
    /// Creates a Mini-style summary; completed turns should pass protocol duration when available.
    pub(crate) fn new(model: String, elapsed_seconds: Option<u64>) -> Self {
        Self {
            model,
            elapsed_seconds,
        }
    }

    fn line(&self) -> Line<'static> {
        let duration = crate::status_indicator_widget::fmt_elapsed_compact(
            self.elapsed_seconds.unwrap_or_default(),
        );
        Line::from(format!("{} · {duration}", self.model)).dim()
    }
}
impl HistoryCell for FinalMessageSeparator {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        vec![truncate_line_with_ellipsis_if_overflow(
            self.line(),
            usize::from(width),
        )]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![Line::from(format!(
            "{} · {}",
            self.model,
            crate::status_indicator_widget::fmt_elapsed_compact(
                self.elapsed_seconds.unwrap_or_default(),
            )
        ))]
    }
}
