//! Shared launcher for the scenario examples.
//!
//! Each scenario is a thin `examples/<name>.rs` that calls [`run`] with its
//! emitter name. This module owns the build-and-locate boilerplate every
//! scenario otherwise repeats.

use std::{env, ffi::OsStr, path::PathBuf, process::Command};

/// Every emitter a scenario runs, paired with the crate whose `examples/`
/// directory holds it.
///
/// The table is exhaustive on purpose. A default arm sends every unlisted name
/// to one of the two crates, so a pure-VT demo nobody added here builds from
/// the widget crate instead. The failure then surfaces as cargo's own "no
/// example target" message, which names neither this file nor the absent entry.
const EMITTERS: &[(&str, &str)] = &[
    ("example_diagnostics_app", "stoat_widgets"),
    ("example_doc_tooltip_app", "stoat_widgets"),
    ("example_gutter_app", "stoat_widgets"),
    ("example_panel_app", "stoat_widgets"),
    ("example_panes_app", "stoat_widgets"),
    ("example_scale_app", "stoat_widgets"),
    ("example_sketch_app", "stoat_widgets"),
    ("example_smooth_scroll_pages_app", "stoat_widgets"),
    ("example_split_scroll_app", "stoat_widgets"),
    ("example_edit_app", "stoatty_protocol"),
    ("example_hello_app", "stoatty_protocol"),
    ("example_image_app", "stoatty_protocol"),
    ("example_smooth_scroll_history_app", "stoatty_protocol"),
];

/// Build the `emitter` program and open the stoatty window, sized to `size`
/// cells (`[cols, rows]`), running it as the shell.
///
/// `emitter` names an entry in [`EMITTERS`]. The window renders that program's
/// output end to end through the bytes to PTY to parse to grid to render path.
/// `size` is the scene's cell extent, so the window opens close to the content
/// it shows.
pub fn run(emitter: &str, size: [u16; 2]) {
    let program = build_emitter(emitter);
    stoatty_bin::app::run_with_shell(
        program.to_string_lossy().into_owned(),
        Vec::new(),
        Some(size),
    );
}

/// Build the `emitter` program and return the path to the compiled binary.
///
/// Running an example builds the example but not the emitter, which lives in
/// another crate, so build it here and locate it in the same target profile
/// directory as this example.
///
/// The compiled program is handed to the PTY directly rather than run through
/// `cargo run`. Cargo writes its progress lines to the inherited stderr, the
/// PTY captures them, and the renderer paints them as cells before the emitter
/// writes its first byte, which corrupts a scene drawn at exact positions.
fn build_emitter(emitter: &str) -> PathBuf {
    let example = env::current_exe().expect("locate the running example");
    let profile_dir = example
        .ancestors()
        .nth(2)
        .expect("example lives under a target profile directory");

    let mut command = Command::new(env!("CARGO"));
    command.args([
        "build",
        "-p",
        emitter_package(emitter),
        "--example",
        emitter,
    ]);
    if profile_dir.file_name() == Some(OsStr::new("release")) {
        command.arg("--release");
    }

    let status = command.status().expect("run cargo build for the emitter");
    assert!(status.success(), "building {emitter} failed");

    profile_dir.join("examples").join(emitter)
}

/// The crate `emitter` is built from, per [`EMITTERS`].
///
/// Panics when the name is absent. The scenario has nothing to run without an
/// emitter, and a guess at the crate reports the wrong problem.
fn emitter_package(emitter: &str) -> &'static str {
    EMITTERS
        .iter()
        .find(|(name, _)| *name == emitter)
        .map(|(_, package)| *package)
        .unwrap_or_else(|| {
            panic!("no emitter named {emitter}; add it to EMITTERS in examples/common/launcher.rs")
        })
}
