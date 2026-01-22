use crate::{
    actions::{Action, Value},
    display_map::BlockRowKind,
    editor::Editor,
    git::DiffStatus,
    keymap::{Binding, Key, KeymapContext},
    view::View,
    workspace::Workspace,
};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
    DefaultTerminal, Frame,
};
use std::{io, path::PathBuf};

pub struct Stoat {
    should_exit: bool,
    bindings: Vec<Binding>,
    mode: &'static str,
    workspace: Workspace,
    viewport_height: u16,
}

impl Stoat {
    pub fn new() -> Self {
        Self {
            should_exit: false,
            bindings: Vec::new(),
            mode: "normal",
            workspace: Workspace::default(),
            viewport_height: 24,
        }
    }

    pub fn open_file(&mut self, path: PathBuf) -> io::Result<()> {
        self.workspace.open_file(path)
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

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        self.viewport_height = area.height;

        match self.workspace.active_pane().active_view() {
            Some(View::Editor(editor)) => {
                self.render_editor(frame, area, editor);
            },
            None => {
                let text = Text::styled("Stoat", Style::default().fg(Color::Cyan));
                let paragraph = Paragraph::new(text).centered();
                frame.render_widget(paragraph, area);
            },
        }
    }

    fn render_editor(&self, frame: &mut Frame<'_>, area: Rect, editor: &Editor) {
        let gutter_width = 4u16;
        let content_width = area.width.saturating_sub(gutter_width);

        let vertical = Layout::horizontal([
            Constraint::Length(gutter_width),
            Constraint::Length(content_width),
        ]);
        let [gutter_area, content_area] = vertical.areas(area);

        let snapshot = editor.display_snapshot();
        let buffer_lines: Vec<&str> = snapshot.lines().collect();
        let scroll_offset = editor.scroll_offset.0 as usize;
        let visible_lines = area.height as usize;
        let total_display_lines = snapshot.line_count() as usize;

        let mut gutter_lines = Vec::new();
        let mut content_lines = Vec::new();

        for i in 0..visible_lines {
            let display_row = (scroll_offset + i) as u32;
            if (display_row as usize) < total_display_lines {
                match snapshot.classify_row(display_row) {
                    BlockRowKind::BufferRow { buffer_row } => {
                        let has_deletion = snapshot.has_deletion_after(buffer_row);
                        let (marker, color) = match snapshot.line_diff_status(buffer_row) {
                            DiffStatus::Added => {
                                if has_deletion {
                                    ("~", Color::Yellow)
                                } else {
                                    ("+", Color::Green)
                                }
                            },
                            DiffStatus::Modified => ("~", Color::Yellow),
                            DiffStatus::Unchanged => {
                                if has_deletion {
                                    ("-", Color::Red)
                                } else {
                                    (" ", Color::DarkGray)
                                }
                            },
                        };
                        let num_str = format!("{}{:>3}", marker, buffer_row + 1);
                        gutter_lines.push(Line::from(Span::styled(
                            num_str,
                            Style::default().fg(color),
                        )));
                        let line_content =
                            buffer_lines.get(buffer_row as usize).copied().unwrap_or("");
                        content_lines.push(Line::from(line_content));
                    },
                    BlockRowKind::Block { block, line_index } => {
                        let num_str = "   -".to_string();
                        gutter_lines.push(Line::from(Span::styled(
                            num_str,
                            Style::default().fg(Color::Red),
                        )));
                        content_lines.push(Line::from(Span::styled(
                            block.get_line(line_index),
                            Style::default().fg(Color::Red),
                        )));
                    },
                }
            } else {
                gutter_lines.push(Line::from(Span::styled(
                    "  ~ ",
                    Style::default().fg(Color::DarkGray),
                )));
                content_lines.push(Line::from(""));
            }
        }

        let gutter = Paragraph::new(gutter_lines);
        let content = Paragraph::new(content_lines);

        frame.render_widget(gutter, gutter_area);
        frame.render_widget(content, content_area);
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
            Action::ScrollDown(n) => {
                self.with_active_editor(|editor| editor.scroll_down(n));
                Value::Null
            },
            Action::ScrollUp(n) => {
                self.with_active_editor(|editor| editor.scroll_up(n));
                Value::Null
            },
            Action::PageDown => {
                let half_page = (self.viewport_height / 2) as u32;
                self.with_active_editor(|editor| editor.scroll_down(half_page));
                Value::Null
            },
            Action::PageUp => {
                let half_page = (self.viewport_height / 2) as u32;
                self.with_active_editor(|editor| editor.scroll_up(half_page));
                Value::Null
            },
        }
    }

    fn with_active_editor<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Editor),
    {
        if let Some(View::Editor(editor)) = self.workspace.active_pane_mut().active_view_mut() {
            f(editor);
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

pub fn run(mut stoat: Stoat) -> io::Result<()> {
    ratatui::run(|terminal| stoat.run(terminal))
}
