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
    RunSubmitJobDef,
    RunSubmitJob,
    "RunSubmitJob",
    ActionKind::RunSubmitJob,
    "submit as a Job",
    "Run the line at the Run pane prompt as Enter does, and send the block that \
     it starts to the Job Tray, so the pane keeps a Stub of it. A line that \
     starts no process runs as on Enter.",
    ActionPriority::Normal,
    palette_visible = false
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
    RunJobToggleDef,
    RunJobToggle,
    "RunJobToggle",
    ActionKind::RunJobToggle,
    "move a command to or from Jobs",
    "Move the newest running block of the focused Run pane to the Job Tray, \
     where it runs on as a Job. With no running block, bring the newest Job of \
     the pane back to its block.",
    ActionPriority::Normal,
    palette_visible = false
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
    RunAgainDef,
    RunAgain,
    "RunAgain",
    ActionKind::RunAgain,
    "run the Cmd again",
    "Run a finished Cmd again in place: the Cmd of the palette's Run Block, \
     or the newest finished Cmd of the focused Run pane. The run starts with \
     an empty output in the directory the Cmd first ran in, and its status \
     names the run's count. A running Cmd stays as it is.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    RunBlockToPaneDef,
    RunBlockToPane,
    "RunBlockToPane",
    ActionKind::RunBlockToPane,
    "move the Cmd to the Run pane",
    "Move the Cmd of the palette's Run Block, running or finished, onto a Run \
     pane's stack and close the palette. The Run pane takes the focus, and \
     its prompt takes typing.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    RunLocationListDef,
    RunLocationList,
    "RunLocationList",
    ActionKind::RunLocationList,
    "open the matches as a location list",
    "Open the matches of the newest finished block of matches on the focused \
     Run pane as a location list. A location picked there opens beside the \
     Run pane, so its stack stays.",
    ActionPriority::Normal,
    palette_visible = false
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
    description: "Shell command to run in the Run pane.",
}];

define_action_def!(
    RunDef,
    "Run",
    ActionKind::Run,
    "run command",
    "Run a shell command in the Run pane. Its Cmd joins the stack of the Run \
     pane on show, or of a new Run pane that splits the focused pane downward, \
     and the focus stays where it is. A claude line starts in an agent tab \
     instead.",
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
