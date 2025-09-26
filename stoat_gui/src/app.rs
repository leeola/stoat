use crate::{actions::*, editor::view::EditorView};
use gpui::{
    App, Application, Bounds, Focusable, KeyBinding, WindowBounds, WindowOptions, prelude::*, px,
    size,
};
use stoat::Stoat;

pub fn run_with_stoat(stoat: Option<Stoat>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let stoat = stoat.unwrap_or_else(|| Stoat::new(cx));

        // Register modal keybindings
        cx.bind_keys([
            KeyBinding::new("i", ModalKeyI, Some("EditorView")),
            KeyBinding::new("escape", ModalKeyEscape, Some("EditorView")),
            // Add basic alphanumeric keys for insert mode
            KeyBinding::new("a", ModalKeyA, Some("EditorView")),
            KeyBinding::new("b", ModalKeyB, Some("EditorView")),
            KeyBinding::new("c", ModalKeyC, Some("EditorView")),
            KeyBinding::new("d", ModalKeyD, Some("EditorView")),
            KeyBinding::new("e", ModalKeyE, Some("EditorView")),
            KeyBinding::new("f", ModalKeyF, Some("EditorView")),
            KeyBinding::new("g", ModalKeyG, Some("EditorView")),
            KeyBinding::new("h", ModalKeyH, Some("EditorView")),
            KeyBinding::new("j", ModalKeyJ, Some("EditorView")),
            KeyBinding::new("k", ModalKeyK, Some("EditorView")),
            KeyBinding::new("l", ModalKeyL, Some("EditorView")),
            KeyBinding::new("m", ModalKeyM, Some("EditorView")),
            KeyBinding::new("n", ModalKeyN, Some("EditorView")),
            KeyBinding::new("o", ModalKeyO, Some("EditorView")),
            KeyBinding::new("p", ModalKeyP, Some("EditorView")),
            KeyBinding::new("q", ModalKeyQ, Some("EditorView")),
            KeyBinding::new("r", ModalKeyR, Some("EditorView")),
            KeyBinding::new("s", ModalKeyS, Some("EditorView")),
            KeyBinding::new("t", ModalKeyT, Some("EditorView")),
            KeyBinding::new("u", ModalKeyU, Some("EditorView")),
            KeyBinding::new("v", ModalKeyV, Some("EditorView")),
            KeyBinding::new("w", ModalKeyW, Some("EditorView")),
            KeyBinding::new("x", ModalKeyX, Some("EditorView")),
            KeyBinding::new("y", ModalKeyY, Some("EditorView")),
            KeyBinding::new("z", ModalKeyZ, Some("EditorView")),
            KeyBinding::new("space", ModalKeySpace, Some("EditorView")),
        ]);

        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| EditorView::new(stoat, cx)),
            )
            .unwrap();

        // Focus the editor after window creation
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.focus_handle(cx));
            })
            .unwrap();

        cx.on_window_closed(|cx| {
            cx.quit();
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}

pub fn run_with_paths(paths: Vec<std::path::PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let mut stoat = Stoat::new(cx);

        // Load files if provided
        if !paths.is_empty() {
            let path_refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_ref()).collect();
            stoat.load_files(&path_refs, cx);
        }

        // Register modal keybindings
        cx.bind_keys([
            KeyBinding::new("i", ModalKeyI, Some("EditorView")),
            KeyBinding::new("escape", ModalKeyEscape, Some("EditorView")),
            // Add basic alphanumeric keys for insert mode
            KeyBinding::new("a", ModalKeyA, Some("EditorView")),
            KeyBinding::new("b", ModalKeyB, Some("EditorView")),
            KeyBinding::new("c", ModalKeyC, Some("EditorView")),
            KeyBinding::new("d", ModalKeyD, Some("EditorView")),
            KeyBinding::new("e", ModalKeyE, Some("EditorView")),
            KeyBinding::new("f", ModalKeyF, Some("EditorView")),
            KeyBinding::new("g", ModalKeyG, Some("EditorView")),
            KeyBinding::new("h", ModalKeyH, Some("EditorView")),
            KeyBinding::new("j", ModalKeyJ, Some("EditorView")),
            KeyBinding::new("k", ModalKeyK, Some("EditorView")),
            KeyBinding::new("l", ModalKeyL, Some("EditorView")),
            KeyBinding::new("m", ModalKeyM, Some("EditorView")),
            KeyBinding::new("n", ModalKeyN, Some("EditorView")),
            KeyBinding::new("o", ModalKeyO, Some("EditorView")),
            KeyBinding::new("p", ModalKeyP, Some("EditorView")),
            KeyBinding::new("q", ModalKeyQ, Some("EditorView")),
            KeyBinding::new("r", ModalKeyR, Some("EditorView")),
            KeyBinding::new("s", ModalKeyS, Some("EditorView")),
            KeyBinding::new("t", ModalKeyT, Some("EditorView")),
            KeyBinding::new("u", ModalKeyU, Some("EditorView")),
            KeyBinding::new("v", ModalKeyV, Some("EditorView")),
            KeyBinding::new("w", ModalKeyW, Some("EditorView")),
            KeyBinding::new("x", ModalKeyX, Some("EditorView")),
            KeyBinding::new("y", ModalKeyY, Some("EditorView")),
            KeyBinding::new("z", ModalKeyZ, Some("EditorView")),
            KeyBinding::new("space", ModalKeySpace, Some("EditorView")),
        ]);

        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| EditorView::new(stoat, cx)),
            )
            .unwrap();

        // Focus the editor after window creation
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.focus_handle(cx));
            })
            .unwrap();

        cx.on_window_closed(|cx| {
            cx.quit();
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}
