//! Transcript palette — pi's dark theme behind role indirection.
//!
//! The renderer never names a color; it asks for a [`Fg`] or [`Bg`] role and
//! gets a precomputed [`Style`]. Every color below is pi's `dark.json` OKHSL
//! token resolved to sRGB through pi's own color math (`pi-tui/dist/oklab.js`,
//! `okhslToRgb`) at authoring time, so hi and pi render identical tints.
//!
//! One built-in palette for now: a light palette, a `theme` setting, and
//! terminal-query `system` support are deliberate follow-ups — mirroring pi,
//! where `system` is a separate theme that queries the terminal.

use ratatui::style::{Color, Style};

/// Foreground roles used across the transcript and markdown renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fg {
    /// Secondary text: tool output, link URLs.
    Muted,
    /// De-emphasized text: system lines.
    Dim,
    /// Turn errors.
    Error,
    /// Text inside the user block.
    UserText,
    /// Tool block title (the tool name).
    ToolTitle,
    /// Tool output body text.
    ToolOutput,
    /// The selection accent: menu markers and the selected row
    /// (pi's selectList `accent` role; the palette's lavender).
    Accent,
    /// Markdown roles (pi's `md*` family).
    Heading,
    Link,
    LinkUrl,
    Code,
    CodeBlock,
    CodeBlockBorder,
    Quote,
    QuoteBorder,
    Hr,
    ListBullet,
}

/// Background roles — the block fills that identify a transcript surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bg {
    /// User message block.
    UserMessage,
    /// Tool execution, still running.
    ToolPending,
    /// Tool execution, finished successfully.
    ToolSuccess,
    /// Tool execution, failed or refused.
    ToolError,
}

/// The dark palette (pi parity). A unit struct so further palettes slot in
/// behind the same API without touching call sites.
#[derive(Clone, Copy, Default)]
pub struct Theme;

/// The theme every renderer uses today.
pub const THEME: Theme = Theme;

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Theme {
    /// Foreground style for a role.
    pub fn fg(&self, role: Fg) -> Style {
        let color = match role {
            Fg::Muted => rgb(0x9da5a9),
            Fg::Dim => rgb(0x7e888e),
            Fg::Error => rgb(0xea7f81),
            Fg::UserText => rgb(0xdee0e1),
            Fg::ToolTitle => rgb(0xdee0e1),
            Fg::Accent => rgb(0xa798d7),
            Fg::Heading => rgb(0xcd9a22),
            Fg::Link => rgb(0x69add0),
            Fg::LinkUrl | Fg::Quote | Fg::QuoteBorder | Fg::Hr | Fg::CodeBlockBorder => {
                rgb(0x9da5a9)
            }
            Fg::Code | Fg::ListBullet => rgb(0xa798d7),
            Fg::CodeBlock => rgb(0x68b78d),
            Fg::ToolOutput => rgb(0x9da5a9),
        };
        Style::new().fg(color)
    }

    /// Background style for a block fill.
    pub fn bg(&self, role: Bg) -> Style {
        let color = match role {
            Bg::UserMessage => rgb(0x213b49),
            Bg::ToolPending => rgb(0x34383a),
            Bg::ToolSuccess => rgb(0x254131),
            Bg::ToolError => rgb(0x5b282a),
        };
        Style::new().bg(color)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fg_roles_resolve_to_pi_dark_values() {
        assert_eq!(THEME.fg(Fg::Accent).fg, Some(rgb(0xa798d7)));
        assert_eq!(THEME.fg(Fg::Error).fg, Some(rgb(0xea7f81)));
        assert_eq!(THEME.fg(Fg::Heading).fg, Some(rgb(0xcd9a22)));
        assert_eq!(THEME.fg(Fg::Link).fg, Some(rgb(0x69add0)));
        assert_eq!(THEME.fg(Fg::CodeBlock).fg, Some(rgb(0x68b78d)));
    }

    #[test]
    fn bg_roles_resolve_to_pi_dark_values() {
        assert_eq!(THEME.bg(Bg::UserMessage).bg, Some(rgb(0x213b49)));
        assert_eq!(THEME.bg(Bg::ToolPending).bg, Some(rgb(0x34383a)));
        assert_eq!(THEME.bg(Bg::ToolSuccess).bg, Some(rgb(0x254131)));
        assert_eq!(THEME.bg(Bg::ToolError).bg, Some(rgb(0x5b282a)));
    }

    #[test]
    fn shared_muted_value_covers_five_roles() {
        // mdLinkUrl, mdQuote, mdQuoteBorder, mdHr, mdCodeBlockBorder all map
        // to the `muted` token in pi's dark.json.
        for role in [
            Fg::LinkUrl,
            Fg::Quote,
            Fg::QuoteBorder,
            Fg::Hr,
            Fg::CodeBlockBorder,
        ] {
            assert_eq!(THEME.fg(role).fg, Some(rgb(0x9da5a9)));
        }
    }
}
