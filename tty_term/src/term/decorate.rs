//! The stoatty decorations stamped onto a projected grid.
//!
//! Borders, panels, popovers, icons, bars, text runs, polylines, scales, and
//! minimaps arrive over the APC protocol rather than the VT stream, so a
//! projection paints them after the cells are in place. Each function here takes
//! the decoded commands and the [`Grid`] to stamp them on, and the damage-aware
//! ones skip a decoration whose rows the projection did not rewrite.

use super::{Damage, StoredTextRun};
use crate::grid::{
    from_command::{grid_border_style, minimap_strip_from_command, panel_grid, popover_overlay},
    Bar, Border, BorderEdge, Grid, Icon, IconKind, Minimap, MinimapView, PagePool, Polyline, Rgb,
    ScrollRegion, TextRun,
};
use std::{collections::HashMap, mem, sync::Arc};
use stoatty_protocol::command::{
    self, BarCommand, BorderCommand, IconCommand, LineLayoutCommand, MinimapCommand, PanelCommand,
    PolylineCommand, PopoverCommand, ScaleCommand, ScrollRegionCommand,
};

/// Mark the grid rows the damage-tracked decorations occupy, clamped to `rows`.
///
/// Each border and panel region spans `top..=top+height-1` and each scale block
/// spans `top..top+scale`. Borders and scales stamp those rows' cells, so the
/// span mirrors what [`frame_region`] and [`apply_scales`] touch. A panel does
/// not stamp cells, but its chrome covers the same rows, so it is tracked here
/// too.
pub(super) fn decoration_footprint(
    borders: &[BorderCommand],
    panels: &[PanelCommand],
    scales: &[ScaleCommand],
    rows: usize,
    out: &mut Vec<bool>,
) {
    out.clear();
    out.resize(rows, false);

    for border in borders {
        if border.width == 0 || border.height == 0 {
            continue;
        }
        let top = border.top as usize;
        if top >= rows {
            continue;
        }
        let bottom = (top + border.height as usize - 1).min(rows - 1);
        out[top..=bottom].fill(true);
    }

    for panel in panels {
        if panel.width == 0 || panel.height == 0 {
            continue;
        }
        let top = panel.top as usize;
        if top >= rows {
            continue;
        }
        let bottom = (top + panel.height as usize - 1).min(rows - 1);
        out[top..=bottom].fill(true);
    }

    for scale in scales {
        let top = scale.top as usize;
        let end = (top + scale.scale as usize).min(rows);
        if top < end {
            out[top..end].fill(true);
        }
    }
}

/// Whether any row of the half-open span `top..end` is dirty in `rows_dirty`.
///
/// Clamped to `rows`, so a region declared past the screen tests only the part
/// that lands on it. Wire coordinates are untrusted and may point anywhere.
fn span_is_dirty(top: usize, end: usize, rows: usize, rows_dirty: &Damage) -> bool {
    (top..end.min(rows)).any(|row| rows_dirty.is_dirty(row))
}

/// Stamp every stored border region's perimeter edges onto `grid`, over the rows
/// `rows_dirty` marks.
///
/// The cell projection resets borders to none, so a stamped row has to be restamped
/// after it. Only the projected rows lost theirs, which is what `rows_dirty` names.
/// Pass [`Damage::Full`] to stamp regardless, for a resize that cleared the cells or
/// a command list that no longer matches the grid.
///
/// Edges outside the grid are skipped, so a region may extend past it.
pub(super) fn apply_borders(grid: &mut Grid, commands: &[BorderCommand], rows_dirty: &Damage) {
    let rows = grid.rows();
    for command in commands {
        let top = command.top as usize;
        let end = top + command.height as usize;
        if span_is_dirty(top, end, rows, rows_dirty) {
            frame_region(grid, command, rows_dirty);
        }
    }
}

fn frame_region(grid: &mut Grid, command: &BorderCommand, rows_dirty: &Damage) {
    if command.width == 0 || command.height == 0 {
        return;
    }

    let border = Border {
        style: grid_border_style(command.style),
        color: Rgb::new(command.color[0], command.color[1], command.color[2]),
    };

    let rows = grid.rows();
    let cols = grid.cols();
    let top = command.top as usize;
    let left = command.left as usize;
    let bottom = top + command.height as usize - 1;
    let right = left + command.width as usize - 1;
    let last_col = right.min(cols.saturating_sub(1));

    // A horizontal edge is one contiguous run, so it takes one call for the row
    // rather than one per column.
    if left < cols {
        if top < rows && rows_dirty.is_dirty(top) {
            grid.set_border_edge(top, left..last_col + 1, BorderEdge::Top, border);
        }
        if bottom < rows && rows_dirty.is_dirty(bottom) {
            grid.set_border_edge(bottom, left..last_col + 1, BorderEdge::Bottom, border);
        }
    }

    for row in top..=bottom.min(rows.saturating_sub(1)) {
        if !rows_dirty.is_dirty(row) {
            continue;
        }
        grid.set_border_edge(row, left..left + 1, BorderEdge::Left, border);
        grid.set_border_edge(row, right..right + 1, BorderEdge::Right, border);
    }
}

/// Claim each stored scale command's block on `grid`, over the rows `rows_dirty`
/// marks.
///
/// The cell projection resets every cell to [`Scale::Single`], so a claimed row has
/// to be reclaimed after it, and only the projected rows lost theirs. Pass
/// [`Damage::Full`] to claim regardless, as [`apply_borders`] documents.
///
/// An origin outside the grid is skipped, since wire coordinates are untrusted and
/// may point past the screen. A block is claimed whole or not at all, since one call
/// places all of it.
pub(super) fn apply_scales(grid: &mut Grid, commands: &[ScaleCommand], rows_dirty: &Damage) {
    let rows = grid.rows();
    for command in commands {
        let (row, col) = (command.top as usize, command.left as usize);
        if row >= rows || col >= grid.cols() {
            continue;
        }
        // A scale below 2 claims the origin cell alone, anything above it a square.
        let end = row + (command.scale as usize).max(1);
        if span_is_dirty(row, end, rows, rows_dirty) {
            grid.place_scaled(row, col, command.scale);
        }
    }
}

/// Replace the grid's overlay list with each stored popover command's region.
///
/// Overlays are grid-level rather than per-cell, so the full list is set each
/// projection rather than stamped per cell. The region is clamped or clipped by
/// the renderer, so out-of-grid anchors need no guard here.
pub(super) fn apply_popovers(grid: &mut Grid, commands: &[PopoverCommand]) {
    grid.fill_overlays(commands.len(), |index| popover_overlay(&commands[index]));
}

pub(super) fn apply_panels(grid: &mut Grid, commands: &[PanelCommand], seqs: &[u32]) {
    grid.fill_panels(commands.len(), |index| {
        panel_grid(&commands[index], seqs[index])
    });
}

/// Set the grid's scrollable region from the stored command, or clear it.
///
/// Runs each projection like the other command appliers, since the grid's
/// scroll region is set rather than derived from cells. The renderer clamps or
/// clips an out-of-grid rectangle, so wire coordinates need no guard here.
pub(super) fn apply_scroll_region(grid: &mut Grid, command: Option<ScrollRegionCommand>) {
    grid.set_scroll_region(command.map(|command| ScrollRegion {
        top: command.top,
        left: command.left,
        width: command.width,
        height: command.height,
        offset: command.offset,
    }));
}

/// Replace the grid's icon list with each stored icon command's icon.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The renderer clamps an out-of-grid anchor, so wire
/// coordinates need no guard here.
pub(super) fn apply_icons(grid: &mut Grid, commands: &[IconCommand], seqs: &[u32]) {
    grid.fill_icons(commands.len(), |index| {
        let command = &commands[index];
        Icon {
            top: command.top,
            left: command.left,
            kind: grid_icon_kind(command.kind),
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
            size: command.size,
            offset: command.offset,
            seq: seqs[index],
        }
    });
}

fn grid_icon_kind(kind: command::IconKind) -> IconKind {
    match kind {
        command::IconKind::Error => IconKind::Error,
        command::IconKind::Warning => IconKind::Warning,
        command::IconKind::Info => IconKind::Info,
    }
}

/// Apply the stored logical-line layout to the grid, or clear it when none is
/// set, so [`apply_text_runs`] and [`apply_bars`] can resolve against it.
pub(super) fn apply_line_layout(grid: &mut Grid, command: Option<&LineLayoutCommand>) {
    grid.set_line_heights(
        command
            .map(|command| command.heights.clone())
            .unwrap_or_default(),
    );
}

/// Stamp the buffered pages' text runs and bars into the composed `out` grid,
/// translated from page-local rows to the window's rows.
///
/// Each page the window straddles contributes its slot decorations shifted by
/// the whole-row gap between the page's document start and the window top `top`,
/// in sixteenth-cell units. A decoration lying fully above or below the composed
/// rows is dropped. The sub-cell scroll fraction stays with the renderer, so
/// these shift by the same pixel offset as the cells [`PagePool::compose`]
/// copied.
///
/// The page decorations are already in grid form (see
/// [`PagePool::set_decorations`]), so re-stamping is Copy-field arithmetic plus
/// an `Arc` bump per run, with no per-frame decode. They carry `seq` 0 as the
/// base layer of the composite, so any declared panel above occludes them.
pub(super) fn stamp_pool_decorations(pool: &PagePool, out: &mut Grid, top: i64, page_rows: usize) {
    let out_rows = out.rows() as i64;
    let out_rows_16 = out_rows * 16;
    let first_page = top.div_euclid(page_rows as i64);
    let last_page = (top + out_rows - 1).div_euclid(page_rows as i64);

    // Built straight into the target's own lists rather than into fresh ones it
    // would then adopt. A gliding pool stamps every frame, so what is already
    // here is exactly what would otherwise be dropped and reallocated.
    let mut text_runs = mem::take(out.text_runs_mut());
    let mut bars = mem::take(out.bars_mut());
    let mut polylines = mem::take(out.polylines_mut());
    text_runs.clear();
    bars.clear();
    // Paths keep their entries, so each one's point list is refilled rather than
    // collected fresh. `kept` counts the survivors, and the tail is dropped once
    // the pages are walked.
    let mut kept = 0;

    for page in first_page..=last_page {
        let Some((page_runs, page_bars, page_polylines)) = pool.page_decorations(page as u64)
        else {
            continue;
        };
        let shift = 16 * (page * page_rows as i64 - top);

        for run in page_runs {
            let row = run.row as i64 + shift;
            if row + 16 <= 0 || row >= out_rows_16 {
                continue;
            }
            if let Ok(row) = i16::try_from(row) {
                text_runs.push(TextRun { row, ..run.clone() });
            }
        }

        for bar in page_bars {
            let y = bar.y as i64 + shift;
            if y + bar.height as i64 <= 0 || y >= out_rows_16 {
                continue;
            }
            if let Ok(y) = i16::try_from(y) {
                bars.push(Bar { y, ..*bar });
            }
        }

        for polyline in page_polylines {
            // Shifted whole rather than per point. A path that straddles the
            // edge stays intact for the renderer to scissor, since clipping a
            // segment here would need its pixel geometry. A point that cannot
            // survive the shift drops the whole path, because a partial point
            // list would draw a wrong line rather than none.
            if kept == polylines.len() {
                polylines.push(Polyline {
                    points: Vec::new(),
                    width: 0,
                    color: Rgb::new(0, 0, 0),
                    seq: 0,
                });
            }
            let slot = &mut polylines[kept];

            slot.points.clear();
            let shifted = polyline.points.iter().try_for_each(|&[x, y]| {
                match i16::try_from(y as i64 + shift) {
                    Ok(y) => {
                        slot.points.push([x, y]);
                        Ok(())
                    },
                    Err(_) => Err(()),
                }
            });
            if shifted.is_err() {
                continue;
            }
            if slot
                .points
                .iter()
                .all(|&[_, y]| (y as i64) < 0 || y as i64 >= out_rows_16)
            {
                continue;
            }

            slot.width = polyline.width;
            slot.color = polyline.color;
            slot.seq = polyline.seq;
            kept += 1;
        }
    }

    polylines.truncate(kept);
    out.set_text_runs(text_runs);
    out.set_bars(bars);
    out.set_polylines(polylines);
}

/// Replace the grid's text-run list with each stored text-run command's run.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The declared row is a logical row resolved through the
/// line layout, so a run tracks expansions above it. The renderer clamps an
/// out-of-grid anchor, so wire coordinates need no guard here.
pub(super) fn apply_text_runs(grid: &mut Grid, runs: &[StoredTextRun], seqs: &[u32]) {
    grid.fill_text_runs(runs.len(), |grid, index| {
        let run = &runs[index];
        TextRun {
            col: run.col,
            row: resolve_logical_row(grid, run.row),
            scale: run.scale,
            color: Rgb::new(run.color[0], run.color[1], run.color[2]),
            bg: run.bg.map(|b| Rgb::new(b[0], b[1], b[2])),
            text: Arc::clone(&run.text),
            seq: seqs[index],
        }
    });
}

/// Replace the grid's bar list with each stored bar command's rectangle.
///
/// Grid-level like the overlays, so the full list is set each projection rather
/// than stamped per cell. The declared `y` is a logical row resolved through the
/// line layout, so a bar tracks expansions above it.
pub(super) fn apply_bars(grid: &mut Grid, commands: &[BarCommand], seqs: &[u32]) {
    grid.fill_bars(commands.len(), |grid, index| {
        let command = &commands[index];
        Bar {
            x: command.x,
            y: resolve_logical_row(grid, command.y),
            width: command.width,
            height: command.height,
            color: Rgb::new(command.color[0], command.color[1], command.color[2]),
            seq: seqs[index],
        }
    });
}

/// Replace the grid's polyline list with each stored command's path.
///
/// Grid-level like the bars, so the full list is set each projection. Points
/// pass through unresolved, because a path is free geometry rather than a
/// component anchored to a logical row.
///
/// Each path's point list is refilled where it sits rather than cloned from the
/// command, since this re-runs on every line-layout change and a commit graph
/// declares a path per lane.
pub(super) fn apply_polylines(grid: &mut Grid, commands: &[PolylineCommand], seqs: &[u32]) {
    let mut polylines = mem::take(grid.polylines_mut());

    polylines.resize_with(commands.len(), || Polyline {
        points: Vec::new(),
        width: 0,
        color: Rgb::new(0, 0, 0),
        seq: 0,
    });
    for ((slot, command), &seq) in polylines.iter_mut().zip(commands).zip(seqs) {
        slot.points.clear();
        slot.points.extend_from_slice(&command.points);
        slot.width = command.width;
        slot.color = Rgb::new(command.color[0], command.color[1], command.color[2]);
        slot.seq = seq;
    }

    grid.set_polylines(polylines);
}

/// Project the declared minimap strips onto `grid`, each joined with its view.
///
/// The line summaries a strip renders are projected separately (see
/// [`Grid::set_minimap_contents`]), so this small join runs on every strip or
/// view change without re-cloning the content stores.
pub(super) fn apply_minimaps(
    grid: &mut Grid,
    commands: &[MinimapCommand],
    seqs: &[u32],
    views: &HashMap<u32, MinimapView>,
) {
    grid.fill_minimaps(commands.len(), |index| {
        let command = &commands[index];
        Minimap {
            strip: minimap_strip_from_command(command.clone()),
            seq: seqs[index],
            view: views.get(&command.strip_id).copied(),
        }
    });
}

/// Resolve a component's declared logical row, in sixteenth-cell units, to the
/// physical row it sits on by adding the whole-row expansion above its line.
///
/// A negative row is off the top with no logical line, so it passes through.
fn resolve_logical_row(grid: &Grid, row: i16) -> i16 {
    if row < 0 {
        return row;
    }

    let logical_line = (row / 16) as usize;
    let expansion = grid
        .line_start_row(logical_line)
        .saturating_sub(logical_line);
    let shift = i16::try_from(expansion.saturating_mul(16)).unwrap_or(i16::MAX);
    row.saturating_add(shift)
}
