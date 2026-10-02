//! The two-state settings one action flips, which the key hints box marks on or
//! off beside the key that flips them.
//!
//! A hint row stores which toggle its binding flips, never the state, so the
//! rows stay cached across a flip. Each frame reads every state once into
//! [`ToggleStates`], and the paint looks the row's toggle up there.

use crate::{app::Stoat, keymap::ResolvedAction};
use stoat_action::{registry, ActionKind};

/// A two-state setting that one action flips, and the same key turns back.
///
/// The hints box marks a row on or off when its binding flips one of these.
/// An action that only sets a state, or that cycles more than two, is no toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Toggle {
    FollowChanges,
    LiveReload,
    DiffWheelWalk,
    KeyHints,
    SyntaxHighlight,
    LspStatus,
    InlayHints,
    MacroRecording,
}

impl Toggle {
    /// Every toggle, in declaration order.
    pub(crate) const ALL: [Toggle; 8] = [
        Toggle::FollowChanges,
        Toggle::LiveReload,
        Toggle::DiffWheelWalk,
        Toggle::KeyHints,
        Toggle::SyntaxHighlight,
        Toggle::LspStatus,
        Toggle::InlayHints,
        Toggle::MacroRecording,
    ];

    /// The toggle an action of `kind` flips, or `None` for an action that flips
    /// no two-state setting.
    fn for_kind(kind: ActionKind) -> Option<Self> {
        Some(match kind {
            ActionKind::FollowChanges => Toggle::FollowChanges,
            ActionKind::LiveReload => Toggle::LiveReload,
            ActionKind::DiffWheelWalk => Toggle::DiffWheelWalk,
            ActionKind::ToggleKeyHints => Toggle::KeyHints,
            ActionKind::ToggleSyntaxHighlight => Toggle::SyntaxHighlight,
            ActionKind::ToggleLspStatus => Toggle::LspStatus,
            ActionKind::ToggleInlayHints => Toggle::InlayHints,
            ActionKind::RecordMacro => Toggle::MacroRecording,
            _ => return None,
        })
    }

    fn is_on(self, stoat: &Stoat) -> bool {
        match self {
            Toggle::FollowChanges => stoat.follow_changes,
            Toggle::LiveReload => stoat.live_reload,
            Toggle::DiffWheelWalk => stoat.diff_wheel_walk,
            Toggle::KeyHints => stoat.key_hints_visible,
            Toggle::SyntaxHighlight => stoat.syntax_highlight,
            Toggle::LspStatus => stoat.lsp_status_pinned,
            Toggle::InlayHints => stoat.inlay_hints_enabled,
            Toggle::MacroRecording => stoat.macro_recording.is_some(),
        }
    }
}

/// Which toggles are on, read once per frame so the paint needs no borrow of
/// the app.
///
/// One bit per [`Toggle`], at the toggle's declaration index.
#[derive(Clone, Copy, Default)]
pub(crate) struct ToggleStates(u32);

impl ToggleStates {
    /// The state of every [`Toggle`] in `stoat` now.
    pub(crate) fn read(stoat: &Stoat) -> Self {
        Toggle::ALL
            .into_iter()
            .filter(|toggle| toggle.is_on(stoat))
            .collect()
    }

    pub(crate) fn is_on(self, toggle: Toggle) -> bool {
        self.0 & (1 << toggle as u32) != 0
    }
}

impl FromIterator<Toggle> for ToggleStates {
    fn from_iter<I: IntoIterator<Item = Toggle>>(toggles: I) -> Self {
        Self(
            toggles
                .into_iter()
                .fold(0, |bits, toggle| bits | 1 << toggle as u32),
        )
    }
}

/// The toggle a binding flips, read off the action that names its hint row.
///
/// That action is the first one that is not a mode switch, as in
/// [`crate::keymap_state::binding_display_desc`]. An action called with an
/// argument answers `None`, because an argument names a target instead of
/// flipping a state.
pub(crate) fn binding_toggle(actions: &[ResolvedAction]) -> Option<Toggle> {
    let action = actions.iter().find(|action| action.name != "SetMode")?;
    if !action.args.is_empty() {
        return None;
    }
    Toggle::for_kind(registry::lookup(&action.name)?.def.kind())
}

#[cfg(test)]
mod tests {
    use super::{binding_toggle, Toggle, ToggleStates};
    use crate::{keymap::ResolvedAction, Stoat};

    fn action(name: &str) -> ResolvedAction {
        ResolvedAction {
            name: name.to_string(),
            args: Vec::new(),
        }
    }

    #[test]
    fn binding_toggle_reads_the_action_that_names_the_row() {
        assert_eq!(
            [
                binding_toggle(&[action("FollowChanges"), action("SetMode")]),
                binding_toggle(&[action("SetMode"), action("ToggleInlayHints")]),
                binding_toggle(&[action("GotoNextChange")]),
                binding_toggle(&[action("SetMode")]),
            ],
            [
                Some(Toggle::FollowChanges),
                Some(Toggle::InlayHints),
                None,
                None
            ],
        );
    }

    #[test]
    fn toggle_states_read_the_session_flags() {
        let mut h = Stoat::test();
        let on = |stoat: &Stoat| -> Vec<Toggle> {
            let states = ToggleStates::read(stoat);
            Toggle::ALL
                .into_iter()
                .filter(|toggle| states.is_on(*toggle))
                .collect()
        };
        assert_eq!(
            on(&h.stoat),
            [Toggle::DiffWheelWalk, Toggle::SyntaxHighlight],
            "the two flags that start on",
        );

        h.stoat.follow_changes = true;
        h.stoat.live_reload = true;
        h.stoat.inlay_hints_enabled = true;
        assert_eq!(
            on(&h.stoat),
            [
                Toggle::FollowChanges,
                Toggle::LiveReload,
                Toggle::DiffWheelWalk,
                Toggle::SyntaxHighlight,
                Toggle::InlayHints,
            ],
        );
    }
}
