mod stoat_app;

use gpui::{
    px, size, App, AppContext, Application, Bounds, SharedString, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use stoat_app::StoatApp;

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
            |_window, cx| cx.new(StoatApp::new),
        )
        .expect("open root window");
        cx.activate(true);
    });
}
