use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::{
    style::{Color, Style},
    text::Text,
    widgets::Paragraph,
    Frame,
};
use std::io;

pub struct Stoat {
    terminal_events: EventStream,
    dirty: bool,
}

impl Default for Stoat {
    fn default() -> Self {
        Self::new()
    }
}

impl Stoat {
    pub fn new() -> Self {
        Self {
            terminal_events: EventStream::new(),
            dirty: true,
        }
    }

    /// Returns `Ok(true)` when a frame should be rendered, `Ok(false)` to exit.
    pub async fn draw(&mut self) -> io::Result<bool> {
        if self.dirty {
            self.dirty = false;
            return Ok(true);
        }

        loop {
            let Some(event) = self.terminal_events.next().await else {
                return Ok(false);
            };
            let event = event?;

            if let Some(should_continue) = self.process_terminal(event) {
                return Ok(should_continue);
            }
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        let text = Text::styled("Stoat", Style::default().fg(Color::Cyan));
        let paragraph = Paragraph::new(text).centered();
        frame.render_widget(paragraph, frame.area());
    }

    fn process_terminal(&mut self, event: Event) -> Option<bool> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.process_key(key.code, key.modifiers)
            },
            Event::Resize(_, _) => {
                self.dirty = true;
                Some(true)
            },
            _ => None,
        }
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
