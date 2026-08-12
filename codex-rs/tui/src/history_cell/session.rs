//! Session headers, onboarding guidance, and transcript cards.

use super::*;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::width::display_width;

/// Render `lines` inside a border whose inner width is at least `inner_width`.
///
/// This is useful when callers have already clamped their content to a
/// specific width and want the border math centralized here instead of
/// duplicating padding logic in the TUI widgets themselves.
pub(crate) fn with_border_with_inner_width(
    lines: Vec<Line<'static>>,
    inner_width: usize,
) -> Vec<Line<'static>> {
    use crate::line_truncation::line_width;

    let max_line_width = lines.iter().map(line_width).max().unwrap_or(0);
    let content_width = inner_width.max(max_line_width);

    let mut out = Vec::with_capacity(lines.len() + 2);
    let border_inner_width = content_width + 2;
    out.push(vec![format!("╭{}╮", "─".repeat(border_inner_width)).dim()].into());

    for line in lines.into_iter() {
        let used_width = line_width(&line);
        let span_count = line.spans.len();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(span_count + 4);
        spans.push(Span::from("│ ").dim());
        spans.extend(line);
        if used_width < content_width {
            spans.push(Span::from(" ".repeat(content_width - used_width)).dim());
        }
        spans.push(Span::from(" │").dim());
        out.push(Line::from(spans));
    }

    out.push(vec![format!("╰{}╯", "─".repeat(border_inner_width)).dim()].into());

    out
}

#[derive(Debug)]
pub struct SessionInfoCell(CompositeHistoryCell);

impl HistoryCell for SessionInfoCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.0.display_lines(width)
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.0.desired_height(width)
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.0.transcript_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.0.raw_lines()
    }
}

pub(crate) fn new_session_info(
    config: &Config,
    _requested_model: &str,
    session: &ThreadSessionState,
    _is_first_event: bool,
    _auth_plan: Option<PlanType>,
    _show_fast_status: bool,
) -> SessionInfoCell {
    // The initial identity is ordinary history, not application chrome. Once
    // committed it belongs to native terminal scrollback like every other row.
    let header = SessionHeaderHistoryCell::new(
        session.model.clone(),
        session.reasoning_effort.clone(),
        /*show_fast_status*/ false,
        config.cwd.to_path_buf(),
        CODEX_CLI_VERSION,
    );
    SessionInfoCell(CompositeHistoryCell {
        parts: vec![Box::new(header)],
    })
}

#[derive(Debug)]
pub(crate) struct SessionHeaderHistoryCell {
    directory: PathBuf,
}

impl SessionHeaderHistoryCell {
    pub(crate) fn new(
        model: String,
        reasoning_effort: Option<ReasoningEffortConfig>,
        show_fast_status: bool,
        directory: PathBuf,
        version: &'static str,
    ) -> Self {
        Self::new_with_style(
            model,
            Style::default(),
            reasoning_effort,
            show_fast_status,
            directory,
            version,
        )
    }

    pub(crate) fn new_with_style(
        _model: String,
        _model_style: Style,
        _reasoning_effort: Option<ReasoningEffortConfig>,
        _show_fast_status: bool,
        directory: PathBuf,
        _version: &'static str,
    ) -> Self {
        Self { directory }
    }

    fn format_directory(&self, max_width: Option<usize>) -> String {
        Self::format_directory_inner(&self.directory, max_width)
    }

    pub(crate) fn format_directory_inner(directory: &Path, max_width: Option<usize>) -> String {
        let formatted = if let Some(rel) = relativize_to_home(directory) {
            if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display())
            }
        } else {
            directory.display().to_string()
        };

        if let Some(max_width) = max_width {
            if max_width == 0 {
                return String::new();
            }
            if display_width(formatted.as_str()) > max_width {
                return crate::text_formatting::center_truncate_path(&formatted, max_width);
            }
        }

        formatted
    }
}

impl HistoryCell for SessionHeaderHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let max_width = usize::from(width);
        let directory = self.format_directory(Some(max_width));
        vec![
            Line::default(),
            Line::from("ycode").bold(),
            truncate_line_with_ellipsis_if_overflow(Line::from(directory).dim(), max_width),
            Line::default(),
        ]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![
            Line::from("ycode"),
            Line::from(self.format_directory(/*max_width*/ None)),
        ]
    }
}
