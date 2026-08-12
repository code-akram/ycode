mod bel;
mod osc9;

use std::io;

use bel::BelBackend;
use codex_terminal_detection::TerminalInfo;
use codex_terminal_detection::TerminalName;
use codex_terminal_detection::terminal_info;
use osc9::Osc9Backend;

#[derive(Debug)]
pub enum DesktopNotificationBackend {
    Osc9(Osc9Backend),
    Bel(BelBackend),
}

impl DesktopNotificationBackend {
    fn detect() -> Self {
        if supports_osc9(&terminal_info()) {
            Self::Osc9(Osc9Backend::new())
        } else {
            Self::Bel(BelBackend)
        }
    }

    pub fn method(&self) -> &'static str {
        match self {
            DesktopNotificationBackend::Osc9(_) => "osc9",
            DesktopNotificationBackend::Bel(_) => "bel",
        }
    }

    pub fn notify(&mut self, message: &str) -> io::Result<()> {
        match self {
            DesktopNotificationBackend::Osc9(backend) => backend.notify(message),
            DesktopNotificationBackend::Bel(backend) => backend.notify(message),
        }
    }
}

pub fn detect_backend() -> DesktopNotificationBackend {
    DesktopNotificationBackend::detect()
}

fn supports_osc9(terminal: &TerminalInfo) -> bool {
    matches!(
        terminal.name,
        TerminalName::Ghostty
            | TerminalName::Iterm2
            | TerminalName::Kitty
            | TerminalName::WarpTerminal
            | TerminalName::WezTerm
    )
}

#[cfg(test)]
mod tests {
    use super::supports_osc9;
    use codex_terminal_detection::TerminalInfo;
    use codex_terminal_detection::TerminalName;
    use pretty_assertions::assert_eq;

    fn test_terminal(name: TerminalName) -> TerminalInfo {
        TerminalInfo {
            name,
            term_program: None,
            version: None,
            term: None,
            multiplexer: None,
        }
    }

    #[test]
    fn supports_osc9_for_supported_terminals() {
        for name in [
            TerminalName::Ghostty,
            TerminalName::Iterm2,
            TerminalName::Kitty,
            TerminalName::WarpTerminal,
            TerminalName::WezTerm,
        ] {
            assert!(
                supports_osc9(&test_terminal(name)),
                "{name:?} should support OSC 9"
            );
        }
    }

    #[test]
    fn supports_osc9_for_unsupported_terminals() {
        for name in [
            TerminalName::AppleTerminal,
            TerminalName::Alacritty,
            TerminalName::Dumb,
            TerminalName::GnomeTerminal,
            TerminalName::Konsole,
            TerminalName::Unknown,
            TerminalName::VsCode,
            TerminalName::Vte,
            TerminalName::WindowsTerminal,
        ] {
            assert_eq!(
                supports_osc9(&test_terminal(name)),
                false,
                "{name:?} should not support OSC 9"
            );
        }
    }
}
