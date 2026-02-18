use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

/// Pending find-char mode direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindCharMode {
    Forward,
    Backward,
    TillForward,
    TillBackward,
}

impl Stoat {
    /// Set pending find-char mode to forward (`f`).
    pub fn find_char_forward(&mut self, _cx: &mut Context<Self>) {
        self.find_char_pending = Some(FindCharMode::Forward);
    }

    /// Set pending find-char mode to backward (`F`).
    pub fn find_char_backward(&mut self, _cx: &mut Context<Self>) {
        self.find_char_pending = Some(FindCharMode::Backward);
    }

    /// Set pending find-char mode to till-forward (`t`).
    pub fn till_char_forward(&mut self, _cx: &mut Context<Self>) {
        self.find_char_pending = Some(FindCharMode::TillForward);
    }

    /// Set pending find-char mode to till-backward (`T`).
    pub fn till_char_backward(&mut self, _cx: &mut Context<Self>) {
        self.find_char_pending = Some(FindCharMode::TillBackward);
    }

    /// Execute the find-char operation with the given character and mode.
    ///
    /// For each cursor, scans the current line in the given direction for `ch`.
    /// `Forward`/`Backward` land on the character; `TillForward`/`TillBackward`
    /// land one position before it. Respects count prefix.
    pub fn find_char_with(&mut self, ch: &str, mode: FindCharMode, cx: &mut Context<Self>) {
        self.record_selection_change();
        let count = self.take_count();
        let buffer_snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&buffer_snapshot);
            if newest_sel.head() != cursor_pos {
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: cursor_pos,
                        end: cursor_pos,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &buffer_snapshot,
                );
            }
        }

        let target_char = match ch.chars().next() {
            Some(c) => c,
            None => return,
        };

        let mut selections = self.selections.all::<Point>(&buffer_snapshot);
        for selection in &mut selections {
            let head = selection.head();
            let head_offset = buffer_snapshot.point_to_offset(head);

            let found = match mode {
                FindCharMode::Forward | FindCharMode::TillForward => {
                    find_forward(&buffer_snapshot, head_offset, head.row, target_char, count)
                },
                FindCharMode::Backward | FindCharMode::TillBackward => {
                    find_backward(&buffer_snapshot, head_offset, head.row, target_char, count)
                },
            };

            if let Some(found_offset) = found {
                let adjusted = match mode {
                    FindCharMode::TillForward => {
                        // One position before the found char
                        if found_offset > head_offset {
                            prev_char_offset(&buffer_snapshot, found_offset)
                        } else {
                            found_offset
                        }
                    },
                    FindCharMode::TillBackward => {
                        // One position after the found char
                        next_char_offset(&buffer_snapshot, found_offset)
                    },
                    _ => found_offset,
                };

                let new_pos = buffer_snapshot.offset_to_point(adjusted);
                selection.start = new_pos;
                selection.end = new_pos;
                selection.reversed = false;
                selection.goal = text::SelectionGoal::None;
            }
        }

        self.selections.select(selections.clone(), &buffer_snapshot);
        if let Some(last) = selections.last() {
            self.cursor.move_to(last.head());
        }

        cx.notify();
    }
}

fn find_forward(
    snapshot: &text::BufferSnapshot,
    offset: usize,
    row: u32,
    target: char,
    count: u32,
) -> Option<usize> {
    let mut pos = offset;
    let mut chars = snapshot.chars_at(offset);

    // Skip current char
    if let Some(first) = chars.next() {
        pos += first.len_utf8();
    }

    let mut found_count = 0u32;
    for ch in chars {
        let ch_point = snapshot.offset_to_point(pos);
        if ch_point.row != row {
            break;
        }
        if ch == target {
            found_count += 1;
            if found_count == count {
                return Some(pos);
            }
        }
        pos += ch.len_utf8();
    }

    None
}

fn find_backward(
    snapshot: &text::BufferSnapshot,
    offset: usize,
    row: u32,
    target: char,
    count: u32,
) -> Option<usize> {
    if offset == 0 {
        return None;
    }

    let mut pos = offset;
    let chars = snapshot.reversed_chars_at(offset);

    let mut found_count = 0u32;
    for ch in chars {
        pos -= ch.len_utf8();
        let ch_point = snapshot.offset_to_point(pos);
        if ch_point.row != row {
            break;
        }
        if ch == target {
            found_count += 1;
            if found_count == count {
                return Some(pos);
            }
        }
    }

    None
}

fn prev_char_offset(snapshot: &text::BufferSnapshot, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut chars = snapshot.reversed_chars_at(offset);
    match chars.next() {
        Some(ch) => offset - ch.len_utf8(),
        None => offset,
    }
}

fn next_char_offset(snapshot: &text::BufferSnapshot, offset: usize) -> usize {
    let mut chars = snapshot.chars_at(offset);
    match chars.next() {
        Some(ch) => offset + ch.len_utf8(),
        None => offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn find_forward_basic(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.find_char_with("o", FindCharMode::Forward, cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 4));
        });
    }

    #[gpui::test]
    fn find_forward_with_count(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.pending_count = Some(2);
            s.find_char_with("o", FindCharMode::Forward, cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 7));
        });
    }

    #[gpui::test]
    fn find_backward_basic(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(Point::new(0, 10));
            s.find_char_with("o", FindCharMode::Backward, cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 7));
        });
    }

    #[gpui::test]
    fn till_forward_stops_before(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.find_char_with("o", FindCharMode::TillForward, cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 3));
        });
    }

    #[gpui::test]
    fn till_backward_stops_after(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(Point::new(0, 10));
            s.find_char_with("o", FindCharMode::TillBackward, cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 8));
        });
    }

    #[gpui::test]
    fn no_match_stays_put(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.find_char_with("z", FindCharMode::Forward, cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 0));
        });
    }

    #[gpui::test]
    fn stays_on_current_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello\nworld", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.find_char_with("w", FindCharMode::Forward, cx);
            // 'w' is on line 1, should not find it
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 0));
        });
    }
}
