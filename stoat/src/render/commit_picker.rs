use crate::{
    commit_picker::{CommitColumn, CommitPicker, CommitPickerRole, COMMIT_COLUMNS},
    render::{
        file_finder::file_finder_layout,
        review::style_rgb,
        table::{self, Column, Width},
        text::write_str_clipped,
    },
    review_session::ReviewSession,
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Clear, StatefulWidget, Widget},
};
use std::cmp::Ordering;
use stoatty_widgets::{polyline::Polyline, ApcScene};

/// Rows the list keeps however small the body gets, so a short modal still
/// shows enough history to choose from, and dragging the separator all the way
/// up cannot leave the table unreadable.
pub(crate) const MIN_LIST_ROWS: u16 = 5;

/// The on-screen rectangles of the commit picker modal.
///
/// The diff preview sits *below* the list rather than beside it, so each side
/// of a changed line gets the modal's full width instead of half of it.
///
/// Shared by the renderer and the wheel hit-test so a scroll lands on the
/// region it appears over.
pub(crate) struct CommitPickerLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border: input row, separator, header, list, then preview.
    pub(crate) inner: Rect,
    /// The commit table's column labels, above the rows they name. Reserved
    /// here and painted when the rows become a table, so the rects below it
    /// settle once rather than shifting a row later.
    #[allow(dead_code)]
    pub(crate) header: Rect,
    /// The node graph, left of the table and spanning the same rows. `None`
    /// when the list is filtered or has no lanes to draw.
    pub(crate) graph: Option<Rect>,
    /// The commit rows.
    pub(crate) list: Rect,
    /// The selected commit's diff, at the modal's full width. `None` when the
    /// body has no rows left for it.
    pub(crate) preview: Option<Rect>,
}

impl CommitPickerLayout {
    /// The graph column and the table as one rect, which is what scrolls
    /// together and so what a pool covers.
    pub(crate) fn body(&self) -> Rect {
        match self.graph {
            Some(graph) => Rect::new(
                graph.x,
                self.list.y,
                graph.width + self.list.width,
                self.list.height,
            ),
            None => self.list,
        }
    }
}

/// Cells one lane occupies, the second of which separates it from its
/// neighbour so adjacent lanes read as distinct runs rather than a block.
const LANE_CELLS: u16 = 2;

/// Lanes the graph column will draw before it stops widening. A history deep
/// enough to exceed this is rare, and letting the column keep growing would
/// eat the table it exists to annotate.
const MAX_DRAWN_LANES: u16 = 8;

/// Width in cells of a graph column showing `lanes`, including the blank cell
/// separating it from the table's first column.
pub(crate) fn graph_width(lanes: u16) -> u16 {
    lanes.min(MAX_DRAWN_LANES) * LANE_CELLS + 1
}

/// Lanes the graph column should draw for `picker`, or `None` when it hides.
///
/// The graph is laid out over `commits`, so it only lines up while the visible
/// list is that same sequence in that same order. A fuzzy filter both drops and
/// reorders rows, and edges drawn across the survivors would claim an adjacency
/// the history does not have, so the column collapses rather than lie.
pub(crate) fn graph_lanes(picker: &CommitPicker) -> Option<u16> {
    let unfiltered = picker.filtered.iter().copied().eq(0..picker.commits.len());
    unfiltered.then_some(picker.graph.1)
}

/// Lay out the commit picker within `area`, or `None` when `area` is too small
/// to host it.
///
/// Takes its box from [`file_finder_layout`] so the picker sizes and centers
/// like the rest of the modal family, then splits the body horizontally rather
/// than using that layout's side-by-side split.
///
/// `lanes` reserves the graph column, and comes from [`graph_lanes`] so every
/// caller agrees on whether it is showing. `list_percent` is the share of the
/// body the table takes, from [`modal_split`](crate::app::Stoat::modal_split).
pub(crate) fn commit_picker_layout(
    area: Rect,
    lanes: Option<u16>,
    zoom: i8,
    list_percent: u16,
) -> Option<CommitPickerLayout> {
    // A commit row plus its diff preview both want room, and the history behind
    // them is arbitrarily long, so the picker asks for the whole area rather
    // than measuring a list it would only ever outgrow. Only the box comes from
    // that layout, so the share it splits its own body by never shows.
    let box_layout = file_finder_layout(
        area,
        (u16::MAX, u16::MAX),
        zoom,
        crate::render::picker::DEFAULT_LIST_PERCENT,
    )?;
    let inner = box_layout.inner;

    let header = Rect::new(inner.x, inner.y + 2, inner.width, 1);
    let body_top = header.y + 1;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);

    let list_height = (body_height * list_percent / 100)
        .max(MIN_LIST_ROWS)
        .min(body_height);

    // The graph takes its width off the left of the header and rows, never off
    // the preview, which keeps the diff at the modal's full width.
    let graph_cells = lanes.filter(|&l| l > 0).map(graph_width).unwrap_or(0);
    let graph_cells = graph_cells.min(inner.width);
    let graph = (graph_cells > 0).then(|| Rect::new(inner.x, body_top, graph_cells, list_height));

    let table_x = inner.x + graph_cells;
    let table_width = inner.width - graph_cells;
    let header = Rect::new(table_x, header.y, table_width, header.height);
    let list = Rect::new(table_x, body_top, table_width, list_height);

    // One row goes to the separator between the two, so a body with nothing
    // left over shows the list alone.
    let preview_height = body_height.saturating_sub(list_height + 1);
    let preview = (preview_height > 0).then(|| {
        Rect::new(
            inner.x,
            list.y + list_height + 1,
            inner.width,
            preview_height,
        )
    });

    Some(CommitPickerLayout {
        modal: box_layout.modal,
        inner,
        header,
        graph,
        list,
        preview,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_commit_picker(
    picker: &mut CommitPicker,
    ws: &mut Workspace,
    theme: &Theme,
    area: Rect,
    zoom: i8,
    list_percent: u16,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Some(layout) = commit_picker_layout(area, graph_lanes(picker), zoom, list_percent) else {
        return;
    };

    // The same rows serve both roles, so the title is what tells the user
    // whether picking one starts a review or just closes the listing. A drilled
    // scope names itself instead, since which commits are listed matters more
    // there than which role listed them.
    let title = match &picker.scope_label {
        Some(label) => format!(" {label} "),
        None => match picker.role {
            CommitPickerRole::PickBase => " review from commit ".to_string(),
            CommitPickerRole::Browse => " git log ".to_string(),
        },
    };
    let modal_style = theme.get(scope::UI_MODAL_PALETTE);
    Clear.render(layout.modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(&title),
        modal_style,
        theme,
        &mut *scene,
    );

    let inner = layout.inner;
    let separator_style = theme.get(scope::UI_BORDER_INACTIVE);

    crate::render::picker::filter_header(buf, inner, ">", &picker.input, ws, theme, &mut *scene);

    if let Some(preview_rect) = layout.preview {
        // Spans the modal rather than the table, since the preview below it
        // does too and the graph column is part of what it divides.
        crate::render::chrome::hline(
            buf,
            inner.x,
            layout.list.y + layout.list.height,
            inner.width,
            separator_style,
            &mut *scene,
        );
        render_preview(picker, preview_rect, theme, buf, scene);
    }

    picker.viewport_rows = Some(layout.list.height as usize);
    let start_row =
        crate::render::picker::window_start(picker.selected, layout.list.height as usize);
    paint_commit_picker_rows(picker, start_row, layout.header, layout.list, theme, buf);
    if let Some(graph_rect) = layout.graph {
        paint_commit_graph(picker, start_row, graph_rect, theme, buf, scene);
    }
}

/// Colors lanes cycle through, so neighbouring lanes stay tellable apart.
///
/// Fixed rather than themed because a lane index carries no meaning a theme
/// could speak to. Each is blended toward the modal background at paint time,
/// which keeps the graph present without competing with the table beside it.
const LANE_COLORS: [[u8; 3]; 6] = [
    [122, 162, 247],
    [158, 206, 106],
    [224, 175, 104],
    [187, 154, 247],
    [125, 207, 255],
    [247, 118, 142],
];

/// Share of the background mixed into a lane color, in percent.
///
/// Enough to settle the palette against the modal background without washing
/// the lanes out. A graph too faint to trace is a graph that does not do its
/// job, so this stays light.
const LANE_BLEND_PERCENT: u16 = 10;

/// Stroke thickness of a lane line, in sixteenths of a cell.
const LANE_STROKE: u16 = 5;

/// Diameter of a commit's node dot, in sixteenths of a cell.
pub(crate) const NODE_DIAMETER: u16 = 10;

/// Diameter of a branch tip's node dot, in sixteenths of a cell.
///
/// Wider than an ordinary commit's, and drawn in the unblended lane color, so
/// the rows a reader orients by stand out from the history running past them.
pub(crate) const BRANCH_NODE_DIAMETER: u16 = 14;

/// Sixteenths a node's halo adds to its diameter.
///
/// A lane runs straight through its own node in nearly the same color, so
/// without a ring of background between them the two read as one thick stroke
/// instead of a dot threaded onto a line.
pub(crate) const NODE_HALO: u16 = 4;

/// Diameter of the background disc punched out of a merge's node, in
/// sixteenths of a cell.
///
/// Leaving a ring is what tells a commit with more than one parent from an
/// ordinary one at a glance, without spending a second color on it.
pub(crate) const MERGE_HOLE: u16 = 4;

/// Sixteenths of the row a lane transition runs vertical at each end, before
/// and after the blend that carries it across.
///
/// A lane has to read as a vertical column right up to the bend, so the line
/// cannot start drifting sideways the moment it leaves a node.
pub(crate) const CURVE_STRAIGHT: i16 = 4;

/// Chords sampled along the blended middle of a lane transition.
///
/// The blend spans only the eight sixteenths [`CURVE_STRAIGHT`] leaves between
/// the stubs, so eight chords put a vertex at every sixteenth and the round
/// caps cover the joins, reading as a curve without the protocol needing a
/// curve primitive.
const CURVE_SEGMENTS: i32 = 8;

/// Paint the node graph for the rows starting at `start_row`, one graph row per
/// table row.
///
/// A node's shape says what kind of commit it marks. A branch tip takes a wider
/// disc in the unblended lane color, since those are the rows a reader navigates
/// by. A merge is hollowed out to a ring, and an ordinary commit stays a filled
/// disc. Each sits inside a halo of the modal background, which is what keeps it
/// readable as a dot against the lane running through it.
///
/// Under a terminal whose theme resolves to RGB this strokes the lanes as
/// polylines and writes no glyphs, matching how every other sub-cell component
/// here degrades. The fallback draws one glyph per lane per row, which cannot
/// express a curve spanning a row boundary, so it approximates: a dot at the
/// node, a bar where a lane runs straight through, and a tick at the node where
/// the row spawns or absorbs a lane.
pub(crate) fn paint_commit_graph(
    picker: &CommitPicker,
    start_row: usize,
    area: Rect,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let (rows, lanes) = &picker.graph;
    if area.width == 0 || area.height == 0 || *lanes == 0 {
        return;
    }

    // A stroked path needs a host that can draw one and a theme that names its
    // color, the same gate every other sub-cell component here uses.
    let rich = scene.live() && style_rgb(theme.get(scope::UI_TEXT).fg).is_some();
    let background = style_rgb(theme.get(scope::UI_MODAL_PALETTE).bg);
    let drawn = (*lanes).min(MAX_DRAWN_LANES);
    let raw_lane_color = |lane: u16| LANE_COLORS[lane.min(drawn - 1) as usize % LANE_COLORS.len()];
    let lane_color = |lane: u16| match background {
        Some(bg) => blend(raw_lane_color(lane), bg, LANE_BLEND_PERCENT),
        None => raw_lane_color(lane),
    };
    // A lane past the drawn width folds onto the last one rather than falling
    // off the column, so a deep history still shows every commit's node.
    let clamp = |lane: u16| lane.min(drawn - 1);

    // The lanes are laid out over `commits`, and the column only shows while
    // that is exactly what the list displays, so the two walk together.
    for (row_idx, (row, commit)) in rows
        .iter()
        .zip(&picker.commits)
        .skip(start_row)
        .take(area.height as usize)
        .enumerate()
    {
        let y = row_idx as u16;
        let node_lane = clamp(row.node_lane);
        let tip = picker.branch_tips.contains_key(&commit.sha);
        // A tip that is also a merge draws as a tip. Which branch a row heads
        // is what a reader navigates by, so it outranks how the row was made.
        let merge = !tip && commit.parents.len() >= 2;

        for edge in &row.edges {
            let (from, to) = (clamp(edge.from_lane), clamp(edge.to_lane));
            match rich {
                true => {
                    Polyline {
                        points: edge_points(from, to, y),
                        width: LANE_STROKE,
                        color: lane_color(from),
                    }
                    .render(area, buf, scene);
                },
                false => paint_edge_glyphs(buf, area, from, to, y, theme),
            }
        }

        match rich {
            true => {
                let diameter = match tip {
                    true => BRANCH_NODE_DIAMETER,
                    false => NODE_DIAMETER,
                };

                if let Some(bg) = background {
                    dot(area, node_lane, y, diameter + NODE_HALO, bg, buf, scene);
                }
                let color = match tip {
                    true => raw_lane_color(node_lane),
                    false => lane_color(node_lane),
                };
                dot(area, node_lane, y, diameter, color, buf, scene);

                // Without a background to punch the ring out of, a merge keeps
                // the filled disc rather than losing its node entirely.
                if merge && let Some(bg) = background {
                    dot(area, node_lane, y, MERGE_HOLE, bg, buf, scene);
                }
            },
            false => {
                let x = area.x + node_lane * LANE_CELLS;
                if x < area.x + area.width {
                    buf[(x, area.y + y)]
                        .set_char(match (tip, merge) {
                            (true, _) => '◉',
                            (false, true) => '○',
                            (false, false) => '●',
                        })
                        .set_style(theme.get(scope::UI_TEXT));
                }
            },
        }
    }
}

/// Stroke a disc `width` sixteenths across, centered on `lane` in row `y`.
///
/// A path of one point has no direction, which the renderer resolves to a round
/// cap and so to a dot. That is the only circle the protocol offers, so every
/// node, halo, and merge hole is one of these.
fn dot(
    area: Rect,
    lane: u16,
    y: u16,
    width: u16,
    color: [u8; 3],
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    Polyline {
        points: vec![[lane_center(lane), row_center(y)]],
        width,
        color,
    }
    .render(area, buf, scene);
}

/// Horizontal center of `lane` in sixteenths from the column's left edge.
fn lane_center(lane: u16) -> i16 {
    (lane * LANE_CELLS) as i16 * 16 + 8
}

/// Vertical center of row `y` in sixteenths from the column's top edge.
fn row_center(y: u16) -> i16 {
    y as i16 * 16 + 8
}

/// The points of the segment running from row `y`'s center to the next row's.
///
/// A lane that keeps its column is a straight run of two points. One that
/// changes column runs straight, bends, then runs straight again: a vertical
/// stub [`CURVE_STRAIGHT`] long at each end of the row, with a smoothstep in x
/// against a linear y confined to the middle between them.
///
/// The stubs are what let a lane read as a column rather than a drift, and the
/// smoothstep's zero slope at both ends is tangent to them, so the three pieces
/// join without a corner.
fn edge_points(from: u16, to: u16, y: u16) -> Vec<[i16; 2]> {
    let (x0, x1) = (lane_center(from), lane_center(to));
    let (y0, y1) = (row_center(y), row_center(y + 1));
    if from == to {
        return vec![[x0, y0], [x1, y1]];
    }

    let (bend_top, bend_bottom) = (y0 + CURVE_STRAIGHT, y1 - CURVE_STRAIGHT);
    let mut points = Vec::with_capacity(CURVE_SEGMENTS as usize + 3);

    points.push([x0, y0]);
    points.extend((0..=CURVE_SEGMENTS).map(|step| {
        let t = step as f32 / CURVE_SEGMENTS as f32;
        let eased = t * t * (3.0 - 2.0 * t);
        [
            x0 + ((x1 - x0) as f32 * eased).round() as i16,
            bend_top + ((bend_bottom - bend_top) as f32 * t).round() as i16,
        ]
    }));
    points.push([x1, y1]);

    points
}

/// The fallback's one glyph for an edge, written at its origin lane.
fn paint_edge_glyphs(buf: &mut Buffer, area: Rect, from: u16, to: u16, y: u16, theme: &Theme) {
    let glyph = match from.cmp(&to) {
        Ordering::Equal => '│',
        Ordering::Less => '├',
        Ordering::Greater => '┤',
    };
    let x = area.x + from * LANE_CELLS;
    if x < area.x + area.width {
        buf[(x, area.y + y)]
            .set_char(glyph)
            .set_style(theme.get(scope::UI_TEXT_MUTED));
    }
}

/// Mix `percent` of `toward` into `color`.
fn blend(color: [u8; 3], toward: [u8; 3], percent: u16) -> [u8; 3] {
    let mix = |a: u8, b: u8| {
        let a = u16::from(a) * (100 - percent);
        let b = u16::from(b) * percent;
        ((a + b) / 100) as u8
    };
    [
        mix(color[0], toward[0]),
        mix(color[1], toward[1]),
        mix(color[2], toward[2]),
    ]
}

/// The commit table's columns, in [`CommitColumn`] order.
///
/// The title fills, since it is the column worth the most room and the only one
/// whose useful length has no bound. The rest size to what they hold: a sha
/// stops at the abbreviation git prints, a branch or author gets enough to
/// recognise without crowding out the title, and a relative age never needs
/// more than a few characters.
const COLUMNS: [Column; COMMIT_COLUMNS] = [
    Column {
        label: "Commit",
        width: Width::Fit { min: 7, max: 12 },
    },
    Column {
        label: "Branch",
        width: Width::Fit { min: 6, max: 20 },
    },
    Column {
        label: "Title",
        width: Width::Fill,
    },
    Column {
        label: "Author",
        width: Width::Fit { min: 6, max: 16 },
    },
    Column {
        label: "Date",
        // A relative age never exceeds four characters ("12mo" is the longest
        // any bucket produces), and the label is four too.
        width: Width::Fixed(4),
    },
];

/// The columns in display order, so a cell index resolves to the column it
/// belongs to. Parallel to [`COLUMNS`].
const COLUMN_ORDER: [CommitColumn; COMMIT_COLUMNS] = [
    CommitColumn::Commit,
    CommitColumn::Branch,
    CommitColumn::Title,
    CommitColumn::Author,
    CommitColumn::Date,
];

/// `scroll` clamped so the last page of `session`'s diff still fills `height`.
///
/// The diff's row count is only known once its session has built, so the wheel
/// and the paging keys let the scroll run past the end and it is pulled back
/// here. Split out from the renderer because a pooled preview page clamps
/// against the same session without a live picker to mutate.
pub(crate) fn clamped_preview_scroll(scroll: usize, session: &ReviewSession, height: u16) -> usize {
    let last_page =
        crate::render::commits::preview_row_count(session).saturating_sub(height as usize);
    scroll.min(last_page)
}

/// Paint the commit table into `list`, with its column labels in `header`,
/// covering the `list.height` rows starting at `start_row`.
///
/// The window is the caller's choice rather than derived from the selection,
/// so a pooled page can paint a span the live picker is not looking at.
///
/// Widths come from the picker's measure over the whole filtered list, so every
/// window agrees on where the columns sit. Match highlights are translated out
/// of each row's joined haystack onto the column showing them, and one whose
/// cell truncated it away is dropped.
pub(crate) fn paint_commit_picker_rows(
    picker: &CommitPicker,
    start_row: usize,
    header: Rect,
    area: Rect,
    theme: &Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }

    let visible = picker.window(start_row, rows);

    let text_x = area.x + 1;
    let table_width = area.width.saturating_sub(1);
    let widths = table::resolve_widths(&COLUMNS, &picker.col_widest, table_width);
    let starts = table::column_starts(&widths);

    let muted = theme.get(scope::UI_TEXT_MUTED);
    let header_area = Rect::new(text_x, header.y, table_width, header.height);
    paint_column_header(
        buf,
        header_area,
        &widths,
        picker.filter_column,
        muted,
        theme.get(scope::UI_SEARCH_MATCH),
    );

    let row_style = theme.get(scope::UI_TEXT);
    let selected_style = theme.get(scope::UI_SELECTION);
    let match_style = theme.get(scope::UI_SEARCH_MATCH);
    let sha_style = theme.get(scope::VCS_COMMIT_SHA);
    let branch_style = theme.get(scope::VCS_COMMIT_METADATA);
    let end_x = area.x + area.width;

    for (row_idx, (row, indices)) in visible
        .iter()
        .zip(picker.match_indices.iter().skip(start_row))
        .enumerate()
    {
        let y = area.y + row_idx as u16;
        let is_selected = start_row + row_idx == picker.selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };
        for col in area.x..end_x {
            buf[(col, y)].set_char(' ').set_style(style);
        }

        let bounds = Rect::new(text_x, y, table_width, 1);
        for (column, (&width, &start)) in widths.iter().zip(starts.iter()).enumerate() {
            // The selection's own background carries the row, so leaving the
            // metadata colors off it keeps the highlight readable.
            let cell_style = if is_selected {
                style
            } else {
                match COLUMN_ORDER[column] {
                    CommitColumn::Commit => sha_style,
                    CommitColumn::Branch | CommitColumn::Author | CommitColumn::Date => {
                        branch_style
                    },
                    CommitColumn::Title => style,
                }
            };
            // A scoped query dims every column it does not search, so the one
            // being searched is obvious. The selected row keeps its background
            // and gives up only its foreground, which would otherwise lose the
            // selection entirely.
            let cell_style = match picker.filter_column {
                Some(active) if active != COLUMN_ORDER[column] => dimmed(cell_style, muted),
                _ => cell_style,
            };
            table::paint_cell(
                buf,
                text_x + start,
                y,
                &row.cells[column].text,
                width,
                cell_style,
                bounds,
            );
        }

        let cell_starts: Vec<usize> = row.cells.iter().map(|cell| cell.start).collect();
        let cell_lens: Vec<usize> = row
            .cells
            .iter()
            .map(|cell| cell.text.chars().count())
            .collect();
        for &offset in indices {
            let Some(col) =
                table::cell_column(offset as usize, &cell_starts, &cell_lens, &widths, &starts)
            else {
                continue;
            };
            let col = text_x + col;
            if col < end_x {
                buf[(col, y)].set_style(match_style);
            }
        }
    }
}

/// Paint the column labels, marking which one the query is scoped to.
///
/// The active label takes the search-match color and the rest stay muted, so
/// the header says what Shift-Tab has selected. With no column scoped every
/// label is muted, which is the unscoped table's usual look.
fn paint_column_header(
    buf: &mut Buffer,
    area: Rect,
    widths: &[u16],
    active: Option<CommitColumn>,
    muted: ratatui::style::Style,
    match_style: ratatui::style::Style,
) {
    table::paint_header(buf, area, &COLUMNS, widths, |column| match active {
        Some(active) if active == COLUMN_ORDER[column] => match_style,
        _ => muted,
    });
}

/// `style` with the muted foreground, keeping whatever background it carries.
///
/// Dimming a cell must not drop the selection's background, which is the only
/// thing marking which row the cursor is on.
fn dimmed(style: ratatui::style::Style, muted: ratatui::style::Style) -> ratatui::style::Style {
    match muted.fg {
        Some(fg) => style.fg(fg),
        None => style,
    }
}

/// Draw the selected commit's cached diff, or a placeholder while its
/// background build is still running.
///
/// The stored scroll is clamped here, where the diff's row count is known.
/// Stopping a page short of the end is what keeps the wheel from scrolling the
/// diff off into blank space.
fn render_preview(
    picker: &mut CommitPicker,
    area: Rect,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let session = picker
        .selected_commit()
        .and_then(|commit| picker.preview_sessions.get(&commit.sha))
        .cloned();
    match session {
        Some(session) => {
            picker.preview_scroll =
                clamped_preview_scroll(picker.preview_scroll, &session, area.height);
            crate::render::commits::render_commit_preview(
                &session,
                theme,
                area,
                picker.preview_scroll,
                buf,
                scene,
            )
        },
        None => {
            write_str_clipped(
                buf,
                area.x,
                area.y,
                "loading diff...",
                theme.get(scope::UI_TEXT_MUTED),
                area.x + area.width,
            );
        },
    }
}
