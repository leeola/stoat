use crate::stoat::{Stoat, StoatEvent};
use gpui::Context;
use stoat_lsp::point_to_lsp_position;

impl Stoat {
    pub fn lsp_goto_definition(&mut self, cx: &mut Context<Self>) {
        self.lsp_goto("textDocument/definition", "No definition found", cx);
    }

    pub fn lsp_goto_type_definition(&mut self, cx: &mut Context<Self>) {
        self.lsp_goto(
            "textDocument/typeDefinition",
            "No type definition found",
            cx,
        );
    }

    pub fn lsp_goto_implementation(&mut self, cx: &mut Context<Self>) {
        self.lsp_goto("textDocument/implementation", "No implementation found", cx);
    }

    fn lsp_goto(&mut self, method: &str, not_found_msg: &str, cx: &mut Context<Self>) {
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

        let method = method.to_string();
        let not_found = not_found_msg.to_string();
        let server_ids = lsp.active_servers();

        cx.spawn(async move |this, cx| {
            for server_id in server_ids {
                let request = lsp.request(
                    server_id,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": method,
                        "params": {
                            "textDocument": { "uri": uri.as_str() },
                            "position": { "line": position.line, "character": position.character }
                        }
                    }),
                );

                let Ok(handle) = request else { continue };
                let Ok(response) = handle.await else { continue };

                if let Ok(locations) = stoat_lsp::response::parse_goto_response(&response) {
                    if let Some(loc) = locations.first() {
                        let loc = loc.clone();
                        this.update(cx, |s, cx| {
                            s.navigate_to_lsp_location(&loc, cx);
                        })
                        .ok();
                        return;
                    }
                }
            }

            this.update(cx, |_, cx| {
                cx.emit(StoatEvent::FlashMessage(not_found));
            })
            .ok();
        })
        .detach();
    }
}
