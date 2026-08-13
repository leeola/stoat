//! Projections from a decoded protocol command to the grid type it declares.
//!
//! Every conversion in this direction lives here, whichever module consumes the
//! result. Gathering them is what keeps the wire format from spreading: a
//! reader adding a command has one place to write its projection, and a reader
//! asking what a grid type is built from has one place to look.
//!
//! A command enum's map to its grid enum lives here when the conversion that
//! needs it does. The icon-kind map stays in the decoration module, since
//! nothing here calls it.
//!
//! [`StoredTextRun`] lives here for the same reason: it is a decoded command as
//! the terminal holds it, so the conversion into it and the projection out of it
//! are both conversions in this direction.

use crate::grid::{
    Bar, BorderStyle, MinimapStrip, Overlay, Panel, PanelShadow, Polyline, PoolRegion, Rgb, Rgba,
    TextRun,
};
use std::sync::Arc;
use stoatty_protocol::command::{
    self, BarCommand, MinimapCommand, PanelCommand, PolylineCommand, PoolRegionCommand,
    PopoverCommand, TextRunCommand,
};

/// A declared text run as the terminal holds it between projections.
///
/// Mirrors [`TextRunCommand`], the wire shape, but shares its text rather than
/// owning a `String`. Every dirty projection hands the whole run list to the
/// grid, so an owned string would be rebuilt into the grid's shared text once
/// per frame per run, and a gutter declares one run per visible line.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StoredTextRun {
    pub(crate) col: i16,
    pub(crate) row: i16,
    pub(crate) scale: u16,
    pub(crate) color: [u8; 3],
    pub(crate) bg: Option<[u8; 3]>,
    pub(crate) text: Arc<str>,
}

impl From<TextRunCommand> for StoredTextRun {
    fn from(command: TextRunCommand) -> Self {
        StoredTextRun {
            col: command.col,
            row: command.row,
            scale: command.scale,
            color: command.color,
            bg: command.bg,
            text: Arc::from(command.text),
        }
    }
}

/// Project a declared text run into its grid [`TextRun`].
///
/// `row` is the run's declared row already resolved through the target grid's
/// line layout, and `seq` its declaration order among the non-cell components.
/// Both come from the caller because a pool page has neither. A page carries no
/// line layout, so its row resolution is the identity, and its runs are the base
/// layer of the composite, so they take `seq` 0.
pub(crate) fn text_run_from_command(run: &StoredTextRun, row: i16, seq: u32) -> TextRun {
    TextRun {
        col: run.col,
        row,
        scale: run.scale,
        color: Rgb::new(run.color[0], run.color[1], run.color[2]),
        bg: run.bg.map(|bg| Rgb::new(bg[0], bg[1], bg[2])),
        text: Arc::clone(&run.text),
        seq,
    }
}

/// Project a declared [`BarCommand`] into its grid [`Bar`]. See
/// [`text_run_from_command`] for where `y` and `seq` come from.
pub(crate) fn bar_from_command(command: &BarCommand, y: i16, seq: u32) -> Bar {
    Bar {
        x: command.x,
        y,
        width: command.width,
        height: command.height,
        color: Rgb::new(command.color[0], command.color[1], command.color[2]),
        seq,
    }
}

/// Refill `slot` from a declared [`PolylineCommand`].
///
/// Fills a slot instead of returning a path, because both callers rebuild their
/// whole list whenever anything about it changes, and a returning constructor
/// allocates a point vector per path per rebuild. A commit graph declares a path
/// per lane, so that is the allocation this shape exists to avoid.
///
/// Points pass through unresolved, because a path is free geometry rather than a
/// component anchored to a logical row. See [`text_run_from_command`] for where
/// `seq` comes from.
pub(crate) fn fill_polyline(slot: &mut Polyline, command: &PolylineCommand, seq: u32) {
    slot.points.clear();
    slot.points.extend_from_slice(&command.points);
    slot.width = command.width;
    slot.color = Rgb::new(command.color[0], command.color[1], command.color[2]);
    slot.seq = seq;
}

/// Project a declared [`PoolRegionCommand`] into the grid's [`PoolRegion`].
pub(crate) fn pool_region_from_command(command: PoolRegionCommand) -> PoolRegion {
    PoolRegion {
        pool: command.pool,
        window: command.window,
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
    }
}

/// Project a declared [`MinimapCommand`] into the grid's [`MinimapStrip`],
/// resolving its wire color triples.
pub(crate) fn minimap_strip_from_command(command: MinimapCommand) -> MinimapStrip {
    MinimapStrip {
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
        strip_id: command.strip_id,
        content_id: command.content_id,
        lines_per_cell: command.lines_per_cell,
        max_columns: command.max_columns,
        bg: Rgba::new(command.bg[0], command.bg[1], command.bg[2], command.bg[3]),
        thumb: Rgba::new(
            command.thumb[0],
            command.thumb[1],
            command.thumb[2],
            command.thumb[3],
        ),
        thumb_border: Rgb::new(
            command.thumb_border[0],
            command.thumb_border[1],
            command.thumb_border[2],
        ),
        palette: command
            .palette
            .into_iter()
            .map(|entry| Rgb::new(entry[0], entry[1], entry[2]))
            .collect(),
    }
}

pub(crate) fn popover_overlay(command: &PopoverCommand) -> Overlay {
    Overlay {
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
        fill: Rgb::new(command.fill[0], command.fill[1], command.fill[2]),
        border: Rgb::new(command.border[0], command.border[1], command.border[2]),
        content_fg: Rgb::new(
            command.content_fg[0],
            command.content_fg[1],
            command.content_fg[2],
        ),
        scale: command.scale,
        offset: command.offset,
        bold: command.bold,
        content: command.content.clone(),
    }
}

pub(crate) fn panel_grid(command: &PanelCommand, seq: u32) -> Panel {
    Panel {
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
        style: grid_border_style(command.style),
        border: Rgb::new(command.border[0], command.border[1], command.border[2]),
        corner_radius: command.corner_radius,
        fill: command.fill.map(|[r, g, b]| Rgb::new(r, g, b)),
        shadow: grid_panel_shadow(command.shadow),
        inset_x: command.inset_x,
        above_pools: command.above_pools,
        seq,
    }
}

pub(crate) fn grid_border_style(style: command::BorderStyle) -> BorderStyle {
    match style {
        command::BorderStyle::Light => BorderStyle::Light,
        command::BorderStyle::Heavy => BorderStyle::Heavy,
        command::BorderStyle::Double => BorderStyle::Double,
        command::BorderStyle::Rounded => BorderStyle::Rounded,
    }
}

fn grid_panel_shadow(shadow: command::PanelShadow) -> PanelShadow {
    match shadow {
        command::PanelShadow::None_ => PanelShadow::None_,
        command::PanelShadow::Drop => PanelShadow::Drop,
        command::PanelShadow::Tucked => PanelShadow::Tucked,
        command::PanelShadow::Overhang => PanelShadow::Overhang,
    }
}
