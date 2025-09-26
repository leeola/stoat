use super::element::EditorElement;
use gpui::{Context, IntoElement, Render, Window};
use stoat::Stoat;

pub struct EditorView {
    stoat: Stoat,
}

impl EditorView {
    pub fn new(stoat: Stoat) -> Self {
        Self { stoat }
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Create a fresh element each time, following Zed's pattern
        EditorElement::new(self.stoat.clone())
    }
}
