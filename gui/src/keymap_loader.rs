use crate::settings::Settings;
use stoat::keymap::Keymap;

/// Default `stcfg` source bundled with the GUI crate. Same file
/// the TUI loads from (`stoat/src/app.rs:42`); included here so
/// the GUI does not depend on stoat's private `DEFAULT_KEYMAP`
/// constant.
pub const DEFAULT_KEYMAP: &str = include_str!("../../config.stcfg");

/// Parse `source` as `stcfg` and compile the result into a
/// [`Keymap`]. Parse errors are reported via `tracing::error!`
/// and produce an empty `Keymap`, matching the TUI's fallback
/// behavior so a corrupt user config does not block startup.
pub fn compile_from_source(source: &str) -> Keymap {
    let (config, errors) = stoat_config::parse(source);
    if !errors.is_empty() {
        tracing::error!(
            target: "stoat::keymap",
            "stcfg parse errors: {}",
            stoat_config::format_errors(source, &errors),
        );
    }
    let config = config.unwrap_or_else(|| stoat_config::Config {
        blocks: Vec::new(),
        themes: Vec::new(),
    });
    Keymap::compile(&config)
}

/// Compile the bundled default keymap source. Convenience wrapper
/// over [`compile_from_source`] for callers that want the GUI's
/// out-of-the-box bindings.
pub fn compile_default_keymap() -> Keymap {
    compile_from_source(DEFAULT_KEYMAP)
}

/// Compile a keymap from the parsed `Config` already cached on
/// the [`Settings`] global. Avoids re-parsing the source string
/// on every settings change.
pub fn compile_from_settings(settings: &Settings) -> Keymap {
    Keymap::compile(&settings.config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use stoat::{
        keymap::{KeymapState, StateValue},
        keymap_state,
    };

    struct TestState {
        values: HashMap<String, StateValue>,
    }

    impl KeymapState for TestState {
        fn get(&self, field: &str) -> Option<&StateValue> {
            self.values.get(field)
        }
    }

    fn normal_state() -> TestState {
        let mut values = HashMap::new();
        values.insert("mode".into(), StateValue::String("normal".into()));
        TestState { values }
    }

    #[test]
    fn compile_default_keymap_returns_non_empty() {
        let keymap = compile_default_keymap();
        let bindings = keymap.active_bindings(&normal_state());
        assert!(
            !bindings.is_empty(),
            "default keymap should have bindings active in normal mode"
        );
    }

    #[test]
    fn compile_from_source_handles_parse_failure_with_empty() {
        let keymap = compile_from_source("not a valid @@@ stcfg");
        let bindings = keymap.active_bindings(&normal_state());
        assert!(
            bindings.is_empty(),
            "parse failure should produce empty keymap"
        );
    }

    #[test]
    fn compile_from_settings_uses_cached_config() {
        let settings = Settings::load_from_source("on key { x -> Quit(); }");
        let keymap = compile_from_settings(&settings);
        let bindings = keymap.active_bindings(&normal_state());
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].0, "x");
    }

    fn insert_state(claude_focused: bool) -> TestState {
        let mut values = HashMap::new();
        values.insert("mode".into(), StateValue::String("insert".into()));
        values.insert("claude_focused".into(), StateValue::Bool(claude_focused));
        TestState { values }
    }

    fn enter_event() -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        )
    }

    #[test]
    fn default_keymap_enter_in_editor_insert_accepts_completion() {
        let keymap = compile_default_keymap();
        let actions = keymap
            .lookup(&insert_state(false), &enter_event())
            .expect("Enter has a binding when claude is not focused");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "AcceptCompletion");
    }

    #[test]
    fn default_keymap_enter_in_chat_insert_submits_to_claude() {
        let keymap = compile_default_keymap();
        let actions = keymap
            .lookup(&insert_state(true), &enter_event())
            .expect("Enter has a binding when claude is focused");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "ClaudeSubmit");
    }

    fn ctrl_event(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::CONTROL,
        )
    }

    #[test]
    fn default_keymap_ctrl_a_in_insert_goes_to_line_start() {
        let keymap = compile_default_keymap();
        let actions = keymap
            .lookup(&insert_state(false), &ctrl_event('a'))
            .expect("Ctrl-a has an insert-mode binding");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "GotoLineStart");
    }

    #[test]
    fn default_keymap_ctrl_e_in_insert_goes_to_line_end() {
        let keymap = compile_default_keymap();
        let actions = keymap
            .lookup(&insert_state(false), &ctrl_event('e'))
            .expect("Ctrl-e has an insert-mode binding");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "GotoLineEnd");
    }

    fn alt_event(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::ALT)
    }

    #[test]
    fn default_keymap_alt_left_in_insert_moves_to_prev_word() {
        let keymap = compile_default_keymap();
        let actions = keymap
            .lookup(
                &insert_state(false),
                &alt_event(crossterm::event::KeyCode::Left),
            )
            .expect("Alt-Left has an insert-mode binding");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "MovePrevWordStart");
    }

    #[test]
    fn default_keymap_alt_right_in_insert_moves_to_next_word() {
        let keymap = compile_default_keymap();
        let actions = keymap
            .lookup(
                &insert_state(false),
                &alt_event(crossterm::event::KeyCode::Right),
            )
            .expect("Alt-Right has an insert-mode binding");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "MoveNextWordStart");
    }

    #[test]
    fn default_keymap_alt_backspace_in_insert_deletes_word_backward() {
        let keymap = compile_default_keymap();
        let actions = keymap
            .lookup(
                &insert_state(false),
                &alt_event(crossterm::event::KeyCode::Backspace),
            )
            .expect("Alt-Backspace has an insert-mode binding");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "DeleteWordBackward");
    }

    #[test]
    fn default_keymap_alt_delete_in_insert_deletes_word_forward() {
        let keymap = compile_default_keymap();
        let actions = keymap
            .lookup(
                &insert_state(false),
                &alt_event(crossterm::event::KeyCode::Delete),
            )
            .expect("Alt-Delete has an insert-mode binding");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "DeleteWordForward");
    }

    #[test]
    fn default_keymap_ctrl_z_in_insert_undoes() {
        let keymap = compile_default_keymap();
        let actions = keymap
            .lookup(&insert_state(false), &ctrl_event('z'))
            .expect("Ctrl-z has an insert-mode binding");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "Undo");
    }

    #[test]
    fn default_keymap_ctrl_shift_z_in_insert_redoes() {
        let keymap = compile_default_keymap();
        // Ctrl+Shift+z reaches lookup as {Char('Z'), CONTROL}: the input
        // pipeline uppercases shifted letters and drops SHIFT, so the redo
        // binding is Ctrl-Z, mirroring normal mode's `U -> Redo()`.
        let event = keymap_state::normalize_shift_event(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('z'),
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        ));
        let actions = keymap
            .lookup(&insert_state(false), &event)
            .expect("Ctrl-Shift-z has an insert-mode binding");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "Redo");
    }
}
