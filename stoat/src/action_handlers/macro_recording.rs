use crate::{
    app::{Stoat, UpdateEffect},
    register::Register,
};
use crossterm::event::{Event, KeyEvent};

/// State for an in-progress macro recording. The `keys` vector grows
/// every time `Stoat::handle_key` accepts a key while this is `Some`,
/// excluding the `Q` keypress that toggles recording itself.
pub(crate) struct MacroRecording {
    pub(crate) register: Register,
    pub(crate) keys: Vec<KeyEvent>,
}

/// Toggle recording. Off -> start recording into the most-recently
/// selected register (or [`Register::Unnamed`] when none was set);
/// on -> stop, store the captured key sequence on
/// [`Stoat::macros`].
pub(super) fn toggle_record(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(rec) = stoat.macro_recording.take() {
        stoat.macros.insert(rec.register, rec.keys);
    } else {
        let register = stoat.consume_selected_register();
        stoat.macro_recording = Some(MacroRecording {
            register,
            keys: Vec::new(),
        });
    }
    UpdateEffect::Redraw
}

/// Arm the replay chord. The next char keypress in normal/select
/// mode names a register and triggers [`execute_replay`].
pub(super) fn arm_replay(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_macro_replay = true;
    UpdateEffect::Redraw
}

/// Resolve the register from `ch` and replay its stored macro by
/// re-feeding each captured [`KeyEvent`] through [`Stoat::update`].
/// No-op when the register is empty or unnamed.
///
/// Replaying a register already partway through a replay is refused. The keys
/// are re-fed through the path that started this one, so a macro naming itself,
/// directly or through another, would otherwise never stop.
pub(crate) fn execute_replay(stoat: &mut Stoat, ch: char) -> UpdateEffect {
    let Some(register) = super::yank::register_for_char(ch) else {
        return UpdateEffect::None;
    };
    if stoat.replaying_registers.contains(&register) {
        return UpdateEffect::None;
    }
    let Some(keys) = stoat.macros.get(&register).cloned() else {
        return UpdateEffect::None;
    };

    stoat.replaying_registers.push(register);
    let mut effect = UpdateEffect::None;
    for key in keys {
        let outcome = stoat.update(Event::Key(key));
        if matches!(outcome, UpdateEffect::Quit) {
            stoat.replaying_registers.pop();
            return UpdateEffect::Quit;
        }
        if matches!(outcome, UpdateEffect::Redraw) {
            effect = UpdateEffect::Redraw;
        }
    }
    stoat.replaying_registers.pop();
    effect
}

/// Append `key` to the active recording's key buffer. No-op when no
/// recording is in progress. Called from [`Stoat::handle_key`]
/// before chord dispatch so every keypress between `Q` toggles is
/// captured.
///
/// Keys a replay re-feeds are skipped. They are the expansion of the one key
/// that named the register, which was itself captured, so recording them too
/// would store the inner macro's body inline where its name belongs.
pub(crate) fn capture(stoat: &mut Stoat, key: &KeyEvent) {
    if !stoat.replaying_registers.is_empty() {
        return;
    }
    if let Some(rec) = stoat.macro_recording.as_mut() {
        rec.keys.push(*key);
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::{dispatch, focused_editor_mut},
        test_harness::keys,
        Stoat,
    };
    use crossterm::event::{Event, KeyCode};
    use stoat_action as action;

    fn primary_offset(h: &mut crate::test_harness::TestHarness) -> usize {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        stoat_text::cursor_offset(
            buf_snap.rope(),
            buf_snap.resolve_anchor(&sel.tail()),
            buf_snap.resolve_anchor(&sel.head()),
        )
    }

    #[test]
    fn record_then_replay_repeats_keys() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world");
        dispatch(&mut h.stoat, &action::RecordMacro);
        h.type_keys("l l l");
        dispatch(&mut h.stoat, &action::RecordMacro);
        assert_eq!(primary_offset(&mut h), 3);

        dispatch(&mut h.stoat, &action::ReplayMacro);
        assert!(h.stoat.pending_macro_replay);
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('"'))));
        assert!(!h.stoat.pending_macro_replay);
        assert_eq!(primary_offset(&mut h), 6);
    }

    #[test]
    fn replay_with_unset_register_is_noop() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello");
        let before = primary_offset(&mut h);
        dispatch(&mut h.stoat, &action::ReplayMacro);
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('a'))));
        assert!(!h.stoat.pending_macro_replay);
        assert_eq!(primary_offset(&mut h), before);
    }

    #[test]
    fn recording_into_named_register_via_select_register() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello");
        dispatch(&mut h.stoat, &action::SelectRegister);
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('a'))));
        dispatch(&mut h.stoat, &action::RecordMacro);
        h.type_keys("l l");
        dispatch(&mut h.stoat, &action::RecordMacro);
        assert_eq!(primary_offset(&mut h), 2);
        // Replay from a should advance again by 2.
        dispatch(&mut h.stoat, &action::ReplayMacro);
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('a'))));
        assert_eq!(primary_offset(&mut h), 4);
    }

    #[test]
    fn non_char_key_during_replay_chord_clears_arm() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello");
        dispatch(&mut h.stoat, &action::ReplayMacro);
        assert!(h.stoat.pending_macro_replay);
        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        assert!(!h.stoat.pending_macro_replay);
    }

    /// A macro in register `a` that moves two columns, left stored and not
    /// replayed. The register selection is consumed by the recording, so a
    /// later recording goes to the unnamed register.
    fn record_two_column_macro_in_a(h: &mut crate::test_harness::TestHarness) {
        h.type_keys("\" a");
        h.type_keys("Q");
        h.type_keys("l l");
        h.type_keys("Q");
    }

    #[test]
    fn recording_a_replay_stores_the_register_not_its_body() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world");
        record_two_column_macro_in_a(&mut h);

        h.type_keys("Q");
        h.type_keys("q a");
        h.type_keys("Q");

        let stored = h
            .stoat
            .macros
            .get(&crate::register::Register::Unnamed)
            .expect("macro stored");
        let typed: Vec<KeyCode> = stored.iter().map(|k| k.code).collect();
        assert_eq!(
            typed,
            vec![KeyCode::Char('q'), KeyCode::Char('a')],
            "the replay was recorded as the inner macro's body"
        );
    }

    #[test]
    fn replaying_a_recorded_replay_runs_the_inner_macro() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world");
        record_two_column_macro_in_a(&mut h);

        h.type_keys("Q");
        h.type_keys("q a");
        h.type_keys("Q");
        let after_recording = primary_offset(&mut h);

        h.type_keys("q \"");
        assert_eq!(
            primary_offset(&mut h) - after_recording,
            2,
            "the outer macro moved by its first recorded key, not the inner macro"
        );
    }

    #[test]
    fn a_macro_replaying_itself_terminates() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world");

        h.type_keys("\" a");
        h.type_keys("Q");
        h.type_keys("q a");
        h.type_keys("Q");

        h.type_keys("q a");
        assert_eq!(primary_offset(&mut h), 0, "the macro moves nothing");
    }

    #[test]
    fn q_toggle_is_not_captured_in_macro() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello");
        dispatch(&mut h.stoat, &action::RecordMacro);
        h.type_keys("l");
        dispatch(&mut h.stoat, &action::RecordMacro);
        // Macro should be exactly one MoveRight (l), not the
        // surrounding RecordMacro dispatches.
        let stored = h
            .stoat
            .macros
            .get(&crate::register::Register::Unnamed)
            .expect("macro stored");
        assert_eq!(stored.len(), 1);
    }
}
