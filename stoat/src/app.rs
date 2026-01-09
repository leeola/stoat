use crate::{
    actions::{Action, Value},
    keymap::{Binding, Key, KeymapContext},
};
use crossterm::event::{self, Event, KeyEventKind};
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
    bindings: Vec<Binding>,
    mode: &'static str,
}

impl Stoat {
    pub fn new() -> Self {
        Self {
            should_exit: false,
            bindings: Vec::new(),
            mode: "normal",
        }
    }

    pub fn keymap<F>(&mut self, key: Key, action: Action, predicate: F)
    where
        F: Fn(&KeymapContext) -> bool + Send + Sync + 'static,
    {
        self.bindings.push(Binding {
            key,
            action,
            predicate: Box::new(predicate),
        });
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
        if let Event::Key(event) = event::read()? {
            if event.kind == KeyEventKind::Press {
                let key = Key::from(event);
                let ctx = KeymapContext {};
                for binding in &self.bindings {
                    if binding.key == key && (binding.predicate)(&ctx) {
                        self.dispatch(binding.action);
                        break;
                    }
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
            Action::SetMode(mode) => {
                self.set_mode(mode);
                Value::Null
            },
        }
    }

    pub fn exit(&mut self) {
        self.should_exit = true;
    }

    pub fn set_mode(&mut self, mode: &'static str) {
        self.mode = mode;
    }
}

impl Default for Stoat {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run() -> io::Result<()> {
    let mut stoat = Stoat::new();
    stoat.keymap(Key::char('q'), Action::Exit, |_| true);
    stoat.keymap(Key::esc(), Action::Exit, |_| true);
    ratatui::run(|terminal| stoat.run(terminal))
}
