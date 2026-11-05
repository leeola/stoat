//! Renders a stoat fixture headlessly and checks what its frames show.
//!
//! The scene runs the `stoat` binary beside this test executable, built with
//! stoat's own fixture feature, over a scratch home on the software adapter.

use image::RgbaImage;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};
use stoatty_bin::{
    config,
    headless::{script, HeadlessSession, Scene, Snapshot},
};
use tempfile::TempDir;

/// The wait for stoat's first frame. A cold start of the debug binary on a
/// loaded machine takes tens of seconds, the bound the stoat pty tests allow.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// The wait for the frame that follows a few keys.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// The quiet that counts as a settled screen.
const SETTLE: Duration = Duration::from_millis(250);

/// Two cursor moves in the basic-diff fixture change the frame, and the first
/// frame draws text over the background.
///
/// Both frames are written into the scratch home, which a failing check keeps
/// and names, so the frames are there to open.
#[test]
fn the_basic_diff_frame_changes_when_the_cursor_moves() {
    let home = scratch_home();
    let mut session =
        HeadlessSession::spawn(basic_diff_scene(home.path())).unwrap_or_else(|error| {
            panic!(
                "{error}. With no wgpu adapter, enter the devshell with `nix develop`, which \
                 registers lavapipe"
            )
        });

    session
        .wait_settled(SETTLE, STARTUP_TIMEOUT)
        .expect("stoat draws its first frame; a stoat built without its fixture feature exits");
    let before = session.snapshot().expect("a snapshot");
    session
        .type_keys(&script::parse("jj").expect("a key script"))
        .expect("the keys type");
    session
        .wait_settled(SETTLE, STEP_TIMEOUT)
        .expect("the frame settles after the keys");
    let after = session.snapshot().expect("a snapshot");
    // The child goes before its scratch home does.
    drop(session);

    save(&before, &home.path().join("before.png"));
    save(&after, &home.path().join("after.png"));
    let changed = before.rgba != after.rgba;
    let colors = distinct_colors(&before);
    let frames = match changed && colors > 2 {
        true => home.path().to_owned(),
        false => home.keep(),
    };
    assert!(
        changed,
        "two cursor moves change the frame; the frames are in {}",
        frames.display(),
    );
    assert!(
        colors > 2,
        "the first frame draws text over the background, not {colors} colors; the frames are \
         in {}",
        frames.display(),
    );
}

/// The basic-diff fixture with its unstaged file open, at 100 by 30 cells on the
/// software adapter, with config, data, and state under `home`.
///
/// The file is named because a fixture opens no file of its own, and `jj` moves
/// nothing in the one-line scratch buffer stoat opens then.
fn basic_diff_scene(home: &Path) -> Scene {
    let config = config::embedded_default();
    Scene {
        program: stoat_binary().to_string_lossy().into_owned(),
        args: ["--fixture", "basic-diff", "unstaged.txt"]
            .map(String::from)
            .to_vec(),
        working_directory: Some(home.to_owned()),
        stoat_dir: None,
        cols: 100,
        rows: 30,
        font_size: 14,
        terminal_font_size: 12,
        scale_factor: 1.0,
        theme: config.resolve_theme(),
        theme_name: "one-dark".to_owned(),
        font_family: config.font_family,
        ligatures: config.ligatures,
        cursor_animation: config.cursor_animation,
        software: true,
        extra_env: [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
        ]
        .map(|(var, dir)| {
            (
                var.to_owned(),
                home.join(dir).to_string_lossy().into_owned(),
            )
        })
        .to_vec(),
    }
}

/// A scratch home holding the `config`, `data`, and `state` directories the
/// scene points the child's XDG variables at.
fn scratch_home() -> TempDir {
    let home = tempfile::tempdir().expect("a scratch home");
    for dir in ["config", "data", "state"] {
        std::fs::create_dir(home.path().join(dir)).expect("a scratch home directory");
    }
    home
}

/// The stoat binary, resolved from this test executable's own directory.
///
/// `CARGO_BIN_EXE_*` names only this package's binaries, and stoat belongs to
/// `stoat_bin`. Walking up from the test binary follows whatever target
/// directory and profile the run uses.
fn stoat_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("this test's own path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let binary = path.join("stoat");

    assert!(
        binary.is_file(),
        "no stoat binary at {}. Testing this package does not build it, so run \
         `cargo build -p stoat_bin --features fixture` first, as scripts/check-fixtures.sh \
         does.",
        binary.display(),
    );
    binary
}

/// Write `shot` to `path` as a PNG.
fn save(shot: &Snapshot, path: &Path) {
    RgbaImage::from_raw(shot.width, shot.height, shot.rgba.clone())
        .expect("the readback is width by height")
        .save(path)
        .expect("write the frame");
}

/// How many distinct RGB colors `shot`'s pixels hold.
fn distinct_colors(shot: &Snapshot) -> usize {
    shot.rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect::<HashSet<_>>()
        .len()
}
