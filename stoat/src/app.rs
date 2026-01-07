use crate::actions::{Action, Value};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::Text,
    widgets::Paragraph,
    DefaultTerminal,
};
use std::io;

pub struct Stoat {
    should_exit: bool,
}

impl Stoat {
    pub fn new() -> Self {
        Self { should_exit: false }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.should_exit {
            terminal.draw(|frame| self.render(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn render(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let vertical = Layout::vertical([Constraint::Min(0)]);
        let [main] = vertical.areas(area);

        let text = Text::styled("Stoat", Style::default().fg(Color::Cyan));
        let paragraph = Paragraph::new(text).centered();
        frame.render_widget(paragraph, main);
    }

    fn handle_events(&mut self) -> io::Result<()> {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        self.dispatch(Action::Exit);
                    },
                    _ => {},
                }
            }
        }
        Ok(())
    }

    pub fn dispatch(&mut self, action: Action) -> Value {
        match action {
            Action::Exit => {
                self.exit();
                Value::Null
            },
        }
    }

    pub fn exit(&mut self) {
        self.should_exit = true;
    }
}

impl Default for Stoat {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run() -> io::Result<()> {
    let mut stoat = Stoat::new();
    ratatui::run(|terminal| stoat.run(terminal))
}
