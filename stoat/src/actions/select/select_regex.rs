use crate::stoat::Stoat;
use gpui::Context;
use regex::Regex;
use text::{Point, Selection, SelectionGoal};

impl Stoat {
    /// Enter select-regex mode, beginning pattern accumulation.
    pub fn select_regex(&mut self, _cx: &mut Context<Self>) {
        self.select_regex_pending = Some(String::new());
    }

    /// Compile the accumulated regex pattern and replace selections with matches.
    ///
    /// For each current selection, extracts the text, runs `regex.find_iter()`,
    /// and creates a new selection at each match. If no matches are found or the
    /// regex is invalid, keeps the original selections.
    pub fn select_regex_submit(&mut self, cx: &mut Context<Self>) {
        let pattern = match self.select_regex_pending.take() {
            Some(p) if !p.is_empty() => p,
            _ => return,
        };

        let regex = match Regex::new(&pattern) {
            Ok(r) => r,
            Err(_) => return,
        };

        self.record_selection_change();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().read(cx);
        let snapshot = buffer.snapshot();

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
                let abs_start = start_offset + m.start();
                let abs_end = start_offset + m.end();
                new_selections.push(Selection {
                    id: self.selections.next_id(),
                    start: snapshot.offset_to_point(abs_start),
                    end: snapshot.offset_to_point(abs_end),
                    reversed: false,
                    goal: SelectionGoal::None,
                });
            }
        }

        if !new_selections.is_empty() {
            self.selections.select(new_selections, &snapshot);
        }

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
            s.select_regex_pending = Some("bar".to_string());
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("foo <|bar||> baz");
    }

    #[gpui::test]
    fn selects_words(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("<|hello world||>", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex_pending = Some(r"\w+".to_string());
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
            s.select_regex_pending = Some("xyz".to_string());
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("<|foo bar baz||>");
    }

    #[gpui::test]
    fn invalid_regex_noop(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("<|hello world||>", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex_pending = Some("[invalid".to_string());
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("<|hello world||>");
    }

    #[gpui::test]
    fn empty_selection_noop(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("foo |bar baz", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex_pending = Some("bar".to_string());
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("foo |bar baz");
    }

    #[gpui::test]
    fn multiline_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_cursor_notation("<|foo\nbar\nbaz||>", cx).unwrap();
        stoat.update(|s, cx| {
            s.select_regex_pending = Some("bar".to_string());
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
            s.select_regex_submit(cx);
        });
        stoat.assert_cursor_notation("aaa\nfoo <|bar||> baz\nccc");
    }
}
