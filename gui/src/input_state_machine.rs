use crate::workspace::Workspace;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gpui::{Context, FocusHandle, Keystroke, WeakEntity, Window};
use stoat::{
    keymap::{Keymap, KeymapState, StateValue},
    keymap_state::{normalize_shift_event, resolve_action},
};
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

    /// Drive one platform keystroke through the input pipeline:
    /// translate it to the crossterm shape the keymap engine matches
    /// against, fold an ASCII digit into the pending count when one
    /// is in flight (normal/select modes only), look up bindings
    /// against `self` as the [`KeymapState`], resolve each match into
    /// a [`stoat_action::Action`] via [`resolve_action`], and forward
    /// the resolved actions to [`Workspace::dispatch_action`].
    ///
    /// Keystrokes the crossterm shape cannot represent (modifier-only
    /// events, unknown named keys) are silently dropped. Unknown
    /// action names and bad arg shapes are dropped after a
    /// `tracing::warn` inside [`resolve_action`].
    pub fn feed(&mut self, keystroke: &Keystroke, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(event) = keystroke_to_key_event(keystroke) else {
            return;
        };
        let event = normalize_shift_event(event);

        let count_active_mode = self.mode() == "normal" || self.mode() == "select";
        let digit = unmodified_ascii_digit(&event);

        if count_active_mode && self.pending_count.is_some() {
            if let Some(d) = digit {
                let new_count = self
                    .pending_count
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(d);
                self.pending_count = Some(new_count);
                cx.notify();
                return;
            }
        }

        let actions = self
            .keymap
            .lookup(self, &event)
            .map(<[_]>::to_vec)
            .unwrap_or_default();

        if actions.is_empty() {
            if count_active_mode {
                if let Some(d) = digit {
                    self.pending_count = Some(d);
                    cx.notify();
                }
            }
            return;
        }

        let workspace = self.workspace.clone();
        let mut dispatched = false;
        for ra in &actions {
            if let Some(action) = resolve_action(&ra.name, &ra.args) {
                dispatched = true;
                let _ = workspace.update(cx, |ws, cx| ws.dispatch_action(action, window, cx));
            }
        }

        if dispatched && self.pending_count.is_some() {
            self.pending_count = None;
            cx.notify();
        }
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

fn keystroke_to_key_event(keystroke: &Keystroke) -> Option<KeyEvent> {
    let mut modifiers = KeyModifiers::empty();
    if keystroke.modifiers.control {
        modifiers |= KeyModifiers::CONTROL;
    }
    if keystroke.modifiers.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if keystroke.modifiers.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    if keystroke.modifiers.platform {
        modifiers |= KeyModifiers::SUPER;
    }

    let code = match keystroke.key.as_str() {
        "space" => KeyCode::Char(' '),
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        s if function_key_index(s).is_some() => KeyCode::F(function_key_index(s)?),
        s => {
            let mut chars = s.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(first)
        },
    };

    Some(KeyEvent::new(code, modifiers))
}

fn function_key_index(key: &str) -> Option<u8> {
    let rest = key.strip_prefix('f')?;
    if rest.is_empty() {
        return None;
    }
    rest.parse().ok()
}

fn unmodified_ascii_digit(event: &KeyEvent) -> Option<u32> {
    if !event.modifiers.is_empty() {
        return None;
    }
    match event.code {
        KeyCode::Char(ch) if ch.is_ascii_digit() => ch.to_digit(10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Entity, Modifiers, TestAppContext};
    use std::path::PathBuf;
    use stoat_config::Config;

    fn empty_keymap() -> Keymap {
        Keymap::compile(&Config {
            blocks: Vec::new(),
            themes: Vec::new(),
        })
    }

    fn compile_keymap(src: &str) -> Keymap {
        let (config, errors) = stoat_config::parse(src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        Keymap::compile(&config.expect("expected config"))
    }

    fn new_workspace(cx: &mut TestAppContext) -> Entity<Workspace> {
        cx.update(|cx| cx.new(|cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx)))
    }

    fn new_state_machine_with_keymap(
        cx: &mut TestAppContext,
        keymap: Keymap,
    ) -> Entity<InputStateMachine> {
        let workspace = new_workspace(cx);
        cx.update(|cx| cx.new(|_| InputStateMachine::new(workspace.downgrade(), keymap)))
    }

    fn new_state_machine(cx: &mut TestAppContext) -> Entity<InputStateMachine> {
        new_state_machine_with_keymap(cx, empty_keymap())
    }

    fn key(name: &str) -> Keystroke {
        Keystroke {
            modifiers: Modifiers::default(),
            key: name.into(),
            key_char: None,
        }
    }

    fn key_with(name: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: name.into(),
            key_char: None,
        }
    }

    fn feed_in_window<R>(
        cx: &mut TestAppContext,
        sm: &Entity<InputStateMachine>,
        f: impl FnOnce(&mut InputStateMachine, &mut Window, &mut Context<'_, InputStateMachine>) -> R,
    ) -> R {
        let (_, vcx) = cx.add_window_view(|_window, _cx| gpui::Empty);
        sm.update_in(vcx, f)
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

    #[test]
    fn feed_digit_seeds_pending_count() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let stroke = key("5");
        feed_in_window(&mut cx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), Some(5)));
    }

    #[test]
    fn feed_digit_extends_pending_count() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let first = key("5");
        let second = key("2");
        feed_in_window(&mut cx, &sm, |sm, window, cx| {
            sm.feed(&first, window, cx);
            sm.feed(&second, window, cx);
        });
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), Some(52)));
    }

    #[test]
    fn feed_digit_in_insert_mode_does_not_seed_count() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        sm.update(&mut cx, |sm, _| {
            sm.mode = StateValue::String("insert".into());
        });
        let stroke = key("5");
        feed_in_window(&mut cx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn feed_modified_digit_does_not_seed_count() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let stroke = key_with(
            "5",
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        );
        feed_in_window(&mut cx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn feed_unmapped_key_is_no_op() {
        let mut cx = TestAppContext::single();
        let sm = new_state_machine(&mut cx);
        let stroke = key("q");
        feed_in_window(&mut cx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn feed_matched_action_clears_pending_count() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { q -> Quit(); }");
        let sm = new_state_machine_with_keymap(&mut cx, keymap);
        sm.update(&mut cx, |sm, _| sm.pending_count = Some(3));
        let stroke = key("q");
        feed_in_window(&mut cx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn feed_uppercase_letter_normalizes_shift() {
        let mut cx = TestAppContext::single();
        let keymap = compile_keymap("on key { G -> Quit(); }");
        let sm = new_state_machine_with_keymap(&mut cx, keymap);
        sm.update(&mut cx, |sm, _| sm.pending_count = Some(3));
        let stroke = key_with(
            "g",
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        );
        feed_in_window(&mut cx, &sm, |sm, window, cx| sm.feed(&stroke, window, cx));
        sm.read_with(&cx, |sm, _| assert_eq!(sm.pending_count(), None));
    }

    #[test]
    fn keystroke_to_key_event_handles_named_keys() {
        for (name, expected_code) in [
            ("space", KeyCode::Char(' ')),
            ("enter", KeyCode::Enter),
            ("escape", KeyCode::Esc),
            ("tab", KeyCode::Tab),
            ("backspace", KeyCode::Backspace),
            ("delete", KeyCode::Delete),
            ("up", KeyCode::Up),
            ("down", KeyCode::Down),
            ("left", KeyCode::Left),
            ("right", KeyCode::Right),
            ("f1", KeyCode::F(1)),
            ("f12", KeyCode::F(12)),
        ] {
            let stroke = key(name);
            let event = keystroke_to_key_event(&stroke).expect(name);
            assert_eq!(event.code, expected_code, "code for {name}");
            assert_eq!(event.modifiers, KeyModifiers::empty(), "mods for {name}");
        }
    }

    #[test]
    fn keystroke_to_key_event_maps_all_modifiers() {
        let stroke = key_with(
            "a",
            Modifiers {
                control: true,
                alt: true,
                shift: true,
                platform: true,
                function: false,
            },
        );
        let event = keystroke_to_key_event(&stroke).expect("translate");
        assert_eq!(event.code, KeyCode::Char('a'));
        assert_eq!(
            event.modifiers,
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER
        );
    }

    #[test]
    fn keystroke_to_key_event_rejects_multi_char_unknown() {
        let stroke = key("noodle");
        assert!(keystroke_to_key_event(&stroke).is_none());
    }
}
