use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Convert selected text to uppercase. In normal mode with collapsed cursor, operates on char
    /// under cursor.
    pub fn uppercase(&mut self, cx: &mut Context<Self>) {
        self.case_transform(cx, |s| s.to_uppercase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn uppercases_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            let snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![text::Selection {
                    id,
                    start: text::Point::new(0, 0),
                    end: text::Point::new(0, 5),
                    reversed: false,
                    goal: text::SelectionGoal::None,
                }],
                &snapshot,
            );
            s.uppercase(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "HELLO");
        });
    }
}
