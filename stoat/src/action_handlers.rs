use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    command_palette::CommandPalette,
    diff_map::DiffMap,
    editor_state::EditorState,
    git,
    pane::{Axis, Direction, View},
};
use std::path::Path;
use stoat_action::{Action, ActionKind, OpenFile};
use stoat_language::structural_diff;

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
        ActionKind::OpenFile => {
            let open = action
                .as_any()
                .downcast_ref::<OpenFile>()
                .expect("OpenFile action downcast");
            open_file(stoat, &open.path);
            UpdateEffect::Redraw
        },
        ActionKind::OpenCommandPalette => {
            stoat.command_palette = Some(CommandPalette::new());
            UpdateEffect::Redraw
        },
        ActionKind::OpenReview => {
            open_review(stoat);
            UpdateEffect::Redraw
        },
    }
}

fn open_file(stoat: &mut Stoat, path: &Path) -> Option<BufferId> {
    let absolute = std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(path));
    let content = match std::fs::read_to_string(&absolute) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::error!("failed to read {}: {}", absolute.display(), e);
            return None;
        },
    };

    let (buffer_id, buffer) = stoat.buffers.open(&absolute, &content);
    if let Some(lang) = stoat.language_registry.for_path(&absolute) {
        stoat.buffers.set_language(buffer_id, lang);
    }
    let new_editor_id =
        stoat
            .editors
            .insert(EditorState::new(buffer_id, buffer, stoat.executor.clone()));

    let focused = stoat.panes.focus();
    let old = match stoat.panes.pane(focused).view {
        View::Editor(eid) => Some(eid),
        View::Label(_) => None,
    };
    stoat.panes.pane_mut(focused).view = View::Editor(new_editor_id);

    if let Some(old_id) = old {
        let still_referenced = stoat
            .panes
            .split_panes()
            .any(|(_, p)| matches!(p.view, View::Editor(eid) if eid == old_id));
        if !still_referenced {
            stoat.editors.remove(old_id);
        }
    }

    Some(buffer_id)
}

fn open_review(stoat: &mut Stoat) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("open_review: cannot determine cwd: {e}");
            return;
        },
    };
    let repo = match git::discover_repo(&cwd) {
        Some(r) => r,
        None => {
            tracing::warn!("open_review: not inside a git repository");
            return;
        },
    };

    let changed = git::changed_files(&repo);
    let first = match changed.first() {
        Some(f) => f,
        None => {
            tracing::warn!("open_review: no changed files");
            return;
        },
    };
    let path = first.path.clone();
    let base_text = git::head_content(&repo, &path).unwrap_or_default();

    let buffer_id = match open_file(stoat, &path) {
        Some(id) => id,
        None => return,
    };

    let buffer_text = {
        let shared = match stoat.buffers.get(buffer_id) {
            Some(b) => b,
            None => return,
        };
        let guard = shared.read().expect("buffer poisoned");
        guard.rope().to_string()
    };

    let diff_result = {
        let lang = stoat.language_registry.for_path(&path);
        match lang {
            Some(ref l) => {
                structural_diff::diff_with_language_or_lines(l, &base_text, &buffer_text)
            },
            None => structural_diff::diff(&base_text, &buffer_text),
        }
    };

    let diff_map = DiffMap::from_structural_changes(diff_result, &base_text, &buffer_text);

    if let Some(shared) = stoat.buffers.get(buffer_id) {
        let mut guard = shared.write().expect("buffer poisoned");
        guard.diff_map = Some(diff_map);
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
