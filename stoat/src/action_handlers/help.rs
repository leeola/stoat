use crate::app::{Stoat, UpdateEffect};

pub(super) fn help_select_prev(stoat: &mut Stoat) -> UpdateEffect {
    apply_to_help(stoat, |h| h.move_selection(-1))
}

pub(super) fn help_select_next(stoat: &mut Stoat) -> UpdateEffect {
    apply_to_help(stoat, |h| h.move_selection(1))
}

pub(super) fn help_scope_toggle(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;
    let workspaces = &mut stoat.workspaces;
    let Some(help) = stoat.help.as_mut() else {
        return UpdateEffect::None;
    };
    help.toggle_scope_pub(&workspaces[active_idx]);
    UpdateEffect::Redraw
}

pub(super) fn help_scroll_detail_up(stoat: &mut Stoat) -> UpdateEffect {
    apply_to_help(stoat, |h| h.scroll_detail(-5))
}

pub(super) fn help_scroll_detail_down(stoat: &mut Stoat) -> UpdateEffect {
    apply_to_help(stoat, |h| h.scroll_detail(5))
}

pub(super) fn help_jump_first(stoat: &mut Stoat) -> UpdateEffect {
    apply_to_help(stoat, |h| h.jump_selection(0))
}

pub(super) fn help_jump_last(stoat: &mut Stoat) -> UpdateEffect {
    apply_to_help(stoat, |h| {
        let last = h.filtered().len().saturating_sub(1);
        h.jump_selection(last);
    })
}

fn apply_to_help(stoat: &mut Stoat, f: impl FnOnce(&mut crate::help::Help)) -> UpdateEffect {
    let Some(help) = stoat.help.as_mut() else {
        return UpdateEffect::None;
    };
    f(help);
    UpdateEffect::Redraw
}

/// Submit the currently-selected help entry. Called from `SubmitPromptInput`
/// when `HelpSearch` is the focused target. Closes the help modal on dispatch
/// via [`super::dispatch`] for any action that resolves.
pub(super) fn help_submit(stoat: &mut Stoat) -> UpdateEffect {
    use crate::help::HelpOutcome;

    let Some(help) = stoat.help.as_ref() else {
        return UpdateEffect::None;
    };
    let outcome = help.dispatch_selected_pub();
    match outcome {
        HelpOutcome::None => UpdateEffect::Redraw,
        HelpOutcome::Close => {
            super::close_help(stoat);
            UpdateEffect::Redraw
        },
        HelpOutcome::Dispatch(entry, params) => {
            super::close_help(stoat);
            match (entry.create)(&params) {
                Ok(action) => super::dispatch(stoat, &*action),
                Err(e) => {
                    tracing::warn!("help dispatch `{}`: {e}", entry.def.name());
                    UpdateEffect::Redraw
                },
            }
        },
    }
}

/// Cancel help on Escape from normal mode (inside help). Kept separate from
/// the generic `CancelPromptInput` path so it runs only when the user is in
/// normal-mode-within-help.
pub(super) fn help_cancel(stoat: &mut Stoat) -> UpdateEffect {
    super::close_help(stoat);
    UpdateEffect::Redraw
}
