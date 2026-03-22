use crate::{
    app::{Stoat, UpdateEffect},
    pane::{Axis, Direction},
};
use stoat_action::{Action, ActionKind};

pub fn dispatch(stoat: &mut Stoat, action: &dyn Action) -> UpdateEffect {
    match action.kind() {
        ActionKind::Quit => UpdateEffect::Quit,
        ActionKind::SplitRight => {
            stoat.panes.split(Axis::Vertical);
            UpdateEffect::Redraw
        },
        ActionKind::SplitDown => {
            stoat.panes.split(Axis::Horizontal);
            UpdateEffect::Redraw
        },
        ActionKind::FocusLeft => {
            stoat.panes.focus_direction(Direction::Left);
            UpdateEffect::Redraw
        },
        ActionKind::FocusRight => {
            stoat.panes.focus_direction(Direction::Right);
            UpdateEffect::Redraw
        },
        ActionKind::FocusUp => {
            stoat.panes.focus_direction(Direction::Up);
            UpdateEffect::Redraw
        },
        ActionKind::FocusDown => {
            stoat.panes.focus_direction(Direction::Down);
            UpdateEffect::Redraw
        },
        ActionKind::FocusNext => {
            stoat.panes.focus_next();
            UpdateEffect::Redraw
        },
        ActionKind::FocusPrev => {
            stoat.panes.focus_prev();
            UpdateEffect::Redraw
        },
        ActionKind::ClosePane => {
            stoat.panes.close(stoat.panes.focus());
            UpdateEffect::Redraw
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_action::Quit;

    fn stoat() -> Stoat {
        Stoat::new()
    }

    #[test]
    fn dispatch_quit() {
        assert_eq!(dispatch(&mut stoat(), &Quit), UpdateEffect::Quit);
    }
}
