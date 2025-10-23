use crate::{
    actions::*,
    editor_element::EditorElement,
    editor_style::EditorStyle,
    scroll,
    stoat::{KeyContext, Stoat},
};
use gpui::{
    div, point, App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, ScrollWheelEvent, Styled, Window,
};
use std::sync::Arc;
use tracing::debug;

pub struct EditorView {
    pub(crate) stoat: Entity<Stoat>,
    focus_handle: FocusHandle,
    this: Option<Entity<Self>>,
    /// Cached editor style (Arc makes cloning cheap - just bumps refcount)
    pub(crate) editor_style: Arc<EditorStyle>,
}

impl EditorView {
    pub fn new(stoat: Entity<Stoat>, cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();

        // Create cached editor style once from config (Arc makes cloning cheap)
        let config = stoat.read(cx).config().clone();
        let editor_style = Arc::new(EditorStyle::new(&config));

        Self {
            stoat,
            focus_handle,
            this: None,
            editor_style,
        }
    }

    pub fn set_entity(&mut self, entity: Entity<Self>) {
        self.this = Some(entity);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    // ==== Action handlers ====

    fn handle_insert_text(
        &mut self,
        command: &InsertText,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // For TextEditor context, only allow insertion in insert mode
        // For other contexts (FileFinder, CommandPalette, etc), always allow
        let key_context = self.stoat.read(cx).key_context();
        let mode = self.stoat.read(cx).mode().to_string();

        if key_context == KeyContext::TextEditor && mode != "insert" {
            return; // No-op for normal/visual/etc modes
        }

        self.stoat.update(cx, |stoat, cx| {
            stoat.insert_text(&command.0, cx);
        });
        cx.notify();
    }

    fn handle_delete_left(
        &mut self,
        _: &DeleteLeft,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_left(cx);
        });
        cx.notify();
    }

    fn handle_delete_right(
        &mut self,
        _: &DeleteRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_right(cx);
        });
        cx.notify();
    }

    fn handle_delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_word_left(cx);
        });
        cx.notify();
    }

    fn handle_delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_word_right(cx);
        });
        cx.notify();
    }

    fn handle_select_next_symbol(
        &mut self,
        _: &SelectNextSymbol,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_next_symbol(cx);
        });
        cx.notify();
    }

    fn handle_select_prev_symbol(
        &mut self,
        _: &SelectPrevSymbol,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_prev_symbol(cx);
        });
        cx.notify();
    }

    fn handle_select_next_token(
        &mut self,
        _: &SelectNextToken,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_next_token(cx);
        });
        cx.notify();
    }

    fn handle_select_prev_token(
        &mut self,
        _: &SelectPrevToken,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_prev_token(cx);
        });
        cx.notify();
    }

    fn handle_select_left(
        &mut self,
        _: &SelectLeft,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_left(cx);
        });
        cx.notify();
    }

    fn handle_select_right(
        &mut self,
        _: &SelectRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_right(cx);
        });
        cx.notify();
    }

    fn handle_select_up(&mut self, _: &SelectUp, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_up(cx);
        });
        cx.notify();
    }

    fn handle_select_down(
        &mut self,
        _: &SelectDown,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_down(cx);
        });
        cx.notify();
    }

    fn handle_select_to_line_start(
        &mut self,
        _: &SelectToLineStart,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_to_line_start(cx);
        });
        cx.notify();
    }

    fn handle_select_to_line_end(
        &mut self,
        _: &SelectToLineEnd,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_to_line_end(cx);
        });
        cx.notify();
    }

    fn handle_new_line(&mut self, _: &NewLine, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.new_line(cx);
        });
        cx.notify();
    }

    fn handle_delete_line(
        &mut self,
        _: &DeleteLine,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_line(cx);
        });
        cx.notify();
    }

    fn handle_delete_to_end_of_line(
        &mut self,
        _: &DeleteToEndOfLine,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_to_end_of_line(cx);
        });
        cx.notify();
    }

    fn handle_move_up(&mut self, _: &MoveUp, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_up(cx);
        });
        cx.notify();
    }

    fn handle_move_down(&mut self, _: &MoveDown, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_down(cx);
        });
        cx.notify();
    }

    fn handle_move_left(&mut self, _: &MoveLeft, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_left(cx);
        });
        cx.notify();
    }

    fn handle_move_right(
        &mut self,
        _: &MoveRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_right(cx);
        });
        cx.notify();
    }

    fn handle_move_to_line_start(
        &mut self,
        _: &MoveToLineStart,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_to_line_start(cx);
        });
        cx.notify();
    }

    fn handle_move_to_line_end(
        &mut self,
        _: &MoveToLineEnd,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_to_line_end(cx);
        });
        cx.notify();
    }

    fn handle_move_word_left(
        &mut self,
        _: &MoveWordLeft,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_word_left(cx);
        });
        cx.notify();
    }

    fn handle_move_word_right(
        &mut self,
        _: &MoveWordRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_word_right(cx);
        });
        cx.notify();
    }

    fn handle_move_to_file_start(
        &mut self,
        _: &MoveToFileStart,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_to_file_start(cx);
        });
        cx.notify();
    }

    fn handle_move_to_file_end(
        &mut self,
        _: &MoveToFileEnd,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_to_file_end(cx);
        });
        cx.notify();
    }

    fn handle_page_up(&mut self, _: &PageUp, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.page_up(cx);
        });
        cx.notify();
    }

    fn handle_page_down(&mut self, _: &PageDown, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.page_down(cx);
        });
        cx.notify();
    }

    fn handle_enter_insert_mode(
        &mut self,
        _: &EnterInsertMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_insert_mode(cx);
        });
        cx.notify();
    }

    fn handle_enter_normal_mode(
        &mut self,
        _: &EnterNormalMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_normal_mode(cx);
        });
        cx.notify();
    }

    fn handle_enter_space_mode(
        &mut self,
        _: &EnterSpaceMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_space_mode(cx);
        });
        cx.notify();
    }

    fn handle_enter_pane_mode(
        &mut self,
        _: &EnterPaneMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_pane_mode(cx);
        });
        cx.notify();
    }

    fn handle_enter_git_filter_mode(
        &mut self,
        _: &EnterGitFilterMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_git_filter_mode(cx);
        });
        cx.notify();
    }

    fn handle_enter_visual_mode(
        &mut self,
        _: &EnterVisualMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_visual_mode(cx);
        });
        cx.notify();
    }

    fn handle_set_key_context(
        &mut self,
        action: &SetKeyContext,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.handle_set_key_context(action.0, cx);
        });
        cx.notify();
    }

    fn handle_set_mode(
        &mut self,
        action: &SetMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.set_mode_by_name(&action.0, cx);
        });
        cx.notify();
    }

    // FIXME: BufferFinder handlers moved to PaneGroupView
    // FIXME: GitStatus handlers moved to PaneGroupView

    fn handle_toggle_diff_hunk(
        &mut self,
        _: &ToggleDiffHunk,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        tracing::info!("handle_toggle_diff_hunk called in EditorView");
        self.stoat.update(cx, |stoat, cx| {
            stoat.toggle_diff_hunk(cx);
        });
        cx.notify();
    }

    fn handle_goto_next_hunk(
        &mut self,
        _: &GotoNextHunk,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.goto_next_hunk(cx);
        });
        cx.notify();
    }

    fn handle_goto_prev_hunk(
        &mut self,
        _: &GotoPrevHunk,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.goto_prev_hunk(cx);
        });
        cx.notify();
    }

    fn handle_diff_review_next_hunk(
        &mut self,
        _: &DiffReviewNextHunk,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.diff_review_next_hunk(cx);
        });
        cx.notify();
    }

    fn handle_diff_review_prev_hunk(
        &mut self,
        _: &DiffReviewPrevHunk,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.diff_review_prev_hunk(cx);
        });
        cx.notify();
    }

    fn handle_diff_review_approve_hunk(
        &mut self,
        _: &DiffReviewApproveHunk,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.diff_review_approve_hunk(cx);
        });
        cx.notify();
    }

    fn handle_diff_review_dismiss(
        &mut self,
        _: &DiffReviewDismiss,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.diff_review_dismiss(cx);
        });
        cx.notify();
    }

    fn handle_diff_review_toggle_approval(
        &mut self,
        _: &DiffReviewToggleApproval,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.diff_review_toggle_approval(cx);
        });
        cx.notify();
    }

    fn handle_diff_review_next_unreviewed_hunk(
        &mut self,
        _: &DiffReviewNextUnreviewedHunk,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.diff_review_next_unreviewed_hunk(cx);
        });
        cx.notify();
    }

    fn handle_diff_review_reset_progress(
        &mut self,
        _: &DiffReviewResetProgress,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.diff_review_reset_progress(cx);
        });
        cx.notify();
    }

    fn handle_diff_review_cycle_comparison_mode(
        &mut self,
        _: &DiffReviewCycleComparisonMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.diff_review_cycle_comparison_mode(cx);
        });
        cx.notify();
    }

    fn handle_write_file(
        &mut self,
        _: &WriteFile,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            if let Err(e) = stoat.write_file(cx) {
                tracing::error!("WriteFile action failed: {}", e);
            }
        });
        cx.notify();
    }

    fn handle_write_all(&mut self, _: &WriteAll, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            if let Err(e) = stoat.write_all(cx) {
                tracing::error!("WriteAll action failed: {}", e);
            }
        });
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!(
            "KeyDownEvent: keystroke={:?}, key_char={:?}",
            event.keystroke, event.keystroke.key_char
        );

        let key_context = self.stoat.read(cx).key_context();
        let mode = self.stoat.read(cx).mode().to_string();

        // Only dispatch InsertText for contexts/modes that allow text input
        let should_insert = match key_context {
            KeyContext::FileFinder | KeyContext::CommandPalette | KeyContext::BufferFinder => true,
            KeyContext::TextEditor => mode == "insert",
            _ => false,
        };

        if should_insert {
            if let Some(key_char) = &event.keystroke.key_char {
                window.dispatch_action(Box::new(InsertText(key_char.clone())), cx);
            }
        }
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let mode = self.stoat.read(cx).mode().to_string();

        let view_entity = self
            .this
            .clone()
            .expect("EditorView entity not set - call set_entity() after creation");

        div()
            .id("editor")
            .key_context({
                let context = self.stoat.read(cx).key_context();
                let mut ctx = gpui::KeyContext::new_with_defaults();
                ctx.add(context.as_str());
                ctx.set("mode", mode.clone());
                ctx
            })
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::handle_insert_text))
            .on_action(cx.listener(Self::handle_delete_left))
            .on_action(cx.listener(Self::handle_delete_right))
            .on_action(cx.listener(Self::handle_delete_word_left))
            .on_action(cx.listener(Self::handle_delete_word_right))
            .on_action(cx.listener(Self::handle_select_next_symbol))
            .on_action(cx.listener(Self::handle_select_prev_symbol))
            .on_action(cx.listener(Self::handle_select_next_token))
            .on_action(cx.listener(Self::handle_select_prev_token))
            .on_action(cx.listener(Self::handle_select_left))
            .on_action(cx.listener(Self::handle_select_right))
            .on_action(cx.listener(Self::handle_select_up))
            .on_action(cx.listener(Self::handle_select_down))
            .on_action(cx.listener(Self::handle_select_to_line_start))
            .on_action(cx.listener(Self::handle_select_to_line_end))
            .on_action(cx.listener(Self::handle_new_line))
            .on_action(cx.listener(Self::handle_delete_line))
            .on_action(cx.listener(Self::handle_delete_to_end_of_line))
            .on_action(cx.listener(Self::handle_move_up))
            .on_action(cx.listener(Self::handle_move_down))
            .on_action(cx.listener(Self::handle_move_left))
            .on_action(cx.listener(Self::handle_move_right))
            .on_action(cx.listener(Self::handle_move_to_line_start))
            .on_action(cx.listener(Self::handle_move_to_line_end))
            .on_action(cx.listener(Self::handle_move_word_left))
            .on_action(cx.listener(Self::handle_move_word_right))
            .on_action(cx.listener(Self::handle_move_to_file_start))
            .on_action(cx.listener(Self::handle_move_to_file_end))
            .on_action(cx.listener(Self::handle_page_up))
            .on_action(cx.listener(Self::handle_page_down))
            .on_action(cx.listener(Self::handle_enter_insert_mode))
            .on_action(cx.listener(Self::handle_enter_normal_mode))
            .on_action(cx.listener(Self::handle_enter_visual_mode))
            .on_action(cx.listener(Self::handle_enter_space_mode))
            .on_action(cx.listener(Self::handle_enter_pane_mode))
            .on_action(cx.listener(Self::handle_enter_git_filter_mode))
            .on_action(cx.listener(Self::handle_set_key_context))
            .on_action(cx.listener(Self::handle_set_mode))
            // FIXME: BufferFinder actions now handled by PaneGroupView
            // FIXME: GitStatus actions now handled by PaneGroupView
            .on_action(cx.listener(Self::handle_toggle_diff_hunk))
            .on_action(cx.listener(Self::handle_goto_next_hunk))
            .on_action(cx.listener(Self::handle_goto_prev_hunk))
            .on_action(cx.listener(Self::handle_diff_review_next_hunk))
            .on_action(cx.listener(Self::handle_diff_review_prev_hunk))
            .on_action(cx.listener(Self::handle_diff_review_approve_hunk))
            .on_action(cx.listener(Self::handle_diff_review_toggle_approval))
            .on_action(cx.listener(Self::handle_diff_review_next_unreviewed_hunk))
            .on_action(cx.listener(Self::handle_diff_review_reset_progress))
            .on_action(cx.listener(Self::handle_diff_review_dismiss))
            .on_action(cx.listener(Self::handle_diff_review_cycle_comparison_mode))
            .on_action(cx.listener(Self::handle_write_file))
            .on_action(cx.listener(Self::handle_write_all))
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_scroll_wheel(cx.listener(
                |view: &mut EditorView,
                 event: &ScrollWheelEvent,
                 _window: &mut Window,
                 cx: &mut Context<'_, EditorView>| {
                    // Invert Y direction for natural scrolling
                    let delta = match event.delta {
                        gpui::ScrollDelta::Pixels(pixels) => {
                            scroll::ScrollDelta::Pixels(point(pixels.x, -pixels.y))
                        },
                        gpui::ScrollDelta::Lines(lines) => {
                            scroll::ScrollDelta::Lines(point(lines.x, -lines.y))
                        },
                    };
                    let fast_scroll = event.modifiers.alt;

                    view.stoat.update(cx, |stoat, cx| {
                        stoat.handle_scroll(&delta, fast_scroll, cx);
                    });
                    cx.notify();
                },
            ))
            .relative() // Enable absolute positioning for children
            .size_full()
            .child(EditorElement::new(view_entity, self.editor_style.clone()))
        // FIXME: Minimap rendering will be integrated into EditorElement following Zed's approach
    }
}

impl crate::content_view::ContentView for EditorView {
    fn view_type(&self) -> crate::content_view::ViewType {
        crate::content_view::ViewType::Editor
    }

    fn stoat(&self) -> Option<&Entity<Stoat>> {
        Some(&self.stoat)
    }
}
