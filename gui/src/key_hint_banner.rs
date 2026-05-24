//! Inline key-hint banner shown under the status bar while a transient
//! chord mode (`goto`, `z`, bracket/match, `space`/`space_*`) is active.
//!
//! Subscribes to the [`InputStateMachine`] (which calls `cx.notify()`
//! on every mode transition) and, when the active mode is transient,
//! renders a one-line summary of that mode's live bindings drawn from
//! [`stoat::keymap::Keymap::active_bindings`] -- the same source the
//! help modal uses. Non-transient modes render nothing.

use crate::{
    input_state_machine::InputStateMachine,
    theme::{border_inactive_color, statusbar_focused_color, statusbar_text_color},
};
use gpui::{
    div, App, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, Window,
};
use stoat_action::registry;

pub struct KeyHintBanner {
    input_state_machine: Entity<InputStateMachine>,
    _subscription: Subscription,
}

impl KeyHintBanner {
    pub fn new(input_state_machine: Entity<InputStateMachine>, cx: &mut Context<'_, Self>) -> Self {
        let subscription = cx.observe(&input_state_machine, |_, _, cx| cx.notify());
        Self {
            input_state_machine,
            _subscription: subscription,
        }
    }

    /// One-line hint for the active mode, or `None` when the mode is not
    /// a transient chord mode (so the banner stays hidden).
    pub(crate) fn current_hint(&self, cx: &App) -> Option<String> {
        let sm = self.input_state_machine.read(cx);
        let mode = sm.mode();
        if !is_transient_mode(mode) {
            return None;
        }
        let entries: Vec<(String, String)> = sm
            .keymap()
            .active_bindings(sm)
            .into_iter()
            .filter_map(|(label, actions)| actions.first().map(|a| (label, a.name.clone())))
            .collect();
        hint_line(mode, &entries)
    }
}

impl Render for KeyHintBanner {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        match self.current_hint(cx) {
            Some(text) => div()
                .absolute()
                .bottom_4()
                .right_4()
                .p_3()
                .rounded_md()
                .bg(statusbar_focused_color(cx).opacity(0.95))
                .border_1()
                .border_color(border_inactive_color(cx))
                .shadow_lg()
                .text_color(statusbar_text_color(cx))
                .child(SharedString::from(text)),
            None => div(),
        }
    }
}

/// Whether `mode` is a transient chord mode that warrants an inline
/// hint: the named submodes plus the `space` family.
fn is_transient_mode(mode: &str) -> bool {
    matches!(
        mode,
        "goto" | "z" | "bracket_next" | "bracket_prev" | "match"
    ) || mode == "space"
        || mode.starts_with("space_")
}

/// Format the active-binding `entries` -- `(key_label,
/// first_action_name)` pairs -- into `mode: key (desc)  key (desc)
/// ...`. Names absent from the action registry (e.g. the `SetMode`
/// boilerplate every mode carries on `Escape`) are dropped. Returns
/// `None` for non-transient modes or when no entry resolves.
fn hint_line(mode: &str, entries: &[(String, String)]) -> Option<String> {
    if !is_transient_mode(mode) {
        return None;
    }
    let hints: Vec<String> = entries
        .iter()
        .filter_map(|(label, name)| {
            let def = registry::lookup(name)?.def;
            Some(format!("{label} ({})", def.short_desc()))
        })
        .collect();
    (!hints.is_empty()).then(|| format!("{mode}: {}", hints.join("  ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use gpui::{AppContext, TestAppContext};
    use std::path::PathBuf;
    use stoat::keymap::StateValue;

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, n)| (k.to_string(), n.to_string()))
            .collect()
    }

    #[test]
    fn is_transient_mode_covers_named_and_space_family() {
        for mode in [
            "goto",
            "z",
            "bracket_next",
            "bracket_prev",
            "match",
            "space",
        ] {
            assert!(is_transient_mode(mode), "{mode} should be transient");
        }
        assert!(is_transient_mode("space_git"));
        assert!(is_transient_mode("space_pane_nav"));
        for mode in ["normal", "insert", "select", "prompt", "rebase"] {
            assert!(!is_transient_mode(mode), "{mode} should not be transient");
        }
    }

    #[test]
    fn hint_line_formats_bindings_and_drops_unregistered_actions() {
        let pairs = entries(&[
            ("i", "GotoFirstNonwhitespace"),
            ("Escape", "SetMode"),
            ("j", "GotoLastLine"),
        ]);
        let i_desc = registry::lookup("GotoFirstNonwhitespace")
            .expect("registered")
            .def
            .short_desc();
        let j_desc = registry::lookup("GotoLastLine")
            .expect("registered")
            .def
            .short_desc();
        assert_eq!(
            hint_line("goto", &pairs),
            Some(format!("goto: i ({i_desc})  j ({j_desc})"))
        );
    }

    #[test]
    fn hint_line_none_for_non_transient_mode() {
        let pairs = entries(&[("i", "GotoFirstNonwhitespace")]);
        assert_eq!(hint_line("normal", &pairs), None);
        assert_eq!(hint_line("insert", &pairs), None);
    }

    #[test]
    fn hint_line_none_when_no_entry_resolves() {
        let pairs = entries(&[("Escape", "SetMode")]);
        assert_eq!(hint_line("goto", &pairs), None);
    }

    fn state_machine(cx: &mut TestAppContext) -> Entity<InputStateMachine> {
        let ws =
            cx.update(|cx| cx.new(|cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx)));
        ws.read_with(cx, |w, _| w.input_state_machine().clone())
    }

    #[test]
    fn current_hint_lists_live_goto_bindings() {
        let mut cx = TestAppContext::single();
        let ism = state_machine(&mut cx);
        ism.update(&mut cx, |sm, _| {
            sm.set_mode_for_test(StateValue::String("goto".into()))
        });
        let banner = cx.update(|cx| cx.new(|cx| KeyHintBanner::new(ism.clone(), cx)));

        let hint = banner
            .read_with(&cx, |b, cx| b.current_hint(cx))
            .expect("goto is transient and has bindings");
        assert!(hint.starts_with("goto: "), "hint: {hint}");
        assert!(hint.contains("i ("), "expected goto motion keys: {hint}");
    }

    #[test]
    fn current_hint_hidden_in_normal_mode() {
        let mut cx = TestAppContext::single();
        let ism = state_machine(&mut cx);
        let banner = cx.update(|cx| cx.new(|cx| KeyHintBanner::new(ism.clone(), cx)));
        assert_eq!(banner.read_with(&cx, |b, cx| b.current_hint(cx)), None);
    }
}
