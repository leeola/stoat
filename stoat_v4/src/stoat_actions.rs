//! Action implementations for Stoat.
//!
//! These demonstrate the Context<Self> pattern - methods can spawn self-updating tasks.

use crate::{
    actions::*,
    file_finder::{load_file_preview, load_text_only, PreviewData},
    stoat::Stoat,
    worktree::Entry,
};
use gpui::Context;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use std::{num::NonZeroU64, path::PathBuf};
use text::{Buffer, BufferId};
use tracing::debug;

impl Stoat {
    // ==== Editing actions ====

    /// Insert text at cursor
    pub fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();
        self.buffer_item.read(cx).buffer().update(cx, |buffer, _| {
            let offset = buffer.offset_from_point(cursor);
            buffer.edit([(offset..offset, text)]);
        });

        // Move cursor forward
        let new_col = cursor.column + text.len() as u32;
        self.cursor.move_to(text::Point::new(cursor.row, new_col));

        // Reparse for syntax highlighting
        self.buffer_item.update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Delete character before cursor
    pub fn delete_left(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();
        if cursor.column == 0 {
            return; // At start of line
        }

        self.buffer_item.read(cx).buffer().update(cx, |buffer, _| {
            let offset = buffer.offset_from_point(cursor);
            buffer.edit([(offset - 1..offset, "")]);
        });

        // Move cursor back
        self.cursor
            .move_to(text::Point::new(cursor.row, cursor.column - 1));

        // Reparse
        self.buffer_item.update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    // ==== Movement actions ====

    /// Move cursor up
    pub fn move_up(&mut self, _cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        if pos.row > 0 {
            self.cursor
                .move_to(text::Point::new(pos.row - 1, pos.column));
            self.ensure_cursor_visible();
        }
    }

    /// Move cursor down
    pub fn move_down(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        let max_row = self.buffer_item.read(cx).buffer().read(cx).max_point().row;

        if pos.row < max_row {
            self.cursor
                .move_to(text::Point::new(pos.row + 1, pos.column));
            self.ensure_cursor_visible();
        }
    }

    /// Move cursor left
    pub fn move_left(&mut self, _cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        if pos.column > 0 {
            self.cursor
                .move_to(text::Point::new(pos.row, pos.column - 1));
        }
    }

    /// Move cursor right
    pub fn move_right(&mut self, cx: &mut Context<Self>) {
        let pos = self.cursor.position();
        let line_len = self
            .buffer_item
            .read(cx)
            .buffer()
            .read(cx)
            .line_len(pos.row);

        if pos.column < line_len {
            self.cursor
                .move_to(text::Point::new(pos.row, pos.column + 1));
        }
    }

    // ==== Mode actions ====

    /// Enter insert mode
    pub fn enter_insert_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "insert".to_string();
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    /// Enter normal mode
    pub fn enter_normal_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "normal".to_string();
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    // ==== File finder actions ====

    /// Open file finder.
    ///
    /// This demonstrates Context<Self> - can create entities and scan worktree.
    pub fn open_file_finder(&mut self, cx: &mut Context<Self>) {
        debug!("Opening file finder");

        // Save current mode
        self.file_finder_previous_mode = Some(self.mode.clone());
        self.mode = "file_finder".to_string();

        // Create input buffer
        let buffer_id = BufferId::from(NonZeroU64::new(2).unwrap());
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        self.file_finder_input = Some(input_buffer);

        // Scan worktree
        let entries = self.worktree.lock().snapshot().entries(false);
        debug!(file_count = entries.len(), "Loaded files from worktree");

        self.file_finder_files = entries;
        self.file_finder_filtered = self
            .file_finder_files
            .iter()
            .map(|e| PathBuf::from(e.path.as_unix_str()))
            .collect();
        self.file_finder_selected = 0;

        // Load preview for first file
        self.load_preview_for_selected(cx);

        cx.notify();
    }

    /// Move to next file in finder.
    ///
    /// Demonstrates spawning async task with Context<Self>.
    pub fn file_finder_next(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        if self.file_finder_selected + 1 < self.file_finder_filtered.len() {
            self.file_finder_selected += 1;
            debug!(selected = self.file_finder_selected, "File finder: next");

            // Load preview for newly selected file
            self.load_preview_for_selected(cx);
            cx.notify();
        }
    }

    /// Move to previous file in finder
    pub fn file_finder_prev(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        if self.file_finder_selected > 0 {
            self.file_finder_selected -= 1;
            debug!(selected = self.file_finder_selected, "File finder: prev");

            // Load preview for newly selected file
            self.load_preview_for_selected(cx);
            cx.notify();
        }
    }

    /// Select file in finder
    pub fn file_finder_select(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        if self.file_finder_selected < self.file_finder_filtered.len() {
            let relative_path = &self.file_finder_filtered[self.file_finder_selected];
            debug!(file = ?relative_path, "File finder: select");

            // Build absolute path
            let root = self.worktree.lock().snapshot().root().to_path_buf();
            let abs_path = root.join(relative_path);

            // Load file (simplified - just read text for now)
            if let Ok(contents) = std::fs::read_to_string(&abs_path) {
                // Detect language
                let language = abs_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(stoat_text::Language::from_extension)
                    .unwrap_or(stoat_text::Language::PlainText);

                // Update buffer
                self.buffer_item.update(cx, |item, cx| {
                    item.set_language(language);
                    item.buffer().update(cx, |buffer, _| {
                        let len = buffer.len();
                        buffer.edit([(0..len, contents.as_str())]);
                    });
                    let _ = item.reparse(cx);
                });

                // Reset cursor
                self.cursor.move_to(text::Point::new(0, 0));
            }
        }

        self.file_finder_dismiss(cx);
    }

    /// Dismiss file finder
    pub fn file_finder_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        debug!("Dismissing file finder");

        // Restore previous mode
        self.mode = self
            .file_finder_previous_mode
            .take()
            .unwrap_or_else(|| "normal".to_string());

        // Clear state
        self.file_finder_input = None;
        self.file_finder_files.clear();
        self.file_finder_filtered.clear();
        self.file_finder_selected = 0;
        self.file_finder_preview = None;
        self.file_finder_preview_task = None;

        cx.notify();
    }

    /// Load preview for selected file.
    ///
    /// KEY METHOD: Demonstrates Context<Self> pattern with async tasks.
    /// Uses `cx.spawn` to get `WeakEntity<Self>` for self-updating.
    pub fn load_preview_for_selected(&mut self, cx: &mut Context<Self>) {
        // Cancel existing task
        self.file_finder_preview_task = None;

        // Get selected file path
        let relative_path = match self.file_finder_filtered.get(self.file_finder_selected) {
            Some(path) => path.clone(),
            None => {
                self.file_finder_preview = None;
                return;
            },
        };

        // Build absolute path
        let root = self.worktree.lock().snapshot().root().to_path_buf();
        let abs_path = root.join(&relative_path);
        let abs_path_for_highlight = abs_path.clone();

        // Spawn async task with WeakEntity<Self> handle
        // This is the key pattern: cx.spawn gives us self handle!
        self.file_finder_preview_task = Some(cx.spawn(async move |this, mut cx| {
            // Phase 1: Load plain text immediately
            if let Some(text) = load_text_only(&abs_path).await {
                // Update self through entity handle
                let _ = this.update(&mut cx, |stoat, cx| {
                    stoat.file_finder_preview = Some(PreviewData::Plain(text));
                    cx.notify();
                });
            }

            // Phase 2: Load syntax-highlighted version
            if let Some(highlighted) = load_file_preview(&abs_path_for_highlight).await {
                let _ = this.update(&mut cx, |stoat, cx| {
                    stoat.file_finder_preview = Some(highlighted);
                    cx.notify();
                });
            }

            Ok(())
        }));
    }

    /// Filter files based on query
    pub fn filter_files(&mut self, query: &str, cx: &mut Context<Self>) {
        if query.is_empty() {
            // No query: show all files
            self.file_finder_filtered = self
                .file_finder_files
                .iter()
                .map(|e| PathBuf::from(e.path.as_unix_str()))
                .collect();
        } else {
            // Fuzzy match
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

            let candidates: Vec<&str> = self
                .file_finder_files
                .iter()
                .map(|e| e.path.as_unix_str())
                .collect();

            let mut matches = pattern.match_list(candidates, &mut self.file_finder_matcher);
            matches.sort_by(|a, b| b.1.cmp(&a.1));
            matches.truncate(100);

            self.file_finder_filtered = matches
                .into_iter()
                .map(|(path, _score)| PathBuf::from(path))
                .collect();
        }

        // Reset selection
        self.file_finder_selected = 0;

        // Load preview for newly selected (top) file
        self.load_preview_for_selected(cx);

        cx.notify();
    }

    // ==== File finder state accessors ====

    /// Get file finder input buffer
    pub fn file_finder_input(&self) -> Option<&gpui::Entity<Buffer>> {
        self.file_finder_input.as_ref()
    }

    /// Get filtered files
    pub fn file_finder_filtered(&self) -> &[PathBuf] {
        &self.file_finder_filtered
    }

    /// Get selected index
    pub fn file_finder_selected(&self) -> usize {
        self.file_finder_selected
    }

    /// Get preview data
    pub fn file_finder_preview(&self) -> Option<&PreviewData> {
        self.file_finder_preview.as_ref()
    }
}
