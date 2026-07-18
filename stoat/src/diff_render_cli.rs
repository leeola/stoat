//! ANSI-rendering of [`ReviewHunk`] for the `stoat diff` CLI subcommand.
//!
//! Mirrors the per-row content of the TUI review pane in
//! [`crate::render::review`] but writes byte streams to an `io::Write`
//! instead of painting ratatui cells. Color choices match
//! [`crate::display_map::syntax_theme::DiffTheme::default`] (green
//! adds, red deletes, cyan italic moves, dim context).

use crate::review::{MoveProvenance, ReviewHunk, ReviewRow};
use crossterm::{
    queue,
    style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor},
};
use std::{
    io::{self, Write},
    ops::Range,
};

const NUM_COL_WIDTH: usize = 5;
const SEPARATOR_WIDTH: usize = 1;
const MIN_WIDTH: usize = 20;
const FALLBACK_WIDTH: u16 = 80;

#[derive(Debug, Clone)]
pub struct CliRenderOptions {
    pub layout: CliLayout,
    pub width: u16,
    pub color: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CliLayout {
    SideBySide,
    Unified,
}

/// Render `hunks` for one file (`rel_path`) to `out`. No-op when
/// `hunks` is empty so callers can drive a multi-file loop without
/// special-casing.
pub fn render_diff<W: Write>(
    out: &mut W,
    rel_path: &str,
    hunks: &[ReviewHunk],
    opts: &CliRenderOptions,
) -> io::Result<()> {
    if hunks.is_empty() {
        return Ok(());
    }
    let width = (opts.width as usize).max(MIN_WIDTH);
    write_header(out, rel_path, opts.color)?;
    match opts.layout {
        CliLayout::Unified => render_unified(out, hunks, opts.color, width),
        CliLayout::SideBySide => render_side_by_side(out, hunks, opts.color, width),
    }
}

/// Returns the terminal column count. Falls back to 80 when stdout is
/// not a TTY or the platform reports no size.
pub fn detect_width() -> u16 {
    crossterm::terminal::size()
        .map(|(cols, _)| cols)
        .unwrap_or(FALLBACK_WIDTH)
}

/// Returns whether ANSI color should be emitted, given the explicit
/// `--no-color` flag and the standard CLI conventions: respects the
/// `NO_COLOR` env var (any value disables) and `TERM=dumb`.
pub fn detect_color_enabled(no_color_flag: bool) -> bool {
    let no_color_env_set = std::env::var_os("NO_COLOR").is_some();
    let term = std::env::var("TERM").ok();
    color_enabled(no_color_flag, no_color_env_set, term.as_deref())
}

/// The color decision as pure logic, split from [`detect_color_enabled`]'s env
/// reads so the flag > `NO_COLOR` > `TERM=dumb` precedence is unit-testable.
fn color_enabled(no_color_flag: bool, no_color_env_set: bool, term: Option<&str>) -> bool {
    if no_color_flag {
        return false;
    }
    if no_color_env_set {
        return false;
    }
    if term == Some("dumb") {
        return false;
    }
    true
}

fn write_header<W: Write>(out: &mut W, rel_path: &str, color: bool) -> io::Result<()> {
    let mut p = Painter::new(out, color);
    p.set(CellStyle::Dim)?;
    write!(p.out, "--- {rel_path}")?;
    p.reset()?;
    writeln!(p.out)
}

fn render_unified<W: Write>(
    out: &mut W,
    hunks: &[ReviewHunk],
    color: bool,
    width: usize,
) -> io::Result<()> {
    let max_text = width.saturating_sub(1).max(1);
    for hunk in hunks {
        write_hunk_marker(out, color)?;
        for row in &hunk.rows {
            match row {
                ReviewRow::Context { right, .. } => {
                    write_unified_line(
                        out,
                        color,
                        ' ',
                        LineKind::Context,
                        &right.text,
                        &right.moved_spans,
                        right.move_provenance.as_ref(),
                        max_text,
                    )?;
                },
                ReviewRow::Changed { left, right } => {
                    if let Some(l) = left {
                        write_unified_line(
                            out,
                            color,
                            '-',
                            LineKind::Deleted,
                            &l.text,
                            &l.moved_spans,
                            l.move_provenance.as_ref(),
                            max_text,
                        )?;
                    }
                    if let Some(r) = right {
                        write_unified_line(
                            out,
                            color,
                            '+',
                            LineKind::Added,
                            &r.text,
                            &r.moved_spans,
                            r.move_provenance.as_ref(),
                            max_text,
                        )?;
                    }
                },
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_unified_line<W: Write>(
    out: &mut W,
    color: bool,
    prefix: char,
    kind: LineKind,
    text: &str,
    moved_spans: &[Range<usize>],
    move_provenance: Option<&MoveProvenance>,
    max_cols: usize,
) -> io::Result<()> {
    let base = line_kind_style(kind);
    let mut p = Painter::new(out, color);
    p.set(base)?;
    write!(p.out, "{prefix}")?;
    let mut col = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        if col >= max_cols {
            break;
        }
        let in_moved = moved_spans
            .iter()
            .any(|s| byte_idx >= s.start && byte_idx < s.end);
        let style = if in_moved { CellStyle::Moved } else { base };
        p.set(style)?;
        write!(p.out, "{ch}")?;
        col += 1;
    }
    if let Some(prov) = move_provenance
        && col + 2 < max_cols
    {
        p.set(base)?;
        write!(p.out, "  ")?;
        p.set(CellStyle::Moved)?;
        let chip = format!("<- {}:{}", prov.rel_path, prov.line + 1);
        let available = max_cols - col - 2;
        for ch in chip.chars().take(available) {
            write!(p.out, "{ch}")?;
        }
    }
    p.reset()?;
    writeln!(p.out)
}

fn render_side_by_side<W: Write>(
    out: &mut W,
    hunks: &[ReviewHunk],
    color: bool,
    width: usize,
) -> io::Result<()> {
    let half = width.saturating_sub(SEPARATOR_WIDTH) / 2;
    let left_content = half.saturating_sub(NUM_COL_WIDTH);
    let right_total = width.saturating_sub(half).saturating_sub(SEPARATOR_WIDTH);
    let right_content = right_total.saturating_sub(NUM_COL_WIDTH);

    for hunk in hunks {
        write_hunk_marker(out, color)?;
        for row in &hunk.rows {
            let mut p = Painter::new(out, color);
            match row {
                ReviewRow::Context { left, right } => {
                    write_num_col(&mut p, Some(left.line_num))?;
                    write_side_text(
                        &mut p,
                        &left.text,
                        &[],
                        &[],
                        None,
                        SideKind::Context,
                        left_content,
                    )?;
                    p.reset()?;
                    write_separator(&mut p)?;
                    write_num_col(&mut p, Some(right.line_num))?;
                    write_side_text(
                        &mut p,
                        &right.text,
                        &[],
                        &[],
                        None,
                        SideKind::Context,
                        right_content,
                    )?;
                },
                ReviewRow::Changed { left, right } => {
                    if let Some(l) = left {
                        write_num_col(&mut p, Some(l.line_num))?;
                        write_side_text(
                            &mut p,
                            &l.text,
                            &l.change_spans,
                            &l.moved_spans,
                            l.move_provenance.as_ref(),
                            SideKind::Left,
                            left_content,
                        )?;
                    } else {
                        write_num_col(&mut p, None)?;
                        write_blank_side(&mut p, left_content)?;
                    }
                    p.reset()?;
                    write_separator(&mut p)?;
                    if let Some(r) = right {
                        write_num_col(&mut p, Some(r.line_num))?;
                        write_side_text(
                            &mut p,
                            &r.text,
                            &r.change_spans,
                            &r.moved_spans,
                            r.move_provenance.as_ref(),
                            SideKind::Right,
                            right_content,
                        )?;
                    } else {
                        write_num_col(&mut p, None)?;
                        write_blank_side(&mut p, right_content)?;
                    }
                },
            }
            p.reset()?;
            writeln!(p.out)?;
        }
    }
    Ok(())
}

fn write_hunk_marker<W: Write>(out: &mut W, color: bool) -> io::Result<()> {
    let mut p = Painter::new(out, color);
    p.set(CellStyle::Dim)?;
    write!(p.out, "@@")?;
    p.reset()?;
    writeln!(p.out)
}

fn write_separator<W: Write>(p: &mut Painter<'_, W>) -> io::Result<()> {
    p.set(CellStyle::Dim)?;
    write!(p.out, "│")?;
    p.reset()
}

fn write_num_col<W: Write>(p: &mut Painter<'_, W>, num: Option<u32>) -> io::Result<()> {
    p.set(CellStyle::Dim)?;
    match num {
        Some(n) => write!(p.out, "{n:>4} "),
        None => write!(p.out, "....."),
    }
}

fn write_blank_side<W: Write>(p: &mut Painter<'_, W>, content_w: usize) -> io::Result<()> {
    p.set(CellStyle::Plain)?;
    for _ in 0..content_w {
        write!(p.out, " ")?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_side_text<W: Write>(
    p: &mut Painter<'_, W>,
    text: &str,
    change_spans: &[Range<usize>],
    moved_spans: &[Range<usize>],
    move_provenance: Option<&MoveProvenance>,
    kind: SideKind,
    max_cols: usize,
) -> io::Result<()> {
    let change_color = match kind {
        SideKind::Left => CellStyle::Deleted,
        SideKind::Right => CellStyle::Added,
        SideKind::Context => CellStyle::Plain,
    };
    let mut col = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        if col >= max_cols {
            break;
        }
        let in_moved = moved_spans
            .iter()
            .any(|s| byte_idx >= s.start && byte_idx < s.end);
        let in_change = change_spans
            .iter()
            .any(|s| byte_idx >= s.start && byte_idx < s.end);
        let style = if in_moved {
            CellStyle::Moved
        } else if in_change {
            change_color
        } else {
            CellStyle::Plain
        };
        p.set(style)?;
        write!(p.out, "{ch}")?;
        col += 1;
    }
    if let Some(prov) = move_provenance {
        let chip_start = col + 2;
        if chip_start < max_cols {
            p.set(CellStyle::Plain)?;
            write!(p.out, "  ")?;
            p.set(CellStyle::Moved)?;
            let chip = format!("<- {}:{}", prov.rel_path, prov.line + 1);
            let available = max_cols - chip_start;
            let mut chip_used = 0;
            for ch in chip.chars().take(available) {
                write!(p.out, "{ch}")?;
                chip_used += 1;
            }
            col += 2 + chip_used;
        }
    }
    p.set(CellStyle::Plain)?;
    while col < max_cols {
        write!(p.out, " ")?;
        col += 1;
    }
    Ok(())
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum LineKind {
    Context,
    Added,
    Deleted,
}

fn line_kind_style(kind: LineKind) -> CellStyle {
    match kind {
        LineKind::Context => CellStyle::Dim,
        LineKind::Added => CellStyle::Added,
        LineKind::Deleted => CellStyle::Deleted,
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum SideKind {
    Context,
    Left,
    Right,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CellStyle {
    Plain,
    Dim,
    Added,
    Deleted,
    Moved,
}

struct Painter<'a, W: Write> {
    out: &'a mut W,
    color: bool,
    current: CellStyle,
}

impl<'a, W: Write> Painter<'a, W> {
    fn new(out: &'a mut W, color: bool) -> Self {
        Self {
            out,
            color,
            current: CellStyle::Plain,
        }
    }

    fn set(&mut self, target: CellStyle) -> io::Result<()> {
        if !self.color {
            self.current = target;
            return Ok(());
        }
        if self.current == target {
            return Ok(());
        }
        self.current = target;
        queue!(self.out, ResetColor, SetAttribute(Attribute::Reset))?;
        match target {
            CellStyle::Plain => {},
            CellStyle::Dim => queue!(self.out, SetAttribute(Attribute::Dim))?,
            CellStyle::Added => queue!(self.out, SetForegroundColor(Color::DarkGreen))?,
            CellStyle::Deleted => queue!(self.out, SetForegroundColor(Color::DarkRed))?,
            CellStyle::Moved => queue!(
                self.out,
                SetForegroundColor(Color::DarkCyan),
                SetAttribute(Attribute::Italic),
            )?,
        }
        Ok(())
    }

    fn reset(&mut self) -> io::Result<()> {
        if !self.color {
            self.current = CellStyle::Plain;
            return Ok(());
        }
        if self.current != CellStyle::Plain {
            self.current = CellStyle::Plain;
            queue!(self.out, ResetColor, SetAttribute(Attribute::Reset))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::ReviewSide;

    fn side(text: &str, line_num: u32) -> ReviewSide {
        ReviewSide {
            text: text.to_string(),
            line_num,
            change_spans: vec![],
            moved_spans: vec![],
            move_provenance: None,
        }
    }

    fn opts(layout: CliLayout, color: bool) -> CliRenderOptions {
        CliRenderOptions {
            layout,
            width: 80,
            color,
        }
    }

    #[test]
    fn empty_hunks_produces_empty_output() {
        let mut buf = Vec::new();
        render_diff(&mut buf, "x.rs", &[], &opts(CliLayout::Unified, false)).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn unified_no_color_emits_no_sgr() {
        let hunks = vec![ReviewHunk {
            rows: vec![ReviewRow::Changed {
                left: None,
                right: Some(side("added line", 1)),
            }],
        }];
        let mut buf = Vec::new();
        render_diff(&mut buf, "x.rs", &hunks, &opts(CliLayout::Unified, false)).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(!out.contains('\x1b'), "unexpected SGR: {out:?}");
        assert!(out.contains("--- x.rs"));
        assert!(out.contains("+added line"));
    }

    #[test]
    fn unified_color_emits_sgr_for_added_and_deleted() {
        let hunks = vec![ReviewHunk {
            rows: vec![
                ReviewRow::Changed {
                    left: Some(side("old", 1)),
                    right: None,
                },
                ReviewRow::Changed {
                    left: None,
                    right: Some(side("new", 1)),
                },
            ],
        }];
        let mut buf = Vec::new();
        render_diff(&mut buf, "x.rs", &hunks, &opts(CliLayout::Unified, true)).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let has_red = out.contains("\x1b[31m") || out.contains("\x1b[38;5;1m");
        let has_green = out.contains("\x1b[32m") || out.contains("\x1b[38;5;2m");
        assert!(
            has_red,
            "missing red SGR (basic 31 or indexed 38;5;1): {out:?}"
        );
        assert!(
            has_green,
            "missing green SGR (basic 32 or indexed 38;5;2): {out:?}"
        );
        assert!(out.contains("-old"));
        assert!(out.contains("+new"));
    }

    #[test]
    fn side_by_side_renders_both_sides_and_separator() {
        let hunks = vec![ReviewHunk {
            rows: vec![ReviewRow::Changed {
                left: Some(side("old", 1)),
                right: Some(side("new", 1)),
            }],
        }];
        let mut buf = Vec::new();
        let mut o = opts(CliLayout::SideBySide, false);
        o.width = 40;
        render_diff(&mut buf, "x.rs", &hunks, &o).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("old"));
        assert!(out.contains("new"));
        assert!(out.contains('│'));
    }

    #[test]
    fn detect_color_respects_explicit_no_color_flag() {
        assert!(!detect_color_enabled(true));
    }

    #[test]
    fn color_enabled_precedence() {
        // The flag disables regardless of the environment.
        assert!(!color_enabled(true, false, Some("xterm")));
        assert!(!color_enabled(true, true, Some("dumb")));
        // NO_COLOR set disables.
        assert!(!color_enabled(false, true, Some("xterm")));
        // TERM=dumb disables.
        assert!(!color_enabled(false, false, Some("dumb")));
        // Any other TERM, or none, enables.
        assert!(color_enabled(false, false, Some("xterm")));
        assert!(color_enabled(false, false, None));
    }
}
