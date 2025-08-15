//! Main application using stoat EditorEngine with iced GUI framework.

use crate::{
    effect_runner::run_effects, messages::Message, theme::EditorTheme, widget::EditorWidget,
};
use iced::{
    widget::{column, container, row, text},
    Element, Task,
};
use stoat::{EditorEngine, EditorEvent};

/// Main application state.
pub struct App {
    /// Core editor engine containing all business logic
    engine: EditorEngine,
    /// Visual theme for the editor
    theme: EditorTheme,
    /// Current status message (for user feedback)
    status_message: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        let engine =
            EditorEngine::with_text("Hello, World!\nWelcome to Stoat!\n\nTry editing this text...");

        App {
            engine,
            theme: EditorTheme::default(),
            status_message: None,
        }
    }
}

/// Application functions for iced
impl App {
    /// Creates a new app instance.
    pub fn new() -> (Self, Task<Message>) {
        tracing::info!("Creating new Stoat GUI application");

        let engine =
            EditorEngine::with_text("Hello, World!\nWelcome to Stoat!\n\nTry editing this text...");

        let app = App {
            engine,
            theme: EditorTheme::default(),
            status_message: None,
        };

        tracing::info!("GUI application initialized successfully");
        (app, Task::none())
    }

    /// Handle messages and update state.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        tracing::debug!("GUI handling message: {:?}", message);

        match message {
            Message::EditorInput(event) => {
                tracing::debug!("Processing editor input event");
                let effects = self.engine.handle_event(event);
                run_effects(effects)
            },

            Message::ExitRequested => {
                tracing::info!("Exit requested by user");
                let effects = self.engine.handle_event(EditorEvent::Exit);
                run_effects(effects)
            },

            Message::ShowInfo { ref message } => {
                tracing::info!("Showing info message: {}", message);
                self.status_message = Some(message.clone());
                Task::none()
            },

            Message::ShowError { ref message } => {
                tracing::error!("Showing error message: {}", message);
                self.status_message = Some(format!("Error: {}", message));
                Task::none()
            },

            _ => {
                tracing::debug!("Unhandled message type");
                Task::none()
            },
        }
    }

    /// Create the view.
    pub fn view(&self) -> Element<'_, Message> {
        let editor =
            EditorWidget::new(self.engine.state(), &self.theme).on_input(Message::EditorInput);

        let status_bar = self.create_status_bar();

        let content = column![Element::from(editor), status_bar].spacing(10);

        container(content)
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .padding(10)
            .into()
    }

    /// Create status bar
    fn create_status_bar(&self) -> Element<'_, Message> {
        let cursor_pos = self.engine.cursor_position();
        let mode = self.engine.mode();

        let left_info = text(format!(
            "Line {}, Col {} | {} mode | {} lines",
            cursor_pos.line + 1,
            cursor_pos.column + 1,
            mode_display_name(mode),
            self.engine.line_count()
        ));

        let right_info = if let Some(ref status) = self.status_message {
            text(status.clone())
        } else {
            text("")
        };

        container(row![left_info, iced::widget::horizontal_space(), right_info].spacing(10))
            .padding(5)
            .width(iced::Length::Fill)
            .into()
    }

    /// Get window title
    pub fn title(&self) -> String {
        let file_name = self
            .engine
            .file_path()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled");

        let dirty_marker = if self.engine.is_dirty() { " *" } else { "" };

        format!("GUI v2 - {}{}", file_name, dirty_marker)
    }
}

fn mode_display_name(mode: stoat::actions::EditMode) -> &'static str {
    match mode {
        stoat::actions::EditMode::Normal => "NORMAL",
        stoat::actions::EditMode::Insert => "INSERT",
        stoat::actions::EditMode::Visual { line_mode: false } => "VISUAL",
        stoat::actions::EditMode::Visual { line_mode: true } => "V-LINE",
        stoat::actions::EditMode::Command => "COMMAND",
    }
}

/// Run the GUI application.
pub fn run() -> iced::Result {
    iced::run("GUI v2", App::update, App::view)
}
