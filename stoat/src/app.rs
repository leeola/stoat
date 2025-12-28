use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::Text,
    widgets::Paragraph,
    DefaultTerminal,
};
use std::io::{self, stdout};

pub fn run() -> io::Result<()> {
    stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let result = run_app(ratatui::init());

    stdout().execute(LeaveAlternateScreen)?;
    disable_raw_mode()?;

    result
}

fn run_app(mut terminal: DefaultTerminal) -> io::Result<()> {
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let vertical = Layout::vertical([Constraint::Min(0)]);
            let [main] = vertical.areas(area);

            let text = Text::styled("Stoat", Style::default().fg(Color::Cyan));
            let paragraph = Paragraph::new(text).centered();
            frame.render_widget(paragraph, main);
        })?;

        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    _ => {},
                }
            }
        }
    }

    Ok(())
}
