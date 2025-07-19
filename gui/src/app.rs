use crate::{canvas, input, state::RenderState};
use iced::Element;
use stoat_core::{input::Action, Stoat};
use tracing::{debug, trace, warn};

/// Main application state
pub struct App {
    /// The render state containing all visual data
    render_state: RenderState,
    /// The Stoat editor instance
    stoat: Stoat,
}

/// Application messages
#[derive(Debug, Clone)]
pub enum Message {
    /// Keyboard event received
    KeyPressed(iced::keyboard::Event),
    /// Tick for updating modal system
    Tick,
}

impl App {
    /// Run the application
    pub fn run() -> iced::Result {
        iced::application("Stoat - Node Editor Prototype", Self::update, Self::view)
            .subscription(Self::subscription)
            .window_size(iced::Size::new(1280.0, 720.0))
            .run_with(Self::new)
    }

    fn new() -> (Self, iced::Task<Message>) {
        // Initialize Stoat with default configuration
        let mut stoat = Stoat::new();

        // Try to load the keymap configuration
        if let Ok(keymap_path) = std::env::current_dir().map(|d| d.join("keymap.ron")) {
            if keymap_path.exists() {
                if let Err(e) = stoat.load_modal_config_from_file(&keymap_path) {
                    warn!("Failed to load keymap.ron: {e}");
                }
            }
        }

        (
            Self {
                render_state: RenderState::stub(),
                stoat,
            },
            iced::Task::none(),
        )
    }

    fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::KeyPressed(event) => {
                // Update tick before processing key
                self.stoat.tick();

                if let iced::keyboard::Event::KeyPressed { key, modifiers, .. } = event {
                    // Convert Iced key to Stoat key
                    if let Some(stoat_key) = input::convert_key(key, modifiers) {
                        // Process key through modal system
                        if let Some(action) = self.stoat.user_input(stoat_key) {
                            // Handle the action
                            self.handle_action(action)
                        } else {
                            iced::Task::none()
                        }
                    } else {
                        iced::Task::none()
                    }
                } else {
                    iced::Task::none()
                }
            },
            Message::Tick => {
                // Update the modal system's timeout handling
                self.stoat.tick();
                iced::Task::none()
            },
        }
    }

    fn view(&self) -> Element<'_, Message> {
        use iced::widget::{column, container, text};

        // Create status bar with mode display
        let mode_text = format!("Mode: {}", self.stoat.current_mode().as_str());
        let status_bar = container(text(mode_text).size(16))
            .width(iced::Length::Fill)
            .padding(5)
            .style(|theme: &iced::Theme| container::Style {
                background: Some(iced::Background::Color(
                    theme.extended_palette().background.strong.color,
                )),
                ..Default::default()
            });

        // Create the main content
        let canvas = iced::widget::canvas(canvas::NodeCanvas::new(&self.render_state))
            .width(iced::Length::Fill)
            .height(iced::Length::Fill);

        // Combine status bar and canvas
        column![status_bar, canvas].into()
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        // Keyboard subscription
        iced::keyboard::on_key_press(|key, modifiers| {
            Some(Message::KeyPressed(iced::keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key.clone(),
                physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::KeyA),
                location: iced::keyboard::Location::Standard,
                modifiers,
                text: None,
            }))
        })
    }

    fn handle_action(&mut self, action: Action) -> iced::Task<Message> {
        match action {
            Action::ExitApp => {
                // Exit the application
                iced::exit()
            },
            Action::ChangeMode(mode) => {
                // Mode change is handled internally by ModalSystem
                debug!("Changed to {} mode", mode.as_str());
                iced::Task::none()
            },
            Action::Move(direction) => {
                trace!("Move {direction:?}");
                // TODO: Implement movement in the canvas
                iced::Task::none()
            },
            Action::Delete => {
                trace!("Delete");
                iced::Task::none()
            },
            Action::DeleteLine => {
                trace!("Delete line");
                iced::Task::none()
            },
            Action::Yank => {
                trace!("Yank");
                iced::Task::none()
            },
            Action::YankLine => {
                trace!("Yank line");
                iced::Task::none()
            },
            Action::Paste => {
                trace!("Paste");
                iced::Task::none()
            },
            Action::Jump(target) => {
                trace!("Jump to {target:?}");
                iced::Task::none()
            },
            Action::InsertChar => {
                trace!("Insert character");
                // TODO: Get the actual character from the last key press
                iced::Task::none()
            },
            Action::CommandInput => {
                trace!("Command input");
                iced::Task::none()
            },
            Action::ExecuteCommand => {
                trace!("Execute command");
                iced::Task::none()
            },
            Action::ShowActionList => {
                trace!("Show action list");
                // TODO: Display available actions
                iced::Task::none()
            },
            Action::ShowCommandPalette => {
                trace!("Show command palette");
                // TODO: Display command palette
                iced::Task::none()
            },
        }
    }
}
