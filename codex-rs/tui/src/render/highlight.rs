//! Fixed syntax highlighting for the built-in TUI presentation.
//!
//! The extended `two_face` grammar set remains so fenced code, shell commands,
//! and diffs keep broad language coverage. Styling always uses its terminal-native
//! ANSI theme; there is no runtime theme selection or custom theme loading.

use ratatui::style::Color as RtColor;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::Color as SyntectColor;
use syntect::highlighting::Highlighter;
use syntect::highlighting::Style as SyntectStyle;
use syntect::highlighting::Theme;
use syntect::parsing::Scope;
use syntect::parsing::SyntaxReference;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

// Syntect/bat encode ANSI palette semantics in alpha:
// `a=0` => indexed ANSI palette via RGB payload, `a=1` => terminal default.
const ANSI_ALPHA_INDEX: u8 = 0x00;
const ANSI_ALPHA_DEFAULT: u8 = 0x01;
const OPAQUE_ALPHA: u8 = 0xFF;

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        two_face::theme::extra()
            .get(EmbeddedThemeName::Ansi)
            .clone()
    })
}

/// Raw RGB background colors extracted from syntax-theme diff scopes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DiffScopeBackgroundRgbs {
    pub inserted: Option<(u8, u8, u8)>,
    pub deleted: Option<(u8, u8, u8)>,
}

pub(crate) fn diff_scope_background_rgbs() -> DiffScopeBackgroundRgbs {
    diff_scope_background_rgbs_for_theme(theme())
}

fn diff_scope_background_rgbs_for_theme(theme: &Theme) -> DiffScopeBackgroundRgbs {
    let highlighter = Highlighter::new(theme);
    let inserted = scope_background_rgb(&highlighter, "markup.inserted")
        .or_else(|| scope_background_rgb(&highlighter, "diff.inserted"));
    let deleted = scope_background_rgb(&highlighter, "markup.deleted")
        .or_else(|| scope_background_rgb(&highlighter, "diff.deleted"));
    DiffScopeBackgroundRgbs { inserted, deleted }
}

fn scope_background_rgb(highlighter: &Highlighter<'_>, scope_name: &str) -> Option<(u8, u8, u8)> {
    let scope = Scope::new(scope_name).ok()?;
    let bg = highlighter.style_mod_for_stack(&[scope]).background?;
    Some((bg.r, bg.g, bg.b))
}

pub(crate) fn foreground_style_for_scopes(scope_names: &[&str]) -> Option<Style> {
    let highlighter = Highlighter::new(theme());
    scope_names.iter().find_map(|scope_name| {
        let scope = Scope::new(scope_name).ok()?;
        let fg = highlighter.style_mod_for_stack(&[scope]).foreground?;
        convert_syntect_color(fg).map(|fg| Style::default().fg(fg))
    })
}

#[allow(clippy::disallowed_methods)]
fn ansi_palette_color(index: u8) -> RtColor {
    match index {
        0x00 => RtColor::Black,
        0x01 => RtColor::Red,
        0x02 => RtColor::Green,
        0x03 => RtColor::Yellow,
        0x04 => RtColor::Blue,
        0x05 => RtColor::Magenta,
        0x06 => RtColor::Cyan,
        0x07 => RtColor::Gray,
        n => RtColor::Indexed(n),
    }
}

#[allow(clippy::disallowed_methods)]
fn convert_syntect_color(color: SyntectColor) -> Option<RtColor> {
    match color.a {
        ANSI_ALPHA_INDEX => Some(ansi_palette_color(color.r)),
        ANSI_ALPHA_DEFAULT => None,
        OPAQUE_ALPHA => Some(RtColor::Rgb(color.r, color.g, color.b)),
        _ => Some(RtColor::Rgb(color.r, color.g, color.b)),
    }
}

fn convert_style(syn_style: SyntectStyle) -> Style {
    let mut rt_style = Style::default();
    if let Some(fg) = convert_syntect_color(syn_style.foreground) {
        rt_style = rt_style.fg(fg);
    }
    rt_style
}

fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let syntax_set = syntax_set();
    let normalized = lang.to_ascii_lowercase();
    let patched = match normalized.as_str() {
        "csharp" | "c-sharp" => "c#",
        "cu" | "cuh" | "cppm" | "cxxm" | "ixx" => "cpp",
        "golang" => "go",
        "python3" => "python",
        "shell" => "bash",
        _ => lang,
    };

    syntax_set
        .find_syntax_by_token(patched)
        .or_else(|| syntax_set.find_syntax_by_name(patched))
        .or_else(|| {
            let lower = patched.to_ascii_lowercase();
            syntax_set
                .syntaxes()
                .iter()
                .find(|syntax| syntax.name.to_ascii_lowercase() == lower)
        })
        .or_else(|| syntax_set.find_syntax_by_extension(lang))
}

const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;
pub(crate) const MAX_HIGHLIGHT_LINE_BYTES: usize = 4 * 1024;

pub(crate) fn exceeds_highlight_limits(total_bytes: usize, total_lines: usize) -> bool {
    total_bytes > MAX_HIGHLIGHT_BYTES || total_lines > MAX_HIGHLIGHT_LINES
}

fn highlight_to_line_spans_with_theme(
    code: &str,
    lang: &str,
    theme: &Theme,
) -> Option<Vec<Vec<Span<'static>>>> {
    if code.is_empty()
        || code.len() > MAX_HIGHLIGHT_BYTES
        || code.lines().count() > MAX_HIGHLIGHT_LINES
        || code
            .lines()
            .any(|line| line.len() > MAX_HIGHLIGHT_LINE_BYTES)
    {
        return None;
    }

    let syntax = find_syntax(lang)?;
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges = highlighter.highlight_line(line, syntax_set()).ok()?;
        let mut spans = Vec::new();
        for (style, text) in ranges {
            let text = text.trim_end_matches(['\n', '\r']);
            if !text.is_empty() {
                spans.push(Span::styled(text.to_string(), convert_style(style)));
            }
        }
        if spans.is_empty() {
            spans.push(Span::raw(String::new()));
        }
        lines.push(spans);
    }
    Some(lines)
}

fn highlight_to_line_spans(code: &str, lang: &str) -> Option<Vec<Vec<Span<'static>>>> {
    highlight_to_line_spans_with_theme(code, lang, theme())
}

pub(crate) fn highlight_code_to_lines(code: &str, lang: &str) -> Vec<Line<'static>> {
    if let Some(line_spans) = highlight_to_line_spans(code, lang) {
        line_spans.into_iter().map(Line::from).collect()
    } else {
        let mut result: Vec<Line<'static>> = code
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect();
        if result.is_empty() {
            result.push(Line::from(String::new()));
        }
        result
    }
}

pub(crate) fn highlight_bash_to_lines(script: &str) -> Vec<Line<'static>> {
    highlight_code_to_lines(script, "bash")
}

pub(crate) fn highlight_code_to_styled_spans(
    code: &str,
    lang: &str,
) -> Option<Vec<Vec<Span<'static>>>> {
    highlight_to_line_spans(code, lang)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn ansi_theme_uses_only_terminal_palette_markers() {
        let theme = theme();
        let colors = theme
            .scopes
            .iter()
            .flat_map(|item| [item.style.foreground, item.style.background])
            .flatten()
            .chain(theme.settings.foreground)
            .chain(theme.settings.background);
        assert!(
            colors
                .into_iter()
                .all(|color| { matches!(color.a, ANSI_ALPHA_INDEX | ANSI_ALPHA_DEFAULT) })
        );
    }

    #[test]
    fn highlights_known_language_and_preserves_text() {
        let lines = highlight_code_to_lines("fn main() {}\n", "rust");
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "fn main() {}"
        );
    }

    #[test]
    fn unknown_language_falls_back_to_plain_text() {
        assert_eq!(
            highlight_code_to_lines("plain", "definitely-unknown"),
            vec![Line::from("plain")]
        );
    }

    #[test]
    fn oversized_line_falls_back_to_plain_text() {
        let code = "x".repeat(MAX_HIGHLIGHT_LINE_BYTES + 1);
        assert_eq!(
            highlight_code_to_lines(&code, "rust"),
            vec![Line::from(code)]
        );
    }
}
