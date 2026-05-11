use crate::{
    action::{define_action, impl_gpui_action},
    Action, ActionDef, ActionKind, ActionPriority, ActionTarget, ParamDef, ParamKind,
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
    ActionTarget::Root
);

define_action!(
    RunSubmitDef,
    RunSubmit,
    "RunSubmit",
    ActionKind::RunSubmit,
    "submit command",
    "Submit the current command line to the shell.",
    ActionPriority::Normal,
    ActionTarget::Modal
);

define_action!(
    RunInterruptDef,
    RunInterrupt,
    "RunInterrupt",
    ActionKind::RunInterrupt,
    "interrupt command",
    "Send SIGINT to the running shell command.",
    ActionPriority::Normal,
    ActionTarget::Modal
);

define_action!(
    RunHistoryPrevDef,
    RunHistoryPrev,
    "RunHistoryPrev",
    ActionKind::RunHistoryPrev,
    "previous command in history",
    "Replace the run input with the previous entry in command history.",
    ActionPriority::Normal,
    ActionTarget::Modal
);

define_action!(
    RunHistoryNextDef,
    RunHistoryNext,
    "RunHistoryNext",
    ActionKind::RunHistoryNext,
    "next command in history",
    "Replace the run input with the next entry in command history, or clear the input when past the end.",
    ActionPriority::Normal,
    ActionTarget::Modal
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

    fn target(&self) -> ActionTarget {
        ActionTarget::Root
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

impl_gpui_action!(Run, "Run");
