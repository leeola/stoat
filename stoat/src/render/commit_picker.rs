use crate::{
    commit_picker::{CommitColumn, CommitPicker, CommitPickerRole, CommitRow, COMMIT_COLUMNS},
    render::{
        file_finder::file_finder_layout,
        review::style_rgb,
        table::{self, Column, Width},
        text::write_str_clipped,
    },
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

/// Share of the body the commit list takes, the rest going to the diff below
/// it. A percentage rather than a row count so the split holds at any modal
/// size.
const LIST_BODY_PERCENT: u16 = 40;

/// Rows the list keeps however small the body gets, so a short modal still
/// shows enough history to choose from.
const MIN_LIST_ROWS: u16 = 5;

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

/// Cells one lane occupies, the second of which separates it from its
/// neighbour so adjacent lanes read as distinct runs rather than a block.
const LANE_CELLS: u16 = 2;

/// Lanes the graph column will draw before it stops widening. A history deep
/// enough to exceed this is rare, and letting the column keep growing would
/// eat the table it exists to annotate.
const MAX_DRAWN_LANES: u16 = 8;

/// Width in cells of a graph column showing `lanes`, including the blank cell
/// separating it from the table's first column.
fn graph_width(lanes: u16) -> u16 {
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
/// caller agrees on whether it is showing.
pub(crate) fn commit_picker_layout(
    area: Rect,
    lanes: Option<u16>,
    zoom: i8,
) -> Option<CommitPickerLayout> {
    // A commit row plus its diff preview both want room, and the history behind
    // them is arbitrarily long, so the picker asks for the whole area rather
    // than measuring a list it would only ever outgrow.
    let box_layout = file_finder_layout(area, (u16::MAX, u16::MAX), zoom)?;
    let inner = box_layout.inner;

    let header = Rect::new(inner.x, inner.y + 2, inner.width, 1);
    let body_top = header.y + 1;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);

    let list_height = (body_height * LIST_BODY_PERCENT / 100)
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

pub(crate) fn render_commit_picker(
    picker: &mut CommitPicker,
    ws: &mut Workspace,
    theme: &Theme,
    area: Rect,
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Some(layout) = commit_picker_layout(area, graph_lanes(picker), zoom) else {
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
    paint_commit_picker_rows(picker, layout.header, layout.list, theme, buf);
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
const LANE_BLEND_PERCENT: u16 = 20;

/// Stroke thickness of a lane line, in sixteenths of a cell.
const LANE_STROKE: u16 = 2;

/// Diameter of a commit's node dot, in sixteenths of a cell.
const NODE_DIAMETER: u16 = 6;

/// Intermediate points sampled along a lane transition. Four segments read as a
/// curve at cell scale without the protocol needing a curve primitive.
const CURVE_SEGMENTS: i32 = 4;

/// Paint the node graph for the rows starting at `start_row`, one graph row per
/// table row.
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

    // A theme that resolves to RGB means a terminal that can show the stroked
    // path, the same gate every other sub-cell component here uses.
    let rich = style_rgb(theme.get(scope::UI_TEXT).fg).is_some();
    let background = style_rgb(theme.get(scope::UI_MODAL_PALETTE).bg);
    let drawn = (*lanes).min(MAX_DRAWN_LANES);
    let lane_color = |lane: u16| {
        let raw = LANE_COLORS[lane.min(drawn - 1) as usize % LANE_COLORS.len()];
        match background {
            Some(bg) => blend(raw, bg, LANE_BLEND_PERCENT),
            None => raw,
        }
    };
    // A lane past the drawn width folds onto the last one rather than falling
    // off the column, so a deep history still shows every commit's node.
    let clamp = |lane: u16| lane.min(drawn - 1);

    for (row_idx, row) in rows
        .iter()
        .skip(start_row)
        .take(area.height as usize)
        .enumerate()
    {
        let y = row_idx as u16;
        let node_lane = clamp(row.node_lane);

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
                let center = [lane_center(node_lane), row_center(y)];
                Polyline {
                    points: vec![center],
                    width: NODE_DIAMETER,
                    color: lane_color(node_lane),
                }
                .render(area, buf, scene);
            },
            false => {
                let x = area.x + node_lane * LANE_CELLS;
                if x < area.x + area.width {
                    buf[(x, area.y + y)]
                        .set_char('●')
                        .set_style(theme.get(scope::UI_TEXT));
                }
            },
        }
    }
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
/// changes column is sampled along a smoothstep in x against a linear y, so it
/// leaves and arrives vertically and reads as an S rather than a corner.
fn edge_points(from: u16, to: u16, y: u16) -> Vec<[i16; 2]> {
    let (x0, x1) = (lane_center(from), lane_center(to));
    let (y0, y1) = (row_center(y), row_center(y + 1));
    if from == to {
        return vec![[x0, y0], [x1, y1]];
    }

    (0..=CURVE_SEGMENTS)
        .map(|step| {
            let t = step as f32 / CURVE_SEGMENTS as f32;
            let eased = t * t * (3.0 - 2.0 * t);
            [
                x0 + ((x1 - x0) as f32 * eased).round() as i16,
                y0 + ((y1 - y0) as f32 * t).round() as i16,
            ]
        })
        .collect()
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

/// Paint the commit table into `list`, with its column labels in `header`,
/// following the selection so the selected row stays visible.
///
/// Widths resolve over the rows actually about to be painted, so the columns
/// fit what is on screen rather than the whole history. Match highlights are
/// translated out of each row's joined haystack onto the column showing them,
/// and one whose cell truncated it away is dropped.
fn paint_commit_picker_rows(
    picker: &CommitPicker,
    header: Rect,
    area: Rect,
    theme: &Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let start_row = crate::render::picker::window_start(picker.selected, rows);

    let visible: Vec<CommitRow> = picker
        .filtered
        .iter()
        .skip(start_row)
        .take(rows)
        .map(|&idx| picker.row(idx))
        .collect();

    let text_x = area.x + 1;
    let table_width = area.width.saturating_sub(1);
    let widest: Vec<u16> = (0..COMMIT_COLUMNS)
        .map(|column| {
            visible
                .iter()
                .map(|row| row.cells[column].text.chars().count() as u16)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let widths = table::resolve_widths(&COLUMNS, &widest, table_width);
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
            let last_page = crate::render::commits::preview_row_count(&session)
                .saturating_sub(area.height as usize);
            picker.preview_scroll = picker.preview_scroll.min(last_page);
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
