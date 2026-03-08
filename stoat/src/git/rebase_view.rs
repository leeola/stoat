use crate::git::{
    diff_summary::DiffPreviewElement,
    rebase::{RebaseCommit, RebaseInProgress, RebaseOperation, RebasePhase},
    status::DiffPreviewData,
};
use gpui::{
    div, prelude::FluentBuilder, px, rgb, rgba, FontWeight, InteractiveElement, IntoElement,
    ParentElement, RenderOnce, ScrollHandle, StatefulInteractiveElement, Styled, Window,
};

#[derive(IntoElement)]
pub struct RebaseView {
    phase: RebasePhase,
    commits: Vec<RebaseCommit>,
    selected: usize,
    preview: Option<DiffPreviewData>,
    scroll_handle: ScrollHandle,
    in_progress: Option<RebaseInProgress>,
    base_ref: String,
}

impl RebaseView {
    pub fn new(
        phase: RebasePhase,
        commits: Vec<RebaseCommit>,
        selected: usize,
        preview: Option<DiffPreviewData>,
        scroll_handle: ScrollHandle,
        in_progress: Option<RebaseInProgress>,
        base_ref: String,
    ) -> Self {
        Self {
            phase,
            commits,
            selected,
            preview,
            scroll_handle,
            in_progress,
            base_ref,
        }
    }

    fn op_color(op: &RebaseOperation) -> gpui::Rgba {
        match op {
            RebaseOperation::Pick => rgb(0x6a9955),
            RebaseOperation::Reword => rgb(0x569cd6),
            RebaseOperation::Edit => rgb(0x4ec9b0),
            RebaseOperation::Squash => rgb(0xdcdcaa),
            RebaseOperation::Fixup => rgb(0xce9178),
            RebaseOperation::Drop => rgb(0xf14c4c),
        }
    }
}

impl RenderOnce for RebaseView {
    fn render(self, window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let viewport_width = f32::from(window.viewport_size().width);
        let viewport_height = f32::from(window.viewport_size().height);

        match self.phase {
            RebasePhase::Planning => self
                .render_planning(viewport_width, viewport_height)
                .into_any_element(),
            _ => self
                .render_progress(viewport_width, viewport_height)
                .into_any_element(),
        }
    }
}

impl RebaseView {
    fn render_planning(self, viewport_width: f32, viewport_height: f32) -> impl IntoElement {
        let show_preview = viewport_width > 1000.0 && self.preview.is_some();
        let is_empty = self.commits.is_empty();
        let commit_count = self.commits.len();
        let selected = self.selected;
        let base_ref_display = self.base_ref.clone();

        let title = format!("Interactive Rebase ({commit_count} commits onto {base_ref_display})");

        let header = div()
            .p(px(8.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .font_weight(FontWeight::SEMIBOLD)
            .child(title);

        let commit_list = if is_empty {
            div()
                .id("rebase-list")
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(rgb(0x808080))
                        .text_size(px(13.0))
                        .child("No commits to rebase"),
                )
        } else {
            div()
                .id("rebase-list")
                .flex()
                .flex_col()
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.scroll_handle)
                .children(self.commits.iter().enumerate().map(|(i, commit)| {
                    let op_color = Self::op_color(&commit.operation);
                    let op_label = format!("{:7}", commit.operation.as_str());

                    div()
                        .flex()
                        .gap_2()
                        .px(px(8.0))
                        .py(px(3.0))
                        .when(i == selected, |d| d.bg(rgb(0x3b4261)))
                        .child(
                            div()
                                .text_color(op_color)
                                .text_size(px(11.0))
                                .font_weight(FontWeight::BOLD)
                                .w(px(56.0))
                                .child(op_label),
                        )
                        .child(
                            div()
                                .text_color(rgb(0xce9178))
                                .text_size(px(11.0))
                                .w(px(60.0))
                                .child(commit.short_hash.clone()),
                        )
                        .child(
                            div()
                                .text_color(rgb(0xd4d4d4))
                                .text_size(px(11.0))
                                .flex_1()
                                .child(commit.message.clone()),
                        )
                        .child(
                            div()
                                .text_color(rgb(0x808080))
                                .text_size(px(10.0))
                                .child(format!("{} {}", commit.author, commit.date)),
                        )
                }))
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
            .child(key_hint("p", "pick"))
            .child(key_hint("s", "squash"))
            .child(key_hint("f", "fixup"))
            .child(key_hint("e", "edit"))
            .child(key_hint("d", "drop"))
            .child(key_hint("w", "reword"))
            .child(key_hint("J/K", "move"))
            .child(key_hint("enter", "execute"))
            .child(key_hint("esc", "cancel"));

        let preview_elem = DiffPreviewElement::new(self.preview);

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
                    .when(is_empty, |d| d.w(px(500.0)).h(px(200.0)))
                    .when(!is_empty, |d| d.w_3_4().h(px(viewport_height * 0.85)))
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(header)
                    .child(if show_preview {
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .w(px(viewport_width * 0.75 * 0.45))
                                    .border_r_1()
                                    .border_color(rgb(0x3e3e42))
                                    .child(commit_list),
                            )
                            .child(div().flex().flex_col().flex_1().child(preview_elem))
                    } else {
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .overflow_hidden()
                            .child(div().flex().flex_col().flex_1().child(commit_list))
                    })
                    .child(footer),
            )
    }

    fn render_progress(self, _viewport_width: f32, viewport_height: f32) -> impl IntoElement {
        let (step, total) = match &self.phase {
            RebasePhase::PausedConflict { step, total } => (*step, *total),
            RebasePhase::PausedEdit { step, total } => (*step, *total),
            RebasePhase::PausedReword { step, total } => (*step, *total),
            RebasePhase::Planning => (0, 0),
        };

        let head_name = self
            .in_progress
            .as_ref()
            .map(|ip| ip.head_name.clone())
            .unwrap_or_default();

        let status_msg = match &self.phase {
            RebasePhase::PausedConflict { .. } => {
                "Conflicts detected \u{2014} resolve and press c to continue"
            },
            RebasePhase::PausedEdit { .. } => {
                "Stopped for edit \u{2014} make changes and press c to continue"
            },
            RebasePhase::PausedReword { .. } => {
                "Editing commit message \u{2014} press m to open message, c to continue"
            },
            RebasePhase::Planning => "",
        };

        let status_color = match &self.phase {
            RebasePhase::PausedConflict { .. } => rgb(0xf14c4c),
            _ => rgb(0xdcdcaa),
        };

        let header = div()
            .p(px(8.0))
            .border_b_1()
            .border_color(rgb(0x3e3e42))
            .bg(rgb(0x252526))
            .text_color(rgb(0xd4d4d4))
            .font_weight(FontWeight::SEMIBOLD)
            .child(format!("Rebase in Progress \u{2014} Step {step}/{total}"));

        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .p(px(16.0))
            .gap_4()
            .child(
                div()
                    .text_color(rgb(0x808080))
                    .text_size(px(12.0))
                    .child(format!("Branch: {head_name}")),
            )
            .child(
                div()
                    .text_color(status_color)
                    .text_size(px(13.0))
                    .child(status_msg),
            );

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
            .child(key_hint("c", "continue"))
            .child(key_hint("a", "abort"))
            .child(key_hint("s", "skip"))
            .child(key_hint("m", "edit msg"))
            .child(key_hint("esc", "dismiss"));

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
                    .w(px(500.0))
                    .h(px(viewport_height.min(300.0)))
                    .bg(rgb(0x1e1e1e))
                    .border_1()
                    .border_color(rgb(0x3e3e42))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .child(header)
                    .child(body)
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
