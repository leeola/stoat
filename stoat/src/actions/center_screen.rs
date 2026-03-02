use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    pub fn center_screen(&mut self, cx: &mut Context<Self>) {
        let viewport = self.viewport_lines.unwrap_or(30.0);
        let cursor_row = self.cursor.position().row as f32;
        let target_y = (cursor_row - viewport / 2.0).max(0.0);
        self.scroll
            .start_animation_to(gpui::point(self.scroll.position.x, target_y));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use text::Point;

    #[gpui::test]
    fn centers_viewport_on_cursor(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let mut lines = vec![];
            for i in 0..100 {
                lines.push(format!("Line {i}"));
            }
            s.insert_text(&lines.join("\n"), cx);
            s.set_cursor_position(Point::new(50, 0));
            s.center_screen(cx);

            let target = 50.0 - 30.0 / 2.0;
            assert_eq!(s.scroll.target_position.unwrap().y, target);
        });
    }

    #[gpui::test]
    fn clamps_at_top(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("short\nfile", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.center_screen(cx);

            assert_eq!(s.scroll.target_position.unwrap().y, 0.0);
        });
    }
}
