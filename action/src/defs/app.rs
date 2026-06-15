use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    QuitDef,
    Quit,
    "quit",
    ActionKind::Quit,
    "close pane or exit",
    "Close the focused pane. Exit the application when closing the last remaining pane.",
    ActionPriority::Common,
    true,
    true,
    &["q"]
);

define_action!(
    QuitForceDef,
    QuitForce,
    "quit!",
    ActionKind::QuitForce,
    "close pane or exit, discarding unsaved changes",
    "Close the focused pane. Exit immediately when closing the last remaining pane, discarding any unsaved buffers without confirmation. See also quit, which confirms before discarding on the last pane.",
    ActionPriority::Common,
    true,
    true,
    &["q!"]
);

define_action!(
    WriteQuitDef,
    WriteQuit,
    "write-quit",
    ActionKind::WriteQuit,
    "write the focused buffer, then close pane or exit",
    "Write the focused buffer to disk, then close the focused pane. Exit when closing the last remaining pane, confirming first if other buffers have unsaved changes. A path-less scratch buffer clears its dirty flag without writing.",
    ActionPriority::Common,
    true,
    true,
    &["wq", "x"]
);

define_action!(
    ReloadConfigDef,
    ReloadConfig,
    "reload-config",
    ActionKind::ReloadConfig,
    "reload settings and theme from the user config",
    "Rebuild the settings and theme from the bundled default layered with the user config on disk. Discards session-only runtime overrides such as :set and tab-bar. A user config that fails to parse is rejected with a warning, leaving the current settings in place.",
    ActionPriority::Common,
    true,
    true,
    &["config-reload"]
);

define_action!(
    QuitAllDef,
    QuitAll,
    "quit-all",
    ActionKind::QuitAll,
    "exit stoat, closing all panes",
    "Exit the application immediately, closing every pane and viewport. See also quit, which closes the current pane and only exits when it is the last.",
    ActionPriority::Common,
    true,
    true,
    &["qa"]
);

define_action!(
    DismissModalDef,
    DismissModal,
    "DismissModal",
    ActionKind::DismissModal,
    "dismiss the active modal",
    "Close the topmost modal in the workspace's modal layer. Dispatched by the modal layer's backdrop click handler and bindable from the keymap (typically `Escape` while a modal is open).",
    ActionPriority::Common
);

define_action!(
    IncreaseFontSizeDef,
    IncreaseFontSize,
    "IncreaseFontSize",
    ActionKind::IncreaseFontSize,
    "increase the editor font size",
    "Scale the editor buffer font up one step for the current session. The override is session-only (not persisted) and affects only the editor buffer font, not the UI or terminal fonts.",
    ActionPriority::Common,
    false
);

define_action!(
    DecreaseFontSizeDef,
    DecreaseFontSize,
    "DecreaseFontSize",
    ActionKind::DecreaseFontSize,
    "decrease the editor font size",
    "Scale the editor buffer font down one step for the current session. The override is session-only (not persisted) and affects only the editor buffer font, not the UI or terminal fonts.",
    ActionPriority::Common,
    false
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn quit() {
        assert_eq!(Quit.kind(), ActionKind::Quit);
        assert_eq!(Quit.def().name(), "quit");
        assert!(Quit.def().params().is_empty());
        assert_eq!(Quit.def().short_desc(), "close pane or exit");
    }

    #[test]
    fn quit_force() {
        assert_eq!(QuitForce.kind(), ActionKind::QuitForce);
        assert_eq!(QuitForce.def().name(), "quit!");
        assert!(QuitForce.def().params().is_empty());
        assert_eq!(
            QuitForce.def().short_desc(),
            "close pane or exit, discarding unsaved changes"
        );
    }

    #[test]
    fn write_quit() {
        assert_eq!(WriteQuit.kind(), ActionKind::WriteQuit);
        assert_eq!(WriteQuit.def().name(), "write-quit");
        assert!(WriteQuit.def().params().is_empty());
        assert_eq!(
            WriteQuit.def().short_desc(),
            "write the focused buffer, then close pane or exit"
        );
    }

    #[test]
    fn reload_config() {
        assert_eq!(ReloadConfig.kind(), ActionKind::ReloadConfig);
        assert_eq!(ReloadConfig.def().name(), "reload-config");
        assert!(ReloadConfig.def().params().is_empty());
        assert_eq!(ReloadConfig.def().aliases(), &["config-reload"]);
    }

    #[test]
    fn quit_all() {
        assert_eq!(QuitAll.kind(), ActionKind::QuitAll);
        assert_eq!(QuitAll.def().name(), "quit-all");
        assert!(QuitAll.def().params().is_empty());
        assert_eq!(QuitAll.def().short_desc(), "exit stoat, closing all panes");
    }

    #[test]
    fn dismiss_modal() {
        assert_eq!(DismissModal.kind(), ActionKind::DismissModal);
        assert_eq!(DismissModal.def().name(), "DismissModal");
        assert!(DismissModal.def().params().is_empty());
    }

    #[test]
    fn increase_font_size() {
        assert_eq!(IncreaseFontSize.kind(), ActionKind::IncreaseFontSize);
        assert_eq!(IncreaseFontSize.def().name(), "IncreaseFontSize");
        assert!(IncreaseFontSize.def().params().is_empty());
        assert!(!IncreaseFontSize.def().hint_visible());
    }

    #[test]
    fn decrease_font_size() {
        assert_eq!(DecreaseFontSize.kind(), ActionKind::DecreaseFontSize);
        assert_eq!(DecreaseFontSize.def().name(), "DecreaseFontSize");
        assert!(DecreaseFontSize.def().params().is_empty());
        assert!(!DecreaseFontSize.def().hint_visible());
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(Quit);
        assert!(action.as_any().downcast_ref::<Quit>().is_some());
    }
}
