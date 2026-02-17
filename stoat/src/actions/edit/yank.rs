use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Yank (copy) selected text to clipboard and enter normal mode.
    pub fn yank(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        let selections = self.selections.all::<text::Point>(&snapshot);
        let mut texts = Vec::new();
        for selection in &selections {
            if !selection.is_empty() {
                let start = snapshot.point_to_offset(selection.start);
                let end = snapshot.point_to_offset(selection.end);
                let text: String = snapshot.text_for_range(start..end).collect();
                texts.push(text);
            }
        }

        if !texts.is_empty() {
            let combined = texts.join("\n");
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(combined));
        }

        self.set_mode_by_name("normal", cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn enters_normal_mode_after_yank(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_mode_by_name("visual", cx);
            s.yank(cx);
            assert_eq!(s.mode(), "normal");
        });
    }
}
