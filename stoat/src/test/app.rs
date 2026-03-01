use crate::{
    app_state::AppState,
    claude::{state::ChatMessage, view::ClaudeView},
    content_view::{PaneContent, ViewType},
    git::diff::HunkLineOrigin,
    input_simulator::parse_input_sequence,
    pane::{Member, PaneId},
    pane_group::view::PaneGroupView,
    stoat::KeyContext,
    test::cursor_notation,
    worktree::Worktree,
    Stoat,
};
use gpui::{App, Axis, Entity, TestAppContext, VisualTestContext, Window};
use parking_lot::Mutex;
use std::{collections::HashMap, path::PathBuf, sync::Arc};

pub struct TestApp<'a> {
    pub view: Entity<PaneGroupView>,
    cx: &'a mut VisualTestContext,
}

impl<'a> TestApp<'a> {
    pub fn new(cx: &'a mut TestAppContext) -> Self {
        let keymap = super::test_keymap();
        let config = crate::config::Config::default();
        let (view, cx) =
            cx.add_window_view(|_window, cx| PaneGroupView::new(config, vec![], keymap, cx));
        cx.update(|window, cx| {
            view.read(cx).focus_active_editor(window, cx);
        });
        Self { view, cx }
    }

    pub fn new_with_text(text: &str, cx: &'a mut TestAppContext) -> Self {
        let app = Self::new(cx);
        let text = text.to_string();
        let view = app.view.clone();
        app.cx.update(|_window, cx| {
            if let Some(stoat) = view.read(cx).active_stoat(cx) {
                stoat.update(cx, |s, cx| {
                    let buffer_item = s.active_buffer(cx);
                    let buffer = buffer_item.read(cx).buffer().clone();
                    let len = buffer.read(cx).len();
                    buffer.update(cx, |buf, _| {
                        buf.edit([(0..len, text.as_str())]);
                    });
                });
            }
        });
        app
    }

    pub fn with_fixture(
        fixture: &super::git_fixture::GitFixture,
        cx: &'a mut TestAppContext,
    ) -> Self {
        let app = Self::new(cx);
        let fixture_dir = fixture.dir().to_path_buf();
        let changed_files: Vec<PathBuf> = fixture.changed_files().to_vec();
        let view = app.view.clone();
        app.cx.update(|_window, cx| {
            view.update(cx, |pgv, cx| {
                let new_worktree = Arc::new(Mutex::new(Worktree::new(fixture_dir.clone())));
                pgv.app_state.worktree = new_worktree;
                if let Some(stoat_entity) = pgv.active_stoat(cx) {
                    stoat_entity.update(cx, |s, cx| {
                        s.worktree = Arc::new(Mutex::new(Worktree::new(fixture_dir.clone())));
                        if let Some(path) = changed_files.first() {
                            let _ = s.load_file(path, cx);
                        }
                    });
                }
            });
        });
        app
    }

    pub fn type_input(&mut self, input: &str) {
        let keystrokes = parse_input_sequence(input);
        for keystroke in keystrokes {
            self.cx.update(|window, cx| {
                window.dispatch_keystroke(keystroke, cx);
            });
        }
        self.cx.run_until_parked();
    }

    pub fn snapshot_layout(&mut self) -> String {
        let view = self.view.clone();
        self.cx.update(|_window, cx| {
            let pgv = view.read(cx);
            format_member(
                pgv.pane_group.root(),
                &pgv.pane_contents,
                pgv.active_pane,
                cx,
            )
        })
    }

    pub fn flush(&mut self) {
        let view = self.view.clone();
        self.cx.update(|window, cx| {
            view.update(cx, |pgv, entity_cx| {
                pgv.process_pending_actions(window, entity_cx);
            });
        });
    }

    pub fn snapshot_active(&mut self) -> String {
        let view = self.view.clone();
        self.cx.update(|window, cx| {
            let pgv = view.read(cx);
            let pane_id = pgv.active_pane;
            let content = pgv.pane_contents.get(&pane_id);

            match content {
                Some(PaneContent::Editor(editor)) => {
                    let stoat = editor.read(cx).stoat.clone();
                    snapshot_editor(&stoat, pane_id, &pgv.app_state, cx)
                },
                Some(PaneContent::Claude(claude_view)) => {
                    snapshot_claude(claude_view, pane_id, &pgv.app_state, window, cx)
                },
                Some(PaneContent::Static(_)) => {
                    format!("[static] pane={pane_id}")
                },
                None => format!("[empty] pane={pane_id}"),
            }
        })
    }
}

fn format_member(
    member: &Member,
    pane_contents: &HashMap<PaneId, PaneContent>,
    active_pane: PaneId,
    _cx: &App,
) -> String {
    match member {
        Member::Pane(id) => {
            let type_label = pane_contents
                .get(id)
                .map(|c| match c.view_type() {
                    ViewType::Editor => "editor",
                    ViewType::Static => "static",
                    ViewType::Claude => "claude",
                })
                .unwrap_or("unknown");
            if *id == active_pane {
                format!("[{type_label}*]")
            } else {
                format!("[{type_label}]")
            }
        },
        Member::Axis(axis) => {
            let children: Vec<String> = axis
                .members
                .iter()
                .map(|m| format_member(m, pane_contents, active_pane, _cx))
                .collect();
            match axis.axis {
                Axis::Horizontal => children.join(" | "),
                Axis::Vertical => children.join("\n---\n"),
            }
        },
    }
}

fn snapshot_editor(
    stoat: &Entity<Stoat>,
    pane_id: PaneId,
    app_state: &AppState,
    cx: &App,
) -> String {
    let s = stoat.read(cx);
    let mode = s.mode().to_string();
    let key_ctx = s.key_context();

    let mut header = format!("[editor] pane={pane_id} mode={mode}");
    if key_ctx != KeyContext::TextEditor {
        header.push_str(&format!(" ctx={}", key_context_label(key_ctx)));
    }

    if stoat.read(cx).line_selection.is_some() {
        return format_line_selection(stoat, &header, cx);
    }

    match key_ctx {
        KeyContext::CommandPalette => format_command_palette(app_state, &header, cx),
        KeyContext::FileFinder => format_file_finder(app_state, &header, cx),
        KeyContext::BufferFinder => format_buffer_finder(app_state, &header, cx),
        KeyContext::DiffReview => format_diff_review(stoat, &header, cx),
        KeyContext::ConflictReview => format_conflict_review(stoat, &header, cx),
        KeyContext::Git => format_git_status(app_state, &header, cx),
        _ => format_editor_buffer(stoat, &header, cx),
    }
}

fn format_editor_buffer(stoat: &Entity<Stoat>, header: &str, cx: &App) -> String {
    let mut result = header.to_string();
    result.push_str(&format_buffer_lines(stoat, cx));
    result
}

fn format_diff_review(stoat: &Entity<Stoat>, header: &str, cx: &App) -> String {
    let mut result = {
        let s = stoat.read(cx);
        let rs = &s.review_state;
        let source = rs.source.display_name();
        let file_count = rs.files.len();
        let file_idx = rs.file_idx;
        let hunk_idx = rs.hunk_idx;
        let current_file = rs
            .files
            .get(file_idx)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<none>".into());
        let hunk_count = s
            .active_buffer(cx)
            .read(cx)
            .diff()
            .map(|d| d.hunks.len())
            .unwrap_or(0);
        let approved = rs
            .files
            .get(file_idx)
            .and_then(|p| rs.approved_hunks.get(p))
            .map(|s| s.len())
            .unwrap_or(0);
        let follow = if rs.follow { " follow" } else { "" };

        format!(
            "{header}\nsource={source} file={current_file} ({}/{file_count}) hunk={}/{hunk_count} approved={approved}{follow}",
            file_idx + 1,
            hunk_idx + 1
        )
    };

    result.push_str(&format_buffer_lines(stoat, cx));
    result
}

fn format_line_selection(stoat: &Entity<Stoat>, header: &str, cx: &App) -> String {
    let s = stoat.read(cx);
    let ls = match &s.line_selection {
        Some(ls) => ls,
        None => return format!("{header}\n<no line selection>"),
    };
    let mut result = format!(
        "{header}\nselected={}/{}",
        ls.selected_count(),
        ls.total_changeable_count()
    );
    for (i, line) in ls.hunk_lines.lines.iter().enumerate() {
        let origin = match line.origin {
            HunkLineOrigin::Context => ' ',
            HunkLineOrigin::Addition => '+',
            HunkLineOrigin::Deletion => '-',
        };
        let sel = if ls.selected[i] { "*" } else { " " };
        let cur = if i == ls.cursor_line { ">" } else { " " };
        result.push_str(&format!("\n{cur}{sel}{origin}{}", line.content.trim_end()));
    }
    result
}

fn format_git_status(app_state: &AppState, header: &str, _cx: &App) -> String {
    let gs = &app_state.git_status;
    let filter = gs.filter.display_name();
    let mut result = format!(
        "{header}\nfilter={filter} files={}/{}",
        gs.filtered.len(),
        gs.files.len()
    );
    for (i, entry) in gs.filtered.iter().enumerate() {
        let marker = if i == gs.selected { ">" } else { " " };
        let staged = if entry.staged { "S" } else { " " };
        result.push_str(&format!(
            "\n{marker}{staged} {} {}",
            entry.status,
            entry.path.display()
        ));
    }
    result
}

fn format_conflict_review(stoat: &Entity<Stoat>, header: &str, cx: &App) -> String {
    let mut result = {
        let s = stoat.read(cx);
        let cs = &s.conflict_state;
        let file_count = cs.files.len();
        let file_idx = cs.file_idx;
        let conflict_idx = cs.conflict_idx;
        let current_file = cs
            .files
            .get(file_idx)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<none>".into());
        let conflict_count = s.active_buffer(cx).read(cx).conflicts().len();
        let resolved = cs.resolutions.len();

        format!(
            "{header}\nfile={current_file} ({}/{file_count}) conflict={}/{conflict_count} resolved={resolved}",
            file_idx + 1,
            conflict_idx + 1
        )
    };

    result.push_str(&format_buffer_lines(stoat, cx));
    result
}

fn format_buffer_lines(stoat: &Entity<Stoat>, cx: &App) -> String {
    let s = stoat.read(cx);
    let buffer_item = s.active_buffer(cx);
    let buffer = buffer_item.read(cx).buffer().read(cx);
    let text = buffer.text();
    let selections = s.active_selections(cx);

    let marked_text = if selections.iter().all(|sel| sel.is_empty()) {
        let offsets: Vec<usize> = selections
            .iter()
            .map(|sel| super::point_to_offset(&text, sel.head()))
            .collect();
        cursor_notation::format(&text, &offsets, &[])
    } else {
        let notation_sels: Vec<cursor_notation::Selection> = selections
            .iter()
            .filter(|sel| !sel.is_empty())
            .map(|sel| cursor_notation::Selection {
                range: super::point_to_offset(&text, sel.start)
                    ..super::point_to_offset(&text, sel.end),
                cursor_at_start: sel.reversed,
            })
            .collect();
        cursor_notation::format(&text, &[], &notation_sels)
    };

    let mut result = String::new();
    if marked_text.is_empty() {
        result.push_str("\n  1:|");
    } else {
        for (i, line) in marked_text.lines().enumerate() {
            result.push_str(&format!("\n{:>3}:{line}", i + 1));
        }
    }
    result
}

fn format_command_palette(app_state: &AppState, header: &str, cx: &App) -> String {
    let input_text = app_state
        .command_palette
        .input
        .as_ref()
        .map(|buf| buf.read(cx).text())
        .unwrap_or_default();

    let mut result = format!("{header}\ninput: \"{input_text}|\"");
    for (i, cmd) in app_state.command_palette.filtered.iter().enumerate() {
        let marker = if i == app_state.command_palette.selected {
            "> "
        } else {
            "  "
        };
        result.push_str(&format!("\n{marker}{}", cmd.name));
    }
    result
}

fn format_file_finder(app_state: &AppState, header: &str, cx: &App) -> String {
    let input_text = app_state
        .file_finder
        .input
        .as_ref()
        .map(|buf| buf.read(cx).text())
        .unwrap_or_default();

    let mut result = format!("{header}\ninput: \"{input_text}|\"");
    for (i, path) in app_state.file_finder.filtered.iter().enumerate() {
        let marker = if i == app_state.file_finder.selected {
            "> "
        } else {
            "  "
        };
        result.push_str(&format!("\n{marker}{}", path.display()));
    }
    result
}

fn format_buffer_finder(app_state: &AppState, header: &str, cx: &App) -> String {
    let input_text = app_state
        .buffer_finder
        .input
        .as_ref()
        .map(|buf| buf.read(cx).text())
        .unwrap_or_default();

    let mut result = format!("{header}\ninput: \"{input_text}|\"");
    for (i, entry) in app_state.buffer_finder.filtered.iter().enumerate() {
        let marker = if i == app_state.buffer_finder.selected {
            "> "
        } else {
            "  "
        };
        let label = entry
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| entry.display_name.clone());
        result.push_str(&format!("\n{marker}{label}"));
    }
    result
}

fn snapshot_claude(
    claude_view: &Entity<ClaudeView>,
    pane_id: PaneId,
    app_state: &AppState,
    window: &Window,
    cx: &App,
) -> String {
    let cv = claude_view.read(cx);
    let stoat = cv.stoat();
    let s = stoat.read(cx);
    let mode = s.mode().to_string();
    let key_ctx = s.key_context();
    let input_focused = cv.input_is_focused(window, cx);
    let focus_label = if input_focused { "input" } else { "main" };

    let mut header = format!("[claude] pane={pane_id} mode={mode} focus={focus_label}");
    if key_ctx != KeyContext::Claude && key_ctx != KeyContext::TextEditor {
        header.push_str(&format!(" ctx={}", key_context_label(key_ctx)));
    }

    match key_ctx {
        KeyContext::CommandPalette => format_command_palette(app_state, &header, cx),
        KeyContext::FileFinder => format_file_finder(app_state, &header, cx),
        KeyContext::BufferFinder => format_buffer_finder(app_state, &header, cx),
        _ => {
            let mut result = header;

            let input_stoat = cv.input_stoat();
            let input_buffer_item = input_stoat.read(cx).active_buffer(cx);
            let input_text = input_buffer_item.read(cx).buffer().read(cx).text();
            result.push_str(&format!("\ninput: \"{input_text}|\""));

            result.push_str("\n---");

            let state = cv.state_entity().read(cx);
            for msg in &state.messages {
                let (role, text) = match msg {
                    ChatMessage::User(t) => ("You", t.as_str()),
                    ChatMessage::Assistant(t) => ("Claude", t.as_str()),
                    ChatMessage::System(t) => ("System", t.as_str()),
                    ChatMessage::Error(t) => ("Error", t.as_str()),
                };
                result.push_str(&format!("\n{role}: {text}"));
            }

            result
        },
    }
}

fn key_context_label(ctx: KeyContext) -> &'static str {
    match ctx {
        KeyContext::TextEditor => "TextEditor",
        KeyContext::Git => "Git",
        KeyContext::FileFinder => "FileFinder",
        KeyContext::BufferFinder => "BufferFinder",
        KeyContext::CommandPalette => "CommandPalette",
        KeyContext::CommandPaletteV2 => "CommandPaletteV2",
        KeyContext::InlineInput => "InlineInput",
        KeyContext::DiffReview => "DiffReview",
        KeyContext::ConflictReview => "ConflictReview",
        KeyContext::HelpModal => "HelpModal",
        KeyContext::AboutModal => "AboutModal",
        KeyContext::Claude => "Claude",
    }
}
