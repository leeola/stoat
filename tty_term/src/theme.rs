//! The color set a terminal resolves its cells' colors against.

use crate::grid::Rgb;

/// The palette a [`Terminal`](crate::term::Terminal) resolves cell colors
/// against.
///
/// `foreground` and `background` are the defaults for text and the screen when
/// a cell names no explicit color. `cursor` is the cursor block's color, carried
/// here for the renderer to read. `ansi` holds the 16 standard palette entries
/// (indices 0-7 normal, 8-15 bright) that ANSI named and low-indexed colors
/// resolve to; the 6x6x6 color cube and grayscale ramp at indices 16-255 are
/// derived from fixed xterm formulas and are not part of the theme.
///
/// [`Theme::default`] is the built-in xterm-ish palette, so a terminal given no
/// configured theme renders exactly as it did before themes existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub ansi: [Rgb; 16],
}

impl Default for Theme {
    fn default() -> Theme {
        Theme {
            foreground: Rgb::new(0xcc, 0xcc, 0xcc),
            background: Rgb::new(0x00, 0x00, 0x00),
            cursor: Rgb::new(0xd9, 0xd9, 0xd9),
            ansi: [
                Rgb::new(0x00, 0x00, 0x00),
                Rgb::new(0xcd, 0x00, 0x00),
                Rgb::new(0x00, 0xcd, 0x00),
                Rgb::new(0xcd, 0xcd, 0x00),
                Rgb::new(0x00, 0x00, 0xee),
                Rgb::new(0xcd, 0x00, 0xcd),
                Rgb::new(0x00, 0xcd, 0xcd),
                Rgb::new(0xe5, 0xe5, 0xe5),
                Rgb::new(0x7f, 0x7f, 0x7f),
                Rgb::new(0xff, 0x00, 0x00),
                Rgb::new(0x00, 0xff, 0x00),
                Rgb::new(0xff, 0xff, 0x00),
                Rgb::new(0x5c, 0x5c, 0xff),
                Rgb::new(0xff, 0x00, 0xff),
                Rgb::new(0x00, 0xff, 0xff),
                Rgb::new(0xff, 0xff, 0xff),
            ],
        }
    }
}
