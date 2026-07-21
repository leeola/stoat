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

    fn command_name(&self) -> Option<&'static str> {
        Some("open")
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
        &["o", "edit"]
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

const OPEN_CONFIG_PARAMS: &[ParamDef] = &[ParamDef {
    name: "target",
    kind: ParamKind::String,
    value_source: ValueSource::None,
    required: false,
    description: "stoat (default) or stoatty",
}];

#[derive(Debug)]
pub struct OpenConfigDef;

impl ActionDef for OpenConfigDef {
    fn name(&self) -> &'static str {
        "OpenConfig"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("config")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenConfig
    }

    fn params(&self) -> &'static [ParamDef] {
        OPEN_CONFIG_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "open the user config file"
    }

    fn long_desc(&self) -> &'static str {
        "Open a user config in the focused pane. Omitted or `stoat` opens ~/.config/stoat/config.stcfg; `stoatty` opens the terminal's ~/.config/stoatty/config.toml. A config that does not yet exist is created from the matching built-in default."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct OpenConfig {
    /// Which program's config to open. [`None`] means stoat's own.
    pub target: Option<String>,
}

impl OpenConfig {
    pub const DEF: &OpenConfigDef = &OpenConfigDef;
}

impl Action for OpenConfig {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct ToggleMinimapDef;

impl ActionDef for ToggleMinimapDef {
    fn name(&self) -> &'static str {
        "ToggleMinimap"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("minimap")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ToggleMinimap
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "show or hide the minimap"
    }

    fn long_desc(&self) -> &'static str {
        "Toggle the right-edge minimap strip on editor panes under stoatty, overriding the editor.minimap setting for this session."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct ToggleMinimap;

impl ToggleMinimap {
    pub const DEF: &ToggleMinimapDef = &ToggleMinimapDef;
}

impl Action for ToggleMinimap {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct ToggleWrapDef;

impl ActionDef for ToggleWrapDef {
    fn name(&self) -> &'static str {
        "ToggleWrap"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("wrap")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ToggleWrap
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "toggle soft wrap in the focused editor"
    }

    fn long_desc(&self) -> &'static str {
        "Toggle soft wrap in the focused editor, overriding the editor.wrap setting until toggled back."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct ToggleWrap;

impl ToggleWrap {
    pub const DEF: &ToggleWrapDef = &ToggleWrapDef;
}

impl Action for ToggleWrap {
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

    fn command_name(&self) -> Option<&'static str> {
        Some("buffer")
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
        &["b"]
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

const AUTO_RELOAD_PARAMS: &[ParamDef] = &[ParamDef {
    name: "state",
    kind: ParamKind::String,
    value_source: ValueSource::None,
    required: true,
    description: "on, off, or follow",
}];

#[derive(Debug)]
pub struct AutoReloadDef;

impl ActionDef for AutoReloadDef {
    fn name(&self) -> &'static str {
        "AutoReload"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::AutoReload
    }

    fn params(&self) -> &'static [ParamDef] {
        AUTO_RELOAD_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "follow the focused buffer's file"
    }

    fn long_desc(&self) -> &'static str {
        "Set auto-reload for the focused buffer. \"on\" re-reads the file as new content is appended and follows the tail. \"follow\" re-reads and jumps the cursor to the first region each change touches, for watching in-place edits live. \"off\" stops it, as does running :auto-reload follow a second time. Follow is per-buffer and opt-in, so opening :diff never enables it. Only file-backed buffers can reload."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Normal
    }
}

#[derive(Debug)]
pub struct AutoReload {
    pub state: String,
}

impl AutoReload {
    pub const DEF: &AutoReloadDef = &AutoReloadDef;
}

impl Action for AutoReload {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

const AUTO_RELOAD_CONFIG_PARAMS: &[ParamDef] = &[ParamDef {
    name: "state",
    kind: ParamKind::String,
    value_source: ValueSource::None,
    required: true,
    description: "on or off",
}];

#[derive(Debug)]
pub struct AutoReloadConfigDef;

impl ActionDef for AutoReloadConfigDef {
    fn name(&self) -> &'static str {
        "AutoReloadConfig"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::AutoReloadConfig
    }

    fn params(&self) -> &'static [ParamDef] {
        AUTO_RELOAD_CONFIG_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "toggle config auto-reload"
    }

    fn long_desc(&self) -> &'static str {
        "Flip the running value of the `config.auto_reload` setting, which decides whether saving a config file re-applies it right away instead of waiting for a restart. Covers saves of both stoat's own config and the terminal's. The change lasts until stoat exits, or until a later reload of stoat's config re-reads the value written in the file."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Normal
    }
}

#[derive(Debug)]
pub struct AutoReloadConfig {
    pub state: String,
}

impl AutoReloadConfig {
    pub const DEF: &AutoReloadConfigDef = &AutoReloadConfigDef;
}

impl Action for AutoReloadConfig {
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
