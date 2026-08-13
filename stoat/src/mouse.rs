//! Mouse routing: which surface a click lands on, and what it means there.
//!
//! A press arrives as a screen cell and has to become an action on whatever
//! occupies that cell -- a modal, a minimap strip, a pane divider, a terminal
//! grid, a line of text. The hit tests here walk the same layout the paint
//! produced, so a click resolves to the surface the user saw rather than to
//! whatever the model currently holds.

use crossterm::event::{MouseButton, MouseEventKind};
use stoatty_protocol::window_ipc::{MouseButton as IpcMouseButton, MouseKind};

/// Map an aux window's pointer gesture onto the crossterm event kind the pane
/// mouse handlers dispatch on. A wheel becomes a scroll. A button gesture keeps
/// its button.
///
/// [`None`] for the side buttons, which crossterm does not model.
/// [`Stoat::handle_jumplist_buttons`] takes every gesture of those before this
/// runs, so the case does not arise in practice.
pub(crate) fn mouse_event_kind(kind: MouseKind) -> Option<MouseEventKind> {
    Some(match kind {
        MouseKind::Press(button) => MouseEventKind::Down(mouse_button(button)?),
        MouseKind::Release(button) => MouseEventKind::Up(mouse_button(button)?),
        MouseKind::Drag(button) => MouseEventKind::Drag(mouse_button(button)?),
        MouseKind::WheelUp => MouseEventKind::ScrollUp,
        MouseKind::WheelDown => MouseEventKind::ScrollDown,
    })
}

pub(crate) fn mouse_button(button: IpcMouseButton) -> Option<MouseButton> {
    match button {
        IpcMouseButton::Left => Some(MouseButton::Left),
        IpcMouseButton::Middle => Some(MouseButton::Middle),
        IpcMouseButton::Right => Some(MouseButton::Right),
        IpcMouseButton::Back | IpcMouseButton::Forward => None,
    }
}
