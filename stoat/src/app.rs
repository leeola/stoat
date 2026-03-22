use crate::{
    action_handlers,
    pane::{Pane, PaneTree, View},
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::Text,
    widgets::{Block, Borders, Paragraph, Widget},
};
use std::io;
use stoat_action::{Action, Quit};
use tokio::sync::mpsc::{Receiver, Sender};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateEffect {
    Redraw,
    Quit,
    None,
}

pub struct Stoat {
    size: Rect,
    pub panes: PaneTree,
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
            panes: PaneTree::new(Rect::default()),
        }
    }

    pub async fn run(
        &mut self,
        mut events: Receiver<Event>,
        render: Sender<Buffer>,
    ) -> io::Result<()> {
        while let Some(event) = events.recv().await {
            match self.update(event) {
                UpdateEffect::Redraw => {
                    if render.send(self.render()).await.is_err() {
                        break;
                    }
                },
                UpdateEffect::Quit => break,
                UpdateEffect::None => {},
            }
        }
        Ok(())
    }

    fn update(&mut self, event: Event) -> UpdateEffect {
        match event {
            Event::Resize(w, h) => {
                self.size = Rect::new(0, 0, w, h);
                self.panes.resize(self.size);
                UpdateEffect::Redraw
            },
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let Some(action) = self.process_key(key.code, key.modifiers) else {
                    return UpdateEffect::None;
                };
                action_handlers::dispatch(self, &*action)
            },
            _ => UpdateEffect::None,
        }
    }

    fn render(&self) -> Buffer {
        let mut buf = Buffer::empty(self.size);
        let focused = self.panes.focus();
        for (id, pane) in self.panes.split_panes() {
            let is_focused = id == focused;
            render_pane(pane, is_focused, &mut buf);
        }
        buf
    }

    fn process_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Box<dyn Action>> {
        match (code, modifiers) {
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => Some(Box::new(Quit)),
            (KeyCode::Char('q'), KeyModifiers::NONE) => Some(Box::new(Quit)),
            (KeyCode::Esc, KeyModifiers::NONE) => Some(Box::new(Quit)),
            _ => None,
        }
    }
}

fn render_pane(pane: &Pane, is_focused: bool, buf: &mut Buffer) {
    let label = match &pane.view {
        View::Label(s) => s.as_str(),
    };

    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(pane.area);
    block.render(pane.area, buf);

    let text_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Paragraph::new(Text::styled(label, text_style))
        .centered()
        .render(inner, buf);
}
