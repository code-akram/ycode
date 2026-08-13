use crate::color::blend;
use crate::color::is_light;
use crate::terminal_palette::StdoutColorLevel;
use crate::terminal_palette::best_color;
use crate::terminal_palette::best_color_for_level;
use crate::terminal_palette::default_bg;
use crate::terminal_palette::default_fg;
use crate::terminal_palette::rgb_color;
use crate::terminal_palette::stdout_color_level;
use ratatui::style::Color;
use ratatui::style::Style;

const HUMAN_PROMPT_DARK_RGB: (u8, u8, u8) = (45, 212, 191);
const HUMAN_PROMPT_LIGHT_RGB: (u8, u8, u8) = (0, 121, 107);
const OPERATIONAL_BLUE_DARK_RGB: (u8, u8, u8) = (96, 165, 250);
const OPERATIONAL_BLUE_LIGHT_RGB: (u8, u8, u8) = (0, 82, 180);
// Decorative table rules should remain visible without competing with cell content.
const TABLE_SEPARATOR_FG_ALPHA: f32 = 0.20;

pub fn user_message_style() -> Style {
    user_message_style_for(default_bg())
}

/// Returns a low-contrast rule style for separators within markdown tables.
pub(crate) fn table_separator_style() -> Style {
    table_separator_style_for(default_fg(), default_bg(), stdout_color_level())
}

/// Returns the shared accent style for active or selected TUI controls.
pub(crate) fn accent_style() -> Style {
    accent_style_for(default_bg())
}

/// Returns the exclusive foreground style for submitted human intent.
pub(crate) fn human_prompt_style() -> Style {
    human_prompt_style_for(default_bg(), stdout_color_level())
}

/// Returns the operational blue used by paths, commands, references, and active controls.
pub(crate) fn operational_accent_style() -> Style {
    accent_style()
}

pub(crate) fn operational_reference_style() -> Style {
    operational_accent_style().remove_modifier(ratatui::style::Modifier::BOLD)
}

/// Returns the style for a user-authored message using the provided terminal background.
pub fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(user_message_bg(bg)),
        None => Style::default(),
    }
}

/// Returns the shared accent style for the provided terminal background.
pub(crate) fn accent_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    operational_accent_style_for(terminal_bg, stdout_color_level())
}

fn human_prompt_style_for(
    terminal_bg: Option<(u8, u8, u8)>,
    color_level: StdoutColorLevel,
) -> Style {
    let target = if terminal_bg.is_some_and(is_light) {
        HUMAN_PROMPT_LIGHT_RGB
    } else {
        HUMAN_PROMPT_DARK_RGB
    };
    let color = match color_level {
        StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown => Color::LightCyan,
        StdoutColorLevel::TrueColor | StdoutColorLevel::Ansi256 => {
            best_color_for_level(target, color_level)
        }
    };
    Style::default().fg(color).bold()
}

fn operational_accent_style_for(
    terminal_bg: Option<(u8, u8, u8)>,
    color_level: StdoutColorLevel,
) -> Style {
    let target = if terminal_bg.is_some_and(is_light) {
        OPERATIONAL_BLUE_LIGHT_RGB
    } else {
        OPERATIONAL_BLUE_DARK_RGB
    };
    let color = match color_level {
        StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown => Color::LightBlue,
        StdoutColorLevel::TrueColor | StdoutColorLevel::Ansi256 => {
            best_color_for_level(target, color_level)
        }
    };
    Style::default().fg(color).bold()
}

fn table_separator_style_for(
    terminal_fg: Option<(u8, u8, u8)>,
    terminal_bg: Option<(u8, u8, u8)>,
    color_level: StdoutColorLevel,
) -> Style {
    let (Some(fg), Some(bg)) = (terminal_fg, terminal_bg) else {
        return Style::default().dim();
    };
    let separator_rgb = blend(fg, bg, TABLE_SEPARATOR_FG_ALPHA);
    match color_level {
        StdoutColorLevel::TrueColor => Style::default().fg(rgb_color(separator_rgb)),
        StdoutColorLevel::Ansi256 => Style::default().fg(best_color(separator_rgb)),
        StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown => Style::default().dim(),
    }
}

#[allow(clippy::disallowed_methods)]
pub fn user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    best_color(user_message_bg_rgb(terminal_bg))
}

pub(crate) fn user_message_bg_rgb(terminal_bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), 0.04)
    } else {
        ((255, 255, 255), 0.12)
    };
    blend(top, terminal_bg, alpha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::style::Modifier;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;

    #[test]
    fn human_prompt_teal_and_operational_blue_are_distinct_at_every_color_level() {
        for level in [
            StdoutColorLevel::TrueColor,
            StdoutColorLevel::Ansi256,
            StdoutColorLevel::Ansi16,
            StdoutColorLevel::Unknown,
        ] {
            let human = human_prompt_style_for(Some((0, 0, 0)), level);
            let operational = operational_accent_style_for(Some((0, 0, 0)), level);

            assert_ne!(human.fg, operational.fg, "palette collision at {level:?}");
            assert!(human.add_modifier.contains(Modifier::BOLD));
            assert!(operational.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn truecolor_roles_use_fixed_mini_palette_values() {
        let human = human_prompt_style_for(Some((0, 0, 0)), StdoutColorLevel::TrueColor);
        let operational =
            operational_accent_style_for(Some((0, 0, 0)), StdoutColorLevel::TrueColor);

        assert_eq!(human.fg, Some(rgb_color(HUMAN_PROMPT_DARK_RGB)));
        assert_eq!(operational.fg, Some(rgb_color(OPERATIONAL_BLUE_DARK_RGB)));
    }

    #[test]
    fn human_prompt_style_is_owned_only_by_submitted_prompt_rendering() {
        let lib_rs = codex_utils_cargo_bin::find_resource!("src/lib.rs")
            .expect("failed to locate TUI source");
        let src_dir = lib_rs.parent().expect("lib.rs should have a parent");
        let mut source_files = Vec::new();
        collect_rust_files(src_dir, &mut source_files).expect("failed to collect TUI source files");

        let mut call_sites = Vec::new();
        for path in source_files {
            let relative = path
                .strip_prefix(src_dir)
                .expect("source file should be under src")
                .to_string_lossy()
                .replace('\\', "/");
            let contents = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {relative}: {error}"));
            for (index, line) in contents.lines().enumerate() {
                if line.contains("human_prompt_style()") {
                    call_sites.push(format!("{relative}:{}", index + 1));
                }
            }
        }

        assert!(
            call_sites.iter().all(|site| site.starts_with("style.rs:")
                || site.starts_with("history_cell/messages.rs:")),
            "exclusive human-prompt style escaped its owner: {call_sites:?}"
        );
        assert!(
            call_sites
                .iter()
                .any(|site| site.starts_with("history_cell/messages.rs:")),
            "submitted prompt renderer must own the human-prompt style"
        );
    }

    fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                collect_rust_files(&path, files)?;
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
        Ok(())
    }

    #[test]
    fn table_separator_blends_toward_dark_background() {
        let style = table_separator_style_for(
            Some((255, 255, 255)),
            Some((0, 0, 0)),
            StdoutColorLevel::TrueColor,
        );

        assert_eq!(style.fg, Some(rgb_color((51, 51, 51))));
    }

    #[test]
    fn table_separator_blends_toward_light_background() {
        let style = table_separator_style_for(
            Some((0, 0, 0)),
            Some((255, 255, 255)),
            StdoutColorLevel::TrueColor,
        );

        assert_eq!(style.fg, Some(rgb_color((204, 204, 204))));
    }

    #[test]
    fn table_separator_dims_when_palette_aware_color_is_unavailable() {
        let expected = Style::default().dim();

        assert_eq!(
            table_separator_style_for(
                Some((255, 255, 255)),
                Some((0, 0, 0)),
                StdoutColorLevel::Ansi16,
            ),
            expected
        );
        assert_eq!(
            table_separator_style_for(
                /*terminal_fg*/ None,
                Some((0, 0, 0)),
                StdoutColorLevel::TrueColor,
            ),
            expected
        );
    }
}
