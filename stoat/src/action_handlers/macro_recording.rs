use crate::{
    app::{Stoat, UpdateEffect},
    keymap,
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
/// selected register (or `@` when none was set); on -> stop and write the
/// captured keys to that register as text.
///
/// The macro lands in an ordinary register rather than a store of its own, so
/// a user pastes one to read it, edits the text, and writes a macro by hand
/// without ever recording it.
pub(super) fn toggle_record(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(rec) = stoat.macro_recording.take() {
        let text = keymap::macro_to_text(&rec.keys);
        let name = rec.register.name();
        if super::yank::write_fragments_to_register(stoat, rec.register, vec![text]) {
            stoat.set_status(format!("recorded to register {name}"));
        }
    } else {
        let register = stoat.macro_register();
        stoat.set_status(format!("recording to register {}", register.name()));
        stoat.macro_recording = Some(MacroRecording {
            register,
            keys: Vec::new(),
        });
    }
    UpdateEffect::Redraw
}

/// Arm the replay chord. The next char keypress in normal/select
/// mode names a register and triggers [`execute_replay`].
///
/// The pending count is taken here rather than read at the register keypress,
/// because the dispatch running this clears it before that key arrives. Taking
/// it also keeps it off the macro's first key, which would otherwise consume a
/// count meant for the whole replay.
pub(super) fn arm_replay(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_macro_replay = Some(stoat.take_pending_count().unwrap_or(1).max(1));
    UpdateEffect::Redraw
}

/// Resolve the register from `ch` and replay its stored macro `count` times by
/// re-feeding each captured [`KeyEvent`] through [`Stoat::update`].
/// No-op when the register is empty or unnamed.
///
/// The count repeats the whole body rather than any one key, so a macro that
/// ends where it started repeats from there.
///
/// Replaying a register already partway through a replay is refused. The keys
/// are re-fed through the path that started this one, so a macro naming itself,
/// directly or through another, would otherwise never stop.
pub(crate) fn execute_replay(stoat: &mut Stoat, ch: char, count: u32) -> UpdateEffect {
    let register = super::yank::register_for_char(ch);
    if stoat.replaying_registers.contains(&register) {
        stoat.set_status(format!("register {} is already replaying", register.name()));
        return UpdateEffect::Redraw;
    }

    // One value, since a macro is one key sequence. A multi-fragment register
    // holds a multi-selection yank rather than something to replay.
    let text = super::yank::read_register_fragments(stoat, register)
        .filter(|fragments| fragments.len() == 1)
        .map(|mut fragments| fragments.remove(0));
    let Some(text) = text else {
        stoat.set_status(format!("register {} is empty", register.name()));
        return UpdateEffect::Redraw;
    };
    let Some(keys) = keymap::macro_from_text(&text) else {
        stoat.set_status(format!("register {} does not hold keys", register.name()));
        return UpdateEffect::Redraw;
    };

    stoat.replaying_registers.push(register);
    let mut effect = UpdateEffect::None;
    for _ in 0..count {
        for key in &keys {
            let outcome = stoat.update(Event::Key(*key));
            if matches!(outcome, UpdateEffect::Quit) {
                stoat.replaying_registers.pop();
                return UpdateEffect::Quit;
            }
            if matches!(outcome, UpdateEffect::Redraw) {
                effect = UpdateEffect::Redraw;
            }
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
        assert_eq!(h.stoat.pending_macro_replay, Some(1));
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('@'))));
        assert_eq!(h.stoat.pending_macro_replay, None);
        assert_eq!(primary_offset(&mut h), 6);
    }

    /// A count in front of the replay repeats the whole macro, the way a count
    /// in front of any other motion repeats it.
    #[test]
    fn replay_honors_a_count_prefix() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world again");
        dispatch(&mut h.stoat, &action::RecordMacro);
        h.type_keys("l l");
        dispatch(&mut h.stoat, &action::RecordMacro);
        assert_eq!(primary_offset(&mut h), 2);

        h.type_keys("3");
        dispatch(&mut h.stoat, &action::ReplayMacro);
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('@'))));
        assert_eq!(
            primary_offset(&mut h),
            8,
            "three replays of a two-column macro advance six columns",
        );
    }

    /// The count has to be captured when the chord arms, since the dispatch
    /// that armed it clears the pending count before the register char lands.
    #[test]
    fn replay_count_survives_the_register_chord() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world again");
        dispatch(&mut h.stoat, &action::RecordMacro);
        h.type_keys("l");
        dispatch(&mut h.stoat, &action::RecordMacro);

        h.type_keys("4");
        dispatch(&mut h.stoat, &action::ReplayMacro);
        assert_eq!(
            h.stoat.pending_count, None,
            "the arming dispatch consumed the count, so only the chord still holds it",
        );

        h.stoat.update(Event::Key(keys::key(KeyCode::Char('@'))));
        assert_eq!(primary_offset(&mut h), 5, "one from the record, four more");
    }

    #[test]
    fn replay_with_unset_register_is_noop() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello");
        let before = primary_offset(&mut h);
        dispatch(&mut h.stoat, &action::ReplayMacro);
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('a'))));
        assert_eq!(h.stoat.pending_macro_replay, None);
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
        assert_eq!(h.stoat.pending_macro_replay, Some(1));
        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        assert_eq!(h.stoat.pending_macro_replay, None);
    }

    /// A macro in register `a` that moves two columns, left stored and not
    /// replayed. The register selection is consumed by the recording, so a
    /// later recording goes to the default macro register.
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

        assert_eq!(
            stored_macro(&mut h, '@'),
            Some("q a".to_string()),
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

        h.type_keys("q @");
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
        assert_eq!(
            stored_macro(&mut h, '@'),
            Some("l".to_string()),
            "the macro is the one MoveRight, not the RecordMacro dispatches around it",
        );
    }

    /// Recording is modal state with no other sign of itself, so both edges
    /// name the register they use.
    #[test]
    fn recording_sets_a_status() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello");

        h.type_keys("Q");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("recording to register @"),
        );

        h.type_keys("l");
        h.type_keys("Q");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("recorded to register @"),
        );
    }

    /// Each way a replay declines names the register and the reason, so the
    /// keypress that did nothing does not look like a dropped key.
    #[test]
    fn a_replay_that_does_nothing_says_why() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello");

        h.type_keys("q z");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("register z is empty"),
            "an empty register",
        );

        h.stoat.registers.write(
            crate::register::Register::Named('c'),
            vec!["not keys at all".to_string()],
        );
        h.type_keys("q c");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("register c does not hold keys"),
            "a register holding something else",
        );
    }

    /// A macro that names itself is refused, and saying so is what separates a
    /// guarded recursion from a keypress that vanished.
    #[test]
    fn a_re_entrant_replay_says_why() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world");
        h.stoat.registers.write(
            crate::register::Register::Named('r'),
            vec!["q r".to_string()],
        );

        h.type_keys("q r");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("register r is already replaying"),
        );
    }

    /// The text register `name` holds, which for a macro register is the
    /// recorded key sequence.
    fn stored_macro(h: &mut crate::test_harness::TestHarness, name: char) -> Option<String> {
        let register = crate::action_handlers::yank::register_for_char(name);
        crate::action_handlers::yank::read_register_fragments(&mut h.stoat, register)
            .map(|fragments| fragments.join("\n"))
    }

    /// A macro is a register value, so it reads back as text to see, paste,
    /// and edit.
    #[test]
    fn a_recorded_macro_is_readable_as_register_text() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world");
        h.type_keys("Q");
        h.type_keys("l l escape");
        h.type_keys("Q");

        assert_eq!(
            stored_macro(&mut h, '@'),
            Some("l l escape".to_string()),
            "the keys are spelled the way config.stcfg binds them",
        );
    }

    /// Nothing distinguishes a macro register from any other, so text written
    /// by hand replays exactly as a recorded one does.
    #[test]
    fn a_hand_written_register_replays() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world");
        h.stoat.registers.write(
            crate::register::Register::Named('b'),
            vec!["l l l".to_string()],
        );

        h.type_keys("q b");
        assert_eq!(
            primary_offset(&mut h),
            3,
            "three keys nobody recorded still replay",
        );
    }

    /// Recording defaults to `@` rather than the unnamed register, so a yank
    /// between recording and replaying leaves the macro alone.
    #[test]
    fn a_yank_does_not_clobber_the_default_macro_register() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("hello world");
        h.type_keys("Q");
        h.type_keys("l l");
        h.type_keys("Q");

        h.type_keys("v l l");
        h.type_keys("y");
        h.type_keys("escape");
        let before = primary_offset(&mut h);

        h.type_keys("q @");
        assert_eq!(
            primary_offset(&mut h) - before,
            2,
            "the yank landed in the unnamed register, not over the macro",
        );
    }
}
