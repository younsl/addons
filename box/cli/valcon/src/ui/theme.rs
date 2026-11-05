//! Monochrome palette: black, grays, and white from the xterm 256-color grayscale ramp.
//! Every style in the UI comes from here, so the look changes in one place.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding};

use crate::state::Level;
use crate::types::Role;

pub const BG: Color = Color::Indexed(16);
pub const SURFACE: Color = Color::Indexed(234);
pub const BORDER: Color = Color::Indexed(238);
pub const SUBTLE: Color = Color::Indexed(241);
pub const MUTED: Color = Color::Indexed(246);
pub const TEXT: Color = Color::Indexed(252);
pub const FG: Color = Color::Indexed(255);

pub const fn base() -> Style {
    Style::new().fg(TEXT).bg(BG)
}

pub const fn strong() -> Style {
    Style::new().fg(FG).add_modifier(Modifier::BOLD)
}

pub const fn text() -> Style {
    Style::new().fg(TEXT)
}

pub const fn muted() -> Style {
    Style::new().fg(MUTED)
}

pub const fn subtle() -> Style {
    Style::new().fg(SUBTLE)
}

pub const fn selected_row() -> Style {
    Style::new().bg(SURFACE).fg(FG)
}

pub const fn header_row() -> Style {
    Style::new().fg(SUBTLE)
}

pub fn panel(title: &str) -> Block<'static> {
    panel_line(Line::from(Span::styled(format!(" {title} "), muted())))
}

pub fn panel_line(title: Line<'_>) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(title)
        .padding(Padding::horizontal(1))
}

pub const fn role(r: Role) -> Style {
    match r {
        Role::Primary => strong(),
        Role::Replica => muted(),
        Role::Unknown => subtle(),
    }
}

pub const fn level(l: Level) -> Style {
    match l {
        Level::Warn => strong(),
        Level::Change => text(),
        Level::Info => muted(),
    }
}

/// Zero recedes so non-zero values stand out.
pub const fn count(n: u32) -> Style {
    if n == 0 {
        subtle()
    } else {
        Style::new().fg(FG)
    }
}

/// Group status: anything but `available` is worth noticing.
pub fn status(s: &str) -> Style {
    if s == "available" { muted() } else { strong() }
}

#[cfg(test)]
pub const fn is_monochrome(c: Color) -> bool {
    match c {
        Color::Reset => true,
        Color::Indexed(n) => n == 16 || n >= 232,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_is_monochrome() {
        for c in [BG, SURFACE, BORDER, SUBTLE, MUTED, TEXT, FG] {
            assert!(is_monochrome(c), "{c:?}");
        }
        assert!(!is_monochrome(Color::Red));
        assert!(!is_monochrome(Color::Indexed(196)));
    }

    #[test]
    fn styles_cover_every_variant() {
        for r in [Role::Primary, Role::Replica, Role::Unknown] {
            assert!(role(r).fg.is_some_and(is_monochrome));
        }
        for l in [Level::Info, Level::Change, Level::Warn] {
            assert!(level(l).fg.is_some_and(is_monochrome));
        }
        assert_ne!(count(0), count(1));
        assert_ne!(status("available"), status("modifying"));
    }
}
