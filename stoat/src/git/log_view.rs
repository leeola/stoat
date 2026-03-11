use crate::{
    git::{
        diff_summary::DiffPreviewElement,
        log_graph::{CommitEntry, CommitLine, CommitLineSegment, CurveKind, GraphOutput},
        rebase::format_relative_time,
        repository::{CommitFileChange, CommitLogEntry},
        status::DiffPreviewData,
    },
    quick_input::QuickInput,
};
use gpui::{
    canvas, div, point, prelude::FluentBuilder, px, rgb, rgba, uniform_list, Entity, FontWeight,
    Hsla, InteractiveElement, IntoElement, ParentElement, PathBuilder, Pixels, RenderOnce,
    ScrollHandle, StatefulInteractiveElement, Styled, UniformListScrollHandle, Window,
};
use std::collections::BTreeMap;

pub struct GitLogDetailSnapshot {
    pub files: Vec<CommitFileChange>,
    pub selected_file: usize,
    pub preview: Option<DiffPreviewData>,
}

const LANE_COLORS: [u32; 8] = [
    0x569cd6, // blue
    0x6a9955, // green
    0xce9178, // orange
    0xdcdcaa, // yellow
    0xc586c0, // magenta
    0x4ec9b0, // teal
    0xd16969, // red
    0x9cdcfe, // light blue
];

const ROW_HEIGHT: f32 = 22.0;
const LANE_WIDTH: f32 = 16.0;
const LEFT_PADDING: f32 = 12.0;
const COMMIT_RADIUS: f32 = 3.5;
const LINE_WIDTH: f32 = 1.5;
const COMMIT_STROKE_WIDTH: f32 = 1.5;

fn lane_color(idx: usize) -> Hsla {
    rgb(LANE_COLORS[idx % LANE_COLORS.len()]).into()
}

fn lane_center_x(bounds_x: Pixels, lane: f32) -> Pixels {
    bounds_x + px(LEFT_PADDING) + px(lane * LANE_WIDTH) + px(LANE_WIDTH / 2.0)
}

fn row_center_y(
    bounds_y: Pixels,
    row: usize,
    first_visible_row: usize,
    vert_offset: Pixels,
) -> Pixels {
    bounds_y + px((row as i32 - first_visible_row as i32) as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.0)
        - vert_offset
}

fn draw_commit_circle(center_x: Pixels, center_y: Pixels, color: Hsla, window: &mut Window) {
    let radius = px(COMMIT_RADIUS);

    let mut builder = PathBuilder::fill();
    builder.move_to(point(center_x + radius, center_y));
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(center_x - radius, center_y),
    );
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(center_x + radius, center_y),
    );
    builder.close();

    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn graph_width(max_lanes: usize) -> f32 {
    let max_lanes = max_lanes.max(6) as f32;
    LANE_WIDTH * max_lanes + LEFT_PADDING * 2.0
}

#[derive(IntoElement)]
pub struct GitLogView {
    commits: Vec<CommitLogEntry>,
    graph: GraphOutput,
    selected: usize,
    detail: Option<GitLogDetailSnapshot>,
    scroll_handle: UniformListScrollHandle,
    loading: bool,
    search_query: String,
    search_matches: Vec<usize>,
    search_input: Option<Entity<QuickInput>>,
}

impl GitLogView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        commits: Vec<CommitLogEntry>,
        graph: GraphOutput,
        selected: usize,
        detail: Option<GitLogDetailSnapshot>,
        scroll_handle: UniformListScrollHandle,
        loading: bool,
        search_query: String,
        search_matches: Vec<usize>,
        search_input: Option<Entity<QuickInput>>,
    ) -> Self {
        Self {
            commits,
            graph,
            selected,
            detail,
            scroll_handle,
            loading,
            search_query,
            search_matches,
            search_input,
        }
    }

    fn render_graph_canvas(
        entries: Vec<CommitEntry>,
        lines: Vec<CommitLine>,
        max_lanes: usize,
        scroll_handle: ScrollHandle,
    ) -> impl IntoElement {
        let gw = graph_width(max_lanes);

        canvas(
            |_bounds, _window, _cx| {},
            move |bounds, (), window, _cx| {
                let scroll_y = -f32::from(scroll_handle.offset().y);
                let viewport_h = f32::from(bounds.size.height);

                let first_visible_row = (scroll_y / ROW_HEIGHT).floor().max(0.0) as usize;
                let vert_offset = px(scroll_y - first_visible_row as f32 * ROW_HEIGHT);
                let last_visible_row =
                    first_visible_row + (viewport_h / ROW_HEIGHT).ceil() as usize + 1;
                let last_visible_row = last_visible_row.min(entries.len());

                window.paint_layer(bounds, |window| {
                    let visible_range = first_visible_row..last_visible_row;
                    for row_idx in visible_range.clone() {
                        let entry = &entries[row_idx];
                        let color = lane_color(entry.color_idx);
                        let cx_pos = lane_center_x(bounds.origin.x, entry.lane as f32);
                        let cy_pos =
                            row_center_y(bounds.origin.y, row_idx, first_visible_row, vert_offset);
                        draw_commit_circle(cx_pos, cy_pos, color, window);
                    }

                    let commit_lines: Vec<&CommitLine> = lines
                        .iter()
                        .filter(|line| {
                            line.full_interval.start <= visible_range.end
                                && line.full_interval.end >= visible_range.start
                        })
                        .collect();

                    let mut color_groups: BTreeMap<usize, Vec<PathBuilder>> = BTreeMap::new();

                    for line in commit_lines {
                        let Some((start_segment_idx, start_column)) =
                            line.get_first_visible_segment_idx(first_visible_row)
                        else {
                            continue;
                        };

                        let line_x = lane_center_x(bounds.origin.x, start_column as f32);

                        let start_row_idx = line.full_interval.start;
                        let from_y = row_center_y(
                            bounds.origin.y,
                            start_row_idx,
                            first_visible_row,
                            vert_offset,
                        ) + px(COMMIT_RADIUS);

                        let mut current_y = from_y;
                        let mut current_x = line_x;

                        let mut builder = PathBuilder::stroke(px(LINE_WIDTH));
                        builder.move_to(point(line_x, from_y));

                        let segments = &line.segments[start_segment_idx..];

                        for (segment_idx, segment) in segments.iter().enumerate() {
                            let is_last = segment_idx + 1 == segments.len();

                            match segment {
                                CommitLineSegment::Straight { to_row } => {
                                    let mut dest_y = row_center_y(
                                        bounds.origin.y,
                                        *to_row,
                                        first_visible_row,
                                        vert_offset,
                                    );
                                    if is_last {
                                        dest_y -= px(COMMIT_RADIUS);
                                    }

                                    let dest = point(current_x, dest_y);
                                    current_y = dest_y;
                                    builder.line_to(dest);
                                    builder.move_to(dest);
                                },
                                CommitLineSegment::Curve {
                                    to_column,
                                    on_row,
                                    curve_kind,
                                } => {
                                    let mut to_x =
                                        lane_center_x(bounds.origin.x, *to_column as f32);
                                    let mut to_y = row_center_y(
                                        bounds.origin.y,
                                        *on_row,
                                        first_visible_row,
                                        vert_offset,
                                    );

                                    let going_right = to_x > current_x;
                                    let column_shift = if going_right {
                                        px(COMMIT_RADIUS + COMMIT_STROKE_WIDTH)
                                    } else {
                                        px(-COMMIT_RADIUS - COMMIT_STROKE_WIDTH)
                                    };

                                    let control = match curve_kind {
                                        CurveKind::Checkout => {
                                            if is_last {
                                                to_x -= column_shift;
                                            }
                                            builder.move_to(point(current_x, current_y));
                                            point(current_x, to_y)
                                        },
                                        CurveKind::Merge => {
                                            if is_last {
                                                to_y -= px(COMMIT_RADIUS);
                                            }
                                            builder.move_to(point(
                                                current_x + column_shift,
                                                current_y - px(COMMIT_RADIUS),
                                            ));
                                            point(to_x, current_y)
                                        },
                                    };

                                    let row_h = px(ROW_HEIGHT);
                                    let lane_w = px(LANE_WIDTH);
                                    match curve_kind {
                                        CurveKind::Checkout if (to_y - current_y).abs() > row_h => {
                                            let start_curve = point(current_x, current_y + row_h);
                                            builder.line_to(start_curve);
                                            builder.move_to(start_curve);
                                        },
                                        CurveKind::Merge if (to_x - current_x).abs() > lane_w => {
                                            let col_shift =
                                                if going_right { lane_w } else { -lane_w };
                                            let start_curve = point(
                                                current_x + col_shift,
                                                current_y - px(COMMIT_RADIUS),
                                            );
                                            builder.line_to(start_curve);
                                            builder.move_to(start_curve);
                                        },
                                        _ => {},
                                    }

                                    builder.curve_to(point(to_x, to_y), control);
                                    current_y = to_y;
                                    current_x = to_x;
                                    builder.move_to(point(current_x, current_y));
                                },
                            }
                        }

                        builder.close();
                        color_groups
                            .entry(line.color_idx)
                            .or_default()
                            .push(builder);
                    }

                    for (color_idx, builders) in color_groups {
                        let line_color = lane_color(color_idx);
                        for b in builders {
                            if let Ok(path) = b.build() {
                                window.paint_layer(bounds, |window| {
                                    window.paint_path(path, line_color);
                                });
                            }
                        }
                    }
                });
            },
        )
        .w(px(gw))
        .h_full()
    }
}

impl RenderOnce for GitLogView {
    fn render(self, window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let viewport_width = f32::from(window.viewport_size().width);
        let viewport_height = f32::from(window.viewport_size().height);

        let has_detail = self.detail.is_some();
        let show_detail = has_detail && viewport_width > 900.0;
        let is_empty = self.commits.is_empty();
        let commit_count = self.commits.len();
        let selected = self.selected;
        let is_searching = !self.search_query.is_empty();

        let selected_commit = if show_detail {
            self.commits.get(selected).cloned()
        } else {
            None
        };

        let title = if is_searching {
            format!(
                "Git Log ({} commits, {} matches for \"{}\")",
                commit_count,
                self.search_matches.len(),
                self.search_query,
            )
        } else {
            format!("Git Log ({commit_count} commits)")
        };

        let mut header = div()
            .p(px(8.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .font_weight(FontWeight::SEMIBOLD)
            .child(title);

        if let Some(search_input) = self.search_input.clone() {
            header = header.child(div().pt(px(4.0)).child(search_input));
        }

        let commit_list = if is_empty && !self.loading {
            div()
                .id("git-log-list")
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(rgb(0x808080))
                        .text_size(px(13.0))
                        .child("No commits found"),
                )
        } else {
            let has_graph = !self.graph.entries.is_empty();
            let gw = if has_graph {
                graph_width(self.graph.max_lanes)
            } else {
                0.0
            };

            let base_scroll_handle = self.scroll_handle.0.borrow().base_handle.clone();
            let graph_canvas = if has_graph {
                Some(Self::render_graph_canvas(
                    self.graph.entries,
                    self.graph.lines,
                    self.graph.max_lanes,
                    base_scroll_handle,
                ))
            } else {
                None
            };

            let commits = self.commits;
            let search_matches = self.search_matches;
            let loading = self.loading;
            let item_count = commits.len() + if loading { 1 } else { 0 };

            let list = uniform_list("git-log-list", item_count, move |range, _window, _cx| {
                range
                    .map(|i| {
                        if i >= commits.len() {
                            return div()
                                .px(px(8.0))
                                .py(px(4.0))
                                .text_color(rgb(0x808080))
                                .text_size(px(11.0))
                                .child("Loading...");
                        }

                        let commit = &commits[i];
                        let is_selected = i == selected;
                        let is_match = is_searching && search_matches.contains(&i);
                        let dim = is_searching && !is_match;
                        let date_str = format_relative_time(commit.timestamp);

                        let mut row = div()
                            .flex()
                            .h(px(ROW_HEIGHT))
                            .px(px(8.0))
                            .when(is_selected, |d| d.bg(rgb(0x3b4261)))
                            .when(dim, |d| d.opacity(0.35));

                        if has_graph {
                            row = row.child(div().w(px(gw)).flex_shrink_0());
                        }

                        let text_row = div()
                            .flex()
                            .gap_2()
                            .flex_1()
                            .items_center()
                            .child(
                                div()
                                    .text_color(rgb(0xce9178))
                                    .text_size(px(11.0))
                                    .w(px(56.0))
                                    .flex_shrink_0()
                                    .child(commit.short_hash.clone()),
                            )
                            .child(
                                div()
                                    .text_color(rgb(0xd4d4d4))
                                    .text_size(px(11.0))
                                    .flex_1()
                                    .overflow_x_hidden()
                                    .child(commit.message.clone()),
                            )
                            .child(
                                div()
                                    .text_color(rgb(0x808080))
                                    .text_size(px(10.0))
                                    .flex_shrink_0()
                                    .child(format!("{} {}", commit.author, date_str)),
                            );

                        row.child(text_row)
                    })
                    .collect()
            })
            .flex_1()
            .track_scroll(&self.scroll_handle);

            div()
                .id("git-log-content")
                .flex()
                .flex_col()
                .flex_1()
                .overflow_hidden()
                .relative()
                .child(list)
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(gw))
                        .when_some(graph_canvas, |d, c| d.child(c)),
                )
        };

        let footer = div()
            .border_t_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .px(px(8.0))
            .py(px(4.0))
            .flex()
            .gap_4()
            .text_size(px(10.0))
            .text_color(rgb(0x808080))
            .child(key_hint("j/k", "navigate"))
            .child(key_hint("h/l", "detail files"))
            .child(key_hint("/", "search"))
            .child(key_hint("q/esc", "close"));

        let detail_panel = if show_detail {
            if let (Some(ref detail), Some(ref commit)) = (&self.detail, &selected_commit) {
                let date_str = format_relative_time(commit.timestamp);

                let modal_width = viewport_width * 0.75;
                let mut detail_div = div()
                    .flex()
                    .flex_col()
                    .w(px(modal_width * 0.4))
                    .border_l_1()
                    .border_color(rgb(0x3e3e42))
                    .overflow_hidden();

                let meta = div()
                    .p(px(8.0))
                    .border_b_1()
                    .border_color(rgb(0x3e3e42))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(rgb(0xce9178))
                            .text_size(px(10.0))
                            .child(commit.oid.clone()),
                    )
                    .child(
                        div()
                            .text_color(rgb(0x808080))
                            .text_size(px(10.0))
                            .child(format!("{} - {}", commit.author, date_str)),
                    )
                    .child(
                        div()
                            .text_color(rgb(0xd4d4d4))
                            .text_size(px(11.0))
                            .child(commit.message.clone()),
                    );

                detail_div = detail_div.child(meta);

                if !detail.files.is_empty() {
                    let mut files_div = div()
                        .id("git-log-files")
                        .flex()
                        .flex_col()
                        .border_b_1()
                        .border_color(rgb(0x3e3e42))
                        .max_h(px(200.0))
                        .overflow_y_scroll();

                    for (fi, file) in detail.files.iter().enumerate() {
                        let status_color = match file.status.as_str() {
                            "A" => rgb(0x6a9955),
                            "D" => rgb(0xf14c4c),
                            _ => rgb(0xdcdcaa),
                        };
                        files_div = files_div.child(
                            div()
                                .flex()
                                .gap_2()
                                .px(px(8.0))
                                .py(px(1.0))
                                .when(fi == detail.selected_file, |d| d.bg(rgb(0x3b4261)))
                                .child(
                                    div()
                                        .text_color(status_color)
                                        .text_size(px(10.0))
                                        .w(px(14.0))
                                        .child(file.status.clone()),
                                )
                                .child(
                                    div()
                                        .text_color(rgb(0xd4d4d4))
                                        .text_size(px(10.0))
                                        .flex_1()
                                        .child(file.path.to_string_lossy().to_string()),
                                ),
                        );
                    }

                    detail_div = detail_div.child(files_div);
                }

                let preview_elem = DiffPreviewElement::new(detail.preview.clone());
                detail_div = detail_div.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h(px(200.0))
                        .child(preview_elem),
                );

                Some(detail_div)
            } else {
                None
            }
        } else {
            None
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .bg(rgba(0x00000030))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .when(is_empty && !self.loading, |d| d.w(px(500.0)).h(px(200.0)))
                    .when(!is_empty || self.loading, |d| {
                        d.w_3_4().h(px(viewport_height * 0.85))
                    })
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(header)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .overflow_hidden()
                            .child(div().flex().flex_col().flex_1().child(commit_list))
                            .when_some(detail_panel, |d, panel| d.child(panel)),
                    )
                    .child(footer),
            )
    }
}

fn key_hint(key: &str, label: &str) -> impl IntoElement {
    div()
        .flex()
        .gap_1()
        .child(
            div()
                .text_color(rgb(0xd4d4d4))
                .font_weight(FontWeight::BOLD)
                .child(key.to_string()),
        )
        .child(div().text_color(rgb(0x808080)).child(label.to_string()))
}
