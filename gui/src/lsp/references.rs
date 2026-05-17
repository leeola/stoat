use crate::{
    editor::Editor,
    picker::{Picker, PickerDelegate, PickerSecondary},
    theme::statusbar_text_color,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, Entity, IntoElement, ParentElement, Styled, Task, WeakEntity, Window,
};
use lsp_types::Location;
use std::path::PathBuf;
use stoat::host::OffsetEncoding;

/// Picker delegate listing the locations returned by
/// `textDocument/references` for the symbol under the cursor.
///
/// Each row renders as `<rel-path>:<line>:<col>`. Confirm opens the
/// target file in the focused pane (via `Workspace::open_paths`,
/// which deduplicates against the registry) and places the cursor
/// at the location's range start. The picker is bounded by the
/// server's response, so `update_matches` is a no-op (a fuzzy
/// filter against many references would be a separate item).
pub struct ReferencesPickerDelegate {
    locations: Vec<Location>,
    selected: usize,
    workspace: WeakEntity<Workspace>,
    encoding: OffsetEncoding,
}

impl ReferencesPickerDelegate {
    pub fn new(
        locations: Vec<Location>,
        workspace: WeakEntity<Workspace>,
        encoding: OffsetEncoding,
    ) -> Self {
        Self {
            locations,
            selected: 0,
            workspace,
            encoding,
        }
    }
}

impl PickerDelegate for ReferencesPickerDelegate {
    fn match_count(&self) -> usize {
        self.locations.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if ix < self.locations.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, _query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        _window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(loc) = self.locations.get(self.selected).cloned() else {
            return;
        };
        let Some(path) = uri_to_path(&loc.uri) else {
            return;
        };
        let position = loc.range.start;
        let encoding = self.encoding;
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.open_paths(&[path.clone()], cx);
            let Some(editor) = workspace
                .buffer_for_path(&path, cx)
                .and_then(|buffer| editor_for_buffer(workspace, &buffer, cx))
            else {
                return;
            };
            let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
            let rope = mb_snapshot.rope().clone();
            let offset = stoat::lsp::util::lsp_pos_to_byte_offset(&rope, position, encoding);
            editor.update(cx, |ed, cx| {
                let snapshot = ed.multi_buffer().read(cx).snapshot();
                let anchor = snapshot.anchor_at(offset, stoat_text::Bias::Left);
                let new_id = ed
                    .selections()
                    .all_anchors()
                    .iter()
                    .map(|s| s.id)
                    .max()
                    .map(|m| m + 1)
                    .unwrap_or(1);
                let selection = stoat_text::Selection {
                    id: new_id,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: stoat_text::SelectionGoal::None,
                };
                ed.selections_mut().replace_with(vec![selection], &snapshot);
            });
        });
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let label = self
            .locations
            .get(ix)
            .map(format_location)
            .unwrap_or_default();
        let color = statusbar_text_color(cx);
        let mut row = div().px_2().text_color(color).child(label);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }
}

fn format_location(loc: &Location) -> String {
    let path = uri_to_path(&loc.uri)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| loc.uri.as_str().to_string());
    format!(
        "{}:{}:{}",
        path,
        loc.range.start.line + 1,
        loc.range.start.character + 1
    )
}

fn uri_to_path(uri: &lsp_types::Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://").unwrap_or(s);
    Some(PathBuf::from(stripped))
}

fn editor_for_buffer(
    workspace: &Workspace,
    buffer: &Entity<crate::buffer::Buffer>,
    cx: &gpui::App,
) -> Option<Entity<Editor>> {
    let target_id = buffer.entity_id();
    let pane_tree = workspace.pane_tree().read(cx);
    for pane_id in pane_tree.split_pane_ids() {
        let pane = pane_tree.pane(pane_id)?;
        for item in pane.read(cx).items() {
            let Ok(editor) = item.to_any_view().downcast::<Editor>() else {
                continue;
            };
            let mb_singleton = editor
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .cloned();
            if mb_singleton.as_ref().map(Entity::entity_id) == Some(target_id) {
                return Some(editor);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range, Uri};
    use std::str::FromStr;

    fn loc(uri_str: &str, line: u32, character: u32) -> Location {
        Location {
            uri: Uri::from_str(uri_str).unwrap(),
            range: Range {
                start: Position { line, character },
                end: Position {
                    line,
                    character: character + 1,
                },
            },
        }
    }

    #[test]
    fn format_location_renders_one_based_position() {
        let l = loc("file:///tmp/a.rs", 9, 4);
        assert_eq!(format_location(&l), "/tmp/a.rs:10:5");
    }

    #[test]
    fn format_location_keeps_raw_uri_when_not_file_scheme() {
        let l = loc("http://example.com/x", 0, 0);
        assert_eq!(format_location(&l), "http://example.com/x:1:1");
    }
}
