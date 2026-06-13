//! Capture a self-contained snapshot of the current workspace + repo state
//! for later replay. Used to preserve the exact state of an in-progress
//! rebase or other transient workspace configuration so bugs can be
//! reproduced in isolation.
//!
//! The save path writes a single bundle file to
//! `<XDG_DATA_HOME>/stoat/dumps/<id>.dump` containing the working tree
//! (respecting `.gitignore`, always including `.git/` and `.stoat/`) plus
//! the dump metadata in a framed binary format. The load path
//! (`stoat dump open <id>`) extracts the bundle to a fresh tempdir so
//! the original is never mutated.

mod bundle;
pub mod meta;
mod save;
pub(crate) mod snapshot;
mod walker;

/// Top-level workspace directories that bypass `.gitignore` checks
/// when capturing a dump. These hold repo + editor metadata that the
/// load path needs even when a user has gitignored them.
const FORCE_INCLUDE_DIRS: &[&str] = &[".git", ".stoat"];

use crate::host::FsHost;
pub use meta::DumpMeta;
use snafu::{ResultExt, Snafu};
use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};
use stoat_log::data_dir;
use stoat_scheduler::Executor;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

const TIMESTAMP_FORMAT: &[FormatItem<'_>] =
    format_description!("[year]-[month]-[day]_[hour]-[minute]-[second]");

const TIMESTAMP_LEN: usize = 19;

const MAX_NAME_LEN: usize = 64;

const DUMPS_SUBDIR: &str = "dumps";

const ARCHIVE_SUFFIX: &str = ".dump";

/// Stable identifier for a dump. Format: `<YYYY-MM-DD_HH-MM-SS>_<sanitized-name>`.
/// Sortable as a plain string in chronological order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DumpId(String);

impl DumpId {
    /// Build a fresh id from a user-supplied name and a UTC timestamp.
    pub fn new(name: &str, at: OffsetDateTime) -> Result<Self, DumpError> {
        let sanitized = sanitize_name(name)?;
        let at_utc = at.to_offset(UtcOffset::UTC);
        let timestamp = at_utc.format(&TIMESTAMP_FORMAT).map_err(|e| {
            TimeSnafu {
                reason: e.to_string(),
            }
            .build()
        })?;
        Ok(Self(format!("{timestamp}_{sanitized}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn filename(&self) -> String {
        format!("{}{ARCHIVE_SUFFIX}", self.0)
    }

    /// Parse an id out of an archive filename (accepts the full
    /// `<id>.dump` name).
    pub fn from_filename(filename: &str) -> Option<Self> {
        filename
            .strip_suffix(ARCHIVE_SUFFIX)
            .map(|s| Self(s.to_string()))
    }

    /// Parse the embedded UTC timestamp. Returns `None` when the id was
    /// not produced by [`DumpId::new`] (e.g. hand-written id).
    pub fn created_at(&self) -> Option<OffsetDateTime> {
        let ts = self.0.get(..TIMESTAMP_LEN)?;
        let parsed = time::PrimitiveDateTime::parse(ts, &TIMESTAMP_FORMAT).ok()?;
        Some(parsed.assume_utc())
    }

    /// Returns the sanitized name portion, without the timestamp prefix.
    pub fn name(&self) -> Option<&str> {
        self.0.get(TIMESTAMP_LEN + 1..)
    }
}

impl std::fmt::Display for DumpId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One entry in the dumps directory. [`Self::meta`] is `None` if the
/// tarball exists but metadata could not be read (corrupt dump, older
/// schema); the id and file size are still reported so the user can
/// remove it.
#[derive(Debug)]
pub struct DumpEntry {
    pub id: DumpId,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum DumpError {
    #[snafu(display("dump name is empty after sanitization"))]
    EmptyName {
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("dump metadata serialization failed: {reason}"))]
    Ron {
        reason: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("timestamp handling failed: {reason}"))]
    Time {
        reason: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("no dump matches '{query}'"))]
    NotFound {
        query: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("'{query}' matches multiple dumps: {}", display_matches(matches)))]
    Ambiguous {
        query: String,
        matches: Vec<DumpId>,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("dump bundle format: {reason}"))]
    BundleFormat {
        reason: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("workspace walk failed: {reason}"))]
    Walk {
        reason: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to resolve XDG data directory"))]
    ResolveDataDir {
        source: io::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to create directory: {}", path.display()))]
    CreateDir {
        source: io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to read dump file: {}", path.display()))]
    ReadDump {
        source: io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to write dump file: {}", path.display()))]
    WriteDump {
        source: io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to remove dump file: {}", path.display()))]
    RemoveDump {
        source: io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to list directory: {}", path.display()))]
    ListDumpsDir {
        source: io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to read metadata: {}", path.display()))]
    MetadataDump {
        source: io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

fn display_matches(matches: &[DumpId]) -> String {
    matches
        .iter()
        .map(|m| m.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Sanitize a user-supplied name into a path-friendly slug.
///
/// Applies: lowercase, whitespace runs collapsed to `-`, drop chars
/// outside `[a-z0-9_-]`, collapse consecutive `-`, trim leading/trailing
/// `-`, truncate at [`MAX_NAME_LEN`] chars. Empty result is rejected.
pub fn sanitize_name(raw: &str) -> Result<String, DumpError> {
    let filtered: String = raw
        .chars()
        .filter_map(|c| {
            if c.is_whitespace() {
                Some('-')
            } else {
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_alphanumeric() || lower == '_' || lower == '-' {
                    Some(lower)
                } else {
                    None
                }
            }
        })
        .collect();

    let mut collapsed = String::with_capacity(filtered.len());
    let mut prev_dash = true;
    for c in filtered.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push(c);
                prev_dash = true;
            }
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    while collapsed.ends_with('-') {
        collapsed.pop();
    }

    if collapsed.len() > MAX_NAME_LEN {
        collapsed.truncate(MAX_NAME_LEN);
        while collapsed.ends_with('-') {
            collapsed.pop();
        }
    }

    if collapsed.is_empty() {
        return EmptyNameSnafu.fail();
    }
    Ok(collapsed)
}

/// Returns the directory where dump archives are stored:
/// `<XDG_DATA_HOME>/stoat/dumps/`. Does not create the directory.
pub fn dumps_dir() -> Result<PathBuf, DumpError> {
    Ok(data_dir().context(ResolveDataDirSnafu)?.join(DUMPS_SUBDIR))
}

/// List all dump archives, newest first. Ignores non-archive files in
/// the dumps directory. If the dumps directory does not exist yet,
/// returns an empty list.
pub fn list(fs: &dyn FsHost) -> Result<Vec<DumpEntry>, DumpError> {
    let dir = dumps_dir()?;
    if !fs.exists(&dir) {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    let listing = fs
        .list_dir(&dir)
        .with_context(|_| ListDumpsDirSnafu { path: dir.clone() })?;
    for entry in listing {
        if entry.is_dir {
            continue;
        }
        let Some(id) = DumpId::from_filename(entry.name.as_str()) else {
            continue;
        };
        let path = dir.join(entry.name.as_str());
        let meta = fs
            .metadata(&path)
            .with_context(|_| MetadataDumpSnafu { path: path.clone() })?;
        let (size_bytes, modified) = match meta {
            Some(m) => (m.len, Some(m.modified)),
            None => (0, None),
        };
        entries.push(DumpEntry {
            id,
            path,
            size_bytes,
            modified,
        });
    }

    entries.sort_by(|a, b| b.id.as_str().cmp(a.id.as_str()));
    Ok(entries)
}

/// Resolve a user-supplied query to a single dump. Accepts either the
/// full id or the sanitized name suffix. Newest wins when the name
/// suffix matches multiple dumps; errors when the query matches
/// nothing or multiple exact ids.
pub fn resolve(query: &str, fs: &dyn FsHost) -> Result<DumpEntry, DumpError> {
    let mut all = list(fs)?;
    all.retain(|e| e.id.as_str() == query || e.id.name().map(|n| n == query).unwrap_or(false));
    if all.is_empty() {
        return NotFoundSnafu {
            query: query.to_string(),
        }
        .fail();
    }
    Ok(all.remove(0))
}

/// Delete the dump archive identified by `id`.
pub fn remove(id: &DumpId, fs: &dyn FsHost) -> Result<(), DumpError> {
    let path = dumps_dir()?.join(id.filename());
    if !fs.exists(&path) {
        return NotFoundSnafu {
            query: id.as_str().to_string(),
        }
        .fail();
    }
    fs.remove_file(&path)
        .with_context(|_| RemoveDumpSnafu { path: path.clone() })?;
    Ok(())
}

/// Delete every dump older than `days` whole days. Returns the ids of
/// the archives that were removed.
pub fn clean_older_than(
    days: u64,
    fs: &dyn FsHost,
    executor: &Executor,
) -> Result<Vec<DumpId>, DumpError> {
    let now = executor.system_now();
    let cutoff = now
        .checked_sub(std::time::Duration::from_secs(days * 86_400))
        .ok_or_else(|| {
            TimeSnafu {
                reason: "overflow computing cutoff".to_string(),
            }
            .build()
        })?;
    let entries = list(fs)?;
    let mut removed = Vec::new();
    for entry in entries {
        let Some(modified) = entry.modified else {
            continue;
        };
        if modified < cutoff {
            fs.remove_file(&entry.path)
                .with_context(|_| RemoveDumpSnafu {
                    path: entry.path.clone(),
                })?;
            removed.push(entry.id);
        }
    }
    Ok(removed)
}

/// Extract a dump bundle into `dest`. `dest` must already exist.
/// Reads the bundle through `FsHost::read` and re-materialises every
/// captured file through `FsHost::write`, plus a `.stoat/dump.ron` file
/// rebuilt from the bundle header.
pub fn extract(id: &DumpId, dest: &Path, fs: &dyn FsHost) -> Result<(), DumpError> {
    let archive = dumps_dir()?.join(id.filename());
    if !fs.exists(&archive) {
        return NotFoundSnafu {
            query: id.as_str().to_string(),
        }
        .fail();
    }
    read_archive(&archive, dest, fs)
}

/// Low-level reader: extract the bundle at exactly `archive_path` into
/// `dest`. Callers that already know the archive path (tests, internal
/// replay tooling) bypass [`dumps_dir`] via this entry point.
pub(crate) fn read_archive(
    archive_path: &Path,
    dest: &Path,
    fs: &dyn FsHost,
) -> Result<(), DumpError> {
    let mut buf = Vec::new();
    fs.read(archive_path, &mut buf)
        .with_context(|_| ReadDumpSnafu {
            path: archive_path.to_path_buf(),
        })?;
    let (meta_ron, entries) = bundle::deserialize(&buf)?;

    for (rel, content) in &entries {
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            fs.create_dir_all(parent).with_context(|_| CreateDirSnafu {
                path: parent.to_path_buf(),
            })?;
        }
        fs.write(&target, content)
            .with_context(|_| WriteDumpSnafu {
                path: target.clone(),
            })?;
    }

    let meta_target = dest.join(meta::META_PATH_IN_ARCHIVE);
    if let Some(parent) = meta_target.parent() {
        fs.create_dir_all(parent).with_context(|_| CreateDirSnafu {
            path: parent.to_path_buf(),
        })?;
    }
    fs.write(&meta_target, meta_ron.as_bytes())
        .with_context(|_| WriteDumpSnafu {
            path: meta_target.clone(),
        })?;
    Ok(())
}

/// Write a dump bundle for a workspace directory with no in-memory
/// editor state to snapshot, such as the GUI. Captures the working tree
/// under `git_root` (force-including `.git`/`.stoat`) plus minimal
/// metadata -- the input `mode` at capture time and no rebase plan -- to
/// `<XDG_DATA_HOME>/stoat/dumps/<id>.dump`. Returns the generated
/// [`DumpId`].
pub fn save_workspace_dir(
    git_root: &Path,
    mode: &str,
    name: &str,
    at: OffsetDateTime,
    fs: &dyn FsHost,
) -> Result<DumpId, DumpError> {
    let id = DumpId::new(name, at)?;
    let dumps = dumps_dir()?;
    fs.create_dir_all(&dumps).with_context(|_| CreateDirSnafu {
        path: dumps.clone(),
    })?;
    let archive_path = dumps.join(id.filename());
    save::write_workspace_dir_archive(git_root, mode, &id, at, &archive_path, fs)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_basic() {
        assert_eq!(sanitize_name("My Bug #47").unwrap(), "my-bug-47");
    }

    #[test]
    fn sanitize_lowercases() {
        assert_eq!(sanitize_name("HELLO").unwrap(), "hello");
    }

    #[test]
    fn sanitize_collapses_whitespace() {
        assert_eq!(sanitize_name("a   b").unwrap(), "a-b");
    }

    #[test]
    fn sanitize_collapses_dashes() {
        assert_eq!(sanitize_name("a---b").unwrap(), "a-b");
    }

    #[test]
    fn sanitize_trims_boundary_dashes() {
        assert_eq!(sanitize_name("  hello  ").unwrap(), "hello");
        assert_eq!(sanitize_name("---x---").unwrap(), "x");
    }

    #[test]
    fn sanitize_keeps_underscores() {
        assert_eq!(sanitize_name("a_b_c").unwrap(), "a_b_c");
    }

    #[test]
    fn sanitize_drops_punctuation_and_unicode() {
        assert_eq!(sanitize_name("hello!@#world").unwrap(), "helloworld");
        assert_eq!(sanitize_name("café").unwrap(), "caf");
    }

    #[test]
    fn sanitize_empty_errors() {
        assert!(matches!(
            sanitize_name(""),
            Err(DumpError::EmptyName { .. })
        ));
        assert!(matches!(
            sanitize_name("   "),
            Err(DumpError::EmptyName { .. })
        ));
        assert!(matches!(
            sanitize_name("!@#$"),
            Err(DumpError::EmptyName { .. })
        ));
    }

    #[test]
    fn sanitize_truncates_to_max_len() {
        let long = "a".repeat(200);
        let sanitized = sanitize_name(&long).unwrap();
        assert_eq!(sanitized.len(), MAX_NAME_LEN);
    }

    #[test]
    fn sanitize_truncate_removes_trailing_dash() {
        let mut input = "a".repeat(MAX_NAME_LEN - 1);
        input.push('-');
        input.push_str(&"b".repeat(10));
        let sanitized = sanitize_name(&input).unwrap();
        assert!(!sanitized.ends_with('-'));
        assert!(sanitized.len() <= MAX_NAME_LEN);
    }

    #[test]
    fn id_format() {
        let at = time::macros::datetime!(2026-04-19 14:23:11 UTC);
        let id = DumpId::new("my bug", at).unwrap();
        assert_eq!(id.as_str(), "2026-04-19_14-23-11_my-bug");
        assert_eq!(id.filename(), "2026-04-19_14-23-11_my-bug.dump");
        assert_eq!(id.name(), Some("my-bug"));
    }

    #[test]
    fn id_roundtrip_via_filename() {
        let at = time::macros::datetime!(2026-04-19 14:23:11 UTC);
        let id = DumpId::new("x", at).unwrap();
        let parsed = DumpId::from_filename(&id.filename()).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn id_created_at_parses() {
        let at = time::macros::datetime!(2026-04-19 14:23:11 UTC);
        let id = DumpId::new("x", at).unwrap();
        assert_eq!(id.created_at(), Some(at));
    }

    #[test]
    fn workspace_dir_archive_captures_tree_and_mode() {
        use crate::host::FakeFs;
        let fs = FakeFs::new();
        fs.insert_file("/ws/main.rs", "fn main() {}\n");
        fs.insert_file("/ws/sub/lib.rs", "pub fn x() {}\n");
        let at = time::macros::datetime!(2026-04-19 14:23:11 UTC);
        let id = DumpId::new("gui-bug", at).unwrap();
        let archive_path = PathBuf::from("/out/gui-bug.dump");

        save::write_workspace_dir_archive(Path::new("/ws"), "normal", &id, at, &archive_path, &fs)
            .unwrap();

        let mut bytes = Vec::new();
        fs.read(&archive_path, &mut bytes).unwrap();
        let (meta_ron, entries) = bundle::deserialize(&bytes).unwrap();
        let meta = DumpMeta::from_ron(&meta_ron).unwrap();

        assert_eq!(meta.workspace.mode, "normal");
        assert_eq!(meta.git_root, PathBuf::from("/ws"));
        assert!(meta.workspace.rebase.is_none());
        let files: Vec<String> = entries
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, ["main.rs", "sub/lib.rs"]);
    }
}
