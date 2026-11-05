//! Code the fixture-gated stoatty scene tests share.
//!
//! Cargo builds every top-level file in `tests/` as a test target of its own,
//! so this file sits one directory down. The workspace holds no `mod.rs`
//! files, so each test file names this one with a `path` attribute.
//!
//! Each test file compiles into its own crate, and rustc reports a helper that
//! one of those crates never calls as dead code. Only the items that every
//! scene test file calls live here.

use image::{ImageFormat, RgbaImage};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use stoatty_bin::headless::Snapshot;
use tempfile::TempDir;

/// The wait for stoat's first frame. A cold start of the debug binary on a
/// loaded machine takes tens of seconds, the bound the stoat pty tests allow.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// The quiet that counts as a settled screen.
pub const SETTLE: Duration = Duration::from_millis(250);

/// A scratch home holding the `config`, `data`, and `state` directories that
/// [`scratch_env`] points the child's XDG variables at.
///
/// Its stoat config turns direnv off. The direnv message names the fixture's
/// temporary directory, a new path on every run, so a frame with the message
/// never matches its golden.
// The scratch home is this test's own disk state, set up before the child runs.
#[allow(clippy::disallowed_methods)]
pub fn scratch_home() -> TempDir {
    let home = tempfile::tempdir().expect("a scratch home");
    for dir in ["config", "config/stoat", "data", "state"] {
        std::fs::create_dir(home.path().join(dir)).expect("a scratch home directory");
    }
    std::fs::write(
        home.path().join("config/stoat/config.stcfg"),
        "on init {\n    direnv.load = false;\n}\n",
    )
    .expect("the scratch stoat config");
    home
}

/// The child's XDG config, data, and state variables, pointed at the
/// directories [`scratch_home`] makes under `home`.
pub fn scratch_env(home: &Path) -> Vec<(String, String)> {
    [
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
    .to_vec()
}

/// The stoat binary, resolved from this test executable's own directory.
///
/// `CARGO_BIN_EXE_*` names only this package's binaries, and stoat belongs to
/// `stoat_bin`. Walking up from the test binary follows whatever target
/// directory and profile the run uses.
pub fn stoat_binary() -> PathBuf {
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

/// Write `shot` to `path` as a PNG, whatever extension `path` has.
pub fn save(shot: &Snapshot, path: &Path) {
    RgbaImage::from_raw(shot.width, shot.height, shot.rgba.clone())
        .expect("the readback is width by height")
        .save_with_format(path, ImageFormat::Png)
        .expect("write the frame");
}
