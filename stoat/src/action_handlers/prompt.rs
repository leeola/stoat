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
    if let Some(effect) = super::file_finder::file_finder_submit(stoat) {
        return effect;
    }
    if let Some(effect) = super::palette::palette_submit(stoat) {
        return effect;
    }
    if stoat.help.is_some() {
        return super::help::help_submit(stoat);
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
        FocusTarget::SplitPane => ws.panes.pane(ws.panes.focus()).view.clone(),
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
    if let Some(effect) = super::file_finder::file_finder_cancel(stoat) {
        return effect;
    }
    if let Some(effect) = super::palette::palette_cancel(stoat) {
        return effect;
    }
    // Help handles Escape in two stages. The first leaves the search input in
    // normal sub-mode so the list can be navigated with hjkl, and a second
    // Escape - routed via `modal == help && mode == normal { Escape -> ... }` -
    // closes it. Every other input was disposed by a cancel above.
    if stoat.focused_mode() == "insert" {
        stoat.set_focused_mode("normal".into());
        return UpdateEffect::Redraw;
    }
    UpdateEffect::None
}

pub(super) fn prompt_insert_newline(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(effect) = super::palette::palette_insert_newline(stoat) {
        return effect;
    }
    UpdateEffect::None
}

pub(super) fn palette_select_prev(stoat: &mut Stoat) -> UpdateEffect {
    super::palette::palette_move_selection(stoat, -1).unwrap_or(UpdateEffect::None)
}

pub(super) fn palette_select_next(stoat: &mut Stoat) -> UpdateEffect {
    super::palette::palette_move_selection(stoat, 1).unwrap_or(UpdateEffect::None)
}
