use crate::{
    git::{
        diff_summary::DiffPreviewElement,
        log_graph::{ConnectionKind, GraphRow},
        rebase::format_relative_time,
        repository::{CommitFileChange, CommitLogEntry},
        status::DiffPreviewData,
    },
    quick_input::QuickInput,
};
use gpui::{
    canvas, div, point, prelude::FluentBuilder, px, quad, rgb, rgba, Bounds, Corners, Edges,
    Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, PathBuilder, Pixels,
    RenderOnce, ScrollHandle, StatefulInteractiveElement, Styled, Window,
};

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

#[derive(IntoElement)]
pub struct GitLogView {
    commits: Vec<CommitLogEntry>,
    graph: Vec<GraphRow>,
    selected: usize,
    detail: Option<GitLogDetailSnapshot>,
    detail_visible: bool,
    scroll_handle: ScrollHandle,
    loading: bool,
    search_query: String,
    search_matches: Vec<usize>,
    search_input: Option<Entity<QuickInput>>,
}

impl GitLogView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        commits: Vec<CommitLogEntry>,
        graph: Vec<GraphRow>,
        selected: usize,
        detail: Option<GitLogDetailSnapshot>,
        detail_visible: bool,
        scroll_handle: ScrollHandle,
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
            detail_visible,
            scroll_handle,
            loading,
            search_query,
            search_matches,
            search_input,
        }
    }

    fn render_graph_cell(&self, row_idx: usize) -> impl IntoElement {
        const GRAPH_COL_WIDTH: f32 = 12.0;
        const NODE_RADIUS: f32 = 3.0;
        const LINE_WIDTH: f32 = 1.5;

        let max_col = self
            .graph
            .iter()
            .map(|r| {
                let conn_max = r
                    .connections
                    .iter()
                    .map(|c| c.from_col.max(c.to_col))
                    .max()
                    .unwrap_or(0);
                r.column.max(conn_max)
            })
            .max()
            .unwrap_or(0);

        let cell_width = (max_col + 1) as f32 * GRAPH_COL_WIDTH + GRAPH_COL_WIDTH;

        let graph_row = self.graph[row_idx].clone();

        canvas(
            |_bounds, _window, _cx| {},
            move |bounds, (), window, _cx| {
                let origin = bounds.origin;
                let height = f32::from(bounds.size.height);
                let mid_y = height / 2.0;

                let col_x = |col: usize| -> Pixels {
                    px(col as f32 * GRAPH_COL_WIDTH + GRAPH_COL_WIDTH / 2.0)
                };

                for conn in &graph_row.connections {
                    let color: Hsla = rgb(LANE_COLORS[conn.color_index % LANE_COLORS.len()]).into();

                    match conn.kind {
                        ConnectionKind::Straight => {
                            let x = col_x(conn.from_col);
                            let mut builder = PathBuilder::stroke(px(LINE_WIDTH));
                            builder.move_to(point(origin.x + x, origin.y + px(-0.5)));
                            builder.line_to(point(origin.x + x, origin.y + px(height + 0.5)));
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, color);
                            }
                        },
                        ConnectionKind::MergeLeft
                        | ConnectionKind::MergeRight
                        | ConnectionKind::BranchLeft
                        | ConnectionKind::BranchRight => {
                            let commit_x = col_x(graph_row.column);
                            let target_x = col_x(conn.to_col);
                            let mut builder = PathBuilder::stroke(px(LINE_WIDTH));
                            builder.move_to(point(origin.x + commit_x, origin.y + px(mid_y)));
                            builder.cubic_bezier_to(
                                point(origin.x + target_x, origin.y + px(height + 0.5)),
                                point(
                                    origin.x + commit_x + (target_x - commit_x) / 2.0,
                                    origin.y + px(height + 0.5),
                                ),
                                point(
                                    origin.x + commit_x + (target_x - commit_x) / 2.0,
                                    origin.y + px(mid_y),
                                ),
                            );
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, color);
                            }
                        },
                    }
                }

                let node_color: Hsla = graph_row
                    .connections
                    .iter()
                    .find(|c| c.from_col == graph_row.column || c.to_col == graph_row.column)
                    .map(|c| rgb(LANE_COLORS[c.color_index % LANE_COLORS.len()]).into())
                    .unwrap_or_else(|| rgb(LANE_COLORS[0]).into());

                let has_straight_at_col = graph_row
                    .connections
                    .iter()
                    .any(|c| c.kind == ConnectionKind::Straight && c.from_col == graph_row.column);
                if graph_row.has_incoming && !has_straight_at_col {
                    let x = col_x(graph_row.column);
                    let mut builder = PathBuilder::stroke(px(LINE_WIDTH));
                    builder.move_to(point(origin.x + x, origin.y + px(-0.5)));
                    builder.line_to(point(origin.x + x, origin.y + px(mid_y)));
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, node_color);
                    }
                }

                let nx = col_x(graph_row.column);
                let node_bounds = Bounds::new(
                    point(
                        origin.x + nx - px(NODE_RADIUS),
                        origin.y + px(mid_y) - px(NODE_RADIUS),
                    ),
                    gpui::size(px(NODE_RADIUS * 2.0), px(NODE_RADIUS * 2.0)),
                );
                window.paint_quad(quad(
                    node_bounds,
                    Corners::all(px(NODE_RADIUS)),
                    node_color,
                    Edges::default(),
                    Hsla::transparent_black(),
                    gpui::BorderStyle::default(),
                ));
            },
        )
        .w(px(cell_width))
        .flex_shrink_0()
    }
}

impl RenderOnce for GitLogView {
    fn render(self, window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let viewport_width = f32::from(window.viewport_size().width);
        let viewport_height = f32::from(window.viewport_size().height);

        let has_detail = self.detail.is_some();
        let show_detail = self.detail_visible && has_detail && viewport_width > 900.0;
        let is_empty = self.commits.is_empty();
        let commit_count = self.commits.len();
        let selected = self.selected;
        let is_searching = !self.search_query.is_empty();

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
            let has_graph = !self.graph.is_empty();

            let mut list = div()
                .id("git-log-list")
                .flex()
                .flex_col()
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.scroll_handle);

            for (i, commit) in self.commits.iter().enumerate() {
                let is_selected = i == selected;
                let is_match = is_searching && self.search_matches.contains(&i);
                let dim = is_searching && !is_match;

                let date_str = format_relative_time(commit.timestamp);

                let mut row = div()
                    .flex()
                    .px(px(8.0))
                    .when(is_selected, |d| d.bg(rgb(0x3b4261)))
                    .when(dim, |d| d.opacity(0.35));

                if has_graph && i < self.graph.len() {
                    row = row.child(self.render_graph_cell(i));
                }

                let text_row = div()
                    .flex()
                    .gap_2()
                    .py(px(3.0))
                    .flex_1()
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

                row = row.child(text_row);

                list = list.child(row);
            }

            if self.loading {
                list = list.child(
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .text_color(rgb(0x808080))
                        .text_size(px(11.0))
                        .child("Loading..."),
                );
            }

            list
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
            .child(key_hint("enter", "details"))
            .child(key_hint("h/l", "detail files"))
            .child(key_hint("/", "search"))
            .child(key_hint("q/esc", "close"));

        let detail_panel = if show_detail {
            if let Some(ref detail) = self.detail {
                let commit = &self.commits[selected];
                let date_str = format_relative_time(commit.timestamp);

                let modal_width = viewport_width * 0.75;
                let mut detail_div = div()
                    .flex()
                    .flex_col()
                    .w(px(modal_width * 0.4))
                    .border_l_1()
                    .border_color(rgb(0x3e3e42))
                    .overflow_hidden();

                // Commit metadata
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

                // File list
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

                // Diff preview
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
