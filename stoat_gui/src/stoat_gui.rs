use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, IntoElement, Render,
    SharedString, Window, WindowBounds, WindowOptions,
};
use stoat::Stoat;

pub fn run_with_stoat(stoat: Option<Stoat>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let stoat = stoat.unwrap_or_else(|| Stoat::new(cx));
        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| EditorView { stoat }),
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

pub fn run_with_paths(paths: Vec<std::path::PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    Application::new().run(move |cx: &mut App| {
        let mut stoat = Stoat::new(cx);

        // Load files if provided
        if !paths.is_empty() {
            let path_refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_ref()).collect();
            stoat.load_files(&path_refs, cx);
        }

        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| EditorView { stoat }),
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
    stoat: Stoat,
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);
        // let _token_snapshot = self.stoat.token_snapshot();

        // Minimal allocation approach - process line-by-line like Zed
        // Only allocate strings for individual lines, not the entire buffer
        let container = div()
            .bg(rgb(0x1e1e1e))
            .size_full()
            .p(px(20.0))
            .font_family("monospace")
            .text_size(px(14.0))
            .text_color(rgb(0xcccccc))
            .flex()
            .flex_col();

        // Check if buffer is empty
        if buffer_snapshot.len() == 0 {
            return container.child(SharedString::from("Empty buffer - ready for input"));
        }

        // Process chunks efficiently - build lines like Zed does
        // Only allocate strings per line, not for the entire buffer
        let chunks = buffer_snapshot.text_for_range(0..buffer_snapshot.len());
        let mut current_line = String::new();
        let mut lines = Vec::new();

        for chunk in chunks {
            // Process the chunk character by character looking for newlines
            let mut last_pos = 0;
            for (pos, ch) in chunk.char_indices() {
                if ch == '\n' {
                    // Found a newline - complete the current line
                    current_line.push_str(&chunk[last_pos..pos]);
                    lines.push(SharedString::from(current_line.clone()));
                    current_line.clear();
                    last_pos = pos + 1;
                }
            }
            // Add any remaining text in chunk to current line
            if last_pos < chunk.len() {
                current_line.push_str(&chunk[last_pos..]);
            }
        }

        // Add any remaining text as the last line
        if !current_line.is_empty() || lines.is_empty() {
            lines.push(SharedString::from(current_line));
        }

        // Render lines efficiently
        lines.into_iter().fold(container, |container, line| {
            container.child(div().child(line))
        })
    }
}

// TODO: Implement zero-allocation tokenized chunks iterator
// This will combine text chunks from the rope with syntax information from TokenMap
// For now, we're just rendering plain text until we properly implement the iterator
