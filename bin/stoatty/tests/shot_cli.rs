//! Runs the `stoatty shot` binary and checks where its child starts.

use std::process::Command;

/// With no `-d`, the child starts in stoatty's own working directory, as the
/// flag's help says.
///
/// The child writes `pwd -P` to an absolute path, so a child started anywhere
/// else still writes its report into the scratch directory. The XDG
/// directories point there too, because stoatty writes its log under the
/// state directory.
// The report is a file in this test's own scratch directory.
#[allow(clippy::disallowed_methods)]
#[test]
fn shot_starts_its_child_in_stoatty_s_directory() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let report = dir.path().join("pwd");

    let output = Command::new(env!("CARGO_BIN_EXE_stoatty"))
        .current_dir(dir.path())
        .envs(["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"].map(|var| (var, dir.path())))
        .args(["shot", "--out", "shots", "-e", "sh", "-c"])
        .arg(format!(
            "pwd -P > '{}'; printf x; sleep 60",
            report.display()
        ))
        .output()
        .expect("run stoatty shot");

    let expected = std::fs::canonicalize(dir.path()).expect("the scratch directory's real path");
    assert_eq!(
        (
            output.status.success(),
            std::fs::read_to_string(&report).ok()
        ),
        (true, Some(format!("{}\n", expected.display()))),
        "the shot succeeds and its child starts in the scratch directory; stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}
