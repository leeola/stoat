//! Message types for the GUI application.
//!
//! This module defines all the messages that flow through the iced application.
//! Messages either represent user input or responses from async operations.

use stoat::EditorEvent;

/// Messages that the GUI application can handle.
#[derive(Debug, Clone)]
pub enum Message {
    /// User input to be processed by the editor engine
    EditorInput(EditorEvent),

    /// User requested to create a new file
    NewFileRequested,

    /// User requested to exit the application
    ExitRequested,

    /// Clipboard operation completed
    ClipboardSet,

    /// Clipboard content received
    ClipboardReceived(String),

    /// Show an informational message to the user
    ShowInfo { message: String },

    /// Show an error message to the user
    ShowError { message: String },

    /// Update window title
    UpdateTitle { title: String },

    /// No operation (used as placeholder)
    NoOp,
}
