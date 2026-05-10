use gpui::{div, Context, FocusHandle, IntoElement, Render, Styled, Window};

pub(crate) struct StoatApp {
    #[allow(dead_code)]
    focus_handle: FocusHandle,
}

impl StoatApp {
    pub(crate) fn new(cx: &mut Context<'_, Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for StoatApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full()
    }
}
