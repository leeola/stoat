use iced::{widget::text_editor, Sandbox};
use stoat::Stoat;

#[derive(Default)]
pub struct Editor {
    stoat: Stoat,
    content: text_editor::Content,
}

#[derive(Debug, Clone)]
pub enum Message {
    Edit(text_editor::Action),
}

impl Sandbox for Editor {
    type Message = Message;

    fn new() -> Self {
        Self::default()
    }

    fn title(&self) -> String {
        "foo".into()
    }

    fn update(&mut self, message: Self::Message) {
        match message {
            // Message::Edit(action) => self.content.perform(action),
            Message::Edit(_) => {},
        }
    }

    fn view(&self) -> iced::Element<'_, Self::Message> {
        text_editor(&self.content).on_action(Message::Edit).into()
    }
}
