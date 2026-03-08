use crate::keymap::infobox::Infobox;
use gpui::{div, App, Hsla, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window};

#[derive(IntoElement)]
pub struct InfoboxView {
    infobox: Infobox,
    expanded: bool,
}

impl InfoboxView {
    pub fn new(infobox: Infobox, expanded: bool) -> Self {
        Self { infobox, expanded }
    }
}

impl RenderOnce for InfoboxView {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let bg_color: Hsla = gpui::rgb(0x1E1E1E).into();
        let border_color: Hsla = gpui::rgb(0x404040).into();
        let text_muted: Hsla = gpui::rgb(0xA0A0A0).into();
        let key_color: Hsla = gpui::rgb(0x569CD6).into();

        let max_entries = if self.expanded { usize::MAX } else { 10 };

        let entries: Vec<_> = self
            .infobox
            .entries
            .iter()
            .take(max_entries)
            .map(|entry| {
                let keys_str: SharedString = entry.keys.join(", ").into();
                let desc: SharedString = entry.description.clone().into();

                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .min_w(if self.expanded {
                                gpui::px(100.0)
                            } else {
                                gpui::px(60.0)
                            })
                            .text_color(key_color)
                            .font_family(".SystemUIFont")
                            .child(keys_str),
                    )
                    .child(
                        div()
                            .text_color(text_muted)
                            .font_family(".SystemUIFont")
                            .child(desc),
                    )
            })
            .collect();

        let (text_size, padding, gap) = if self.expanded {
            (gpui::px(13.0), gpui::px(16.0), gpui::px(4.0))
        } else {
            (gpui::px(11.0), gpui::px(8.0), gpui::px(2.0))
        };

        let title: SharedString = self.infobox.title.into();

        div()
            .absolute()
            .bottom_2()
            .right_2()
            .p(padding)
            .rounded_md()
            .bg(bg_color.opacity(0.95))
            .border_1()
            .border_color(border_color)
            .shadow_lg()
            .text_size(text_size)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(gap)
                    .child(div().text_color(text_muted).text_xs().mb_1().child(title))
                    .children(entries),
            )
    }
}
