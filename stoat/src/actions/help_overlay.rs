use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};
use tracing::debug;

impl PaneGroupView {
    /// Three-tier `?` behavior:
    /// 1. No infobox visible -> show expanded infobox
    /// 2. Auto-shown (compact) infobox -> expand it
    /// 3. Already expanded -> open full HelpModal
    pub(crate) fn handle_open_help_overlay(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (has_autoinfo, is_expanded) = self
            .active_editor()
            .map(|editor| {
                let stoat = editor.read(cx).stoat.read(cx);
                (stoat.autoinfo.is_some(), stoat.autoinfo_expanded)
            })
            .unwrap_or((false, false));

        debug!(has_autoinfo, is_expanded, "handle_open_help_overlay called");

        match (has_autoinfo, is_expanded) {
            // Already expanded -> open full HelpModal
            (true, true) => {
                if let Some(editor) = self.active_editor() {
                    editor.update(cx, |editor, cx| {
                        editor.stoat.update(cx, |stoat, cx| {
                            stoat.autoinfo = None;
                            stoat.autoinfo_expanded = false;
                            stoat.open_help_modal(cx);
                        });
                    });
                }
            },
            // Auto-shown compact -> expand
            (true, false) => {
                if let Some(editor) = self.active_editor() {
                    editor.update(cx, |editor, cx| {
                        editor.stoat.update(cx, |_, _| {});
                        editor.stoat.update(cx, |stoat, _| {
                            stoat.autoinfo_expanded = true;
                        });
                    });
                }
            },
            // No infobox -> generate expanded infobox for current mode
            (false, _) => {
                if let Some(editor) = self.active_editor() {
                    editor.update(cx, |editor, cx| {
                        editor.stoat.update(cx, |stoat, _| {
                            let mode = stoat.mode().to_string();
                            let display_name = stoat
                                .get_mode(&mode)
                                .map(|m| m.display_name.clone())
                                .unwrap_or_else(|| mode.to_uppercase());
                            let usage = stoat.usage_tracker.lock();
                            stoat.autoinfo = Some(crate::keymap::query::bindings_for_infobox(
                                &stoat.compiled_keymap,
                                &mode,
                                &display_name,
                                &usage,
                            ));
                            stoat.autoinfo_expanded = true;
                        });
                    });
                }
            },
        }
        cx.notify();
    }
}
