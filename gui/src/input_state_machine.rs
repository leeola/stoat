use crate::workspace::Workspace;
use gpui::{FocusHandle, WeakEntity};
use stoat::keymap::{Keymap, KeymapState, StateValue};
use stoat_config::KeyPart;

/// Operator-pending state for multi-stage chords. Variants land
/// with the actions that need them (e.g. textobject selection,
/// surround edits). Empty today; the field is kept to give those
/// follow-ups a place to plug in without reshuffling
/// [`InputStateMachine`].
#[derive(Debug)]
pub enum Operator {}

/// Workspace-hosted entity that owns every per-keystroke piece of
/// state the GUI input pipeline needs. Predicate-visible fields
/// mirror `StoatKeymapState` so the same `stoat::keymap` engine
/// drives both surfaces; matcher state holds the in-progress
/// chord and count between keystrokes; the workspace handle and
/// owned [`Keymap`] are the dispatch hooks the surrounding
/// machinery (observe_keystrokes, dispatch_action, sequence
/// lowering) will reach for in subsequent items.
///
/// The five predicate-visible fields are stored as [`StateValue`]
/// carriers so [`KeymapState::get`] can hand out borrows directly,
/// matching `StoatKeymapState`'s storage layout.
pub struct InputStateMachine {
    mode: StateValue,
    palette_open: StateValue,
    finder_open: StateValue,
    help_open: StateValue,
    claude_focused: StateValue,
    pending_count: Option<u32>,
    pending_chord: Vec<KeyPart>,
    pending_operator: Option<Operator>,
    prev_focused: Option<FocusHandle>,
    workspace: WeakEntity<Workspace>,
    keymap: Keymap,
}

impl InputStateMachine {
    pub fn new(workspace: WeakEntity<Workspace>, keymap: Keymap) -> Self {
        Self {
            mode: StateValue::String("normal".into()),
            palette_open: StateValue::Bool(false),
            finder_open: StateValue::Bool(false),
            help_open: StateValue::Bool(false),
            claude_focused: StateValue::Bool(false),
            pending_count: None,
            pending_chord: Vec::new(),
            pending_operator: None,
            prev_focused: None,
            workspace,
            keymap,
        }
    }

    pub fn mode(&self) -> &str {
        match &self.mode {
            StateValue::String(s) => s.as_str(),
            _ => "",
        }
    }

    pub fn palette_open(&self) -> bool {
        matches!(self.palette_open, StateValue::Bool(true))
    }

    pub fn finder_open(&self) -> bool {
        matches!(self.finder_open, StateValue::Bool(true))
    }

    pub fn help_open(&self) -> bool {
        matches!(self.help_open, StateValue::Bool(true))
    }

    pub fn claude_focused(&self) -> bool {
        matches!(self.claude_focused, StateValue::Bool(true))
    }

    pub fn pending_count(&self) -> Option<u32> {
        self.pending_count
    }

    pub fn pending_chord(&self) -> &[KeyPart] {
        &self.pending_chord
    }

    pub fn pending_operator(&self) -> Option<&Operator> {
        self.pending_operator.as_ref()
    }

    pub fn prev_focused(&self) -> Option<&FocusHandle> {
        self.prev_focused.as_ref()
    }

    pub fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }
}

impl KeymapState for InputStateMachine {
    fn get(&self, field: &str) -> Option<&StateValue> {
        match field {
            "mode" => Some(&self.mode),
            "palette_open" => Some(&self.palette_open),
            "finder_open" => Some(&self.finder_open),
            "help_open" => Some(&self.help_open),
            "claude_focused" => Some(&self.claude_focused),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, TestAppContext};
    use std::path::PathBuf;
    use stoat_config::Config;

    fn empty_keymap() -> Keymap {
        Keymap::compile(&Config {
            blocks: Vec::new(),
            themes: Vec::new(),
        })
    }

    fn new_state_machine(cx: &mut TestAppContext) -> Entity<InputStateMachine> {
        cx.update(|cx| {
            let workspace = cx.new(|cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx));
            cx.new(|_| InputStateMachine::new(workspace.downgrade(), empty_keymap()))
        })
    }

    #[test]
    fn defaults() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        sm.read_with(&cx, |sm, _| {
            assert_eq!(sm.mode(), "normal");
            assert!(!sm.palette_open());
            assert!(!sm.finder_open());
            assert!(!sm.help_open());
            assert!(!sm.claude_focused());
            assert_eq!(sm.pending_count(), None);
            assert!(sm.pending_chord().is_empty());
            assert!(sm.pending_operator().is_none());
            assert!(sm.prev_focused().is_none());
        });
    }

    #[test]
    fn keymap_state_get_returns_predicate_fields() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        sm.read_with(&cx, |sm, _| {
            assert_eq!(sm.get("mode"), Some(&StateValue::String("normal".into())));
            assert_eq!(sm.get("palette_open"), Some(&StateValue::Bool(false)));
            assert_eq!(sm.get("finder_open"), Some(&StateValue::Bool(false)));
            assert_eq!(sm.get("help_open"), Some(&StateValue::Bool(false)));
            assert_eq!(sm.get("claude_focused"), Some(&StateValue::Bool(false)));
            assert_eq!(sm.get("unknown"), None);
        });
    }
}
