use crate::{action::define_action, ActionKind, ActionPriority};

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
    OpenLogsDef,
    OpenLogs,
    "OpenLogs",
    ActionKind::OpenLogs,
    "open the session log file",
    "Open this session's log file in the focused pane and follow it as new lines are written, with the cursor on the last line. Use `:auto-reload off` to stop following. Reports in the status line when the session has no log file.",
    ActionPriority::Normal,
    command_name = "logs"
);

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
