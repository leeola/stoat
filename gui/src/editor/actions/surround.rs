//! Surround action handlers: SurroundAdd, SurroundDelete,
//! SurroundReplace.
//!
//! All three handlers are chord-driven and dispatched directly
//! from [`crate::workspace::Workspace::dispatch_action`]:
//!
//! - `SurroundAdd` -> `Workspace::dispatch_surround_add` arms the single-char chord through
//!   [`crate::input_state_machine::InputStateMachine::arm_surround_add`]. The next char keystroke
//!   produces [`crate::actions::ApplySurroundAddChar`], which the workspace routes to
//!   [`crate::editor::Editor::handle_surround_add`] -- every non-empty selection gets wrapped with
//!   the canonical pair from [`stoat_language::surround::surround_pair_for`].
//!
//! - `SurroundDelete` -> `Workspace::dispatch_surround_delete` arms the single-char chord. The next
//!   char keystroke produces [`crate::actions::ApplySurroundDeleteChar`] which dispatches into
//!   [`crate::editor::Editor::handle_surround_delete`]. The editor walks each cursor's enclosing
//!   pair via [`stoat_language::surround::find_surround_pair`], which consults the buffer's
//!   [`stoat_language::SyntaxMap`] when available so brackets inside string / comment nodes are
//!   skipped.
//!
//! - `SurroundReplace` -> `Workspace::dispatch_surround_replace` arms the two-stage chord
//!   ([`stoat_language::surround::SurroundReplaceStage`]). The first char keystroke transitions
//!   `AwaitFrom -> AwaitTo(from)` without dispatching; the second char keystroke produces
//!   [`crate::actions::ApplySurroundReplaceChar { from, to }`] which dispatches into
//!   [`crate::editor::Editor::handle_surround_replace`].
//!
//! The dispatch lives in [`crate::workspace`]; the mutation
//! helpers live on [`crate::editor::Editor`]. This module is a
//! documentation anchor.
