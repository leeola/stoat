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

use crate::grid::{
    Bar, BorderStyle, MinimapStrip, Overlay, Panel, PanelShadow, Polyline, PoolRegion, Rgb, Rgba,
    TextRun,
};
use stoatty_protocol::command::{
    self, BarCommand, MinimapCommand, PanelCommand, PolylineCommand, PoolRegionCommand,
    PopoverCommand, TextRunCommand,
};

/// Convert a page-local [`TextRunCommand`] to its grid [`TextRun`] at capture
/// time, so the pool projection re-stamps it without re-decoding per frame.
///
/// The declared row passes through unresolved because a pool page carries no
/// line layout, so its logical-to-physical row resolution is the identity. The
/// run is the base layer of a pool composite, so it takes `seq` 0.
pub(super) fn text_run_from_command(command: TextRunCommand) -> TextRun {
    TextRun {
        col: command.col,
        row: command.row,
        scale: command.scale,
        color: Rgb::new(command.color[0], command.color[1], command.color[2]),
        bg: command.bg.map(|bg| Rgb::new(bg[0], bg[1], bg[2])),
        text: command.text.into(),
        seq: 0,
    }
}

/// Convert a page-local [`BarCommand`] to its grid [`Bar`]. See
/// [`text_run_from_command`] for the identity-row and `seq` 0 rationale.
pub(super) fn bar_from_command(command: BarCommand) -> Bar {
    Bar {
        x: command.x,
        y: command.y,
        width: command.width,
        height: command.height,
        color: Rgb::new(command.color[0], command.color[1], command.color[2]),
        seq: 0,
    }
}

/// Convert a page-local [`PolylineCommand`] to its grid [`Polyline`]. See
/// [`text_run_from_command`] for the identity-row and `seq` 0 rationale.
pub(super) fn polyline_from_command(command: PolylineCommand) -> Polyline {
    Polyline {
        points: command.points,
        width: command.width,
        color: Rgb::new(command.color[0], command.color[1], command.color[2]),
        seq: 0,
    }
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
