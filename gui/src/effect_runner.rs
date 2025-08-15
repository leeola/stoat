//! Effect runner that converts stoat Effects to iced Tasks.
//!
//! This module executes side effects requested by the editor core.

use crate::messages::Message;
use iced::{window, Task};
use stoat::Effect;

/// Converts a single Effect into an iced Task.
///
/// This is the bridge between the pure editor core and the effectful GUI world.
/// Each Effect type is mapped to the appropriate iced operation or external
/// library call.
pub fn run_effect(effect: Effect) -> Task<Message> {
    tracing::debug!("Running effect: {:?}", effect);

    match effect {
        Effect::ShowError { message } => Task::done(Message::ShowError { message }),

        Effect::ShowInfo { message } => Task::done(Message::ShowInfo { message }),

        Effect::Exit => window::get_latest().and_then(window::close),

        Effect::SetTitle { title } => Task::done(Message::UpdateTitle { title }),

        Effect::Bell => {
            // On most systems, we can't programmatically ring the bell from GUI apps
            // Just ignore this effect or could show a brief visual indication
            Task::none()
        },
    }
}

/// Converts multiple Effects into a batch of iced Tasks.
pub fn run_effects(effects: Vec<Effect>) -> Task<Message> {
    if effects.is_empty() {
        tracing::trace!("No effects to run");
        Task::none()
    } else {
        tracing::debug!("Running batch of {} effects", effects.len());
        Task::batch(effects.into_iter().map(run_effect))
    }
}
