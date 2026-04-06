use crate::{
    app::{Stoat, UpdateEffect},
    editor_state::EditorState,
    pane::{Axis, Direction, View},
};
use stoat_action::{Action, ActionKind};

pub fn dispatch(stoat: &mut Stoat, action: &dyn Action) -> UpdateEffect {
    match action.kind() {
        ActionKind::Quit => UpdateEffect::Quit,
        ActionKind::SplitRight => split_pane(stoat, Axis::Vertical),
        ActionKind::SplitDown => split_pane(stoat, Axis::Horizontal),
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
            let focused = stoat.panes.focus();
            let editor_id = match stoat.panes.pane(focused).view {
                View::Editor(id) => Some(id),
                _ => None,
            };
            stoat.panes.close(focused);
            if let Some(id) = editor_id {
                stoat.editors.remove(id);
            }
            UpdateEffect::Redraw
        },
    }
}

fn split_pane(stoat: &mut Stoat, axis: Axis) -> UpdateEffect {
    let new_pane_id = stoat.panes.split(axis);
    if let View::Editor(old_editor_id) = stoat.panes.pane(new_pane_id).view {
        if let Some(old_editor) = stoat.editors.get(old_editor_id) {
            let buffer_id = old_editor.buffer_id;
            if let Some(buffer) = stoat.buffers.get(buffer_id) {
                let new_editor_id = stoat.editors.insert(EditorState::new(
                    buffer_id,
                    buffer,
                    stoat.executor.clone(),
                ));
                stoat.panes.pane_mut(new_pane_id).view = View::Editor(new_editor_id);
            }
        }
    }
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use stoat_action::Quit;
    use stoat_scheduler::TestScheduler;

    fn stoat() -> Stoat {
        let scheduler = Arc::new(TestScheduler::new());
        Stoat::new(scheduler.executor())
    }

    #[test]
    fn dispatch_quit() {
        assert_eq!(dispatch(&mut stoat(), &Quit), UpdateEffect::Quit);
    }
}
