//! Floating key-hint overlay shown in the bottom-right corner while a
//! transient chord mode (`goto`, `z`, bracket/match, `space`/`space_*`)
//! is active.
//!
//! Subscribes to the [`InputStateMachine`] (which calls `cx.notify()`
//! on every mode transition) and, when the active mode is transient,
//! renders the live bindings drawn from
//! [`stoat::keymap::Keymap::active_bindings`] -- the same source the
//! help modal uses -- as a vertical list: a mode header, one row per
//! binding (key chip + description), and a `? for full help` footer.
//! Non-transient modes render nothing.

use crate::{input_state_machine::InputStateMachine, theme::ActiveTheme};
use gpui::{
    div, App, Context, Entity, FontWeight, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, Window,
};
use stoat_action::registry;

pub struct KeyHintBanner {
    input_state_machine: Entity<InputStateMachine>,
    _subscription: Subscription,
}

/// Structured hint content for the active transient mode: the mode
/// label and the resolved `(key_label, short_desc)` pairs to display.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HintContent {
    pub(crate) mode: String,
    pub(crate) bindings: Vec<(String, String)>,
}

impl KeyHintBanner {
    pub fn new(input_state_machine: Entity<InputStateMachine>, cx: &mut Context<'_, Self>) -> Self {
        let subscription = cx.observe(&input_state_machine, |_, _, cx| cx.notify());
        Self {
            input_state_machine,
            _subscription: subscription,
        }
    }

    /// Structured hint for the active mode, or `None` when the mode is
    /// not a transient chord mode (so the banner stays hidden).
    pub(crate) fn current_hint(&self, cx: &App) -> Option<HintContent> {
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
        hint_entries(mode, &entries)
    }
}

impl Render for KeyHintBanner {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let Some(hint) = self.current_hint(cx) else {
            return div();
        };

        let text_color = cx.theme().statusbar_text;
        let border_color = cx.theme().border_inactive;
        let bg_color = cx.theme().statusbar_focused;

        let rows: Vec<_> = hint
            .bindings
            .into_iter()
            .map(|(key, desc)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(border_color)
                            .text_color(text_color)
                            .text_xs()
                            .child(SharedString::from(key)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(text_color.opacity(0.7))
                            .child(SharedString::from(desc)),
                    )
            })
            .collect();

        let header = div()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(text_color)
            .child(SharedString::from(format!(
                "{} MODE",
                hint.mode.to_uppercase()
            )));

        let footer = div()
            .mt_2()
            .pt_2()
            .border_t_1()
            .border_color(border_color.opacity(0.5))
            .text_xs()
            .text_color(text_color.opacity(0.7))
            .child(SharedString::from("? for full help"));

        div()
            .absolute()
            .bottom_4()
            .right_4()
            .p_3()
            .rounded_md()
            .bg(bg_color.opacity(0.95))
            .border_1()
            .border_color(border_color)
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(header)
                    .child(div().flex().flex_col().gap_1().children(rows))
                    .child(footer),
            )
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

/// Build the structured hint pairs for `entries` -- `(key_label,
/// first_action_name)` from the live bindings -- as `(key_label,
/// short_desc)`. Names absent from the action registry (e.g. the
/// `SetMode` boilerplate every mode carries on `Escape`) are dropped.
/// Returns `None` for non-transient modes or when no entry resolves.
fn hint_entries(mode: &str, entries: &[(String, String)]) -> Option<HintContent> {
    if !is_transient_mode(mode) {
        return None;
    }
    let bindings: Vec<(String, String)> = entries
        .iter()
        .filter_map(|(label, name)| {
            let def = registry::lookup(name)?.def;
            Some((label.clone(), def.short_desc().to_string()))
        })
        .collect();
    (!bindings.is_empty()).then(|| HintContent {
        mode: mode.to_string(),
        bindings,
    })
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
    fn hint_entries_resolves_pairs_and_drops_unregistered_actions() {
        let pairs = entries(&[
            ("i", "GotoFirstNonwhitespace"),
            ("Escape", "SetMode"),
            ("j", "GotoLastLine"),
        ]);
        let i_desc = registry::lookup("GotoFirstNonwhitespace")
            .expect("registered")
            .def
            .short_desc()
            .to_string();
        let j_desc = registry::lookup("GotoLastLine")
            .expect("registered")
            .def
            .short_desc()
            .to_string();
        assert_eq!(
            hint_entries("goto", &pairs),
            Some(HintContent {
                mode: "goto".to_string(),
                bindings: vec![("i".to_string(), i_desc), ("j".to_string(), j_desc)],
            })
        );
    }

    #[test]
    fn hint_entries_none_for_non_transient_mode() {
        let pairs = entries(&[("i", "GotoFirstNonwhitespace")]);
        assert_eq!(hint_entries("normal", &pairs), None);
        assert_eq!(hint_entries("insert", &pairs), None);
    }

    #[test]
    fn hint_entries_none_when_no_entry_resolves() {
        let pairs = entries(&[("Escape", "SetMode")]);
        assert_eq!(hint_entries("goto", &pairs), None);
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
        assert_eq!(hint.mode, "goto");
        assert!(
            hint.bindings.iter().any(|(key, _)| key == "i"),
            "expected goto motion key `i`: {:?}",
            hint.bindings,
        );
    }

    #[test]
    fn current_hint_hidden_in_normal_mode() {
        let mut cx = TestAppContext::single();
        let ism = state_machine(&mut cx);
        let banner = cx.update(|cx| cx.new(|cx| KeyHintBanner::new(ism.clone(), cx)));
        assert_eq!(banner.read_with(&cx, |b, cx| b.current_hint(cx)), None);
    }
}
