use crate::{Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind, ValueSource};
use std::{any::Any, path::PathBuf};

const PARAMS: &[ParamDef] = &[ParamDef {
    name: "path",
    kind: ParamKind::String,
    value_source: ValueSource::Files,
    required: true,
    description: "Filesystem path of the file to open. Relative paths resolve against the current working directory.",
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

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["o", "open", "edit"]
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

const OPEN_BUFFER_PARAMS: &[ParamDef] = &[ParamDef {
    name: "path",
    kind: ParamKind::String,
    value_source: ValueSource::Buffers,
    required: true,
    description: "Path of an already-open buffer to switch to.",
}];

#[derive(Debug)]
pub struct OpenBufferDef;

impl ActionDef for OpenBufferDef {
    fn name(&self) -> &'static str {
        "OpenBuffer"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenBuffer
    }

    fn params(&self) -> &'static [ParamDef] {
        OPEN_BUFFER_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "switch to an open buffer"
    }

    fn long_desc(&self) -> &'static str {
        "Show an already-open buffer in the focused pane, preserving its unsaved edits."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["b", "buffer"]
    }
}

#[derive(Debug)]
pub struct OpenBuffer {
    pub path: PathBuf,
}

impl OpenBuffer {
    pub const DEF: &OpenBufferDef = &OpenBufferDef;
}

impl Action for OpenBuffer {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct ForceSaveBufferDef;

impl ActionDef for ForceSaveBufferDef {
    fn name(&self) -> &'static str {
        "ForceSaveBuffer"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ForceSaveBuffer
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "save the focused buffer, overwriting external changes"
    }

    fn long_desc(&self) -> &'static str {
        "Write the focused buffer to its backing file even when the file changed on disk since it was opened, overwriting the external edit. The unforced SaveBuffer refuses in that case. No-op for scratch buffers (no path)."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["w!", "write!"]
    }
}

#[derive(Debug)]
pub struct ForceSaveBuffer;

impl ForceSaveBuffer {
    pub const DEF: &ForceSaveBufferDef = &ForceSaveBufferDef;
}

impl Action for ForceSaveBuffer {
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
        assert_eq!(action.def().params()[0].value_source, ValueSource::Files);
    }

    #[test]
    fn downcast_preserves_path() {
        let boxed: Box<dyn Action> = Box::new(OpenFile {
            path: PathBuf::from("/a/b.rs"),
        });
        let recovered = boxed.as_any().downcast_ref::<OpenFile>().expect("downcast");
        assert_eq!(recovered.path, PathBuf::from("/a/b.rs"));
    }

    #[test]
    fn open_buffer_kind_and_name() {
        let action = OpenBuffer {
            path: PathBuf::from("/tmp/x.rs"),
        };
        assert_eq!(action.kind(), ActionKind::OpenBuffer);
        assert_eq!(action.def().name(), "OpenBuffer");
        assert_eq!(action.def().params().len(), 1);
        assert_eq!(action.def().params()[0].name, "path");
        assert_eq!(action.def().params()[0].value_source, ValueSource::Buffers);
    }

    #[test]
    fn open_buffer_downcast_preserves_path() {
        let boxed: Box<dyn Action> = Box::new(OpenBuffer {
            path: PathBuf::from("/a/b.rs"),
        });
        let recovered = boxed
            .as_any()
            .downcast_ref::<OpenBuffer>()
            .expect("downcast");
        assert_eq!(recovered.path, PathBuf::from("/a/b.rs"));
    }
}
