//! Shared launcher for the scenario examples.
//!
//! Each scenario is a thin `examples/<name>.rs` that calls [`run`] with its
//! emitter bin name; this module owns the build-and-locate boilerplate they
//! would otherwise duplicate.

use std::{env, ffi::OsStr, path::PathBuf, process::Command};

/// Build the `bin` emitter and open the stoatty window, sized to `size` cells
/// (`[cols, rows]`), running it as the shell.
///
/// `bin` names an emitter binary; the crate it is built from is resolved from
/// its name (see [`emitter_package`]). The window renders that program's output
/// end to end through the bytes to PTY to parse to grid to render path. `size`
/// is the scene's cell extent, so the window opens close to the content it shows.
pub fn run(bin: &str, size: [u16; 2]) {
    let emitter = build_emitter(bin);
    stoatty::app::run_with_shell(
        emitter.to_string_lossy().into_owned(),
        Vec::new(),
        Some(size),
    );
}

/// Build the `bin` emitter and return the path to the compiled binary.
///
/// Running an example builds the example but not the emitter, which lives in
/// another crate, so build it here and locate it in the same target profile
/// directory as this example.
fn build_emitter(bin: &str) -> PathBuf {
    let example = env::current_exe().expect("locate the running example");
    let profile_dir = example
        .ancestors()
        .nth(2)
        .expect("example lives under a target profile directory");

    let mut command = Command::new(env!("CARGO"));
    command.args(["build", "-p", emitter_package(bin), "--bin", bin]);
    if profile_dir.file_name() == Some(OsStr::new("release")) {
        command.arg("--release");
    }

    let status = command.status().expect("run cargo build for the emitter");
    assert!(status.success(), "building {bin} failed");

    profile_dir.join(bin)
}

/// The crate an emitter bin is built from. Component-using emitters have migrated
/// to `stoatty_widgets`; the rest remain in `stoatty_protocol`.
fn emitter_package(bin: &str) -> &'static str {
    match bin {
        "example_gutter_app"
        | "example_diagnostics_app"
        | "example_doc_tooltip_app"
        | "example_panes_app"
        | "example_scale_app"
        | "example_smooth_scroll_pages_app"
        | "example_split_scroll_app" => "stoatty_widgets",
        _ => "stoatty_protocol",
    }
}
