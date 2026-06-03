//! Terminal notifications emitted through OSC 9 when available, with a
//! BEL fallback for terminals that do not support desktop notifications.

use std::fmt;
use std::io::{self, IsTerminal, Write};

use crossterm::Command;
use crossterm::execute;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalNotificationBackend {
    Osc9 { tmux_passthrough: bool },
    Bel,
}

impl TerminalNotificationBackend {
    pub fn detect() -> Option<Self> {
        if !io::stdout().is_terminal() {
            return None;
        }

        let env = TerminalEnvironment::detect();
        Some(if supports_osc9(&env) {
            Self::Osc9 {
                tmux_passthrough: env.tmux_passthrough,
            }
        } else {
            Self::Bel
        })
    }

    pub fn notify<W: Write>(&self, writer: &mut W, message: &str) -> io::Result<()> {
        let message = sanitize_message(message);
        if message.is_empty() {
            return Ok(());
        }

        match self {
            Self::Osc9 { tmux_passthrough } => execute!(
                writer,
                PostOsc9Notification {
                    message,
                    tmux_passthrough: *tmux_passthrough,
                }
            ),
            Self::Bel => execute!(writer, PostBelNotification),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalEnvironment {
    term_program: Option<String>,
    term: Option<String>,
    ghostty_resources_dir: bool,
    iterm_session_id: bool,
    kitty_window_id: bool,
    warp_session_id: bool,
    wezterm_pane: bool,
    tmux_passthrough: bool,
}

impl TerminalEnvironment {
    fn detect() -> Self {
        Self {
            term_program: std::env::var("TERM_PROGRAM").ok(),
            term: std::env::var("TERM").ok(),
            ghostty_resources_dir: std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some(),
            iterm_session_id: std::env::var_os("ITERM_SESSION_ID").is_some(),
            kitty_window_id: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            warp_session_id: std::env::var_os("WARP_SESSION_ID").is_some(),
            wezterm_pane: std::env::var_os("WEZTERM_PANE").is_some(),
            tmux_passthrough: std::env::var_os("TMUX").is_some(),
        }
    }
}

fn supports_osc9(env: &TerminalEnvironment) -> bool {
    env.ghostty_resources_dir
        || env.iterm_session_id
        || env.kitty_window_id
        || env.warp_session_id
        || env.wezterm_pane
        || env
            .term_program
            .as_deref()
            .is_some_and(is_supported_term_program)
        || env
            .term
            .as_deref()
            .is_some_and(|term| term == "xterm-kitty")
}

fn is_supported_term_program(term_program: &str) -> bool {
    term_program.eq_ignore_ascii_case("ghostty")
        || term_program.eq_ignore_ascii_case("iTerm.app")
        || term_program.eq_ignore_ascii_case("kitty")
        || term_program.eq_ignore_ascii_case("WarpTerminal")
        || term_program.eq_ignore_ascii_case("WezTerm")
}

fn sanitize_message(message: &str) -> String {
    let normalized = message
        .chars()
        .map(|ch| match ch {
            '\n' | '\r' | '\t' => ' ',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect::<String>();

    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PostOsc9Notification {
    message: String,
    tmux_passthrough: bool,
}

impl Command for PostOsc9Notification {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        if self.tmux_passthrough {
            let escaped = self.message.replace('\u{1b}', "\u{1b}\u{1b}");
            write!(f, "\x1bPtmux;\x1b\x1b]9;{escaped}\x07\x1b\\")
        } else {
            write!(f, "\x1b]9;{}\x07", self.message)
        }
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "OSC 9 notifications must be emitted through ANSI",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PostBelNotification;

impl Command for PostBelNotification {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x07")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "BEL notifications must be emitted through ANSI",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with_term_program(term_program: &str) -> TerminalEnvironment {
        TerminalEnvironment {
            term_program: Some(term_program.to_string()),
            term: None,
            ghostty_resources_dir: false,
            iterm_session_id: false,
            kitty_window_id: false,
            warp_session_id: false,
            wezterm_pane: false,
            tmux_passthrough: false,
        }
    }

    #[test]
    fn supports_osc9_for_known_terminals() {
        for term_program in ["ghostty", "iTerm.app", "kitty", "WarpTerminal", "WezTerm"] {
            assert!(supports_osc9(&env_with_term_program(term_program)));
        }
    }

    #[test]
    fn supports_osc9_for_known_terminal_env_vars() {
        let env = TerminalEnvironment {
            term_program: None,
            term: None,
            ghostty_resources_dir: false,
            iterm_session_id: false,
            kitty_window_id: false,
            warp_session_id: false,
            wezterm_pane: true,
            tmux_passthrough: false,
        };

        assert!(supports_osc9(&env));
    }

    #[test]
    fn rejects_osc9_for_unknown_terminal() {
        let env = TerminalEnvironment {
            term_program: Some("Apple_Terminal".to_string()),
            term: Some("xterm-256color".to_string()),
            ghostty_resources_dir: false,
            iterm_session_id: false,
            kitty_window_id: false,
            warp_session_id: false,
            wezterm_pane: false,
            tmux_passthrough: false,
        };

        assert!(!supports_osc9(&env));
    }

    #[test]
    fn sanitize_message_flattens_control_characters() {
        assert_eq!(
            sanitize_message("  hello\nworld\t\u{7}done  "),
            "hello world done"
        );
    }

    #[test]
    fn osc9_command_writes_plain_sequence() {
        let mut ansi = String::new();
        let command = PostOsc9Notification {
            message: "done".to_string(),
            tmux_passthrough: false,
        };

        command.write_ansi(&mut ansi).expect("format");

        assert_eq!(ansi, "\u{1b}]9;done\u{7}");
    }

    #[test]
    fn osc9_command_wraps_tmux_passthrough() {
        let mut ansi = String::new();
        let command = PostOsc9Notification {
            message: "done".to_string(),
            tmux_passthrough: true,
        };

        command.write_ansi(&mut ansi).expect("format");

        assert_eq!(ansi, "\u{1b}Ptmux;\u{1b}\u{1b}]9;done\u{7}\u{1b}\\");
    }

    #[test]
    fn bel_command_writes_bell_character() {
        let mut ansi = String::new();
        PostBelNotification.write_ansi(&mut ansi).expect("format");
        assert_eq!(ansi, "\u{7}");
    }
}
