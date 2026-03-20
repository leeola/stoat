use crate::{kind::ActionKind, param::ParamDef};
use std::{any::Any, fmt::Debug};

pub trait ActionDef: Debug + Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn kind(&self) -> ActionKind;
    fn params(&self) -> &'static [ParamDef];
}

pub trait Action: Debug + Send + 'static {
    fn def(&self) -> &'static dyn ActionDef;
    fn kind(&self) -> ActionKind {
        self.def().kind()
    }
    fn as_any(&self) -> &dyn Any;
}

#[derive(Debug)]
pub struct QuitDef;

impl ActionDef for QuitDef {
    fn name(&self) -> &'static str {
        "Quit"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::Quit
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }
}

#[derive(Debug)]
pub struct Quit;

impl Quit {
    pub const DEF: &QuitDef = &QuitDef;
}

impl Action for Quit {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_kind() {
        assert_eq!(Quit.kind(), ActionKind::Quit);
    }

    #[test]
    fn quit_def() {
        assert_eq!(Quit.def().name(), "Quit");
        assert!(Quit.def().params().is_empty());
    }

    #[test]
    fn quit_downcast() {
        let action: Box<dyn Action> = Box::new(Quit);
        assert!(action.as_any().downcast_ref::<Quit>().is_some());
    }
}
