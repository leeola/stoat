//! The build that made a binary, stamped into that binary.
//!
//! A log from a running binary must tell which code the binary runs. Only the
//! build script knows that, so the script stamps the commit, the commit's
//! datetime, and the build datetime into the crate as compile-time variables.

use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const UNKNOWN: &str = "unknown";

/// Files at the workspace root that crates embed or that every build reads.
///
/// No manifest names them, so the stamp of each binary tracks all of them.
const WORKSPACE_INPUTS: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "config.stcfg",
    "stoatignore",
    "stoatty.toml",
    "themes",
    "assets",
    "vendor",
];

/// Stamp the package whose build script calls this with its commit and build
/// time, as compile-time variables under `prefix`.
///
/// The package reads four variables with `env!`:
///
/// - `<prefix>_BUILD_COMMIT` is the full sha of the commit the package builds from, with `-dirty`
///   when the tree holds uncommitted changes.
/// - `<prefix>_BUILD_COMMIT_TIME` is when that commit was made, as `YYYY-MM-DDTHH:MM:SSZ` in UTC.
/// - `<prefix>_BUILD_TIME` is when the build script ran, in the same form.
/// - `<prefix>_BUILD_INFO` is `<sha>[-dirty] <build date>` with the sha cut to 8 characters, the
///   short form a version string shows.
///
/// A value that no source supplies reads `unknown`. `STOAT_COMMIT` and
/// `STOAT_COMMIT_TIME` in the build environment take precedence over git, so a
/// build from a source tree with no repository still names its commit.
///
/// The build script runs again and takes a new stamp when a crate that the
/// package depends on by path changes, when an input at the workspace root
/// such as `Cargo.lock` changes, or when HEAD moves or git writes the index.
pub fn emit(prefix: &str) {
    if let Some(manifest_dir) = env::var_os("CARGO_MANIFEST_DIR") {
        for path in tracked_paths(Path::new(&manifest_dir)) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-env-changed=STOAT_COMMIT");
    println!("cargo:rerun-if-env-changed=STOAT_COMMIT_TIME");

    let stamp = Stamp {
        commit: commit(),
        commit_time: commit_time(),
        build_time: build_time(),
    };
    for directive in stamp.directives(prefix) {
        println!("{directive}");
    }
}

/// The text of each variable [`emit`] sets.
struct Stamp {
    commit: String,
    commit_time: String,
    build_time: String,
}

impl Stamp {
    /// The cargo directives that set the four variables under `prefix`.
    fn directives(&self, prefix: &str) -> [String; 4] {
        let Self {
            commit,
            commit_time,
            build_time,
        } = self;
        let build_date = build_time.get(..10).unwrap_or(build_time);
        [
            format!("cargo:rustc-env={prefix}_BUILD_COMMIT={commit}"),
            format!("cargo:rustc-env={prefix}_BUILD_COMMIT_TIME={commit_time}"),
            format!("cargo:rustc-env={prefix}_BUILD_TIME={build_time}"),
            format!(
                "cargo:rustc-env={prefix}_BUILD_INFO={} {build_date}",
                short_commit(commit)
            ),
        ]
    }
}

/// The form of `commit` that a version string shows.
///
/// The sha keeps its first 8 characters and its `-dirty` mark. The cut happens
/// here and not in git, so a commit from `STOAT_COMMIT` gets the same form.
/// `unknown` stays as it is.
fn short_commit(commit: &str) -> String {
    let (sha, dirty) = match commit.split_once('-') {
        Some((sha, _)) => (sha, "-dirty"),
        None => (commit, ""),
    };
    let short = sha.get(..8).unwrap_or(sha);
    format!("{short}{dirty}")
}

/// Every path whose change takes the stamp of the package at `package_dir`
/// again.
///
/// The list holds only paths that exist. Cargo reruns a build script on every
/// build when a tracked path does not exist.
fn tracked_paths(package_dir: &Path) -> Vec<PathBuf> {
    let root_inputs = workspace_root(package_dir)
        .into_iter()
        .flat_map(|root| WORKSPACE_INPUTS.iter().map(move |input| root.join(input)));

    // `--git-path` answers for a worktree too, where `.git` is a file. Every
    // move of HEAD appends to `logs/HEAD`, a message-only amend included.
    let git_paths = ["HEAD", "index", "logs/HEAD"]
        .into_iter()
        .filter_map(|name| capture("git", &["rev-parse", "--git-path", name], &[]))
        .map(|path| package_dir.join(path));

    path_dependency_dirs(package_dir)
        .into_iter()
        .chain(root_inputs)
        .chain(git_paths)
        .filter(|path| path.exists())
        .collect()
}

/// The directories of the crate at `package_dir` and of every crate it depends
/// on by path, at any depth.
///
/// Dev-dependencies count too, which tracks more than the binary needs and
/// never less.
fn path_dependency_dirs(package_dir: &Path) -> BTreeSet<PathBuf> {
    let mut dirs = BTreeSet::new();
    let mut pending = vec![package_dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let dir = fs::canonicalize(&dir).unwrap_or(dir);
        if dirs.contains(&dir) {
            continue;
        }

        if let Ok(manifest) = fs::read_to_string(dir.join("Cargo.toml")) {
            pending.extend(
                manifest
                    .lines()
                    .flat_map(path_values)
                    .map(|path| dir.join(path))
                    // A `path` that names a file, such as the source of a
                    // `[[bin]]`, holds no manifest and so names no crate.
                    .filter(|path| path.join("Cargo.toml").is_file()),
            );
        }
        dirs.insert(dir);
    }
    dirs
}

/// The value of each `path = "..."` key on one manifest line.
fn path_values(line: &str) -> impl Iterator<Item = &str> {
    line.split("path").skip(1).filter_map(|after| {
        let quoted = after.trim_start().strip_prefix('=')?.trim_start();
        let (value, _) = quoted.strip_prefix('"')?.split_once('"')?;
        Some(value)
    })
}

/// The root of the workspace that holds the package at `package_dir`.
fn workspace_root(package_dir: &Path) -> Option<PathBuf> {
    package_dir
        .ancestors()
        .find(|dir| {
            fs::read_to_string(dir.join("Cargo.toml"))
                .is_ok_and(|manifest| manifest.lines().any(|line| line.trim() == "[workspace]"))
        })
        .map(Path::to_path_buf)
}

/// The full sha of the commit the package builds from, with `-dirty` when the
/// tree holds uncommitted changes.
fn commit() -> String {
    if let Some(commit) = env_override("STOAT_COMMIT") {
        return commit;
    }
    let Some(sha) = capture("git", &["rev-parse", "HEAD"], &[]) else {
        return UNKNOWN.to_owned();
    };

    // A plain status writes the refreshed index back under `index.lock`, and an
    // editor git job that runs at that time fails on the lock.
    let dirty = capture(
        "git",
        &["--no-optional-locks", "status", "--porcelain"],
        &[],
    )
    .is_some();
    if dirty {
        format!("{sha}-dirty")
    } else {
        sha
    }
}

/// When the commit the package builds from was made, as
/// `YYYY-MM-DDTHH:MM:SSZ` in UTC.
fn commit_time() -> String {
    env_override("STOAT_COMMIT_TIME")
        .or_else(|| {
            // `format-local` renders in the zone `TZ` names.
            capture(
                "git",
                &[
                    "log",
                    "-1",
                    "--date=format-local:%Y-%m-%dT%H:%M:%SZ",
                    "--format=%cd",
                ],
                &[("TZ", "UTC")],
            )
        })
        .unwrap_or_else(|| UNKNOWN.to_owned())
}

/// When the build script ran, as `YYYY-MM-DDTHH:MM:SSZ` in UTC.
///
/// `SOURCE_DATE_EPOCH` does not apply. Nix sets it to a constant, and this time
/// exists to tell one build from another.
fn build_time() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |_| UNKNOWN.to_owned(),
        |since| utc_timestamp(since.as_secs()),
    )
}

/// The UTC datetime `secs` after the Unix epoch, as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc_timestamp(secs: u64) -> String {
    let (days, secs_of_day) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        secs_of_day % 3600 / 60,
        secs_of_day % 60,
    );

    // The count starts at 0000-03-01, so a leap day is the last day of its year.
    // A 400-year era holds 146097 days, and the calendar repeats each era.
    let days = days + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The value of the environment variable `name`, when it is set and not empty.
fn env_override(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

/// The trimmed output of `cmd`, when it ran, exited with success, and printed
/// something.
fn capture(cmd: &str, args: &[&str], envs: &[(&str, &str)]) -> Option<String> {
    let output = Command::new(cmd)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::{path_dependency_dirs, short_commit, utc_timestamp, workspace_root, Stamp};
    use std::{
        collections::BTreeSet,
        fs,
        path::{Path, PathBuf},
    };

    const CLEAN: &str = "6e14badb0d2316841d3029706dd2c0d882617912";
    const DIRTY: &str = "6e14badb0d2316841d3029706dd2c0d882617912-dirty";

    #[test]
    fn the_stamp_names_its_four_variables() {
        let stamp = Stamp {
            commit: DIRTY.to_owned(),
            commit_time: "2026-10-01T16:26:44Z".to_owned(),
            build_time: "2026-10-01T17:02:10Z".to_owned(),
        };
        assert_eq!(
            stamp.directives("STOAT"),
            [
                "cargo:rustc-env=STOAT_BUILD_COMMIT=6e14badb0d2316841d3029706dd2c0d882617912-dirty",
                "cargo:rustc-env=STOAT_BUILD_COMMIT_TIME=2026-10-01T16:26:44Z",
                "cargo:rustc-env=STOAT_BUILD_TIME=2026-10-01T17:02:10Z",
                "cargo:rustc-env=STOAT_BUILD_INFO=6e14badb-dirty 2026-10-01",
            ],
        );
    }

    #[test]
    fn the_short_commit_keeps_the_dirty_mark() {
        assert_eq!(
            [DIRTY, CLEAN, "unknown"].map(short_commit),
            ["6e14badb-dirty", "6e14badb", "unknown"],
        );
    }

    #[test]
    fn a_timestamp_reads_as_utc_to_the_second() {
        assert_eq!(
            [0, 1_709_208_000, 1_790_872_004].map(utc_timestamp),
            [
                "1970-01-01T00:00:00Z",
                "2024-02-29T12:00:00Z",
                "2026-10-01T16:26:44Z"
            ],
        );
    }

    #[test]
    fn the_path_closure_follows_dependencies_to_any_depth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = workspace(dir.path());
        let canonical = |name: &str| ws.join(name).canonicalize().expect("canonical path");
        assert_eq!(
            path_dependency_dirs(&ws.join("app")),
            BTreeSet::from(["app", "lib_a", "lib_b"].map(canonical)),
        );
    }

    #[test]
    fn the_workspace_root_is_the_manifest_that_declares_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = workspace(dir.path());
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            [
                workspace_root(&ws.join("app")),
                workspace_root(empty.path())
            ],
            [Some(ws), None],
        );
    }

    /// A workspace under `root` whose `app` reaches `lib_b` through `lib_a`,
    /// beside an `other` crate that nothing names.
    fn workspace(root: &Path) -> PathBuf {
        let ws = root.join("ws");
        write(
            &ws,
            "Cargo.toml",
            r#"
            [workspace]
            members = ["app", "lib_a", "lib_b", "other"]
            "#,
        );
        write(
            &ws,
            "app/Cargo.toml",
            r#"
            [[bin]]
            name = "app"
            path = "src/main.rs"
            [dependencies]
            lib_a = { path = "../lib_a" }
            [dev-dependencies]
            app = { path = "." }
            "#,
        );
        write(&ws, "app/src/main.rs", "fn main() {}\n");
        write(
            &ws,
            "lib_a/Cargo.toml",
            r#"
            [dependencies.lib_b]
            path = "../lib_b"
            "#,
        );
        write(&ws, "lib_b/Cargo.toml", "[package]\n");
        write(&ws, "other/Cargo.toml", "[package]\n");
        ws
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().expect("a parent directory")).expect("create directory");
        fs::write(path, text).expect("write file");
    }
}
