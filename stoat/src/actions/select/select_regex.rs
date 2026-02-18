use crate::stoat::Stoat;
use gpui::Context;
use regex::Regex;
use text::{Point, Selection, SelectionGoal};

impl Stoat {
    /// Enter select-regex mode, saving current selections for live preview.
    pub fn select_regex(&mut self, _cx: &mut Context<Self>) {
        self.select_regex_base_selections = Some(self.selections.disjoint_anchors_arc());
        self.select_regex_pending = Some(String::new());
    }

    /// Live-preview regex matches against the saved base selections.
    ///
    /// Restores base selections, resolves to points, applies regex, and updates
    /// current selections. Falls back to base selections on empty/invalid pattern
    /// or no matches.
    pub fn select_regex_preview(&mut self, cx: &mut Context<Self>) {
        let Some(ref base) = self.select_regex_base_selections else {
            return;
        };
        let Some(ref pattern) = self.select_regex_pending else {
            return;
        };

        if pattern.is_empty() {
            self.selections.select_anchors(base.clone());
            cx.notify();
            return;
        }
        let Ok(regex) = Regex::new(pattern) else {
            self.selections.select_anchors(base.clone());
            cx.notify();
            return;
        };

        self.selections.select_anchors(base.clone());
        let buffer_item = self.active_buffer(cx);
        let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        let selections: Vec<Selection<Point>> = self.selections.all(&snapshot);

        let mut new_selections = Vec::new();
        for sel in &selections {
            let start_offset = snapshot.point_to_offset(sel.start);
            let end_offset = snapshot.point_to_offset(sel.end);
            if start_offset == end_offset {
                continue;
            }

            let text: String = snapshot.text_for_range(sel.start..sel.end).collect();
            for m in regex.find_iter(&text) {
                if m.start() == m.end() {
                    continue;
                }
                new_selections.push(Selection {
                    id: self.selections.next_id(),
                    start: snapshot.offset_to_point(start_offset + m.start()),
                    end: snapshot.offset_to_point(start_offset + m.end()),
                    reversed: false,
                    goal: SelectionGoal::None,
                });
            }
        }

        if !new_selections.is_empty() {
            self.selections.select(new_selections, &snapshot);
            let newest = self.selections.newest::<Point>(&snapshot);
            self.cursor.move_to(newest.head());
        } else {
            self.selections.select_anchors(base.clone());
            let newest = self.selections.newest::<Point>(&snapshot);
            self.cursor.move_to(newest.head());
        }
        cx.notify();
    }

    /// Cancel select-regex mode, restoring original selections.
    pub fn select_regex_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(base) = self.select_regex_base_selections.take() {
            self.selections.select_anchors(base);
            let buffer_item = self.active_buffer(cx);
            let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
            let newest = self.selections.newest::<Point>(&snapshot);
            self.cursor.move_to(newest.head());
        }
        self.select_regex_pending = None;
        cx.notify();
    }

    /// Confirm the current preview state and exit select-regex mode.
    pub fn select_regex_submit(&mut self, cx: &mut Context<Self>) {
        let had_pattern = self
            .select_regex_pending
            .take()
            .is_some_and(|p| !p.is_empty());
        self.select_regex_base_selections = None;
        if had_pattern {
            self.record_selection_change();
        }
        let buffer_item = self.active_buffer(cx);
        let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        let newest = self.selections.newest::<Point>(&snapshot);
        self.cursor.move_to(newest.head());
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use gpui::TestAppContext;
    use text::Point;

    #[gpui::test]
    fn selects_bar_from_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("<|foo bar baz||>", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex(cx);
            s.select_regex_pending = Some("bar".to_string());
            s.select_regex_preview(cx);
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("foo <|bar||> baz");
    }

    #[gpui::test]
    fn selects_words(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("<|hello world||>", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex(cx);
            s.select_regex_pending = Some(r"\w+".to_string());
            s.select_regex_preview(cx);
            s.select_regex_submit(cx);
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 2);
            assert_eq!(sels[0].start, Point::new(0, 0));
            assert_eq!(sels[0].end, Point::new(0, 5));
            assert_eq!(sels[1].start, Point::new(0, 6));
            assert_eq!(sels[1].end, Point::new(0, 11));
        });
    }

    #[gpui::test]
    fn no_match_keeps_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("<|foo bar baz||>", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex(cx);
            s.select_regex_pending = Some("xyz".to_string());
            s.select_regex_preview(cx);
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("<|foo bar baz||>");
    }

    #[gpui::test]
    fn invalid_regex_noop(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("<|hello world||>", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex(cx);
            s.select_regex_pending = Some("[invalid".to_string());
            s.select_regex_preview(cx);
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("<|hello world||>");
    }

    #[gpui::test]
    fn empty_selection_noop(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("foo |bar baz", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex(cx);
            s.select_regex_pending = Some("bar".to_string());
            s.select_regex_preview(cx);
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("foo |bar baz");
    }

    #[gpui::test]
    fn multiline_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("<|foo\nbar\nbaz||>", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex(cx);
            s.select_regex_pending = Some("bar".to_string());
            s.select_regex_preview(cx);
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("foo\n<|bar||>\nbaz");
    }

    #[gpui::test]
    fn select_line_then_regex(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("aaa\nfoo bar baz\nccc", cx).unwrap();
        stoat.update(|s, cx| {
            s.set_cursor_position(Point::new(1, 0));
            s.select_line(cx);
            s.select_regex(cx);
            s.select_regex_pending = Some("bar".to_string());
            s.select_regex_preview(cx);
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("aaa\nfoo <|bar||> baz\nccc");
    }

    #[gpui::test]
    fn live_preview_narrows_as_you_type(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.insert_text("foo bar baz", cx));
        stoat.type_key("escape");

        stoat.type_action("SelectLine");
        stoat.assert_cursor_notation("<|foo bar baz||>");

        stoat.type_action("SelectRegex");

        stoat.type_key("b");
        stoat.assert_cursor_notation("foo <|b||>ar <|b||>az");

        stoat.type_key("a");
        stoat.assert_cursor_notation("foo <|ba||>r <|ba||>z");

        stoat.type_key("r");
        stoat.assert_cursor_notation("foo <|bar||> baz");

        stoat.type_key("backspace");
        stoat.assert_cursor_notation("foo <|ba||>r <|ba||>z");

        stoat.type_key("enter");
        stoat.assert_cursor_notation("foo <|ba||>r <|ba||>z");
    }

    #[gpui::test]
    fn cancel_restores_original_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.insert_text("foo bar baz", cx));
        stoat.type_key("escape");
        stoat.type_action("SelectLine");
        stoat.assert_cursor_notation("<|foo bar baz||>");

        stoat.type_action("SelectRegex");
        stoat.type_key("b");
        stoat.assert_cursor_notation("foo <|b||>ar <|b||>az");

        stoat.type_key("escape");
        stoat.assert_cursor_notation("<|foo bar baz||>");
    }

    #[gpui::test]
    fn select_regex_then_move_down(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("aaa\nbbb\nccc\nddd", cx);
        stoat.type_action("SelectLine");
        stoat.type_action("SelectLine");
        stoat.type_action("SelectLine");

        stoat.type_action("SelectRegex");
        stoat.type_key("a");
        stoat.type_key("a");
        stoat.type_key("a");
        stoat.type_key("enter");
        stoat.assert_cursor_notation("<|aaa||>\nbbb\nccc\nddd");

        stoat.type_action("MoveDown");
        let sel = stoat.selection();
        assert_eq!(sel.end.row, 1, "move_down should go to next line, not jump");
    }
}
