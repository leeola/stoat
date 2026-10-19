//! The Claude Code plugin that teaches an agent this editor's CLI.
//!
//! An agent beside the user drives the editor through `stoat api`, and a skill
//! is how it learns those verbs without reading their help first. The plugin
//! holds that skill as `stoat:api`, laid out the way `claude --plugin-dir`
//! loads a plugin for one session, so nothing is installed.
//!
//! The binary embeds the skill, so a plugin always teaches the CLI of the
//! binary that wrote it.

use crate::host::FsHost;
use serde_json::json;
use std::{
    io,
    path::{Path, PathBuf},
};
use stoat_log::data_dir;

/// The `stoat:api` skill as the plugin holds it.
pub const API_SKILL: &str = include_str!("../../skills/api/SKILL.md");

const PLUGIN_SUBDIR: &str = "claude-plugin";

/// Returns the directory the plugin lives in:
/// `<XDG_DATA_HOME>/stoat/claude-plugin/`. Does not create the directory.
pub fn plugin_dir() -> io::Result<PathBuf> {
    Ok(data_dir()?.join(PLUGIN_SUBDIR))
}

/// Writes the plugin's manifest and its skill into `dir`, creating the
/// directories they need.
///
/// Every call overwrites both files, so a newer binary refreshes a directory
/// that an older one wrote.
pub fn write(fs: &dyn FsHost, dir: &Path) -> io::Result<()> {
    let manifest_dir = dir.join(".claude-plugin");
    let skill_dir = dir.join("skills").join("api");
    fs.create_dir_all(&manifest_dir)?;
    fs.create_dir_all(&skill_dir)?;

    fs.write(&manifest_dir.join("plugin.json"), manifest().as_bytes())?;
    fs.write(&skill_dir.join("SKILL.md"), API_SKILL.as_bytes())
}

/// The plugin manifest, as pretty JSON that ends in a newline.
fn manifest() -> String {
    let manifest = json!({
        "name": "stoat",
        "description": "Drive a live stoat editor through the stoat api CLI",
        "version": env!("CARGO_PKG_VERSION"),
    });
    let mut text = serde_json::to_string_pretty(&manifest).expect("a JSON value serializes");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::{write, API_SKILL};
    use crate::host::{FakeFs, FsHost};
    use serde_json::{json, Value};
    use std::path::Path;

    #[test]
    fn write_lays_out_the_plugin() {
        let fs = FakeFs::new();
        let dir = Path::new("/data/claude-plugin");
        write(&fs, dir).expect("write the plugin");

        let read = |relative: &str| {
            let mut bytes = Vec::new();
            fs.read(&dir.join(relative), &mut bytes).expect("read back");
            bytes
        };
        let manifest: Value = serde_json::from_slice(&read(".claude-plugin/plugin.json"))
            .expect("the manifest parses");
        assert_eq!(
            (
                manifest,
                read("skills/api/SKILL.md") == API_SKILL.as_bytes()
            ),
            (
                json!({
                    "name": "stoat",
                    "description": "Drive a live stoat editor through the stoat api CLI",
                    "version": env!("CARGO_PKG_VERSION"),
                }),
                true,
            ),
            "the manifest, and whether the skill holds exactly API_SKILL",
        );
    }

    /// Claude Code names a plugin's skill by its frontmatter, so this name is
    /// what makes the command `/stoat:api`.
    #[test]
    fn the_skill_frontmatter_names_api() {
        assert!(API_SKILL.starts_with("---\nname: api\n"));
    }
}
