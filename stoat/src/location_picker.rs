//! Multi-location goto picker.
//!
//! Opened by [`crate::action_handlers::lsp::pump_lsp_jumps`] when a goto
//! request resolves to more than one location. Presents the candidates as
//! `path:line:col  target-line` rows over a preview of the file each names,
//! filtered by the prompt on top. Selecting a row jumps through the same apply
//! path a single-location goto uses.

use crate::picker::{PreviewSource, TargetPicker};
use std::path::PathBuf;

/// One resolved goto candidate. Carries the byte offset to jump to plus
/// the 1-based line/column and the target line's text for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocationEntry {
    pub(crate) path: PathBuf,
    pub(crate) offset: usize,
    pub(crate) line: u32,
    pub(crate) column: u32,
    pub(crate) text: String,
}

/// Modal chooser over the candidates of a multi-location goto.
pub(crate) type LocationPicker = TargetPicker<LocationEntry>;

/// What a query matches a candidate against: where it points and what is
/// there, which is what the row shows.
pub(crate) fn location_haystack(entry: &LocationEntry) -> String {
    format!(
        "{}:{} {}",
        entry.path.display(),
        entry.line,
        entry.text.trim()
    )
}

/// Where a candidate's preview reads from and which 0-based line it centres on.
///
/// An open buffer wins over the file, so a candidate in edited text previews
/// what the reader is looking at rather than what is on disk.
pub(crate) fn location_target(
    ws: &crate::workspace::Workspace,
    entry: &LocationEntry,
) -> Option<(PreviewSource, u32)> {
    let source = match ws.buffers.id_for_path(&entry.path) {
        Some(id) => PreviewSource::Buffer(id),
        None => PreviewSource::File(entry.path.clone()),
    };
    Some((source, entry.line.saturating_sub(1)))
}
