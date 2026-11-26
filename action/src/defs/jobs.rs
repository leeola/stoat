use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    JobGlanceDef,
    JobGlance,
    "JobGlance",
    ActionKind::JobGlance,
    "glance at the next job",
    "Open the Glance of a Job under its Tray Chip: its line, its state and \
     time, its newest output line, and the keys that act on it. The first \
     press opens the newest Job, and each later press opens the Job before \
     the last one glanced at, wrapping to the newest. With no Job the status \
     says so.",
    ActionPriority::Normal
);

define_action!(
    JobGlanceNextDef,
    JobGlanceNext,
    "JobGlanceNext",
    ActionKind::JobGlanceNext,
    "glance at the newer job",
    "Move the open Glance to the Job after it in the Job Tray, wrapping from \
     the newest Job to the oldest.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    JobGlancePrevDef,
    JobGlancePrev,
    "JobGlancePrev",
    ActionKind::JobGlancePrev,
    "glance at the older job",
    "Move the open Glance to the Job before it in the Job Tray, wrapping from \
     the oldest Job to the newest.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    JobGlanceCloseDef,
    JobGlanceClose,
    "JobGlanceClose",
    ActionKind::JobGlanceClose,
    "close the glance",
    "Close the Glance of a Job and leave the Job in the Job Tray.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    JobOpenDef,
    JobOpen,
    "JobOpen",
    ActionKind::JobOpen,
    "open the job",
    "Take the Job of the open Glance out of the Job Tray and show it as a \
     block of a Run pane, which takes the focus. A Run pane that shows its \
     Stub takes it back in place, and else a Run pane takes it at the end of \
     its stack.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    JobRerunDef,
    JobRerun,
    "JobRerun",
    ActionKind::JobRerun,
    "run the job again",
    "Run the ended Job of the open Glance again in the Job Tray, or the ended \
     Cmd of the selected row of the Job List again in its place. A Cmd that \
     still runs stays as it is, and the status says so.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    JobInterruptDef,
    JobInterrupt,
    "JobInterrupt",
    ActionKind::JobInterrupt,
    "stop the job",
    "Send SIGINT to the running Job of the open Glance, or to the running Cmd \
     of the selected row of the Job List, and keep it open. On an ended Cmd \
     the key closes the Glance or the list.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    JobDismissDef,
    JobDismiss,
    "JobDismiss",
    ActionKind::JobDismiss,
    "dismiss the job",
    "Take the ended Job of the open Glance, or of the selected row of the Job \
     List, out of the Job Tray. A Run pane that shows its Stub keeps it as an \
     ended block, and else the Job goes. A Job that still runs stays, and the \
     status says so. A row of a Run pane stays too, and the status says it is \
     not in the tray.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    JobsDef,
    Jobs,
    "Jobs",
    ActionKind::Jobs,
    "list the jobs",
    "Open the Job List, one row for each Job of the Job Tray and for each Cmd \
     of the workspace's Run panes, newest first. A row reads its state, its \
     time, and where it stands, and the filter narrows the rows by their \
     line. Enter opens the selected row, and the keys of the list run it \
     again, stop it, or dismiss it.",
    ActionPriority::Common,
    command_name = "jobs"
);

define_action!(
    JobListScopeToggleDef,
    JobListScopeToggle,
    "JobListScopeToggle",
    ActionKind::JobListScopeToggle,
    "toggle the job list scope",
    "Flip the Job List between every row and the rows whose Cmd still runs.",
    ActionPriority::Normal,
    palette_visible = false
);

define_action!(
    JobRerunLastDef,
    JobRerunLast,
    "JobRerunLast",
    ActionKind::JobRerunLast,
    "run the last command again",
    "Run the last Cmd of the workspace again in its place, a Job in the Job \
     Tray or a block in its Run pane. The last Cmd is the one that started \
     last among the Jobs and the blocks of the Run panes. A Cmd that still \
     runs stays as it is, and the status says so.",
    ActionPriority::Normal
);
