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
    "Turn the focused pane into a run prompt. Each command typed at the prompt \
     runs in a shell of its own in the workspace root, and its output stays in \
     the pane. See also Terminal, which opens a bare subshell pane instead.",
    ActionPriority::Common
);

define_action!(
    RunSubmitDef,
    RunSubmit,
    "RunSubmit",
    ActionKind::RunSubmit,
    "submit command",
    "Run the current command line in a shell of its own. A cd line changes the \
     workspace root instead, since no shell state carries to the next line."
);

define_action!(
    RunInterruptDef,
    RunInterrupt,
    "RunInterrupt",
    ActionKind::RunInterrupt,
    "interrupt command",
    "Send SIGINT to the newest running command."
);

define_action!(
    RunToggleViewDef,
    RunToggleView,
    "RunToggleView",
    ActionKind::RunToggleView,
    "show the raw output or the report",
    "Flip the newest block of the focused Run pane between the report of its \
     finished test run and the raw output that the report was read from.",
    ActionPriority::Normal,
    palette_visible = false
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
    aliases = &["r"],
    command_name = "run",
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
