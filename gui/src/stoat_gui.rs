#![deny(clippy::disallowed_types, clippy::disallowed_methods)]

mod buffer;
mod buffer_registry;
mod diagnostics;
mod diff_map;
mod display_map;
mod executor;
mod globals;
mod keymap_compiler;
mod lsp_state;
mod multi_buffer;
mod panic_hook;
mod settings;
mod stoat_app;
#[cfg(any(test, feature = "test-support"))]
pub mod test;
mod theme;

pub use buffer::{Buffer, BufferEvent};
pub use buffer_registry::{BufferRegistry, BufferRegistryEvent};
pub use diagnostics::{DiagnosticSet, DiagnosticSetEvent};
pub use diff_map::{DiffMap, DiffMapEvent};
pub use display_map::{DisplayMap, DisplayMapEvent};
pub use executor::spawn_with_entity;
pub use globals::{install_production_globals, Globals, LanguageRegistry};
use gpui::{
    px, size, App, AppContext, Application, Bounds, SharedString, TitlebarOptions, WindowBounds,
    WindowOptions,
};
pub use keymap_compiler::{compile_predicate, CompilePredicateError};
pub use lsp_state::{LspState, LspStateEvent};
pub use multi_buffer::{MultiBuffer, MultiBufferEvent};
pub use panic_hook::install_panic_hook;
pub use settings::Settings;
use stoat_app::StoatApp;
pub use theme::Theme;

pub fn run() {
    Application::new().run(|cx: &mut App| {
        tracing::info!("stoat gui starting");
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
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
