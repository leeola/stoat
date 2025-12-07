//! Main workspace view.

use gpui::{div, prelude::*, rgb, Context, Styled, Window};

pub struct Workspace;

impl Workspace {
    pub fn new() -> Self {
        Self
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .flex()
            .items_center()
            .justify_center()
            .child(div().text_color(rgb(0xcdd6f4)).text_xl().child("Stoat"))
    }
}
