use crate::{
    buffer::{BufferId, SharedBuffer},
    display_map::{DisplayMap, DisplayRow, DisplaySnapshot},
    multi_buffer::MultiBuffer,
};
use stoat_text::Point;

pub struct Editor {
    pub buffer_id: BufferId,
    pub buffer: SharedBuffer,
    pub cursor: Point,
    pub scroll_offset: DisplayRow,
    display_map: DisplayMap,
}

impl Editor {
    pub fn new(buffer_id: BufferId, buffer: SharedBuffer) -> Self {
        let multi_buffer = MultiBuffer::singleton(buffer_id, buffer.clone());
        let display_map = DisplayMap::new(multi_buffer);
        Self {
            buffer_id,
            buffer,
            cursor: Point::zero(),
            scroll_offset: DisplayRow(0),
            display_map,
        }
    }

    pub fn display_snapshot(&self) -> DisplaySnapshot {
        self.display_map.snapshot()
    }

    pub fn scroll_up(&mut self, n: u32) {
        self.scroll_offset = DisplayRow(self.scroll_offset.0.saturating_sub(n));
    }

    pub fn scroll_down(&mut self, n: u32) {
        let snapshot = self.display_map.snapshot();
        let max_offset = snapshot.line_count().saturating_sub(1);
        self.scroll_offset = DisplayRow((self.scroll_offset.0 + n).min(max_offset));
    }
}
