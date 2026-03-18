use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::Text,
    widgets::{Paragraph, Widget},
};
use std::io;
use tokio::sync::mpsc::{Receiver, Sender};

pub struct Stoat {
    size: Rect,
}

impl Default for Stoat {
    fn default() -> Self {
        Self::new()
    }
}

impl Stoat {
    pub fn new() -> Self {
        Self {
            size: Rect::default(),
        }
    }

    pub async fn run(
        &mut self,
        mut events: Receiver<Event>,
        render: Sender<Buffer>,
    ) -> io::Result<()> {
        while let Some(event) = events.recv().await {
            match self.update(event) {
                Some(true) => {
                    if render.send(self.render()).await.is_err() {
                        break;
                    }
                },
                Some(false) => break,
                None => {},
            }
        }
        Ok(())
    }

    /// Returns `Some(true)` to redraw, `Some(false)` to quit, `None` for no visible change.
    fn update(&mut self, event: Event) -> Option<bool> {
        match event {
            Event::Resize(w, h) => {
                self.size = Rect::new(0, 0, w, h);
                Some(true)
            },
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.process_key(key.code, key.modifiers)
            },
            _ => None,
        }
    }

    fn render(&self) -> Buffer {
        let mut buf = Buffer::empty(self.size);
        let text = Text::styled("Stoat", Style::default().fg(Color::Cyan));
        let paragraph = Paragraph::new(text).centered();
        paragraph.render(self.size, &mut buf);
        buf
    }

    fn process_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<bool> {
        match (code, modifiers) {
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => Some(false),
            (KeyCode::Char('q'), KeyModifiers::NONE) => Some(false),
            (KeyCode::Esc, KeyModifiers::NONE) => Some(false),
            _ => None,
        }
    }
}
