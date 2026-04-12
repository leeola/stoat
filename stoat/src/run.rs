mod command_buffer;
pub mod pty;
pub mod vterm;

pub use command_buffer::CommandBuffer;
pub use pty::{spawn_oneshot, spawn_shell, PtyNotification, ShellHandle};
use slotmap::new_key_type;
use std::path::PathBuf;
pub use vterm::{OutputBlock, StyledCell, VtermGrid};

new_key_type! {
    pub struct RunId;
}

pub struct RunState {
    pub input: CommandBuffer,
    pub blocks: Vec<OutputBlock>,
    pub scroll_offset: usize,
    pub cwd: PathBuf,
    pub shell_handle: Option<ShellHandle>,
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,
    pub title: Option<String>,
}

impl RunState {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            input: CommandBuffer::new(),
            blocks: Vec::new(),
            scroll_offset: 0,
            cwd,
            shell_handle: None,
            history: Vec::new(),
            history_cursor: None,
            title: None,
        }
    }

    pub fn active_block(&self) -> Option<&OutputBlock> {
        self.blocks.last()
    }

    pub fn active_block_mut(&mut self) -> Option<&mut OutputBlock> {
        self.blocks.last_mut()
    }

    pub fn is_running(&self) -> bool {
        self.blocks.last().is_some_and(|b| !b.finished)
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_cursor {
            Some(i) if i > 0 => i - 1,
            Some(_) => return,
            None => self.history.len() - 1,
        };
        self.history_cursor = Some(idx);
        self.input.set(self.history[idx].clone());
    }

    pub fn history_down(&mut self) {
        let Some(idx) = self.history_cursor else {
            return;
        };
        if idx + 1 < self.history.len() {
            self.history_cursor = Some(idx + 1);
            self.input.set(self.history[idx + 1].clone());
        } else {
            self.history_cursor = None;
            self.input.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty::parse_sentinel_line;
    use ratatui::style::Color;

    #[test]
    fn insert_and_read() {
        let mut buf = CommandBuffer::new();
        buf.insert_char('h');
        buf.insert_char('i');
        assert_eq!(buf.as_str(), "hi");
        assert_eq!(buf.cursor_column(), 2);
    }

    #[test]
    fn insert_at_middle() {
        let mut buf = CommandBuffer::new();
        buf.insert_char('a');
        buf.insert_char('c');
        buf.move_left();
        buf.insert_char('b');
        assert_eq!(buf.as_str(), "abc");
    }

    #[test]
    fn delete_backward_at_start() {
        let mut buf = CommandBuffer::new();
        buf.delete_backward();
        assert_eq!(buf.as_str(), "");
    }

    #[test]
    fn delete_backward_middle() {
        let mut buf = CommandBuffer::new();
        for ch in "abc".chars() {
            buf.insert_char(ch);
        }
        buf.move_left();
        buf.delete_backward();
        assert_eq!(buf.as_str(), "ac");
    }

    #[test]
    fn delete_forward() {
        let mut buf = CommandBuffer::new();
        for ch in "abc".chars() {
            buf.insert_char(ch);
        }
        buf.move_home();
        buf.delete_forward();
        assert_eq!(buf.as_str(), "bc");
    }

    #[test]
    fn move_boundaries() {
        let mut buf = CommandBuffer::new();
        for ch in "hi".chars() {
            buf.insert_char(ch);
        }
        buf.move_right();
        assert_eq!(buf.cursor_column(), 2);
        buf.move_home();
        assert_eq!(buf.cursor_column(), 0);
        buf.move_left();
        assert_eq!(buf.cursor_column(), 0);
    }

    #[test]
    fn multibyte_utf8() {
        let mut buf = CommandBuffer::new();
        buf.insert_char('a');
        buf.insert_char('\u{00e9}');
        buf.insert_char('b');
        assert_eq!(buf.as_str(), "a\u{00e9}b");
        buf.move_left();
        buf.delete_backward();
        assert_eq!(buf.as_str(), "ab");
    }

    #[test]
    fn take_drains() {
        let mut buf = CommandBuffer::new();
        for ch in "hello".chars() {
            buf.insert_char(ch);
        }
        let s = buf.take();
        assert_eq!(s, "hello");
        assert!(buf.is_empty());
        assert_eq!(buf.cursor_column(), 0);
    }

    #[test]
    fn word_movement() {
        let mut buf = CommandBuffer::new();
        for ch in "foo bar baz".chars() {
            buf.insert_char(ch);
        }
        buf.move_home();
        buf.move_word_right();
        assert_eq!(buf.cursor_column(), 4);
        buf.move_word_right();
        assert_eq!(buf.cursor_column(), 8);
        buf.move_word_left();
        assert_eq!(buf.cursor_column(), 4);
        buf.move_word_left();
        assert_eq!(buf.cursor_column(), 0);
    }

    #[test]
    fn plain_text() {
        let mut grid = VtermGrid::new(80);
        grid.feed(b"hello");
        assert_eq!(grid.line_count(), 1);
        let row = grid.row(0);
        let text: String = row[..5].iter().map(|c| c.ch).collect();
        assert_eq!(text, "hello");
    }

    #[test]
    fn newline_creates_row() {
        let mut grid = VtermGrid::new(80);
        grid.feed(b"a\nb");
        assert_eq!(grid.line_count(), 2);
        assert_eq!(grid.row(0)[0].ch, 'a');
        assert_eq!(grid.row(1)[0].ch, 'b');
    }

    #[test]
    fn sgr_color() {
        let mut grid = VtermGrid::new(80);
        grid.feed(b"\x1b[31mR\x1b[0mX");
        assert_eq!(grid.row(0)[0].ch, 'R');
        assert_eq!(grid.row(0)[0].fg, Some(Color::Red));
        assert_eq!(grid.row(0)[1].ch, 'X');
        assert_eq!(grid.row(0)[1].fg, None);
    }

    #[test]
    fn alt_screen_detected() {
        let mut grid = VtermGrid::new(80);
        assert!(!grid.alt_screen_detected);
        grid.feed(b"\x1b[?1049h");
        assert!(grid.alt_screen_detected);
    }

    #[test]
    fn carriage_return() {
        let mut grid = VtermGrid::new(80);
        grid.feed(b"abc\rX");
        assert_eq!(grid.row(0)[0].ch, 'X');
        assert_eq!(grid.row(0)[1].ch, 'b');
    }

    #[test]
    fn erase_in_line() {
        let mut grid = VtermGrid::new(80);
        grid.feed(b"abcdef");
        grid.feed(b"\x1b[3D\x1b[K");
        let row = grid.row(0);
        assert_eq!(row[0].ch, 'a');
        assert_eq!(row[1].ch, 'b');
        assert_eq!(row[2].ch, 'c');
        assert_eq!(row[3].ch, ' ');
        assert_eq!(row[4].ch, ' ');
    }

    #[test]
    fn parse_sentinel() {
        assert_eq!(parse_sentinel_line("__STOAT_abc123__ 0"), Some(0));
        assert_eq!(parse_sentinel_line("__STOAT_abc123__ 1"), Some(1));
        assert_eq!(parse_sentinel_line("__STOAT_abc123__ 127"), Some(127));
        assert_eq!(parse_sentinel_line("not a sentinel"), None);
    }
}
