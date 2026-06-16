//! `cargo run --example sink` opens the stoatty window running the
//! `example_sink_app` emitter as its shell, rendering its baseline TUI end to
//! end through the bytes to PTY to parse to grid to render path.

use std::{env, ffi::OsStr, path::PathBuf, process::Command};

fn main() {
    let emitter = build_emitter();
    stoatty::app::run_with_shell(emitter.to_string_lossy().into_owned());
}

/// Build `example_sink_app` and return the path to the compiled binary.
///
/// Running this example builds the example but not the emitter, which lives in
/// another crate, so build it here and locate it in the same target profile
/// directory as this example.
fn build_emitter() -> PathBuf {
    let example = env::current_exe().expect("locate the running example");
    let profile_dir = example
        .ancestors()
        .nth(2)
        .expect("example lives under a target profile directory");

    let mut command = Command::new(env!("CARGO"));
    command.args([
        "build",
        "-p",
        "stoatty_protocol",
        "--bin",
        "example_sink_app",
    ]);
    if profile_dir.file_name() == Some(OsStr::new("release")) {
        command.arg("--release");
    }

    let status = command.status().expect("run cargo build for the emitter");
    assert!(status.success(), "building example_sink_app failed");

    profile_dir.join("example_sink_app")
}
