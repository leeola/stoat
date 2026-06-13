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
pub(crate) fn execute_replay(stoat: &mut Stoat, ch: char) -> UpdateEffect {
    let Some(register) = crate::register::register_for_char(ch) else {
        return UpdateEffect::None;
    };
    let Some(keys) = stoat.macros.get(&register).cloned() else {
        return UpdateEffect::None;
    };
    let mut effect = UpdateEffect::None;
    for key in keys {
        let outcome = stoat.update(Event::Key(key));
        if matches!(outcome, UpdateEffect::Quit) {
            return UpdateEffect::Quit;
        }
        if matches!(outcome, UpdateEffect::Redraw) {
            effect = UpdateEffect::Redraw;
        }
    }
    effect
}

/// Append `key` to the active recording's key buffer. No-op when no
/// recording is in progress. Called from [`Stoat::handle_key`]
/// before chord dispatch so every keypress between `Q` toggles is
/// captured.
pub(crate) fn capture(stoat: &mut Stoat, key: &KeyEvent) {
    if let Some(rec) = stoat.macro_recording.as_mut() {
        rec.keys.push(*key);
    }
}
