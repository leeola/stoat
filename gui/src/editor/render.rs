use gpui::{
    div, px, rgb, Div, FontStyle, FontWeight, Hsla, ParentElement, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle,
};
use ratatui::style::Color;
use std::ops::Range;
use stoat::{display_map::HighlightStyle as StoatHighlightStyle, DisplaySnapshot};

const NAMED_COLOR_HEX: [u32; 16] = [
    0x000000, // 0 Black
    0xcd0000, // 1 Red
    0x00cd00, // 2 Green
    0xcdcd00, // 3 Yellow
    0x0000ee, // 4 Blue
    0xcd00cd, // 5 Magenta
    0x00cdcd, // 6 Cyan
    0xe5e5e5, // 7 Gray (aka White-7)
    0x7f7f7f, // 8 DarkGray (aka Bright Black)
    0xff0000, // 9 LightRed
    0x00ff00, // 10 LightGreen
    0xffff00, // 11 LightYellow
    0x5c5cff, // 12 LightBlue
    0xff00ff, // 13 LightMagenta
    0x00ffff, // 14 LightCyan
    0xffffff, // 15 White
];

pub(crate) fn ratatui_color_to_hsla(color: Color) -> Option<Hsla> {
    let hex = match color {
        Color::Reset => return None,
        Color::Black => NAMED_COLOR_HEX[0],
        Color::Red => NAMED_COLOR_HEX[1],
        Color::Green => NAMED_COLOR_HEX[2],
        Color::Yellow => NAMED_COLOR_HEX[3],
        Color::Blue => NAMED_COLOR_HEX[4],
        Color::Magenta => NAMED_COLOR_HEX[5],
        Color::Cyan => NAMED_COLOR_HEX[6],
        Color::Gray => NAMED_COLOR_HEX[7],
        Color::DarkGray => NAMED_COLOR_HEX[8],
        Color::LightRed => NAMED_COLOR_HEX[9],
        Color::LightGreen => NAMED_COLOR_HEX[10],
        Color::LightYellow => NAMED_COLOR_HEX[11],
        Color::LightBlue => NAMED_COLOR_HEX[12],
        Color::LightMagenta => NAMED_COLOR_HEX[13],
        Color::LightCyan => NAMED_COLOR_HEX[14],
        Color::White => NAMED_COLOR_HEX[15],
        Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        Color::Indexed(n) => indexed_color_hex(n),
    };
    Some(rgb(hex).into())
}

fn indexed_color_hex(n: u8) -> u32 {
    if (n as usize) < NAMED_COLOR_HEX.len() {
        return NAMED_COLOR_HEX[n as usize];
    }
    if n >= 232 {
        let level = 8u32 + 10u32 * u32::from(n - 232);
        return (level << 16) | (level << 8) | level;
    }
    let offset = u32::from(n - 16);
    let r = offset / 36;
    let g = (offset / 6) % 6;
    let b = offset % 6;
    let channel = |v: u32| if v == 0 { 0 } else { 55 + 40 * v };
    (channel(r) << 16) | (channel(g) << 8) | channel(b)
}

pub(crate) fn convert_highlight_style(src: &StoatHighlightStyle) -> gpui::HighlightStyle {
    gpui::HighlightStyle {
        color: src.foreground.and_then(ratatui_color_to_hsla),
        background_color: src.background.and_then(ratatui_color_to_hsla),
        font_weight: src.bold.and_then(|b| b.then_some(FontWeight::BOLD)),
        font_style: src.italic.and_then(|b| b.then_some(FontStyle::Italic)),
        underline: src.underline.and_then(|b| {
            b.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: None,
                wavy: false,
            })
        }),
        strikethrough: src.strikethrough.and_then(|b| {
            b.then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            })
        }),
        fade_out: None,
    }
}

pub(crate) struct RenderedRow {
    pub text: SharedString,
    pub runs: Vec<(Range<usize>, gpui::HighlightStyle)>,
}

pub(crate) fn build_rendered_rows(
    snapshot: &DisplaySnapshot,
    range: Range<u32>,
) -> Vec<RenderedRow> {
    let count = range.end.saturating_sub(range.start) as usize;
    let mut texts: Vec<String> = vec![String::new(); count];
    let mut runs: Vec<Vec<(Range<usize>, gpui::HighlightStyle)>> = vec![Vec::new(); count];

    let mut current = 0usize;
    for chunk in snapshot.highlighted_chunks(range.clone()) {
        let style = chunk.highlight_style.as_ref().map(convert_highlight_style);
        let mut remaining: &str = chunk.text.as_ref();
        while !remaining.is_empty() && current < count {
            match remaining.find('\n') {
                Some(nl) => {
                    append_run(
                        &mut texts[current],
                        &mut runs[current],
                        &remaining[..nl],
                        style,
                    );
                    current += 1;
                    remaining = &remaining[nl + 1..];
                },
                None => {
                    append_run(&mut texts[current], &mut runs[current], remaining, style);
                    remaining = "";
                },
            }
        }
    }

    texts
        .into_iter()
        .zip(runs)
        .map(|(text, runs)| RenderedRow {
            text: SharedString::from(text),
            runs,
        })
        .collect()
}

fn append_run(
    text: &mut String,
    runs: &mut Vec<(Range<usize>, gpui::HighlightStyle)>,
    segment: &str,
    style: Option<gpui::HighlightStyle>,
) {
    if segment.is_empty() {
        return;
    }
    let start = text.len();
    text.push_str(segment);
    let end = text.len();
    let Some(style) = style else {
        return;
    };
    if let Some((last_range, last_style)) = runs.last_mut() {
        if *last_style == style && last_range.end == start {
            last_range.end = end;
            return;
        }
    }
    runs.push((start..end, style));
}

pub(crate) fn render_row_element(row: RenderedRow) -> Div {
    let RenderedRow { text, runs } = row;
    div().child(StyledText::new(text).with_highlights(runs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{buffer::Buffer, display_map::DisplayMap};
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    fn hex_of(color: Hsla) -> u32 {
        let rgba: gpui::Rgba = color.into();
        let r = (rgba.r * 255.0).round() as u32;
        let g = (rgba.g * 255.0).round() as u32;
        let b = (rgba.b * 255.0).round() as u32;
        (r << 16) | (g << 8) | b
    }

    #[test]
    fn ratatui_color_to_hsla_named_colors() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Black).map(hex_of),
            Some(0x000000),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::Red).map(hex_of),
            Some(0xcd0000),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::White).map(hex_of),
            Some(0xffffff),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_rgb_passthrough() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Rgb(0x12, 0x34, 0x56)).map(hex_of),
            Some(0x123456),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_indexed_named() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(1)).map(hex_of),
            Some(0xcd0000),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(15)).map(hex_of),
            Some(0xffffff),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_indexed_cube() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(16)).map(hex_of),
            Some(0x000000),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(231)).map(hex_of),
            Some(0xffffff),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_indexed_grayscale() {
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(232)).map(hex_of),
            Some(0x080808),
        );
        assert_eq!(
            ratatui_color_to_hsla(Color::Indexed(255)).map(hex_of),
            Some(0xeeeeee),
        );
    }

    #[test]
    fn ratatui_color_to_hsla_reset_returns_none() {
        assert_eq!(ratatui_color_to_hsla(Color::Reset), None);
    }

    fn stoat_style(
        foreground: Option<Color>,
        background: Option<Color>,
        bold: Option<bool>,
        italic: Option<bool>,
        underline: Option<bool>,
        strikethrough: Option<bool>,
    ) -> StoatHighlightStyle {
        StoatHighlightStyle {
            foreground,
            background,
            bold,
            italic,
            underline,
            strikethrough,
        }
    }

    #[test]
    fn convert_highlight_style_passes_through_colors() {
        let style = stoat_style(Some(Color::Red), Some(Color::Blue), None, None, None, None);
        let converted = convert_highlight_style(&style);
        assert_eq!(converted.color.map(hex_of), Some(0xcd0000));
        assert_eq!(converted.background_color.map(hex_of), Some(0x0000ee));
        assert_eq!(converted.font_weight, None);
        assert_eq!(converted.font_style, None);
    }

    #[test]
    fn convert_highlight_style_maps_bold_to_font_weight() {
        let style = stoat_style(None, None, Some(true), None, None, None);
        assert_eq!(
            convert_highlight_style(&style).font_weight,
            Some(FontWeight::BOLD),
        );

        let unset = stoat_style(None, None, Some(false), None, None, None);
        assert_eq!(convert_highlight_style(&unset).font_weight, None);
    }

    #[test]
    fn convert_highlight_style_maps_italic_to_font_style() {
        let style = stoat_style(None, None, None, Some(true), None, None);
        assert_eq!(
            convert_highlight_style(&style).font_style,
            Some(FontStyle::Italic),
        );

        let unset = stoat_style(None, None, None, Some(false), None, None);
        assert_eq!(convert_highlight_style(&unset).font_style, None);
    }

    #[test]
    fn convert_highlight_style_maps_underline_and_strikethrough() {
        let style = stoat_style(None, None, None, None, Some(true), Some(true));
        let converted = convert_highlight_style(&style);
        assert_eq!(
            converted.underline,
            Some(UnderlineStyle {
                thickness: px(1.0),
                color: None,
                wavy: false,
            }),
        );
        assert_eq!(
            converted.strikethrough,
            Some(StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            }),
        );
    }

    fn test_snapshot(cx: &mut TestAppContext, text: &str) -> DisplaySnapshot {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        display_map.update(cx, |dm, _| dm.snapshot())
    }

    #[test]
    fn build_rendered_rows_single_line() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "hello");

        let rows = build_rendered_rows(&snapshot, 0..1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text.as_ref(), "hello");
        assert!(rows[0].runs.is_empty());
    }

    #[test]
    fn build_rendered_rows_splits_on_newline() {
        let mut cx = TestAppContext::single();
        let snapshot = test_snapshot(&mut cx, "ab\ncd\nef");

        let rows = build_rendered_rows(&snapshot, 0..3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text.as_ref(), "ab");
        assert_eq!(rows[1].text.as_ref(), "cd");
        assert_eq!(rows[2].text.as_ref(), "ef");
    }

    #[test]
    fn build_rendered_rows_groups_styled_runs() {
        let mut runs = Vec::<(Range<usize>, gpui::HighlightStyle)>::new();
        let mut text = String::new();
        let style =
            convert_highlight_style(&stoat_style(Some(Color::Red), None, None, None, None, None));

        append_run(&mut text, &mut runs, "foo", Some(style));
        append_run(&mut text, &mut runs, "bar", Some(style));

        assert_eq!(text, "foobar");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 0..6);
    }
}
