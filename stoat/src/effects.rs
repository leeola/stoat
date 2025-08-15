//! Effect types representing side effects as data.
//!
//! Effects describe what should happen to the outside world as a result of
//! processing an event. They are pure data and don't perform the actual
//! side effects - that's left to the effect runner (typically in the GUI layer).

/// Side effects that should be executed by the effect runner.
///
/// Effects are pure data describing what should happen outside the core
/// editor logic. This allows the core to remain pure and testable while
/// still describing all necessary interactions with the external world.
///
/// Currently simplified to only include GUI notification effects while
/// async operations are being redesigned.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Show error message to user
    ShowError { message: String },

    /// Show info message to user
    ShowInfo { message: String },

    /// Close the application
    Exit,

    /// Request window title update
    SetTitle { title: String },

    /// Ring the terminal bell (for error feedback)
    Bell,
}
