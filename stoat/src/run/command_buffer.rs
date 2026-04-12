pub struct CommandBuffer {
    text: String,
    cursor: usize,
}

impl CommandBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn cursor_column(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn delete_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    pub fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..next);
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = prev_char_boundary(&self.text, self.cursor);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = next_char_boundary(&self.text, self.cursor);
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let bytes = self.text.as_bytes();
        let mut pos = prev_char_boundary(&self.text, self.cursor);
        while pos > 0 && bytes[pos - 1] == b' ' {
            pos = prev_char_boundary(&self.text, pos);
        }
        while pos > 0 && bytes[pos - 1] != b' ' {
            pos = prev_char_boundary(&self.text, pos);
        }
        self.cursor = pos;
    }

    pub fn move_word_right(&mut self) {
        let len = self.text.len();
        if self.cursor >= len {
            return;
        }
        let bytes = self.text.as_bytes();
        let mut pos = next_char_boundary(&self.text, self.cursor);
        while pos < len && bytes[pos] != b' ' {
            pos = next_char_boundary(&self.text, pos);
        }
        while pos < len && bytes[pos] == b' ' {
            pos = next_char_boundary(&self.text, pos);
        }
        self.cursor = pos;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }
}

fn prev_char_boundary(s: &str, from: usize) -> usize {
    let mut pos = from.saturating_sub(1);
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn next_char_boundary(s: &str, from: usize) -> usize {
    let mut pos = from + 1;
    while pos < s.len() && !s.is_char_boundary(pos) {
        pos += 1;
    }
    pos.min(s.len())
}
