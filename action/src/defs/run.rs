use crate::{
    action::define_action, Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind,
};
use serde::Deserialize;
use std::any::Any;

define_action!(
    OpenRunDef,
    OpenRun,
    "OpenRun",
    ActionKind::OpenRun,
    "open terminal",
    "Open a terminal pane for running commands.",
    ActionPriority::Common,
    false
);

define_action!(
    OpenTerminalDockDef,
    OpenTerminalDock,
    "OpenTerminalDock",
    ActionKind::OpenTerminalDock,
    "open terminal dock",
    "Open the run pane in a bottom dock, or toggle its visibility when it is already open.",
    ActionPriority::Common,
    false
);

define_action!(
    OpenClaudeTerminalDef,
    OpenClaudeTerminal,
    "OpenClaudeTerminal",
    ActionKind::OpenClaudeTerminal,
    "open claude terminal",
    "Open a terminal pane running the claude CLI in the project root.",
    ActionPriority::Common,
    false
);

define_action!(
    OpenTerminalDef,
    OpenTerminal,
    "terminal",
    ActionKind::OpenTerminal,
    "open shell terminal",
    "Open a terminal pane running your shell in the project root.",
    ActionPriority::Common,
    false
);

define_action!(
    RunSubmitDef,
    RunSubmit,
    "RunSubmit",
    ActionKind::RunSubmit,
    "submit command",
    "Submit the current command line to the shell.",
    ActionPriority::Normal
);

define_action!(
    RunInterruptDef,
    RunInterrupt,
    "RunInterrupt",
    ActionKind::RunInterrupt,
    "interrupt command",
    "Send SIGINT to the running shell command.",
    ActionPriority::Normal
);

define_action!(
    RunHistoryPrevDef,
    RunHistoryPrev,
    "RunHistoryPrev",
    ActionKind::RunHistoryPrev,
    "previous command in history",
    "Replace the run input with the previous entry in command history.",
    ActionPriority::Normal
);

define_action!(
    RunHistoryNextDef,
    RunHistoryNext,
    "RunHistoryNext",
    ActionKind::RunHistoryNext,
    "next command in history",
    "Replace the run input with the next entry in command history, or clear the input when past the end.",
    ActionPriority::Normal
);

const RUN_PARAMS: &[ParamDef] = &[ParamDef {
    name: "command",
    kind: ParamKind::String,
    required: true,
    description: "Shell command to execute in a modal overlay.",
}];

#[derive(Debug)]
pub struct RunDef;

impl ActionDef for RunDef {
    fn name(&self) -> &'static str {
        "Run"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::Run
    }

    fn params(&self) -> &'static [ParamDef] {
        RUN_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "run command"
    }

    fn long_desc(&self) -> &'static str {
        "Run a shell command in a temporary modal overlay. The modal shows output while running and can be dismissed when done."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Run {
    pub command: String,
}

impl Run {
    pub const DEF: &RunDef = &RunDef;
}

impl Action for Run {
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
    fn open_run() {
        assert_eq!(OpenRun.kind(), ActionKind::OpenRun);
        assert_eq!(OpenRun.def().name(), "OpenRun");
        assert!(!OpenRun.def().hint_visible());
    }

    #[test]
    fn open_terminal_dock() {
        assert_eq!(OpenTerminalDock.kind(), ActionKind::OpenTerminalDock);
        assert_eq!(OpenTerminalDock.def().name(), "OpenTerminalDock");
        assert!(!OpenTerminalDock.def().hint_visible());
    }

    #[test]
    fn open_claude_terminal() {
        assert_eq!(OpenClaudeTerminal.kind(), ActionKind::OpenClaudeTerminal);
        assert_eq!(OpenClaudeTerminal.def().name(), "OpenClaudeTerminal");
        assert!(!OpenClaudeTerminal.def().hint_visible());
    }

    #[test]
    fn open_terminal() {
        assert_eq!(OpenTerminal.kind(), ActionKind::OpenTerminal);
        assert_eq!(OpenTerminal.def().name(), "terminal");
        assert!(!OpenTerminal.def().hint_visible());
    }
}
