use ratatui::style::Color;

use crate::theme::TerminalThemeKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTheme {
    pub kind: TerminalThemeKind,
    pub text: Color,
    pub muted: Color,
    pub subtle: Color,
    pub header: Color,
    pub primary: Color,
    pub secondary: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub selection_fg: Color,
    pub selection_bg: Color,
    pub user: Color,
    pub agent: Color,
    pub thought: Color,
    pub tool: Color,
    pub code: Color,
    pub terminal: Color,
    pub quote: Color,
    pub diff_added: Color,
    pub diff_removed: Color,
    pub diff_context: Color,
    pub permission: Color,
}

impl TerminalThemeKind {
    pub fn palette(self) -> TerminalTheme {
        match self {
            Self::Light => TerminalTheme {
                kind: self,
                text: Color::Black,
                muted: Color::Rgb(88, 96, 105),
                subtle: Color::Rgb(106, 115, 125),
                header: Color::Black,
                primary: Color::Rgb(0, 92, 197),
                secondary: Color::Rgb(111, 66, 193),
                accent: Color::Rgb(3, 102, 214),
                success: Color::Rgb(34, 134, 58),
                warning: Color::Rgb(154, 103, 0),
                error: Color::Rgb(203, 36, 49),
                selection_fg: Color::White,
                selection_bg: Color::Rgb(3, 102, 214),
                user: Color::Rgb(3, 102, 214),
                agent: Color::Rgb(34, 134, 58),
                thought: Color::Rgb(88, 96, 105),
                tool: Color::Rgb(111, 66, 193),
                code: Color::Rgb(154, 103, 0),
                terminal: Color::Rgb(154, 103, 0),
                quote: Color::Rgb(88, 96, 105),
                diff_added: Color::Rgb(34, 134, 58),
                diff_removed: Color::Rgb(203, 36, 49),
                diff_context: Color::Rgb(88, 96, 105),
                permission: Color::Rgb(154, 103, 0),
            },
            Self::Dark => TerminalTheme {
                kind: self,
                text: Color::White,
                muted: Color::DarkGray,
                subtle: Color::Gray,
                header: Color::White,
                primary: Color::Cyan,
                secondary: Color::LightMagenta,
                accent: Color::LightBlue,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                selection_fg: Color::Black,
                selection_bg: Color::Cyan,
                user: Color::Cyan,
                agent: Color::Green,
                thought: Color::DarkGray,
                tool: Color::Magenta,
                code: Color::Yellow,
                terminal: Color::LightYellow,
                quote: Color::Gray,
                diff_added: Color::Green,
                diff_removed: Color::Red,
                diff_context: Color::DarkGray,
                permission: Color::Yellow,
            },
            Self::AnsiLight => TerminalTheme {
                kind: self,
                text: Color::Black,
                muted: Color::Black,
                subtle: Color::Black,
                header: Color::Black,
                primary: Color::Blue,
                secondary: Color::Magenta,
                accent: Color::Blue,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                selection_fg: Color::White,
                selection_bg: Color::Blue,
                user: Color::Blue,
                agent: Color::Green,
                thought: Color::Black,
                tool: Color::Magenta,
                code: Color::Yellow,
                terminal: Color::Yellow,
                quote: Color::Black,
                diff_added: Color::Green,
                diff_removed: Color::Red,
                diff_context: Color::Black,
                permission: Color::Yellow,
            },
            Self::AnsiDark => TerminalTheme {
                kind: self,
                text: Color::White,
                muted: Color::White,
                subtle: Color::White,
                header: Color::White,
                primary: Color::Cyan,
                secondary: Color::Magenta,
                accent: Color::Blue,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                selection_fg: Color::Black,
                selection_bg: Color::Cyan,
                user: Color::Cyan,
                agent: Color::Green,
                thought: Color::White,
                tool: Color::Magenta,
                code: Color::Yellow,
                terminal: Color::Yellow,
                quote: Color::White,
                diff_added: Color::Green,
                diff_removed: Color::Red,
                diff_context: Color::White,
                permission: Color::Yellow,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_palettes_use_basic_terminal_colors() {
        for kind in [TerminalThemeKind::AnsiLight, TerminalThemeKind::AnsiDark] {
            let palette = kind.palette();
            let colors = [
                palette.primary,
                palette.secondary,
                palette.accent,
                palette.success,
                palette.warning,
                palette.error,
                palette.selection_fg,
                palette.selection_bg,
            ];
            assert!(colors.iter().all(|color| matches!(
                color,
                Color::Black
                    | Color::Red
                    | Color::Green
                    | Color::Yellow
                    | Color::Blue
                    | Color::Magenta
                    | Color::Cyan
                    | Color::White
            )));
        }
    }

    #[test]
    fn light_and_dark_palettes_have_readable_selection_contrast() {
        for kind in [TerminalThemeKind::Light, TerminalThemeKind::Dark] {
            let palette = kind.palette();
            assert_ne!(palette.selection_fg, palette.selection_bg);
            assert_ne!(palette.text, palette.muted);
        }
    }
}
