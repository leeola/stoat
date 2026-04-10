use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    command_palette::CommandPalette,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, RenderBlock},
    editor_state::EditorState,
    git,
    pane::{Axis, Direction, View},
    review::{self, ReviewRow},
};
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use std::{path::Path, sync::Arc};
use stoat_action::{Action, ActionKind, OpenFile};

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
    let workdir = match repo.workdir() {
        Some(w) => w.to_path_buf(),
        None => return,
    };

    let changed = git::changed_files(&repo);
    if changed.is_empty() {
        tracing::warn!("open_review: no changed files");
        return;
    }

    let mut review_rows: Vec<ReviewRow> = Vec::new();
    let mut blocks: Vec<BlockProperties> = Vec::new();
    let mut current_row: u32 = 0;

    for file in &changed {
        let buffer_text = match std::fs::read_to_string(&file.path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let base_text = git::head_content(&repo, &file.path).unwrap_or_default();
        let lang = stoat.language_registry.for_path(&file.path);
        let hunks = review::extract_review_hunks(lang.as_ref(), &base_text, &buffer_text, 3);
        if hunks.is_empty() {
            continue;
        }

        let rel_path = file
            .path
            .strip_prefix(&workdir)
            .unwrap_or(&file.path)
            .display()
            .to_string();
        let lang_name = lang.as_ref().map(|l| l.name.to_string());

        let total_hunks = hunks.len();
        for (hunk_idx, hunk) in hunks.iter().enumerate() {
            let label = {
                let lang_str = lang_name.as_deref().unwrap_or("");
                format!(
                    "{} --- {}/{} --- {}",
                    rel_path,
                    hunk_idx + 1,
                    total_hunks,
                    lang_str
                )
            };

            let render: RenderBlock = {
                let label = label.clone();
                Arc::new(move |_ctx| {
                    vec![Line::styled(
                        label.clone(),
                        Style::default().fg(Color::Yellow),
                    )]
                })
            };
            blocks.push(BlockProperties {
                placement: BlockPlacement::Above(current_row),
                height: Some(1),
                style: BlockStyle::Fixed,
                render,
                diff_status: None,
                priority: 0,
            });

            current_row += hunk.rows.len() as u32;
            review_rows.extend(hunk.rows.iter().cloned());
        }
    }

    if review_rows.is_empty() {
        tracing::warn!("open_review: no diff hunks to display");
        return;
    }

    // Placeholder buffer: one line per review row for scroll counting.
    let placeholder = " \n".repeat(review_rows.len().saturating_sub(1)) + " ";
    let (buffer_id, buffer) = stoat.buffers.new_scratch();
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.edit(0..0, &placeholder);
        guard.dirty = false;
    }

    let mut editor = EditorState::new(buffer_id, buffer, stoat.executor.clone());
    editor.display_map.insert_blocks(blocks);
    editor.review_rows = Some(review_rows);

    let new_editor_id = stoat.editors.insert(editor);
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
