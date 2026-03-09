use crate::{
    stoat::{Stoat, StoatEvent},
    syntax::SyntaxTheme,
};
use gpui::Context;
use stoat_lsp::point_to_lsp_position;

impl Stoat {
    pub fn lsp_hover(&mut self, cx: &mut Context<Self>) {
        let Some(lsp) = self.lsp_manager.clone() else {
            self.flash("No LSP server running", cx);
            return;
        };

        let Some(file_path) = self.current_file_path.clone() else {
            self.flash("No file open", cx);
            return;
        };

        let worktree_root = self.worktree.lock().root().to_path_buf();
        let abs_path = worktree_root.join(&file_path);
        let uri_str = format!("file://{}", abs_path.display());
        let Ok(uri) = uri_str.parse::<lsp_types::Uri>() else {
            self.flash("Invalid file path", cx);
            return;
        };

        let snapshot = self.active_buffer(cx).read(cx).buffer_snapshot(cx);
        let position = point_to_lsp_position(self.cursor.position(), &snapshot);
        let server_ids = lsp.active_servers();

        cx.spawn(async move |this, cx| {
            for server_id in server_ids {
                let Ok(handle) = lsp.hover(server_id, uri.clone(), position) else {
                    continue;
                };
                let Ok(response) = handle.await else {
                    continue;
                };

                if let Ok(Some(blocks)) = stoat_lsp::response::parse_hover_blocks(&response) {
                    let summary = truncate_hover(&blocks[0].text, 200);
                    this.update(cx, |stoat, cx| {
                        let theme = SyntaxTheme::monokai_dark();
                        stoat.hover_state.set_blocks(blocks, &theme);
                        cx.emit(StoatEvent::FlashMessage(summary));
                    })
                    .ok();
                    return;
                }
            }

            this.update(cx, |_, cx| {
                cx.emit(StoatEvent::FlashMessage("No hover info".into()));
            })
            .ok();
        })
        .detach();
    }
}

fn truncate_hover(text: &str, max_chars: usize) -> String {
    let first_line = text.lines().next().unwrap_or(text);
    if first_line.len() <= max_chars {
        first_line.to_string()
    } else {
        format!("{}...", &first_line[..max_chars])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short() {
        assert_eq!(truncate_hover("fn main()", 200), "fn main()");
    }

    #[test]
    fn truncate_multiline() {
        assert_eq!(truncate_hover("line1\nline2\nline3", 200), "line1");
    }

    #[test]
    fn truncate_long() {
        let long = "a".repeat(300);
        let result = truncate_hover(&long, 200);
        assert_eq!(result.len(), 203);
        assert!(result.ends_with("..."));
    }
}
