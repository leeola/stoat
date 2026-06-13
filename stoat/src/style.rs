//! Native color, modifier, style, and styled-text vocabulary for the shared
//! theme and highlight core.
//!
//! [`Theme`](crate::theme::Theme), the syntax highlight pipeline, and the
//! diff palette describe styling in these terms, and the GUI converts them
//! to its own rendering types at the boundary. The set mirrors the subset
//! of terminal styling the editor actually uses: a 16-color ANSI palette
//! plus 24-bit and 256-indexed colors, the common text modifiers, and the
//! [`Span`] / [`Line`] pair custom block content is rendered as.

use std::{
    borrow::Cow,
    ops::{BitOr, BitOrAssign},
};

/// A foreground or background color.
///
/// [`Reset`](Self::Reset) means "no color" - the renderer falls back to its
/// default rather than painting. [`Indexed`](Self::Indexed) is an ANSI
/// 256-color palette entry; [`Rgb`](Self::Rgb) is a literal 24-bit color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Rgb(u8, u8, u8),
    Indexed(u8),
}

/// A set of text modifiers (bold, italic, ...) held as a bitset.
///
/// The bit assignments are private: only set membership is observable, via
/// [`Self::contains`]. Combine with `|` / `|=`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifier(u16);

impl Modifier {
    pub const BOLD: Modifier = Modifier(0b0000_0000_0001);
    pub const DIM: Modifier = Modifier(0b0000_0000_0010);
    pub const ITALIC: Modifier = Modifier(0b0000_0000_0100);
    pub const UNDERLINED: Modifier = Modifier(0b0000_0000_1000);
    pub const SLOW_BLINK: Modifier = Modifier(0b0000_0001_0000);
    pub const RAPID_BLINK: Modifier = Modifier(0b0000_0010_0000);
    pub const REVERSED: Modifier = Modifier(0b0000_0100_0000);
    pub const HIDDEN: Modifier = Modifier(0b0000_1000_0000);
    pub const CROSSED_OUT: Modifier = Modifier(0b0001_0000_0000);

    pub const fn empty() -> Modifier {
        Modifier(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every modifier in `other` is also set in `self`.
    pub const fn contains(self, other: Modifier) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl BitOr for Modifier {
    type Output = Modifier;

    fn bitor(self, rhs: Modifier) -> Modifier {
        Modifier(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modifier {
    fn bitor_assign(&mut self, rhs: Modifier) {
        self.0 |= rhs.0;
    }
}

/// A foreground color, background color, and text modifiers.
///
/// Absent `fg` / `bg` mean "inherit" - the renderer leaves that channel
/// untouched. Build with the chained [`Self::fg`] / [`Self::bg`] /
/// [`Self::add_modifier`] setters from [`Style::default`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub add_modifier: Modifier,
}

impl Style {
    pub fn fg(mut self, color: Color) -> Style {
        self.fg = Some(color);
        self
    }

    pub fn bg(mut self, color: Color) -> Style {
        self.bg = Some(color);
        self
    }

    /// Add `modifier` to the set already present rather than replacing it.
    pub fn add_modifier(mut self, modifier: Modifier) -> Style {
        self.add_modifier |= modifier;
        self
    }
}

/// A run of text with a single [`Style`].
///
/// Content is owned-or-borrowed for the program's lifetime, so a span built
/// from a string literal stays zero-copy while one built from a computed
/// `String` owns its text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub content: Cow<'static, str>,
    pub style: Style,
}

impl Span {
    /// A span with no styling - the renderer applies its own defaults.
    pub fn raw(content: impl Into<Cow<'static, str>>) -> Span {
        Span {
            content: content.into(),
            style: Style::default(),
        }
    }

    pub fn styled(content: impl Into<Cow<'static, str>>, style: Style) -> Span {
        Span {
            content: content.into(),
            style,
        }
    }
}

/// A single line of styled text, as an ordered list of [`Span`]s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    /// A line of one unstyled [`Span`].
    pub fn raw(content: impl Into<Cow<'static, str>>) -> Line {
        Line {
            spans: vec![Span::raw(content)],
        }
    }
}

impl From<Vec<Span>> for Line {
    fn from(spans: Vec<Span>) -> Line {
        Line { spans }
    }
}

/// Renders the concatenated span content, dropping styling, so a [`Line`]
/// flattens to its plain text via `to_string`.
impl std::fmt::Display for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for span in &self.spans {
            f.write_str(&span.content)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Color, Modifier, Style};

    #[test]
    fn modifier_bits_are_distinct() {
        let all = [
            Modifier::BOLD,
            Modifier::DIM,
            Modifier::ITALIC,
            Modifier::UNDERLINED,
            Modifier::SLOW_BLINK,
            Modifier::RAPID_BLINK,
            Modifier::REVERSED,
            Modifier::HIDDEN,
            Modifier::CROSSED_OUT,
        ];
        let mut combined = Modifier::empty();
        for m in all {
            assert!(!combined.contains(m), "each flag occupies a distinct bit");
            combined |= m;
        }
    }

    #[test]
    fn modifier_contains_tracks_membership() {
        let set = Modifier::BOLD | Modifier::ITALIC;
        assert!(set.contains(Modifier::BOLD));
        assert!(set.contains(Modifier::ITALIC));
        assert!(set.contains(Modifier::BOLD | Modifier::ITALIC));
        assert!(!set.contains(Modifier::UNDERLINED));
        assert!(Modifier::empty().is_empty());
        assert!(!set.is_empty());
    }

    #[test]
    fn style_setters_compose() {
        let style = Style::default()
            .fg(Color::Red)
            .bg(Color::Rgb(0x10, 0x20, 0x30))
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::ITALIC);
        assert_eq!(style.fg, Some(Color::Red));
        assert_eq!(style.bg, Some(Color::Rgb(0x10, 0x20, 0x30)));
        assert!(style
            .add_modifier
            .contains(Modifier::BOLD | Modifier::ITALIC));
    }
}
