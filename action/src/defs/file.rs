use crate::{
    action::define_action, Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind,
};
use serde::Deserialize;
use std::{any::Any, path::PathBuf};

const PARAMS: &[ParamDef] = &[ParamDef {
    name: "path",
    kind: ParamKind::String,
    required: true,
    description: "Filesystem path of the file to open. Relative paths resolve against the current working directory.",
}];

#[derive(Debug)]
pub struct OpenFileDef;

impl ActionDef for OpenFileDef {
    fn name(&self) -> &'static str {
        "open"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["o", "edit", "e"]
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenFile
    }

    fn params(&self) -> &'static [ParamDef] {
        PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "open a file"
    }

    fn long_desc(&self) -> &'static str {
        "Read a file from disk into a buffer and show it in the focused pane."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OpenFile {
    pub path: PathBuf,
}

impl OpenFile {
    pub const DEF: &OpenFileDef = &OpenFileDef;
}

impl Action for OpenFile {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

define_action!(
    OpenConfigDef,
    OpenConfig,
    "open-config",
    ActionKind::OpenConfig,
    "open the user config file",
    "Open the user config at $XDG_CONFIG_HOME/stoat/config.stcfg in the focused pane, creating its parent directory if absent so a first save succeeds. A not-yet-existing config opens as an empty buffer.",
    ActionPriority::Common,
    true,
    true,
    &["config-open"]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_config_kind_and_name() {
        assert_eq!(OpenConfig.kind(), ActionKind::OpenConfig);
        assert_eq!(OpenConfig.def().name(), "open-config");
        assert!(OpenConfig.def().params().is_empty());
        assert_eq!(OpenConfig.def().aliases(), &["config-open"]);
    }

    #[test]
    fn open_file_kind_and_name() {
        let action = OpenFile {
            path: PathBuf::from("/tmp/x.rs"),
        };
        assert_eq!(action.kind(), ActionKind::OpenFile);
        assert_eq!(action.def().name(), "open");
        assert_eq!(action.def().params().len(), 1);
        assert_eq!(action.def().params()[0].name, "path");
    }

    #[test]
    fn downcast_preserves_path() {
        let boxed: Box<dyn Action> = Box::new(OpenFile {
            path: PathBuf::from("/a/b.rs"),
        });
        let recovered = boxed.as_any().downcast_ref::<OpenFile>().expect("downcast");
        assert_eq!(recovered.path, PathBuf::from("/a/b.rs"));
    }
}
