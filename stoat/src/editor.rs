use crate::buffer::{BufferId, SharedBuffer};
use stoat_text::Point;

pub struct Editor {
    pub buffer_id: BufferId,
    pub buffer: SharedBuffer,
    pub cursor: Point,
    pub scroll_offset: u32,
}

impl Editor {
    pub fn new(buffer_id: BufferId, buffer: SharedBuffer) -> Self {
        Self {
            buffer_id,
            buffer,
            cursor: Point::zero(),
            scroll_offset: 0,
        }
    }

    pub fn scroll_up(&mut self, n: u32) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: u32) {
        let line_count = self
            .buffer
            .read()
            .expect("buffer lock poisoned")
            .line_count();
        let max_offset = line_count.saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + n).min(max_offset);
    }
}
