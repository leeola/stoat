use crate::{
    command_palette_v2::CommandPaletteV2, pane_group::view::PaneGroupView, stoat::KeyContext,
};
use gpui::{AppContext, Context, Focusable, Window};

impl PaneGroupView {
    pub(crate) fn handle_open_command_palette_v2(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            let previous_key_context = editor.read(cx).stoat.read(cx).key_context();

            let commands = crate::stoat_actions::build_command_list();

            if self.app_state.command_palette_v2.is_none() {
                let palette = cx.new(|cx| {
                    let mut palette = CommandPaletteV2::new(commands, cx);
                    palette.set_previous_key_context(previous_key_context);
                    palette
                });
                self.app_state.command_palette_v2 = Some(palette);
            } else if let Some(palette) = &self.app_state.command_palette_v2 {
                palette.update(cx, |p, _| {
                    p.set_previous_key_context(previous_key_context);
                });
            }

            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, _| {
                    stoat.set_key_context(KeyContext::CommandPaletteV2);
                    stoat.sync_mode_to_context(&self.app_state);
                });
            });

            if let Some(palette) = &self.app_state.command_palette_v2 {
                let input_focus = palette.read(cx).input().read(cx).focus_handle(cx);
                window.focus(&input_focus);
            }

            cx.notify();
        }
    }
}
