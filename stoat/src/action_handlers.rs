use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    command_palette::CommandPalette,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, DisplayPoint, RenderBlock},
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
use stoat_text::{Bias, SelectionGoal};

pub fn dispatch(stoat: &mut Stoat, action: &dyn Action) -> UpdateEffect {
    match action.kind() {
        ActionKind::Quit => UpdateEffect::Quit,
        ActionKind::SplitRight => split_pane(stoat, Axis::Vertical),
        ActionKind::SplitDown => split_pane(stoat, Axis::Horizontal),
        ActionKind::FocusLeft => {
            stoat
                .active_workspace_mut()
                .panes
                .focus_direction(Direction::Left);
            UpdateEffect::Redraw
        },
        ActionKind::FocusRight => {
            stoat
                .active_workspace_mut()
                .panes
                .focus_direction(Direction::Right);
            UpdateEffect::Redraw
        },
        ActionKind::FocusUp => {
            stoat
                .active_workspace_mut()
                .panes
                .focus_direction(Direction::Up);
            UpdateEffect::Redraw
        },
        ActionKind::FocusDown => {
            stoat
                .active_workspace_mut()
                .panes
                .focus_direction(Direction::Down);
            UpdateEffect::Redraw
        },
        ActionKind::FocusNext => {
            stoat.active_workspace_mut().panes.focus_next();
            UpdateEffect::Redraw
        },
        ActionKind::FocusPrev => {
            stoat.active_workspace_mut().panes.focus_prev();
            UpdateEffect::Redraw
        },
        ActionKind::ClosePane => {
            let ws = stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            let editor_id = match ws.panes.pane(focused).view {
                View::Editor(id) => Some(id),
                _ => None,
            };
            ws.panes.close(focused);
            if let Some(id) = editor_id {
                ws.editors.remove(id);
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
        ActionKind::AddSelectionBelow => add_selection_below(stoat),
    }
}

fn focused_editor_mut(stoat: &mut Stoat) -> Option<&mut EditorState> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    match ws.panes.pane(focused).view {
        View::Editor(id) => ws.editors.get_mut(id),
        _ => None,
    }
}

fn add_selection_below(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let source = editor.selections.newest_anchor().clone();
    let source_head = source.head();
    let source_point = buffer_snapshot.point_for_anchor(&source_head);
    let source_display = display_snapshot.buffer_to_display(source_point);

    let goal_col = match source.goal {
        SelectionGoal::Column(c) => c,
        SelectionGoal::None => source_display.column,
    };

    let max_row = display_snapshot.max_point().row;
    let mut row = source_display.row;
    let target = loop {
        if row >= max_row {
            return UpdateEffect::None;
        }
        row += 1;
        let clamped_col = goal_col.min(display_snapshot.line_len(row));
        let raw = DisplayPoint::new(row, clamped_col);
        let clipped = display_snapshot.clip_point(raw, Bias::Left);
        let Some(buffer_pt) = display_snapshot.display_to_buffer(clipped) else {
            continue;
        };
        let offset = buffer_snapshot.rope().point_to_offset(buffer_pt);
        let anchor = buffer_snapshot.anchor_at(offset, Bias::Right);
        break anchor;
    };

    editor
        .selections
        .insert_cursor(target, SelectionGoal::Column(goal_col), buffer_snapshot);
    UpdateEffect::Redraw
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

    let lang = stoat.language_registry.for_path(&absolute);
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();

    let (buffer_id, buffer) = ws.buffers.open(&absolute, &content);
    if let Some(lang) = lang {
        ws.buffers.set_language(buffer_id, lang);
    }
    let new_editor_id = ws
        .editors
        .insert(EditorState::new(buffer_id, buffer, executor));

    let focused = ws.panes.focus();
    let old = match ws.panes.pane(focused).view {
        View::Editor(eid) => Some(eid),
        View::Label(_) => None,
    };
    ws.panes.pane_mut(focused).view = View::Editor(new_editor_id);

    if let Some(old_id) = old {
        let still_referenced = ws
            .panes
            .split_panes()
            .any(|(_, p)| matches!(p.view, View::Editor(eid) if eid == old_id));
        if !still_referenced {
            ws.editors.remove(old_id);
        }
    }

    Some(buffer_id)
}

fn open_review(stoat: &mut Stoat) {
    let git_root = stoat.active_workspace().git_root.clone();
    let repo = match git::discover_repo(&git_root) {
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
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let (buffer_id, buffer) = ws.buffers.new_scratch();
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.edit(0..0, &placeholder);
        guard.dirty = false;
    }

    let mut editor = EditorState::new(buffer_id, buffer, executor);
    editor.display_map.insert_blocks(blocks);
    editor.review_rows = Some(review_rows);

    let new_editor_id = ws.editors.insert(editor);
    let focused = ws.panes.focus();
    let old = match ws.panes.pane(focused).view {
        View::Editor(eid) => Some(eid),
        View::Label(_) => None,
    };
    ws.panes.pane_mut(focused).view = View::Editor(new_editor_id);
    if let Some(old_id) = old {
        let still_referenced = ws
            .panes
            .split_panes()
            .any(|(_, p)| matches!(p.view, View::Editor(eid) if eid == old_id));
        if !still_referenced {
            ws.editors.remove(old_id);
        }
    }
}

fn split_pane(stoat: &mut Stoat, axis: Axis) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let new_pane_id = ws.panes.split(axis);
    if let View::Editor(old_editor_id) = ws.panes.pane(new_pane_id).view {
        if let Some(old_editor) = ws.editors.get(old_editor_id) {
            let buffer_id = old_editor.buffer_id;
            if let Some(buffer) = ws.buffers.get(buffer_id) {
                let new_editor_id = ws
                    .editors
                    .insert(EditorState::new(buffer_id, buffer, executor));
                ws.panes.pane_mut(new_pane_id).view = View::Editor(new_editor_id);
            }
        }
    }
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use stoat_action::{AddSelectionBelow, Quit};
    use stoat_scheduler::TestScheduler;

    fn stoat() -> Stoat {
        let scheduler = Arc::new(TestScheduler::new());
        Stoat::new(
            scheduler.executor(),
            stoat_config::Settings::default(),
            std::path::PathBuf::new(),
        )
    }

    fn seed_focused_buffer(stoat: &mut Stoat, text: &str) {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer exists");
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.edit(0..0, text);
    }

    fn cursor_display_positions(stoat: &mut Stoat) -> Vec<(u32, u32)> {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let editor = ws.editors.get_mut(editor_id).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let head = sel.head();
                let point = buffer_snapshot.point_for_anchor(&head);
                let display = snapshot.buffer_to_display(point);
                (display.row, display.column)
            })
            .collect()
    }

    #[test]
    fn dispatch_quit() {
        assert_eq!(dispatch(&mut stoat(), &Quit), UpdateEffect::Quit);
    }

    #[test]
    fn add_selection_below_with_no_editor_focus_is_noop() {
        let mut stoat = stoat();
        {
            let ws = stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            ws.panes.pane_mut(focused).view = View::Label("nothing".into());
        }
        assert_eq!(dispatch(&mut stoat, &AddSelectionBelow), UpdateEffect::None);
    }

    #[test]
    fn add_selection_below_adds_cursor_on_next_display_row() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");

        assert_eq!(
            dispatch(&mut stoat, &AddSelectionBelow),
            UpdateEffect::Redraw
        );

        let positions = cursor_display_positions(&mut stoat);
        assert_eq!(positions, vec![(0, 0), (1, 0)]);
    }

    #[test]
    fn add_selection_below_at_last_row_is_noop() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc");

        assert_eq!(dispatch(&mut stoat, &AddSelectionBelow), UpdateEffect::None);
        assert_eq!(cursor_display_positions(&mut stoat), vec![(0, 0)]);
    }

    #[test]
    fn add_selection_below_preserves_goal_column_on_short_line() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "long line\nxx\nlong line\n");

        {
            let ws = stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            let editor_id = match ws.panes.pane(focused).view {
                View::Editor(id) => id,
                _ => unreachable!(),
            };
            let editor = ws.editors.get_mut(editor_id).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let buffer = snapshot.buffer_snapshot();
            let offset = buffer.rope().point_to_offset(stoat_text::Point::new(0, 7));
            let anchor = buffer.anchor_at(offset, Bias::Right);
            editor
                .selections
                .insert_cursor(anchor, SelectionGoal::Column(7), buffer);
        }

        assert_eq!(
            dispatch(&mut stoat, &AddSelectionBelow),
            UpdateEffect::Redraw
        );
        let after_one = cursor_display_positions(&mut stoat);
        assert_eq!(after_one, vec![(0, 0), (0, 7), (1, 2)]);

        assert_eq!(
            dispatch(&mut stoat, &AddSelectionBelow),
            UpdateEffect::Redraw
        );
        let after_two = cursor_display_positions(&mut stoat);
        assert_eq!(after_two, vec![(0, 0), (0, 7), (1, 2), (2, 7)]);
    }
}
