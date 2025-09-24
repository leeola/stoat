use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, IntoElement, Render,
    SharedString, Window, WindowBounds, WindowOptions,
};
use stoat::Stoat;

pub fn run_with_stoat(stoat: Option<Stoat>) -> Result<(), Box<dyn std::error::Error>> {
    let stoat = stoat.unwrap_or_else(Stoat::new);
    let content = SharedString::from(stoat.buffer_contents().to_string());

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| EditorView {
                    content: content.clone(),
                })
            },
        )
        .unwrap();

        cx.on_window_closed(|cx| {
            cx.quit();
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}

struct EditorView {
    content: SharedString,
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xcccccc))
            .size_full()
            .p(px(20.0))
            .font_family("monospace")
            .text_size(px(14.0))
            .child(if self.content.is_empty() {
                SharedString::from("Empty buffer - ready for input")
            } else {
                self.content.clone()
            })
    }
}
