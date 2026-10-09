//! The two-state settings one action flips, which the key hints box marks on or
//! off beside the key that flips them.
//!
//! A hint row stores which toggle its binding flips, never the state, so the
//! rows stay cached across a flip. Each frame reads every state once into
//! [`ToggleStates`], and the paint looks the row's toggle up there.

use crate::{app::Stoat, keymap::ResolvedAction, keymap_state::resolve_focus};
use stoat_action::{registry, ActionKind};
use stoat_config::WrapMode;

/// A two-state setting that one action flips, and the same key turns back.
///
/// The hints box marks a row on or off when its binding flips one of these.
/// An action that only sets a state, or that cycles more than two, is no toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Toggle {
    FollowChanges,
    LiveReload,
    ChangeWalkWrap,
    DiffUnderline,
    DiffBold,
    DiffSyntax,
    KeyHints,
    SyntaxHighlight,
    LspStatus,
    InlayHints,
    MacroRecording,
    DiffView,
    PaneWiden,
    Wrap,
    Minimap,
    TabBar,
}

impl Toggle {
    /// Every toggle, in declaration order.
    pub(crate) const ALL: [Toggle; 16] = [
        Toggle::FollowChanges,
        Toggle::LiveReload,
        Toggle::ChangeWalkWrap,
        Toggle::DiffUnderline,
        Toggle::DiffBold,
        Toggle::DiffSyntax,
        Toggle::KeyHints,
        Toggle::SyntaxHighlight,
        Toggle::LspStatus,
        Toggle::InlayHints,
        Toggle::MacroRecording,
        Toggle::DiffView,
        Toggle::PaneWiden,
        Toggle::Wrap,
        Toggle::Minimap,
        Toggle::TabBar,
    ];

    /// The toggle an action of `kind` flips, or `None` for an action that flips
    /// no two-state setting.
    fn for_kind(kind: ActionKind) -> Option<Self> {
        Some(match kind {
            ActionKind::FollowChanges => Toggle::FollowChanges,
            ActionKind::LiveReload => Toggle::LiveReload,
            ActionKind::ChangeWalkWrap => Toggle::ChangeWalkWrap,
            ActionKind::DiffUnderline => Toggle::DiffUnderline,
            ActionKind::DiffBold => Toggle::DiffBold,
            ActionKind::DiffSyntax => Toggle::DiffSyntax,
            ActionKind::ToggleKeyHints => Toggle::KeyHints,
            ActionKind::ToggleSyntaxHighlight => Toggle::SyntaxHighlight,
            ActionKind::ToggleLspStatus => Toggle::LspStatus,
            ActionKind::ToggleInlayHints => Toggle::InlayHints,
            ActionKind::RecordMacro => Toggle::MacroRecording,
            ActionKind::Diff => Toggle::DiffView,
            ActionKind::TogglePaneWiden => Toggle::PaneWiden,
            ActionKind::ToggleWrap => Toggle::Wrap,
            ActionKind::ToggleMinimap => Toggle::Minimap,
            ActionKind::ToggleTabBar => Toggle::TabBar,
            _ => return None,
        })
    }

    /// Whether the setting is on, read as the action that flips it reads it.
    ///
    /// A focused pane with no editor reads [`Toggle::DiffView`] and
    /// [`Toggle::Wrap`] as off.
    fn is_on(self, stoat: &Stoat) -> bool {
        let ws = stoat.active_workspace();
        match self {
            Toggle::FollowChanges => stoat.follow_changes,
            Toggle::LiveReload => stoat.live_reload,
            Toggle::ChangeWalkWrap => stoat.change_walk_wrap,
            Toggle::DiffUnderline => stoat.diff_underline,
            Toggle::DiffBold => stoat.diff_bold,
            Toggle::DiffSyntax => stoat.diff_syntax,
            Toggle::KeyHints => stoat.key_hints_visible,
            Toggle::SyntaxHighlight => stoat.syntax_highlight,
            Toggle::LspStatus => stoat.lsp_status_pinned,
            Toggle::InlayHints => stoat.inlay_hints_enabled,
            Toggle::MacroRecording => stoat.macro_recording.is_some(),
            Toggle::DiffView => {
                ws.panes.pane(ws.panes.focus()).diff_mode
                    || resolve_focus(ws).is_some_and(|(editor, _)| editor.diff_view)
            },
            Toggle::PaneWiden => ws.panes.widened() == Some(ws.panes.focus()),
            Toggle::Wrap => resolve_focus(ws).is_some_and(|(editor, _)| {
                editor
                    .wrap_override
                    .or(stoat.settings.editor_wrap)
                    .unwrap_or(WrapMode::EditorWidth)
                    != WrapMode::None
            }),
            Toggle::Minimap => stoat.minimap_enabled(),
            Toggle::TabBar => stoat.tab_bar_visible(),
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
    use crate::{
        action_handlers::{dispatch, focused_editor_mut},
        keymap::{ResolvedAction, ResolvedArg},
        Stoat,
    };
    use stoat_action::{ToggleMinimap, TogglePaneWiden, ToggleTabBar, ToggleWrap};
    use stoat_config::Value;

    fn action(name: &str) -> ResolvedAction {
        ResolvedAction {
            name: name.to_string(),
            args: Vec::new(),
        }
    }

    /// Every toggle that is on in `stoat`, in [`Toggle::ALL`] order.
    fn on(stoat: &Stoat) -> Vec<Toggle> {
        let states = ToggleStates::read(stoat);
        Toggle::ALL
            .into_iter()
            .filter(|toggle| states.is_on(*toggle))
            .collect()
    }

    #[test]
    fn binding_toggle_reads_the_action_that_names_the_row() {
        let diff_head = ResolvedAction {
            name: "Diff".to_string(),
            args: vec![ResolvedArg {
                name: None,
                value: Value::String("HEAD".into()),
            }],
        };
        assert_eq!(
            [
                binding_toggle(&[action("FollowChanges"), action("SetMode")]),
                binding_toggle(&[action("SetMode"), action("ToggleInlayHints")]),
                binding_toggle(&[action("GotoNextChange")]),
                binding_toggle(&[action("SetMode")]),
                binding_toggle(&[action("Diff")]),
                binding_toggle(&[diff_head]),
            ],
            [
                Some(Toggle::FollowChanges),
                Some(Toggle::InlayHints),
                None,
                None,
                Some(Toggle::DiffView),
                None,
            ],
        );
    }

    /// The underline, bold, and syntax dials each flip a flag, so their rows
    /// carry a mark. A tint step moves a level of three states, so it flips no
    /// toggle.
    #[test]
    fn a_diff_dial_binding_reads_as_its_toggle() {
        assert_eq!(
            [
                binding_toggle(&[action("DiffUnderline")]),
                binding_toggle(&[action("DiffBold")]),
                binding_toggle(&[action("DiffSyntax")]),
                binding_toggle(&[action("DiffTintDown")]),
                binding_toggle(&[action("DiffTintUp")]),
            ],
            [
                Some(Toggle::DiffUnderline),
                Some(Toggle::DiffBold),
                Some(Toggle::DiffSyntax),
                None,
                None,
            ],
        );
    }

    #[test]
    fn toggle_states_read_the_session_flags() {
        let mut h = Stoat::test();
        assert_eq!(
            on(&h.stoat),
            [
                Toggle::ChangeWalkWrap,
                Toggle::DiffSyntax,
                Toggle::SyntaxHighlight,
                Toggle::Wrap,
                Toggle::Minimap,
            ],
            "the flags a lone scratch pane starts with",
        );

        h.stoat.follow_changes = true;
        h.stoat.live_reload = true;
        h.stoat.diff_underline = true;
        h.stoat.diff_bold = true;
        h.stoat.diff_syntax = false;
        h.stoat.inlay_hints_enabled = true;
        assert_eq!(
            on(&h.stoat),
            [
                Toggle::FollowChanges,
                Toggle::LiveReload,
                Toggle::ChangeWalkWrap,
                Toggle::DiffUnderline,
                Toggle::DiffBold,
                Toggle::SyntaxHighlight,
                Toggle::InlayHints,
                Toggle::Wrap,
                Toggle::Minimap,
            ],
        );
    }

    #[test]
    fn toggle_states_read_the_view_flags() {
        let mut h = Stoat::test();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        dispatch(&mut h.stoat, &TogglePaneWiden);
        dispatch(&mut h.stoat, &ToggleWrap);
        dispatch(&mut h.stoat, &ToggleMinimap);
        dispatch(&mut h.stoat, &ToggleTabBar);

        assert_eq!(
            on(&h.stoat),
            [
                Toggle::ChangeWalkWrap,
                Toggle::DiffSyntax,
                Toggle::SyntaxHighlight,
                Toggle::DiffView,
                Toggle::PaneWiden,
                Toggle::TabBar,
            ],
            "each view toggle reads the state its action flipped",
        );
    }
}
