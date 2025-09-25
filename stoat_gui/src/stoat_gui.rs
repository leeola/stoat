use gpui::{
    App, Application, Bounds, Context, Render, SharedString, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size,
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
        // Get buffer content
        let buffer_snapshot = self.stoat.buffer_snapshot(cx);

        // For now, just render the first 50 lines to avoid performance issues
        // Using text_for_range to avoid allocating the entire buffer
        let chunks = buffer_snapshot.text_for_range(0..buffer_snapshot.len().min(5000));
        let mut lines = Vec::new();
        let mut current_line = String::new();
        let max_lines = 50;

        for chunk in chunks {
            for ch in chunk.chars() {
                if ch == '\n' {
                    lines.push(SharedString::from(current_line.clone()));
                    current_line.clear();
                    if lines.len() >= max_lines {
                        break;
                    }
                } else {
                    current_line.push(ch);
                }
            }
            if lines.len() >= max_lines {
                break;
            }
        }

        // Add last line if any
        if !current_line.is_empty() && lines.len() < max_lines {
            lines.push(SharedString::from(current_line));
        }

        // Return a simple div with the lines
        div()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .p(px(20.0))
            .flex()
            .flex_col()
            .children(if lines.is_empty() {
                vec![div().child(SharedString::from("Empty buffer - ready for input"))]
            } else {
                lines
                    .into_iter()
                    .map(|line| {
                        div()
                            .child(if line.is_empty() {
                                SharedString::from(" ")
                            } else {
                                line
                            })
                            .text_color(rgb(0xcccccc))
                            .text_size(px(14.0))
                    })
                    .collect()
            })
    }
}

// TODO: Implement zero-allocation tokenized chunks iterator
// This will combine text chunks from the rope with syntax information from TokenMap
// For now, we're just rendering plain text until we properly implement the iterator
