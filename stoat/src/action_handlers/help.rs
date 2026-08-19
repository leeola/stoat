use crate::{
    app::{Stoat, UpdateEffect},
    help::{build_help_bindings, Help, SnapshotState},
    keymap::{KeymapState, StateValue},
    keymap_state::{StoatKeymapState, BUILTIN_FIELDS},
};
use std::collections::HashMap;

pub(super) fn help_select_prev(stoat: &mut Stoat) -> UpdateEffect {
    apply_to_help(stoat, |h| h.move_selection(-1))
}

pub(super) fn help_select_next(stoat: &mut Stoat) -> UpdateEffect {
    apply_to_help(stoat, |h| h.move_selection(1))
}

/// Rows a help list page covers.
///
/// A fixed step, where the other list modals derive one from the rows their
/// viewport shows. [`Help`] tracks no row count, and its detail pane already
/// scrolls by a fixed step for the same reason.
const HELP_PAGE_ROWS: i32 = 5;

/// Page the help list's selection by [`HELP_PAGE_ROWS`] in `dir`.
pub(super) fn help_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    apply_to_help(stoat, |h| h.move_selection(dir * HELP_PAGE_ROWS))
}

pub(super) fn help_complete(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;
    let workspaces = &mut stoat.workspaces;
    let Some(help) = stoat.help.as_mut() else {
        return UpdateEffect::None;
    };
    if help.complete_selected(&mut workspaces[active_idx]) {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
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

fn apply_to_help(stoat: &mut Stoat, f: impl FnOnce(&mut Help)) -> UpdateEffect {
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
            close_help(stoat);
            UpdateEffect::Redraw
        },
        HelpOutcome::Dispatch(entry, params) => {
            close_help(stoat);
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
    close_help(stoat);
    UpdateEffect::Redraw
}

/// Open the full help modal, snapshotting the bindings active for the focused
/// mode. Backs the `OpenHelp` action that `?` dispatches from normal mode and
/// most others (goto keeps reverse-search, the space leader keeps the hints
/// toggle).
pub(crate) fn open_help(stoat: &mut Stoat) {
    let active = stoat.active_bindings_for_current_mode();
    let mode = stoat.focused_mode().to_string();
    let context = help_context(stoat);
    let bindings = build_help_bindings(&stoat.keymap, &context);
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    stoat.help = Some(Help::new(&mode, active, bindings, context, ws, executor));
}

/// Snapshot the predicate fields a help binding's conditions may test.
///
/// User variables seed the map so a `SetVar` field is available. The builtin
/// fields then overwrite any that collide, since [`StoatKeymapState`] is the
/// authority on `mode`/`view`/`token` and the rest.
fn help_context(stoat: &Stoat) -> SnapshotState {
    let mut fields: HashMap<String, StateValue> = stoat.user_vars.clone();
    let state = StoatKeymapState::from_stoat(stoat);
    for &field in BUILTIN_FIELDS {
        if let Some(value) = state.get(field) {
            fields.insert(field.to_string(), value.clone());
        }
    }
    SnapshotState(fields)
}

/// Close the help modal, disposing its scratch editor and restoring the
/// mode that was active before the modal opened. No-op when help is not
/// open. Shared between `CancelPromptInput`, Ctrl-C cleanup, and the help
/// `HelpOutcome::Close`/`HelpOutcome::Dispatch` paths.
pub(crate) fn close_help(stoat: &mut Stoat) {
    let Some(help) = stoat.help.take() else {
        return;
    };
    let active_idx = stoat.active_workspace;
    help.dispose(&mut stoat.workspaces[active_idx]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_mark_opens_help_in_normal_mode() {
        let mut h = Stoat::test();
        assert!(h.stoat.help.is_none(), "no help open by default");

        h.type_text("?");
        let all = h
            .stoat
            .help
            .as_ref()
            .expect("? opens help directly in normal mode")
            .filtered()
            .len();
        assert!(all > 0, "help lists the active bindings");

        h.type_text("quit");
        let filtered = h
            .stoat
            .help
            .as_ref()
            .expect("help stays open while typing")
            .filtered()
            .len();
        assert!(
            filtered < all,
            "typing into the help search filters the list ({all} -> {filtered})",
        );
    }
}
