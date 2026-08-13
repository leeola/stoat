use crate::{
    action::{define_action, define_action_def},
    Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind, ValueSource,
};
use std::any::Any;

define_action!(
    OpenRunDef,
    OpenRun,
    "OpenRun",
    ActionKind::OpenRun,
    "open the run-command prompt",
    "Turn the focused pane into a run prompt backed by a shell. Commands typed \
     at the prompt run in the workspace root and their output stays in the \
     pane. See also Terminal, which opens a bare subshell pane instead.",
    ActionPriority::Common
);

define_action!(
    RunSubmitDef,
    RunSubmit,
    "RunSubmit",
    ActionKind::RunSubmit,
    "submit command",
    "Submit the current command line to the shell."
);

define_action!(
    RunInterruptDef,
    RunInterrupt,
    "RunInterrupt",
    ActionKind::RunInterrupt,
    "interrupt command",
    "Send SIGINT to the running shell command."
);

define_action!(
    RunModalDismissDef,
    RunModalDismiss,
    "RunModalDismiss",
    ActionKind::RunModalDismiss,
    "dismiss finished run",
    "Remove a finished modal run and close its overlay."
);

define_action!(
    RunHistoryPrevDef,
    RunHistoryPrev,
    "RunHistoryPrev",
    ActionKind::RunHistoryPrev,
    "previous command in history",
    "Replace the run input with the previous entry in command history."
);

define_action!(
    RunHistoryNextDef,
    RunHistoryNext,
    "RunHistoryNext",
    ActionKind::RunHistoryNext,
    "next command in history",
    "Replace the run input with the next entry in command history, or clear the input when past the end."
);

const RUN_PARAMS: &[ParamDef] = &[ParamDef {
    name: "command",
    kind: ParamKind::String,
    value_source: ValueSource::None,
    required: true,
    description: "Shell command to execute in a modal overlay.",
}];

define_action_def!(
    RunDef,
    "Run",
    ActionKind::Run,
    "run command",
    "Run a shell command in a temporary modal overlay. The modal shows output while running and can be dismissed when done.",
    ActionPriority::Common,
    params = RUN_PARAMS
);

#[derive(Debug)]
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
