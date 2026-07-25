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
    value_source: ValueSource::Values(&["stoat", "stoatty"]),
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
    value_source: ValueSource::Values(&["on", "off", "follow"]),
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
    value_source: ValueSource::Values(&["on", "off"]),
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

#[derive(Debug)]
pub struct ReloadBufferDef;

impl ActionDef for ReloadBufferDef {
    fn name(&self) -> &'static str {
        "ReloadBuffer"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("reload")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ReloadBuffer
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "re-read the focused buffer from disk"
    }

    fn long_desc(&self) -> &'static str {
        "Re-read the focused buffer's backing file, replacing the buffer content. Refuses when the buffer has unsaved edits; use ForceReloadBuffer to discard them. A file missing on disk reports and leaves the buffer untouched. No-op for scratch buffers (no path)."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct ReloadBuffer;

impl ReloadBuffer {
    pub const DEF: &ReloadBufferDef = &ReloadBufferDef;
}

impl Action for ReloadBuffer {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct ForceReloadBufferDef;

impl ActionDef for ForceReloadBufferDef {
    fn name(&self) -> &'static str {
        "ForceReloadBuffer"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ForceReloadBuffer
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "re-read the focused buffer from disk, discarding unsaved edits"
    }

    fn long_desc(&self) -> &'static str {
        "Re-read the focused buffer's backing file even when the buffer has unsaved edits, discarding them. The unforced ReloadBuffer refuses in that case. A file missing on disk reports and leaves the buffer untouched. No-op for scratch buffers (no path)."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["reload!"]
    }
}

#[derive(Debug)]
pub struct ForceReloadBuffer;

impl ForceReloadBuffer {
    pub const DEF: &ForceReloadBufferDef = &ForceReloadBufferDef;
}

impl Action for ForceReloadBuffer {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct ReloadAllDef;

impl ActionDef for ReloadAllDef {
    fn name(&self) -> &'static str {
        "ReloadAll"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("reload-all")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ReloadAll
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "re-read every open file-backed buffer from disk"
    }

    fn long_desc(&self) -> &'static str {
        "Re-read every open file-backed buffer from its backing file, replacing buffer content. A buffer with unsaved edits is skipped; use ForceReloadAll to discard them. A file missing on disk is reported and left untouched. Reports how many buffers reloaded, were skipped, or were missing."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct ReloadAll;

impl ReloadAll {
    pub const DEF: &ReloadAllDef = &ReloadAllDef;
}

impl Action for ReloadAll {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct FontSizeIncDef;

impl ActionDef for FontSizeIncDef {
    fn name(&self) -> &'static str {
        "FontSizeInc"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::FontSizeInc
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "grow the terminal's font size a step"
    }

    fn long_desc(&self) -> &'static str {
        "Ask the hosting stoatty to render one font size larger, re-fitting the cell grid around it. This is the explicit way to reach font zoom, because stoat claims the platform zoom combo to resize whatever the user is looking at instead. Reports that it needs stoatty under any other terminal, which has no way to be asked."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Normal
    }
}

#[derive(Debug)]
pub struct FontSizeInc;

impl FontSizeInc {
    pub const DEF: &FontSizeIncDef = &FontSizeIncDef;
}

impl Action for FontSizeInc {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct FontSizeDecDef;

impl ActionDef for FontSizeDecDef {
    fn name(&self) -> &'static str {
        "FontSizeDec"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::FontSizeDec
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "shrink the terminal's font size a step"
    }

    fn long_desc(&self) -> &'static str {
        "Ask the hosting stoatty to render one font size smaller, re-fitting the cell grid around it. The counterpart to FontSizeInc, and subject to the same stoatty requirement. The terminal holds a floor, so repeated steps stop at its smallest size rather than reaching zero."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Normal
    }
}

#[derive(Debug)]
pub struct FontSizeDec;

impl FontSizeDec {
    pub const DEF: &FontSizeDecDef = &FontSizeDecDef;
}

impl Action for FontSizeDec {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct ForceReloadAllDef;

impl ActionDef for ForceReloadAllDef {
    fn name(&self) -> &'static str {
        "ForceReloadAll"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ForceReloadAll
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn short_desc(&self) -> &'static str {
        "re-read every open file-backed buffer, discarding unsaved edits"
    }

    fn long_desc(&self) -> &'static str {
        "Re-read every open file-backed buffer from its backing file even when a buffer has unsaved edits, discarding them. The unforced ReloadAll skips unsaved buffers instead. A file missing on disk is reported and left untouched."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["reload-all!"]
    }
}

#[derive(Debug)]
pub struct ForceReloadAll;

impl ForceReloadAll {
    pub const DEF: &ForceReloadAllDef = &ForceReloadAllDef;
}

impl Action for ForceReloadAll {
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
