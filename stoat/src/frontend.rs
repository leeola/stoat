use crate::backend::Backend;
use action::Action;

pub mod buffer {
    pub struct Buffer;
}
pub mod session {}
pub mod action {
    pub enum Action {}
}
pub mod message {
    pub enum Message {}
}

#[derive(Default)]
pub struct Stoat {
    backend: Backend,
}
impl Stoat {
    // TODO: include method to restore state, load configs, etc.
    pub fn new() -> Self {
        Self::default()
    }
    pub fn input(&self) {
        todo!()
    }
    pub fn input_echo(&self) -> Action {
        todo!()
    }
}
