use crate::{
    editor::render::color_to_hsla, input_state_machine::InputStateMachine, item::ItemHandle,
    settings::Settings, status_bar::StatusItemView, theme::Theme,
};
use gpui::{
    div, rgb, Context, Entity, Hsla, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, Window,
};
use std::borrow::Cow;

/// Status bar item that surfaces the active editor mode.
/// Renders a 3-letter badge (`NOR`, `INS`, `SEL`, ...) over a
/// per-mode background color. Subscribes to
/// [`InputStateMachine`] notifications via `cx.observe` so a mode
/// transition re-renders the badge immediately.
pub struct ModeBadge {
    input_state_machine: Entity<InputStateMachine>,
    _subscription: Subscription,
}

impl ModeBadge {
    pub fn new(input_state_machine: Entity<InputStateMachine>, cx: &mut Context<'_, Self>) -> Self {
        let subscription = cx.observe(&input_state_machine, |_, _, cx| cx.notify());
        Self {
            input_state_machine,
            _subscription: subscription,
        }
    }
}

impl Render for ModeBadge {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let mode = self.input_state_machine.read(cx).mode().to_string();
        let badge_override = cx
            .try_global::<Settings>()
            .and_then(|s| s.resolved.mode_badges.get(&mode).cloned());
        let (default_label, default_bg_hex, scope) = mode_descriptor(&mode);
        let label = badge_override
            .map(Cow::<'static, str>::Owned)
            .unwrap_or_else(|| Cow::Borrowed(default_label));
        let bg = theme_color(cx, scope).unwrap_or_else(|| rgb(default_bg_hex).into());
        let fg = theme_color(cx, stoat::theme::scope::UI_MODE_LABEL)
            .unwrap_or_else(|| rgb(0xffffff).into());
        div()
            .px_2()
            .bg(bg)
            .text_color(fg)
            .child(SharedString::from(label.into_owned()))
    }
}

impl StatusItemView for ModeBadge {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _cx: &mut Context<'_, Self>,
    ) {
    }
}

fn theme_color(cx: &gpui::App, scope: &str) -> Option<Hsla> {
    let theme = cx.try_global::<Theme>()?;
    let style = theme.0.try_get(scope)?;
    color_to_hsla(style.fg?)
}

/// Per-mode descriptor: 3-letter abbreviation, default
/// background color, and the theme scope to consult before
/// falling back to the default.
///
/// Mirrors the mapping in `stoat::render::pane::mode_segment`.
/// Duplicated rather than shared because reaching the TUI's
/// function would require lifting `stoat::render` (and its
/// children) to `pub`, a wider surface than this badge warrants.
fn mode_descriptor(mode: &str) -> (&'static str, u32, &'static str) {
    use stoat::theme::scope;
    match mode {
        "normal" => ("NOR", 0x0000ff, scope::UI_STATUSLINE_NORMAL),
        "insert" => ("INS", 0x00ff00, scope::UI_STATUSLINE_INSERT),
        "select" => ("SEL", 0xffff00, scope::UI_STATUSLINE_SELECT),
        "prompt" => ("PMT", 0x00ff00, scope::UI_STATUSLINE_PROMPT),
        "run" => ("RUN", 0xff00ff, scope::UI_STATUSLINE_RUN),
        "commits" => ("COM", 0xffff00, scope::UI_STATUSLINE_COMMITS),
        "rebase" => ("REB", 0xff0000, scope::UI_STATUSLINE_REBASE),
        "reword" | "reword_insert" => ("RWD", 0xff0000, scope::UI_STATUSLINE_REWORD),
        "conflict" => ("CNF", 0xff5555, scope::UI_STATUSLINE_CONFLICT),
        "review" => ("REV", 0x00ffff, scope::UI_STATUSLINE_REVIEW),
        "goto" | "z" | "bracket_next" | "bracket_prev" | "match" | "select_goto" | "space"
        | "space_workspace" | "space_pane_nav" | "space_pane_nav_new" | "claude"
        | "project_tree" => (submode_label(mode), 0x808080, scope::UI_STATUSLINE_SUBMODE),
        _ => ("---", 0xc0c0c0, scope::UI_STATUSLINE_DEFAULT),
    }
}

fn submode_label(mode: &str) -> &'static str {
    match mode {
        "goto" => "GTO",
        "z" => "VWA",
        "bracket_next" => "BNX",
        "bracket_prev" => "BPV",
        "match" => "MAT",
        "select_goto" => "SLG",
        "space" => "SPC",
        "space_workspace" => "SWS",
        "space_pane_nav" => "SPN",
        "space_pane_nav_new" => "SNN",
        "claude" => "CLA",
        "project_tree" => "TRE",
        _ => "---",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use gpui::{AppContext, TestAppContext};
    use std::path::PathBuf;

    fn new_state_machine(cx: &mut TestAppContext) -> Entity<InputStateMachine> {
        let workspace =
            cx.update(|cx| cx.new(|cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx)));
        workspace.read_with(cx, |w, _| w.input_state_machine().clone())
    }

    #[test]
    fn mode_descriptor_returns_label_for_known_modes() {
        assert_eq!(mode_descriptor("normal").0, "NOR");
        assert_eq!(mode_descriptor("insert").0, "INS");
        assert_eq!(mode_descriptor("select").0, "SEL");
        assert_eq!(mode_descriptor("space").0, "SPC");
        assert_eq!(mode_descriptor("unknown").0, "---");
    }

    #[test]
    fn new_subscribes_to_state_machine_notifications() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let badge = cx.update(|cx| cx.new(|cx| ModeBadge::new(sm.clone(), cx)));
        badge.read_with(&cx, |b, cx| {
            assert_eq!(b.input_state_machine.read(cx).mode(), "normal");
        });
    }

    #[test]
    fn render_reflects_current_mode_label() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let (_badge, vcx) = cx.add_window_view(|_, cx| ModeBadge::new(sm.clone(), cx));
        vcx.run_until_parked();

        sm.update(vcx, |sm, _| {
            sm.set_mode_for_test(stoat::keymap::StateValue::String("insert".into()))
        });
        vcx.run_until_parked();
        sm.read_with(vcx, |sm, _| assert_eq!(sm.mode(), "insert"));
    }

    #[test]
    fn set_active_pane_item_is_noop() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let badge = cx.update(|cx| cx.new(|cx| ModeBadge::new(sm.clone(), cx)));
        cx.update(|cx| {
            badge.update(cx, |b, cx| b.set_active_pane_item(None, cx));
        });
        badge.read_with(&cx, |b, cx| {
            assert_eq!(b.input_state_machine.read(cx).mode(), "normal");
        });
    }
}
