use crate::{error::Result, input::Input};

/// State to track mode input mapping.
pub struct Mode {}
impl Mode {
    // TODO: Eventually some current state/impls of Stoat are going to have to be passed here,
    // allowing a node impl to alter commands/etc. Alternatively we can just have some sort of
    // `Commmand::Node(..)` pass thru, thereby leaving it all in the Node court.
    //
    // Not sure yet, about anything, at all, ever.
    //
    // pub async fn input(_input: Input, _active: Option<&'_ dyn Node>) -> Result<()> {
    pub fn input(&mut self, _input: Input) -> Result<()> {
        todo!()
    }
}

pub enum ModeState {}

pub enum Command {}
