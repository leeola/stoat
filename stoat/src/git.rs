use std::{collections::HashMap, ops::Range, sync::Arc};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffStatus {
    #[default]
    Unchanged,
    Added,
    Modified,
}

#[derive(Clone, Debug)]
pub struct DeletedHunk {
    pub after_buffer_line: u32,
    pub base_byte_range: Range<usize>,
    pub line_count: u32,
}

#[derive(Clone, Debug, Default)]
pub struct BufferDiff {
    line_status: HashMap<u32, DiffStatus>,
    base_text: Option<Arc<String>>,
    deleted_hunks: Vec<DeletedHunk>,
}

impl BufferDiff {
    pub fn status_for_line(&self, line: u32) -> DiffStatus {
        self.line_status.get(&line).copied().unwrap_or_default()
    }

    pub fn has_deletion_after(&self, line: u32) -> bool {
        self.deleted_hunks
            .iter()
            .any(|h| h.after_buffer_line == line)
    }

    pub fn deleted_hunks(&self) -> &[DeletedHunk] {
        &self.deleted_hunks
    }

    pub fn base_text(&self) -> Option<&Arc<String>> {
        self.base_text.as_ref()
    }

    pub fn deleted_content(&self, hunk: &DeletedHunk) -> &str {
        self.base_text
            .as_ref()
            .map(|t| &t[hunk.base_byte_range.clone()])
            .unwrap_or("")
    }

    pub fn total_deleted_lines(&self) -> u32 {
        self.deleted_hunks.iter().map(|h| h.line_count).sum()
    }

    #[cfg(test)]
    pub fn set_base_text(&mut self, text: Arc<String>) {
        self.base_text = Some(text);
    }

    #[cfg(test)]
    pub fn add_deleted_hunk(&mut self, hunk: DeletedHunk) {
        self.deleted_hunks.push(hunk);
    }
}
