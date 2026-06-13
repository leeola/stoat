use crate::app::{Stoat, UpdateEffect};

/// Submit the currently focused prompt input. Dispatches based on which
/// consumer owns the focused [`crate::input_view::InputView`]. Consumer
/// bindings are added as sites migrate to [`crate::input_view::InputView`];
/// this acts as a no-op for prompt-mode contexts without a registered owner.
pub(super) fn submit_prompt_input(stoat: &mut Stoat) -> UpdateEffect {
    if super::search::search_submit(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::global_search_submit(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::split_selection::submit(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::filter_selections::submit(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::shell::submit(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::lsp::rename_input_submit(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::lsp::workspace_symbol_submit(stoat) {
        return UpdateEffect::Redraw;
    }
    if let Some(crate::input_view::SubmitTarget::Run) = focused_target(stoat) {
        return super::run::run_submit(stoat);
    }
    UpdateEffect::None
}

fn focused_target(stoat: &Stoat) -> Option<crate::input_view::SubmitTarget> {
    use crate::pane::{FocusTarget, View};

    let ws = stoat.active_workspace();
    let view = match ws.focus {
        FocusTarget::SplitPane(_) => ws.panes.pane(ws.panes.focus()).view.clone(),
        FocusTarget::Dock(dock_id) => ws.docks.get(dock_id)?.view.clone(),
    };
    match view {
        View::Run(id) => ws.runs.get(id).map(|r| r.input.target),
        _ => None,
    }
}

pub(super) fn cancel_prompt_input(stoat: &mut Stoat) -> UpdateEffect {
    if super::search::search_cancel(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::global_search_cancel(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::split_selection::cancel(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::filter_selections::cancel(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::shell::cancel(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::lsp::rename_input_cancel(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::lsp::workspace_symbol_cancel(stoat) {
        return UpdateEffect::Redraw;
    }
    // For pane-tied or modal inputs (help), Escape in prompt
    // leaves the user in normal sub-mode so they can navigate with hjkl /
    // drop into modal editing. A second Escape - routed via a separate
    // keymap binding like `mode == normal && help_open { Escape -> ... }` -
    // closes the modal.
    if stoat.mode == "prompt" {
        stoat.mode = "normal".into();
        return UpdateEffect::Redraw;
    }
    UpdateEffect::None
}

pub(super) fn prompt_insert_newline(_stoat: &mut Stoat) -> UpdateEffect {
    UpdateEffect::None
}
