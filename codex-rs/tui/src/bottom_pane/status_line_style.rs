//! Styling for the fixed built-in footer status line.

use ratatui::prelude::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use super::status_line_setup::StatusLineItem;

const STATUS_LINE_SEPARATOR: &str = " · ";

pub(crate) fn status_line_from_segments<I>(segments: I) -> Option<Line<'static>>
where
    I: IntoIterator<Item = (StatusLineItem, String)>,
{
    let mut spans = Vec::new();
    for (item, text) in segments {
        if !spans.is_empty() {
            spans.push(STATUS_LINE_SEPARATOR.dim());
        }
        let span = match item {
            StatusLineItem::ModelWithReasoning => Span::from(text).cyan(),
            StatusLineItem::CurrentDir => Span::from(text).green(),
        };
        spans.push(span);
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_status_line_preserves_order_and_text() {
        let line = status_line_from_segments([
            (
                StatusLineItem::ModelWithReasoning,
                "gpt-5.4 high".to_string(),
            ),
            (StatusLineItem::CurrentDir, "/repo".to_string()),
        ])
        .expect("status line");

        assert_eq!(line.to_string(), "gpt-5.4 high · /repo");
    }
}
