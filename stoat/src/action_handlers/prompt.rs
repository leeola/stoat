use crate::{
    app::{Stoat, UpdateEffect},
    keymap_state::{active_modal, ActiveModal},
};

/// Submit the currently focused prompt input. Dispatches based on which
/// consumer owns the focused [`crate::input_view::InputView`]. Consumer
/// bindings are added as sites migrate to [`crate::input_view::InputView`];
/// this acts as a no-op for prompt-mode contexts without a registered owner.
pub(crate) fn submit_prompt_input(stoat: &mut Stoat) -> UpdateEffect {
    if super::search::search_submit(stoat) {
        return UpdateEffect::Redraw;
    }
    if super::code_search::code_search_select(stoat) {
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
    if let Some(effect) = crate::symbol_finder::symbol_finder_submit(stoat) {
        return effect;
    }
    if let Some(effect) = super::file_finder::file_finder_submit(stoat) {
        return effect;
    }
    if let Some(effect) = super::palette::palette_submit(stoat) {
        return effect;
    }
    if let Some(effect) = picker_submit(stoat) {
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

/// Recall the previous entry from the open prompt's history.
///
/// One verb over every prompt that keeps a history. A prompt that keeps none is
/// absent here, so its Alt-Up does nothing rather than reaching the surface
/// underneath.
pub(crate) fn prompt_history_prev(stoat: &mut Stoat) -> UpdateEffect {
    match active_modal(stoat) {
        Some(ActiveModal::Palette) => {
            super::palette::palette_history_prev(stoat).unwrap_or(UpdateEffect::None)
        },
        Some(ActiveModal::Search) => {
            super::search::search_history_prev(stoat).unwrap_or(UpdateEffect::None)
        },
        _ => UpdateEffect::None,
    }
}

/// Recall the next entry toward the newest in the open prompt's history.
///
/// The counterpart to [`prompt_history_prev`], over the same set of prompts.
pub(crate) fn prompt_history_next(stoat: &mut Stoat) -> UpdateEffect {
    match active_modal(stoat) {
        Some(ActiveModal::Palette) => {
            super::palette::palette_history_next(stoat).unwrap_or(UpdateEffect::None)
        },
        Some(ActiveModal::Search) => {
            super::search::search_history_next(stoat).unwrap_or(UpdateEffect::None)
        },
        _ => UpdateEffect::None,
    }
}

/// Take the open list modal's selection, or `None` when no such modal is open.
///
/// The four small pickers ride the shared prompt actions rather than carrying
/// select and close verbs of their own, so Enter means the same thing over a
/// picker as it does over any other prompt.
fn picker_submit(stoat: &mut Stoat) -> Option<UpdateEffect> {
    match active_modal(stoat)? {
        ActiveModal::Jumplist => Some(super::picker::jumplist_picker_select(stoat)),
        ActiveModal::Diagnostics => Some(super::picker::diagnostics_picker_select(stoat)),
        ActiveModal::Location => Some(super::picker::location_picker_select(stoat)),
        ActiveModal::WorkspacePicker => Some(super::workspace::workspace_picker_select(stoat)),
        ActiveModal::CommitPicker => Some(super::review_walk::commit_picker_select(stoat)),
        _ => None,
    }
}

/// Dismiss the open list modal, or `None` when no such modal is open.
fn picker_cancel(stoat: &mut Stoat) -> Option<UpdateEffect> {
    match active_modal(stoat)? {
        ActiveModal::Jumplist => Some(super::picker::jumplist_picker_close(stoat)),
        ActiveModal::Diagnostics => Some(super::picker::diagnostics_picker_close(stoat)),
        ActiveModal::Location => Some(super::picker::location_picker_close(stoat)),
        ActiveModal::WorkspacePicker => Some(super::workspace::workspace_picker_close(stoat)),
        ActiveModal::CommitPicker => Some(super::review_walk::commit_picker_close(stoat)),
        _ => None,
    }
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
    if super::code_search::close_code_search(stoat) {
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
    if let Some(effect) = crate::symbol_finder::symbol_finder_cancel(stoat) {
        return effect;
    }
    if let Some(effect) = super::file_finder::file_finder_cancel(stoat) {
        return effect;
    }
    if let Some(effect) = super::palette::palette_cancel(stoat) {
        return effect;
    }
    if let Some(effect) = picker_cancel(stoat) {
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

#[cfg(test)]
mod tests {
    use crate::{app::UpdateEffect, input_history::InputHistory, Stoat};
    use stoat_action::{OpenFileFinder, PromptHistoryPrev};

    /// The generic verb reaches the palette's history rather than naming it.
    ///
    /// The Alt-Up tests drive the binding; this drives the action itself, so a
    /// later prompt gaining history cannot quietly drop the palette arm.
    #[test]
    fn the_generic_verb_walks_the_palette_history() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history =
            InputHistory::from_entries(vec!["w".to_string()]);
        h.type_text(":");

        crate::action_handlers::dispatch(&mut h.stoat, &PromptHistoryPrev);

        let palette = h.stoat.command_palette.as_ref().expect("open");
        let ws = h.stoat.active_workspace();
        assert_eq!(
            palette.input.text(ws),
            "w",
            "the generic verb recalled the palette's newest entry"
        );
    }

    /// A prompt with no history absorbs the verb rather than passing it on, so
    /// Alt-Up over the finder never reaches the editor behind it.
    #[test]
    fn the_generic_verb_is_inert_over_a_prompt_without_history() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("alpha\nbeta\n");
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();

        let effect = crate::action_handlers::dispatch(&mut h.stoat, &PromptHistoryPrev);

        assert_eq!(
            effect,
            UpdateEffect::None,
            "the finder answers with nothing"
        );
        assert!(
            h.stoat.file_finder.is_some(),
            "and the finder is still the open modal"
        );
    }
}
