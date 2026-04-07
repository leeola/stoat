use crate::{Action, ActionDef, ActionKind, ParamDef, ParamKind};
use std::{any::Any, path::PathBuf};

const PARAMS: &[ParamDef] = &[ParamDef {
    name: "path",
    kind: ParamKind::String,
    required: true,
}];

#[derive(Debug)]
pub struct OpenFileDef;

impl ActionDef for OpenFileDef {
    fn name(&self) -> &'static str {
        "OpenFile"
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
}

#[derive(Debug)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_file_kind_and_name() {
        let action = OpenFile {
            path: PathBuf::from("/tmp/x.rs"),
        };
        assert_eq!(action.kind(), ActionKind::OpenFile);
        assert_eq!(action.def().name(), "OpenFile");
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
