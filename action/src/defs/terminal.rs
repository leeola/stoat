use crate::{Action, ActionDef, ActionKind, ActionPriority, ParamDef};
use std::any::Any;

#[derive(Debug)]
pub struct TerminalDef;

impl ActionDef for TerminalDef {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::Terminal
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "open a terminal pane"
    }

    fn long_desc(&self) -> &'static str {
        "Open a subshell in the focused pane. The program and arguments come from the terminal.shell and terminal.args settings, falling back to $SHELL and then /bin/sh."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["term"]
    }
}

#[derive(Debug)]
pub struct Terminal;

impl Terminal {
    pub const DEF: &TerminalDef = &TerminalDef;
}

impl Action for Terminal {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
