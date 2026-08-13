use crate::{
    action::{define_action, define_action_def},
    Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind, ValueSource,
};
use std::{any::Any, path::PathBuf};

const PARAMS: &[ParamDef] = &[ParamDef {
    name: "path",
    kind: ParamKind::String,
    value_source: ValueSource::Files,
    required: true,
    description: "Filesystem path of the file to open. Relative paths resolve against the current working directory.",
}];

define_action_def!(
    OpenFileDef,
    "OpenFile",
    ActionKind::OpenFile,
    "open a file",
    "Read a file from disk into a buffer and show it in the focused pane.",
    ActionPriority::Common,
    aliases = &["o", "edit"],
    command_name = "open",
    params = PARAMS
);

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
    value_source: ValueSource::Values(&["stoat", "stoatty"]),
    required: false,
    description: "stoat (default) or stoatty",
}];

define_action_def!(
    OpenConfigDef,
    "OpenConfig",
    ActionKind::OpenConfig,
    "open the user config file",
    "Open a user config in the focused pane. Omitted or `stoat` opens ~/.config/stoat/config.stcfg; `stoatty` opens the terminal's ~/.config/stoatty/config.toml. A config that does not yet exist is created from the matching built-in default.",
    ActionPriority::Common,
    command_name = "config",
    params = OPEN_CONFIG_PARAMS
);

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

define_action!(
    ToggleMinimapDef,
    ToggleMinimap,
    "ToggleMinimap",
    ActionKind::ToggleMinimap,
    "show or hide the minimap",
    "Toggle the right-edge minimap strip on editor panes under stoatty, overriding the editor.minimap setting for this session.",
    ActionPriority::Common,
    command_name = "minimap"
);

define_action!(
    ToggleWrapDef,
    ToggleWrap,
    "ToggleWrap",
    ActionKind::ToggleWrap,
    "toggle soft wrap in the focused editor",
    "Toggle soft wrap in the focused editor, overriding the editor.wrap setting until toggled back.",
    ActionPriority::Common,
    command_name = "wrap"
);

const OPEN_BUFFER_PARAMS: &[ParamDef] = &[ParamDef {
    name: "path",
    kind: ParamKind::String,
    value_source: ValueSource::Buffers,
    required: true,
    description: "Path of an already-open buffer to switch to.",
}];

define_action_def!(
    OpenBufferDef,
    "OpenBuffer",
    ActionKind::OpenBuffer,
    "switch to an open buffer",
    "Show an already-open buffer in the focused pane, preserving its unsaved edits.",
    ActionPriority::Common,
    aliases = &["b"],
    command_name = "buffer",
    params = OPEN_BUFFER_PARAMS
);

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
    value_source: ValueSource::Values(&["on", "off", "follow"]),
    required: true,
    description: "on, off, or follow",
}];

define_action_def!(
    AutoReloadDef,
    "AutoReload",
    ActionKind::AutoReload,
    "follow the focused buffer's file",
    "Set auto-reload for the focused buffer. \"on\" re-reads the file as new content is appended and follows the tail. \"follow\" re-reads and jumps the cursor to the first region each change touches, for watching in-place edits live. \"off\" stops it, as does running :auto-reload follow a second time. Follow is per-buffer and opt-in, so opening :diff never enables it. Only file-backed buffers can reload.",
    ActionPriority::Normal,
    params = AUTO_RELOAD_PARAMS
);

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
    value_source: ValueSource::Values(&["on", "off"]),
    required: true,
    description: "on or off",
}];

define_action_def!(
    AutoReloadConfigDef,
    "AutoReloadConfig",
    ActionKind::AutoReloadConfig,
    "toggle config auto-reload",
    "Flip the running value of the `config.auto_reload` setting, which decides whether saving a config file re-applies it right away instead of waiting for a restart. Covers saves of both stoat's own config and the terminal's. The change lasts until stoat exits, or until a later reload of stoat's config re-reads the value written in the file.",
    ActionPriority::Normal,
    params = AUTO_RELOAD_CONFIG_PARAMS
);

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

define_action!(
    ForceSaveBufferDef,
    ForceSaveBuffer,
    "ForceSaveBuffer",
    ActionKind::ForceSaveBuffer,
    "save the focused buffer, overwriting external changes",
    "Write the focused buffer to its backing file even when the file changed on disk since it was opened, overwriting the external edit. The unforced SaveBuffer refuses in that case. No-op for scratch buffers (no path).",
    ActionPriority::Common,
    aliases = &["w!", "write!"]
);

define_action!(
    ReloadBufferDef,
    ReloadBuffer,
    "ReloadBuffer",
    ActionKind::ReloadBuffer,
    "re-read the focused buffer from disk",
    "Re-read the focused buffer's backing file, replacing the buffer content. Refuses when the buffer has unsaved edits; use ForceReloadBuffer to discard them. A file missing on disk reports and leaves the buffer untouched. No-op for scratch buffers (no path).",
    ActionPriority::Common,
    command_name = "reload"
);

define_action!(
    ForceReloadBufferDef,
    ForceReloadBuffer,
    "ForceReloadBuffer",
    ActionKind::ForceReloadBuffer,
    "re-read the focused buffer from disk, discarding unsaved edits",
    "Re-read the focused buffer's backing file even when the buffer has unsaved edits, discarding them. The unforced ReloadBuffer refuses in that case. A file missing on disk reports and leaves the buffer untouched. No-op for scratch buffers (no path).",
    ActionPriority::Common,
    aliases = &["reload!"]
);

define_action!(
    ReloadAllDef,
    ReloadAll,
    "ReloadAll",
    ActionKind::ReloadAll,
    "re-read every open file-backed buffer from disk",
    "Re-read every open file-backed buffer from its backing file, replacing buffer content. A buffer with unsaved edits is skipped; use ForceReloadAll to discard them. A file missing on disk is reported and left untouched. Reports how many buffers reloaded, were skipped, or were missing.",
    ActionPriority::Common,
    command_name = "reload-all"
);

define_action!(
    FontSizeIncDef,
    FontSizeInc,
    "FontSizeInc",
    ActionKind::FontSizeInc,
    "grow the terminal's font size a step",
    "Ask the hosting stoatty to render one font size larger, re-fitting the cell grid around it. This is the explicit way to reach font zoom, because stoat claims the platform zoom combo to resize whatever the user is looking at instead. Reports that it needs stoatty under any other terminal, which has no way to be asked.",
    ActionPriority::Normal
);

define_action!(
    FontSizeDecDef,
    FontSizeDec,
    "FontSizeDec",
    ActionKind::FontSizeDec,
    "shrink the terminal's font size a step",
    "Ask the hosting stoatty to render one font size smaller, re-fitting the cell grid around it. The counterpart to FontSizeInc, and subject to the same stoatty requirement. The terminal holds a floor, so repeated steps stop at its smallest size rather than reaching zero.",
    ActionPriority::Normal
);

define_action!(
    ForceReloadAllDef,
    ForceReloadAll,
    "ForceReloadAll",
    ActionKind::ForceReloadAll,
    "re-read every open file-backed buffer, discarding unsaved edits",
    "Re-read every open file-backed buffer from its backing file even when a buffer has unsaved edits, discarding them. The unforced ReloadAll skips unsaved buffers instead. A file missing on disk is reported and left untouched.",
    ActionPriority::Common,
    aliases = &["reload-all!"]
);

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
