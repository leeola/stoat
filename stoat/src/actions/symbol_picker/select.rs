use crate::{app_state::SymbolPickerSource, pane_group::view::PaneGroupView};
use gpui::{Context, Window};
use lsp_types::{Location, Range};
use stoat_lsp::lsp_position_to_point;

impl PaneGroupView {
    pub(crate) fn handle_symbol_picker_select(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            let sp = &self.app_state.symbol_picker;
            if sp.selected >= sp.filtered.len() {
                self.handle_symbol_picker_dismiss(_window, cx);
                return;
            }

            let entry = sp.filtered[sp.selected].clone();
            let source = sp.source;

            self.handle_symbol_picker_dismiss(_window, cx);

            match source {
                Some(SymbolPickerSource::Workspace) => {
                    if let Some(uri) = &entry.file_uri {
                        let location = Location {
                            uri: uri.clone(),
                            range: Range {
                                start: entry.range_start,
                                end: entry.range_start,
                            },
                        };
                        editor.update(cx, |editor, cx| {
                            editor.stoat.update(cx, |stoat, cx| {
                                stoat.navigate_to_lsp_location(&location, cx)
                            });
                        });
                    }
                },
                _ => {
                    editor.update(cx, |editor, cx| {
                        editor.stoat.update(cx, |stoat, cx| {
                            let buffer_item = stoat.active_buffer(cx);
                            let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
                            if let Ok(point) = lsp_position_to_point(&entry.range_start, snapshot) {
                                stoat.jump_to_point(point, cx);
                            }
                        });
                    });
                },
            }
        }
    }
}
