use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Toggle case of each character in selection or char under cursor.
    pub fn swap_case(&mut self, cx: &mut Context<Self>) {
        self.case_transform(cx, |s| {
            s.chars()
                .map(|c| {
                    if c.is_uppercase() {
                        c.to_lowercase().collect::<String>()
                    } else {
                        c.to_uppercase().collect::<String>()
                    }
                })
                .collect()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn swaps_case(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
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
            s.swap_case(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hELLO");
        });
    }
}
