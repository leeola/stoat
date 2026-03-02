use crate::{
    app_state::{SymbolEntry, SymbolPickerSource},
    pane_group::view::PaneGroupView,
    stoat::KeyContext,
};
use gpui::Context;

impl PaneGroupView {
    pub(crate) fn handle_open_symbol_picker(
        &mut self,
        symbols: Vec<SymbolEntry>,
        source: SymbolPickerSource,
        _window: &mut gpui::Window,
        cx: &mut Context<'_, Self>,
    ) {
        if symbols.is_empty() {
            self.app_state.flash_message = Some("No symbols found".into());
            cx.notify();
            return;
        }

        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            let (current_mode, current_key_context) = {
                let stoat = editor.read(cx).stoat.read(cx);
                (stoat.mode().to_string(), stoat.key_context())
            };

            self.app_state.open_symbol_picker(
                symbols,
                source,
                current_mode,
                current_key_context,
                cx,
            );

            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, _cx| {
                    stoat.set_key_context(KeyContext::SymbolPicker);
                    stoat.set_mode("symbol_picker");
                    stoat.symbol_picker_input_ref = self.app_state.symbol_picker.input.clone();
                });
            });

            cx.notify();
        }
    }
}
