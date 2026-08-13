//! Shared horizontal geometry for the inline transcript.

use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::terminal_hyperlinks::HyperlinkLine;

const TRANSCRIPT_LEFT_GUTTER: u16 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptGutter {
    pub(crate) left: u16,
    pub(crate) content_width: u16,
}

pub(crate) fn layout(total_width: u16) -> TranscriptGutter {
    let left = TRANSCRIPT_LEFT_GUTTER.min(total_width.saturating_sub(1));
    TranscriptGutter {
        left,
        content_width: total_width.saturating_sub(left).max(1),
    }
}

pub(crate) fn inset_rect(area: Rect) -> Rect {
    let gutter = layout(area.width);
    Rect::new(
        area.x.saturating_add(gutter.left),
        area.y,
        gutter.content_width,
        area.height,
    )
}

pub(crate) fn prefix_hyperlink_lines(
    lines: Vec<HyperlinkLine>,
    total_width: u16,
) -> Vec<HyperlinkLine> {
    let left = usize::from(layout(total_width).left);
    if left == 0 {
        return lines;
    }
    let prefix = " ".repeat(left);
    lines
        .into_iter()
        .map(|mut line| {
            line.line.spans.insert(0, prefix.clone().into());
            for hyperlink in &mut line.hyperlinks {
                hyperlink.columns.start += left;
                hyperlink.columns.end += left;
            }
            line
        })
        .collect()
}

pub(crate) fn prefix_lines(lines: Vec<Line<'static>>, total_width: u16) -> Vec<Line<'static>> {
    prefix_hyperlink_lines(
        lines.into_iter().map(HyperlinkLine::from).collect(),
        total_width,
    )
    .into_iter()
    .map(|line| line.line)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_hyperlinks::TerminalHyperlink;

    #[test]
    fn gutter_collapses_to_keep_one_content_column() {
        assert_eq!(
            layout(/*total_width*/ 0),
            TranscriptGutter {
                left: 0,
                content_width: 1,
            }
        );
        assert_eq!(
            layout(/*total_width*/ 1),
            TranscriptGutter {
                left: 0,
                content_width: 1,
            }
        );
        assert_eq!(
            layout(/*total_width*/ 2),
            TranscriptGutter {
                left: 1,
                content_width: 1,
            }
        );
        assert_eq!(
            layout(/*total_width*/ 80),
            TranscriptGutter {
                left: 2,
                content_width: 78,
            }
        );
    }

    #[test]
    fn prefix_shifts_hyperlink_columns_without_changing_destination() {
        let line = HyperlinkLine {
            line: Line::from("docs"),
            hyperlinks: vec![TerminalHyperlink::web(
                0..4,
                "https://example.com".to_string(),
            )],
        };

        let prefixed = prefix_hyperlink_lines(vec![line], /*total_width*/ 20);

        assert_eq!(prefixed[0].line.to_string(), "  docs");
        assert_eq!(prefixed[0].hyperlinks[0].columns, 2..6);
        assert_eq!(prefixed[0].hyperlinks[0].destination, "https://example.com");
    }
}
