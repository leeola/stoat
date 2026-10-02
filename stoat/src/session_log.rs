use crate::app::Stoat;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// The most characters a workspace name's slug holds.
///
/// A stem is at most 61 bytes and a character at most 4, so the file name stays
/// under the 255-byte limit.
const SLUG_MAX_CHARS: usize = 40;

/// The log file this session writes.
///
/// The session renames the file to carry the active workspace's name, and
/// `path` follows each rename.
pub(crate) struct SessionLog {
    /// The file as the binary opened it, which every later name derives from,
    /// so a second rename never stacks one slug on another.
    base: PathBuf,
    /// Where the file is, which is what `:logs` opens.
    pub(crate) path: PathBuf,
    /// The workspace name `path` was last derived from, empty until the first
    /// rename, so a frame with no name change costs one comparison.
    named_for: String,
}

impl SessionLog {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            base: path.clone(),
            path,
            named_for: String::new(),
        }
    }
}

/// Rename the session log after the active workspace's name.
///
/// A fresh workspace renames nothing, so a session that holds nothing keeps the
/// bare stem.
///
/// A failed rename is logged and not repeated until the name changes again.
pub(crate) fn sync(stoat: &mut Stoat) {
    let Some(mut log) = stoat.session_log.take() else {
        return;
    };
    let ws = stoat.active_workspace();
    if ws.display_name() == log.named_for || ws.is_fresh() {
        stoat.session_log = Some(log);
        return;
    }

    let name = ws.display_name().to_owned();
    let target = named_path(&log.base, &name);
    if target != log.path {
        match stoat.fs_host.rename(&log.path, &target) {
            Ok(()) => {
                // An open `:logs` buffer follows the file, so its auto-reload
                // poll keeps reading the log it shows.
                for ws in stoat.workspaces.values_mut() {
                    ws.buffers.rename_path(&log.path, &target);
                }
                tracing::info!(
                    target: "stoat::session_log",
                    session = %name,
                    path = %target.display(),
                    "session log renamed",
                );
                log.path = target;
            },
            Err(err) => {
                tracing::warn!(
                    target: "stoat::session_log",
                    %err,
                    from = %log.path.display(),
                    to = %target.display(),
                    "session log rename failed",
                );
            },
        }
    }
    log.named_for = name;
    stoat.session_log = Some(log);
}

/// The path the log based at `base` takes for the workspace `name`.
///
/// The slug follows the stem, so the file keeps the prefix it shares with the
/// files that sort next to it. A name with no file-safe character leaves the
/// base as it is.
fn named_path(base: &Path, name: &str) -> PathBuf {
    let slug = name_slug(name);
    match base.file_stem().and_then(OsStr::to_str) {
        Some(stem) if !slug.is_empty() => base.with_file_name(format!("{stem}-{slug}.log")),
        _ => base.to_path_buf(),
    }
}

/// The file-safe form of a workspace name.
///
/// A letter, a digit, or `_` stays. Each run of any other character becomes
/// one `-`, and the slug has no `-` at either end. It holds at most
/// [`SLUG_MAX_CHARS`] characters.
fn name_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut chars = 0;
    for c in name.chars() {
        if chars == SLUG_MAX_CHARS {
            break;
        }
        if c.is_alphanumeric() || c == '_' {
            slug.push(c);
            chars += 1;
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
            chars += 1;
        }
    }
    if slug.ends_with('-') {
        slug.pop();
    }
    slug
}

#[cfg(test)]
mod tests {
    use super::{name_slug, named_path, sync};
    use crate::{
        auto_reload,
        buffer::BufferId,
        host::{FakeFsOp, FsHost},
        test_harness::TestHarness,
        Stoat,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn name_slug_keeps_letters_and_joins_the_rest() {
        assert_eq!(
            [
                "brave otter",
                "fix/login bug!",
                "  ..  ",
                "über café",
                "my_ws",
                "--a--"
            ]
            .map(name_slug),
            [
                "brave-otter",
                "fix-login-bug",
                "",
                "über-café",
                "my_ws",
                "a"
            ],
        );
        assert_eq!(name_slug(&"a".repeat(50)), "a".repeat(40));
        assert_eq!(
            name_slug(&format!("{} b", "a".repeat(39))),
            "a".repeat(39),
            "a cut that ends on a dash drops it",
        );
    }

    #[test]
    fn named_path_appends_the_slug_to_the_stem() {
        let base = Path::new("/logs/headless-stoat-1.log");
        assert_eq!(
            [named_path(base, "brave otter"), named_path(base, "!!")],
            [
                PathBuf::from("/logs/headless-stoat-1-brave-otter.log"),
                base.to_path_buf()
            ],
        );
    }

    #[test]
    fn sync_renames_the_log_as_the_workspace_name_changes() {
        let mut h = Stoat::test();
        let base = PathBuf::from("/logs/headless-stoat-1.log");
        h.fake_fs().insert_file(&base, b"line\n");
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/logs");
        h.stoat.set_session_log(base.clone());
        h.stoat.active_workspace_mut().name = "brave otter".to_string();
        sync(&mut h.stoat);
        assert_eq!(
            log_names(&h),
            ["headless-stoat-1.log"],
            "a fresh workspace renames nothing"
        );

        auto_reload::open_log_buffer(&mut h.stoat, &base);
        let id = h.stoat.focused_editor_ids().expect("the log is open").1;
        sync(&mut h.stoat);
        let renamed = Path::new("/logs/headless-stoat-1-brave-otter.log");
        assert_eq!(
            log_state(&h, renamed),
            (
                vec!["headless-stoat-1-brave-otter.log".to_string()],
                Some(renamed.to_path_buf()),
                Some(id)
            ),
        );

        h.stoat.active_workspace_mut().name = "fix/login bug!".to_string();
        sync(&mut h.stoat);
        let renamed = Path::new("/logs/headless-stoat-1-fix-login-bug.log");
        assert_eq!(
            log_state(&h, renamed),
            (
                vec!["headless-stoat-1-fix-login-bug.log".to_string()],
                Some(renamed.to_path_buf()),
                Some(id)
            ),
            "the second name derives from the base, not from the first name",
        );
    }

    #[test]
    fn sync_keeps_the_path_when_the_rename_fails() {
        let mut h = Stoat::test();
        h.fake_fs().insert_file("/logs/notes.txt", b"notes\n");
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/logs");
        h.open_file(Path::new("/logs/notes.txt"));
        let base = PathBuf::from("/logs/headless-stoat-1.log");
        h.stoat.set_session_log(base.clone());
        h.stoat.active_workspace_mut().name = "brave otter".to_string();

        sync(&mut h.stoat);
        sync(&mut h.stoat);

        let renames = h
            .fake_fs()
            .ops()
            .iter()
            .filter(|op| matches!(op, FakeFsOp::Rename { .. }))
            .count();
        assert_eq!(
            (
                h.stoat.session_log.as_ref().map(|log| log.path.clone()),
                renames
            ),
            (Some(base), 1),
            "the failed rename keeps the path and is not tried again",
        );
    }

    /// The names of the files in `/logs`.
    fn log_names(h: &TestHarness) -> Vec<String> {
        h.fake_fs()
            .list_dir(Path::new("/logs"))
            .expect("the log directory exists")
            .into_iter()
            .map(|entry| entry.name.to_string())
            .collect()
    }

    /// The log directory's file names, the session log's path, and the buffer
    /// open on `path`.
    fn log_state(h: &TestHarness, path: &Path) -> (Vec<String>, Option<PathBuf>, Option<BufferId>) {
        (
            log_names(h),
            h.stoat.session_log.as_ref().map(|log| log.path.clone()),
            h.stoat.active_workspace().buffers.id_for_path(path),
        )
    }
}
