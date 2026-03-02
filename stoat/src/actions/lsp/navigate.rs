use crate::{buffer::item::BufferItem, stoat::Stoat};
use anyhow::Result;
use gpui::{Context, Entity};
use lsp_types::{DocumentChanges, Location, TextEdit, Uri, WorkspaceEdit};
use std::path::PathBuf;
use stoat_lsp::lsp_position_to_point;

impl Stoat {
    /// Navigate to an LSP location (same or different file).
    pub fn navigate_to_lsp_location(&mut self, location: &Location, cx: &mut Context<Self>) {
        let target_path = match uri_to_path(&location.uri) {
            Some(p) => p,
            None => {
                self.flash(format!("Cannot open: {}", location.uri.as_str()), cx);
                return;
            },
        };

        let current_path = self.current_file_path.clone();
        let worktree_root = self.worktree.lock().root().to_path_buf();
        let abs_target = if target_path.is_absolute() {
            target_path
        } else {
            worktree_root.join(&target_path)
        };

        let need_load = match &current_path {
            Some(cur) => {
                let abs_current = worktree_root.join(cur);
                abs_current != abs_target
            },
            None => true,
        };

        if need_load {
            if let Err(e) = self.load_file(&abs_target, cx) {
                self.flash(format!("Failed to open: {e}"), cx);
                return;
            }
        }

        let buffer_item = self.active_buffer(cx);
        let snapshot = buffer_item.read(cx).buffer_snapshot(cx);
        let position = &location.range.start;
        if let Ok(point) = lsp_position_to_point(position, &snapshot) {
            self.jump_to_point(point, cx);
        }
    }

    /// Apply a workspace edit returned by LSP (rename, code action).
    pub fn apply_workspace_edit(
        &mut self,
        edit: &WorkspaceEdit,
        cx: &mut Context<Self>,
    ) -> Result<usize> {
        let mut total_edits = 0;

        if let Some(changes) = &edit.changes {
            total_edits += self.apply_changes(changes, cx)?;
        }

        if let Some(document_changes) = &edit.document_changes {
            total_edits += self.apply_document_changes(document_changes, cx)?;
        }

        Ok(total_edits)
    }

    fn apply_changes(
        &mut self,
        changes: &std::collections::HashMap<Uri, Vec<TextEdit>>,
        cx: &mut Context<Self>,
    ) -> Result<usize> {
        let mut total_edits = 0;
        let worktree_root = self.worktree.lock().root().to_path_buf();

        for (uri, text_edits) in changes {
            let Some(path) = uri_to_path(uri) else {
                continue;
            };
            let abs_path = if path.is_absolute() {
                path
            } else {
                worktree_root.join(&path)
            };

            let current_abs = self
                .current_file_path
                .as_ref()
                .map(|p| worktree_root.join(p));

            let is_current = current_abs.as_ref() == Some(&abs_path);

            if is_current {
                let buffer_item = self.active_buffer(cx);
                apply_text_edits_to_buffer(&buffer_item, text_edits, cx);
                total_edits += text_edits.len();
            } else {
                let buffer_item = self
                    .ensure_buffer_loaded(&abs_path, cx)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                apply_text_edits_to_buffer(&buffer_item, text_edits, cx);
                total_edits += text_edits.len();
                crate::actions::write_file::write_buffer_to_disk(&buffer_item, &abs_path, cx)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
        }

        Ok(total_edits)
    }

    fn apply_document_changes(
        &mut self,
        document_changes: &DocumentChanges,
        cx: &mut Context<Self>,
    ) -> Result<usize> {
        let text_document_edits = match document_changes {
            DocumentChanges::Edits(edits) => edits.clone(),
            DocumentChanges::Operations(ops) => ops
                .iter()
                .filter_map(|op| match op {
                    lsp_types::DocumentChangeOperation::Edit(edit) => Some(edit.clone()),
                    lsp_types::DocumentChangeOperation::Op(_) => None,
                })
                .collect(),
        };

        let mut total_edits = 0;
        let worktree_root = self.worktree.lock().root().to_path_buf();

        for doc_edit in &text_document_edits {
            let uri = &doc_edit.text_document.uri;
            let text_edits: Vec<TextEdit> = doc_edit
                .edits
                .iter()
                .map(|e| match e {
                    lsp_types::OneOf::Left(edit) => edit.clone(),
                    lsp_types::OneOf::Right(annotated) => annotated.text_edit.clone(),
                })
                .collect();

            let Some(path) = uri_to_path(uri) else {
                continue;
            };
            let abs_path = if path.is_absolute() {
                path
            } else {
                worktree_root.join(&path)
            };

            let current_abs = self
                .current_file_path
                .as_ref()
                .map(|p| worktree_root.join(p));

            let is_current = current_abs.as_ref() == Some(&abs_path);

            if is_current {
                let buffer_item = self.active_buffer(cx);
                apply_text_edits_to_buffer(&buffer_item, &text_edits, cx);
                total_edits += text_edits.len();
            } else {
                let buffer_item = self
                    .ensure_buffer_loaded(&abs_path, cx)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                apply_text_edits_to_buffer(&buffer_item, &text_edits, cx);
                total_edits += text_edits.len();
                crate::actions::write_file::write_buffer_to_disk(&buffer_item, &abs_path, cx)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
        }

        Ok(total_edits)
    }

    /// Load a file into a buffer without making it the active buffer.
    ///
    /// Subset of [`load_file`](Self::load_file): reads file, creates/reuses buffer
    /// via BufferStore, stores strong ref. Does NOT change active_buffer_id,
    /// reset cursor/selections, emit FileOpened, or send LSP didOpen.
    pub(crate) fn ensure_buffer_loaded(
        &mut self,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Result<Entity<BufferItem>, String> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {e}"))?;

        let language = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(stoat_text::Language::from_extension)
            .unwrap_or(stoat_text::Language::PlainText);

        let path_buf = path.to_path_buf();

        let (buffer_id, buffer_item_entity) = self
            .buffer_store
            .update(cx, |store, cx| {
                if let Some(buffer_item) = store.get_buffer_by_path(&path_buf) {
                    let buffer_id = buffer_item.read(cx).buffer().read(cx).remote_id();
                    Some((buffer_id, buffer_item))
                } else {
                    store.open_buffer(Some(path_buf.clone()), language, cx)
                }
            })
            .ok_or_else(|| "Failed to create buffer".to_string())?;

        let line_ending = text::LineEnding::detect(&contents);

        self.replace_buffer_content(&contents, &buffer_item_entity, cx);
        buffer_item_entity.update(cx, |item, _cx| {
            item.set_saved_text(contents);
            item.set_line_ending(line_ending);
        });

        if !self
            .open_buffers
            .iter()
            .any(|item| item.read(cx).buffer().read(cx).remote_id() == buffer_id)
        {
            self.open_buffers.push(buffer_item_entity.clone());
        }

        Ok(buffer_item_entity)
    }
}

pub(crate) fn apply_text_edits_to_buffer(
    buffer_item: &Entity<BufferItem>,
    edits: &[TextEdit],
    cx: &mut Context<Stoat>,
) {
    let buffer = buffer_item.read(cx).buffer().clone();
    let snapshot = buffer.read(cx).snapshot();

    let mut offset_edits: Vec<(usize, usize, String)> = edits
        .iter()
        .filter_map(|edit| {
            let start = lsp_position_to_point(&edit.range.start, &snapshot).ok()?;
            let end = lsp_position_to_point(&edit.range.end, &snapshot).ok()?;
            let start_off = snapshot.point_to_offset(start);
            let end_off = snapshot.point_to_offset(end);
            Some((start_off, end_off, edit.new_text.clone()))
        })
        .collect();

    // Apply in reverse order to preserve earlier positions
    offset_edits.sort_by(|a, b| b.0.cmp(&a.0));

    buffer.update(cx, |buf, _| {
        for (start, end, new_text) in &offset_edits {
            buf.edit([(*start..*end, new_text.as_str())]);
        }
    });

    cx.notify();
}

pub(crate) fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    if let Some(path) = s.strip_prefix("file://") {
        Some(PathBuf::from(path))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_to_path_file_scheme() {
        let uri: Uri = "file:///home/user/foo.rs".parse().unwrap();
        assert_eq!(uri_to_path(&uri), Some(PathBuf::from("/home/user/foo.rs")));
    }

    #[test]
    fn uri_to_path_non_file() {
        let uri: Uri = "https://example.com".parse().unwrap();
        assert_eq!(uri_to_path(&uri), None);
    }
}
