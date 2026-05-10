use gpui::{
    div, px, size, App, AppContext, Application, Bounds, Context, IntoElement, Render,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions,
};

pub fn run() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Stoat")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|_cx| EmptyRoot),
        )
        .expect("open root window");
        cx.activate(true);
    });
}

struct EmptyRoot;

impl Render for EmptyRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
    }
}
