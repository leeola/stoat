use crate::stoat::{RenamePendingState, Stoat, StoatEvent};
use gpui::Context;
use stoat_lsp::{buffer_helpers::BufferSnapshotExt, point_to_lsp_position};

impl Stoat {
    /// Enter rename mode: flash the current word and await new name input.
    pub fn lsp_rename_symbol(&mut self, cx: &mut Context<Self>) {
        if self.lsp_manager.is_none() {
            self.flash("No LSP server running", cx);
            return;
        }
        if self.current_file_path.is_none() {
            self.flash("No file open", cx);
            return;
        }

        let word = self.word_under_cursor(cx);
        if word.is_empty() {
            self.flash("No symbol under cursor", cx);
            return;
        }

        self.rename_pending = Some(RenamePendingState {
            old_name: word.clone(),
            new_name: String::new(),
        });
        self.flash(format!("Rename '{word}' -> "), cx);
        cx.notify();
    }

    /// Handle a keystroke while rename is pending.
    /// Returns true if the key was consumed.
    pub fn handle_rename_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(state) = &mut self.rename_pending else {
            return false;
        };

        match key {
            "escape" => {
                self.rename_pending = None;
                self.flash("Rename cancelled", cx);
                cx.notify();
                true
            },
            "enter" => {
                let old = state.old_name.clone();
                let new = state.new_name.clone();
                self.rename_pending = None;

                if new.is_empty() {
                    self.flash("Rename cancelled (empty name)", cx);
                    cx.notify();
                    return true;
                }

                self.execute_rename(&old, &new, cx);
                true
            },
            "backspace" => {
                state.new_name.pop();
                let display = format!("Rename '{}' -> {}", state.old_name, state.new_name);
                cx.emit(StoatEvent::FlashMessage(display));
                cx.notify();
                true
            },
            ch if ch.len() == 1 => {
                state.new_name.push_str(ch);
                let display = format!("Rename '{}' -> {}", state.old_name, state.new_name);
                cx.emit(StoatEvent::FlashMessage(display));
                cx.notify();
                true
            },
            _ => false,
        }
    }

    fn execute_rename(&mut self, _old_name: &str, new_name: &str, cx: &mut Context<Self>) {
        let Some(lsp) = self.lsp_manager.clone() else {
            return;
        };
        let Some(file_path) = self.current_file_path.clone() else {
            return;
        };

        let worktree_root = self.worktree.lock().root().to_path_buf();
        let abs_path = worktree_root.join(&file_path);
        let uri_str = format!("file://{}", abs_path.display());
        let Ok(uri) = uri_str.parse::<lsp_types::Uri>() else {
            return;
        };

        let snapshot = self.active_buffer(cx).read(cx).buffer_snapshot(cx);
        let position = point_to_lsp_position(self.cursor.position(), &snapshot);
        let server_ids = lsp.active_servers();
        let new_name = new_name.to_string();

        cx.spawn(async move |this, cx| {
            for server_id in server_ids {
                let Ok(handle) = lsp.rename(server_id, uri.clone(), position, &new_name) else {
                    continue;
                };
                let Ok(response) = handle.await else {
                    continue;
                };

                if let Ok(Some(edit)) = stoat_lsp::response::parse_rename_response(&response) {
                    this.update(cx, |s, cx| match s.apply_workspace_edit(&edit, cx) {
                        Ok(n) => s.flash(format!("Renamed: {n} edit(s)"), cx),
                        Err(e) => s.flash(format!("Rename failed: {e}"), cx),
                    })
                    .ok();
                    return;
                }
            }

            this.update(cx, |_, cx| {
                cx.emit(StoatEvent::FlashMessage(
                    "Rename failed: no response".into(),
                ));
            })
            .ok();
        })
        .detach();
    }

    fn word_under_cursor(&self, cx: &gpui::App) -> String {
        let buffer_item = self.active_buffer(cx);
        let snapshot = buffer_item.read(cx).buffer_snapshot(cx);
        let pos = self.cursor.position();
        let line = snapshot.line(pos.row);
        let line_str: &str = line.as_ref();
        let col = pos.column as usize;

        if col >= line_str.len() {
            return String::new();
        }

        let start = line_str[..col]
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = line_str[col..]
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|i| col + i)
            .unwrap_or(line_str.len());

        line_str[start..end].to_string()
    }
}
