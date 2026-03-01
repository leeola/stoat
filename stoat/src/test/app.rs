use crate::{
    app_state::AppState,
    claude::{state::ChatMessage, view::ClaudeView},
    content_view::{PaneContent, ViewType},
    input_simulator::parse_input_sequence,
    pane::{Member, PaneId},
    pane_group::view::PaneGroupView,
    stoat::KeyContext,
    test::cursor_notation,
    Stoat,
};
use gpui::{App, Axis, Entity, TestAppContext, VisualTestContext, Window};
use std::collections::HashMap;

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

    match key_ctx {
        KeyContext::CommandPalette => format_command_palette(app_state, &header, cx),
        KeyContext::FileFinder => format_file_finder(app_state, &header, cx),
        KeyContext::BufferFinder => format_buffer_finder(app_state, &header, cx),
        _ => format_editor_buffer(stoat, &header, cx),
    }
}

fn format_editor_buffer(stoat: &Entity<Stoat>, header: &str, cx: &App) -> String {
    let s = stoat.read(cx);
    let buffer_item = s.active_buffer(cx);
    let buffer = buffer_item.read(cx).buffer().read(cx);
    let text = buffer.text();

    let cursor_pos = s.cursor_position();
    let cursor_offset = super::point_to_offset(&text, cursor_pos);

    let marked_text = cursor_notation::format(&text, &[cursor_offset], &[]);

    let mut result = header.to_string();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn new_empty(cx: &mut TestAppContext) {
        let mut app = TestApp::new(cx);
        insta::assert_snapshot!(app.snapshot_layout(), @"[editor*]");
    }

    #[gpui::test]
    fn new_with_text_snapshot(cx: &mut TestAppContext) {
        let mut app = TestApp::new_with_text("hello world", cx);
        insta::assert_snapshot!(app.snapshot_active());
    }

    #[gpui::test]
    fn insert_and_escape(cx: &mut TestAppContext) {
        let mut app = TestApp::new_with_text("hello world", cx);

        app.type_input("i");
        insta::assert_snapshot!("after-i", app.snapshot_active());

        app.type_input("Hi ");
        insta::assert_snapshot!("after-typing", app.snapshot_active());

        app.type_input("<Esc>");
        insta::assert_snapshot!("after-escape", app.snapshot_active());
    }

    #[gpui::test]
    fn claude_command_palette_typing(cx: &mut TestAppContext) {
        let mut app = TestApp::new(cx);

        app.type_input("<Space>l");
        app.flush();
        insta::assert_snapshot!("open-claude", app.snapshot_layout());
        insta::assert_snapshot!("claude-initial", app.snapshot_active());

        app.type_input("i");
        insta::assert_snapshot!("insert-mode", app.snapshot_active());

        app.type_input("foo");
        insta::assert_snapshot!("typed-foo", app.snapshot_active());

        app.type_input("<Esc>");
        insta::assert_snapshot!("escaped-insert", app.snapshot_active());

        app.type_input(":");
        app.flush();
        insta::assert_snapshot!("command-palette", app.snapshot_active());

        app.type_input("test");
        insta::assert_snapshot!("palette-typing", app.snapshot_active());
    }

    #[gpui::test]
    fn claude_escape_then_pane_switch(cx: &mut TestAppContext) {
        let mut app = TestApp::new_with_text("original", cx);

        app.type_input("<Space>l");
        app.flush();
        insta::assert_snapshot!("layout", app.snapshot_layout());

        app.type_input("i");
        app.type_input("hello");
        insta::assert_snapshot!("claude-typing", app.snapshot_active());

        app.type_input("<Esc>");
        insta::assert_snapshot!("after-first-esc", app.snapshot_active());

        app.type_input("<Esc>");
        insta::assert_snapshot!("after-second-esc", app.snapshot_active());

        app.type_input("<Space>ah");
        app.flush();
        insta::assert_snapshot!("switched-to-editor", app.snapshot_active());

        app.type_input("iworld<Esc>");
        insta::assert_snapshot!("editor-typing", app.snapshot_active());
    }

    #[gpui::test]
    fn claude_overlay_dismiss_restores_context(cx: &mut TestAppContext) {
        let mut app = TestApp::new(cx);

        app.type_input("<Space>l");
        app.flush();
        insta::assert_snapshot!("claude-open", app.snapshot_active());

        app.type_input(":");
        app.flush();
        insta::assert_snapshot!("palette-open", app.snapshot_active());

        app.type_input("<Esc>");
        app.flush();
        insta::assert_snapshot!("palette-dismissed", app.snapshot_active());

        app.type_input("i");
        insta::assert_snapshot!("restored-insert", app.snapshot_active());
    }

    #[gpui::test]
    fn claude_input_focus_transitions(cx: &mut TestAppContext) {
        let mut app = TestApp::new(cx);

        app.type_input("<Space>l");
        app.flush();
        insta::assert_snapshot!("initial", app.snapshot_active());

        app.type_input("i");
        insta::assert_snapshot!("focus-input-1", app.snapshot_active());

        app.type_input("<Esc>");
        insta::assert_snapshot!("input-normal-1", app.snapshot_active());

        app.type_input("<Esc>");
        insta::assert_snapshot!("unfocus-input-1", app.snapshot_active());

        app.type_input("i");
        insta::assert_snapshot!("focus-input-2", app.snapshot_active());

        app.type_input("hello");
        insta::assert_snapshot!("typed-hello", app.snapshot_active());

        app.type_input("<Esc>");
        insta::assert_snapshot!("input-normal-2", app.snapshot_active());

        app.type_input("<Esc>");
        insta::assert_snapshot!("unfocus-input-2", app.snapshot_active());
    }
}
