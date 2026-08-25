use crate::{
    action::{define_action, define_action_def},
    Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind, ValueSource,
};
use std::any::Any;

define_action!(
    QuitDef,
    Quit,
    "Quit",
    ActionKind::Quit,
    "close pane or exit",
    "Close the focused pane. Exit the application when closing the last remaining pane.",
    ActionPriority::Common,
    aliases = &["q"]
);

define_action!(
    QuitAllDef,
    QuitAll,
    "QuitAll",
    ActionKind::QuitAll,
    "exit stoat, closing all panes",
    "Exit the application immediately, closing every pane and viewport. See also Quit, which closes the current pane and only exits when it is the last.",
    ActionPriority::Common,
    aliases = &["qa"]
);

define_action!(
    QuitAllConfirmDef,
    QuitAllConfirm,
    "QuitAllConfirm",
    ActionKind::QuitAllConfirm,
    "confirm quit",
    "Confirm the quit-all prompt and exit, discarding the unsaved buffers it warned about.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    QuitAllCancelDef,
    QuitAllCancel,
    "QuitAllCancel",
    ActionKind::QuitAllCancel,
    "cancel quit",
    "Dismiss the quit-all prompt without exiting.",
    ActionPriority::Common,
    palette_visible = false
);

define_action!(
    ShowVersionDef,
    ShowVersion,
    "ShowVersion",
    ActionKind::ShowVersion,
    "show the version",
    "Show stoat's version and build commit as a one-line badge, plus stoatty's version when running inside the stoatty terminal. The badge is dismissed on the next key press.",
    ActionPriority::Normal,
    command_name = "version"
);

define_action!(
    PinModeDef,
    PinMode,
    "PinMode",
    ActionKind::PinMode,
    "pin the current mode",
    "Hold the focused editor's current mode until Escape, or until another \
     binding that does nothing but switch modes. While the mode is pinned, a \
     binding that runs a real action keeps the mode instead of returning to \
     normal, so a chord such as goto or git repeats under one key. The mode \
     name itself does not change, so the statusline and every other visual \
     read exactly as they do unpinned.",
    ActionPriority::Normal,
    palette_visible = false
);

const OPEN_LOGS_PARAMS: &[ParamDef] = &[ParamDef {
    name: "target",
    kind: ParamKind::String,
    value_source: ValueSource::Values(&["stoat", "stoatty"]),
    required: false,
    description: "stoat (default) or stoatty",
}];

define_action_def!(
    OpenLogsDef,
    "OpenLogs",
    ActionKind::OpenLogs,
    "open the session log file",
    "Open a log file in the focused pane and follow it as new lines are written, with the cursor on the last line. Omitted or `stoat` opens this stoat session's log; `stoatty` opens the enclosing terminal's stoatty-<id>.log. Use `:auto-reload off` to stop following. Reports in the status line when there is no such log file.",
    ActionPriority::Normal,
    command_name = "logs",
    params = OPEN_LOGS_PARAMS
);

#[derive(Debug)]
pub struct OpenLogs {
    /// Whose log to open. [`None`] means this stoat session's own.
    pub target: Option<String>,
}

impl OpenLogs {
    pub const DEF: &OpenLogsDef = &OpenLogsDef;
}

impl Action for OpenLogs {
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
    use crate::Action;

    #[test]
    fn quit() {
        assert_eq!(Quit.kind(), ActionKind::Quit);
        assert_eq!(Quit.def().name(), "Quit");
        assert!(Quit.def().params().is_empty());
        assert_eq!(Quit.def().short_desc(), "close pane or exit");
    }

    #[test]
    fn quit_all() {
        assert_eq!(QuitAll.kind(), ActionKind::QuitAll);
        assert_eq!(QuitAll.def().name(), "QuitAll");
        assert!(QuitAll.def().params().is_empty());
        assert_eq!(QuitAll.def().short_desc(), "exit stoat, closing all panes");
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(Quit);
        assert!(action.as_any().downcast_ref::<Quit>().is_some());
    }
}
