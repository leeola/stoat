//! Macro record / replay and SelectRegister action handlers.
//!
//! Unlike the other modules under [`crate::editor::actions`], the
//! three handlers in this scope (`RecordMacro`, `ReplayMacro`,
//! `SelectRegister`) operate on workspace + input-state-machine
//! state rather than per-editor state. The actual dispatch lives
//! in [`crate::workspace::Workspace::dispatch_action`]:
//!
//! - `RecordMacro` -> `Workspace::dispatch_record_macro` toggles
//!   [`crate::input_state_machine::InputStateMachine::toggle_macro_recording`]. On Off -> On, the
//!   workspace's pending register is taken via
//!   [`crate::workspace::Workspace::consume_selected_register`]. On On -> Off, the captured
//!   keystrokes are moved into the input state machine's macro store.
//!
//! - `ReplayMacro` -> `Workspace::dispatch_replay_macro` arms the replay chord. The next
//!   chord-completing char keystroke resolves to a [`stoat::register::Register`] via
//!   [`stoat::register::register_for_char`] and the stored keystrokes are re-fed through the input
//!   state machine.
//!
//! - `SelectRegister` -> `Workspace::dispatch_select_register` arms the register-select chord. The
//!   next char keystroke resolves to a register and is stored in
//!   [`crate::workspace::Workspace::selected_register`].
//!
//! Keystroke capture happens inside
//! [`crate::input_state_machine::InputStateMachine::feed`] -- the
//! peek at the resolved keymap action set excludes the
//! `RecordMacro` toggle from the captured buffer.
