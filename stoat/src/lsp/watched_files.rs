//! The file watchers a language server registers with the editor.
//!
//! A server must learn about files that change on disk outside the editor: a
//! new source file, a deleted one, or a manifest edit. Stoat offers dynamic
//! registration of `workspace/didChangeWatchedFiles`, and a server that uses it
//! turns its own watcher off. From then on, the server learns about the disk
//! only through the notifications the editor sends. A registration that the
//! editor holds and never serves leaves the server blind to the disk after
//! startup.
//!
//! A registration's globs compile once, when the server sends them. Each
//! changed path is then tested against the compiled form.
//!
//! See also:
//! - [`crate::lsp::drain`] for the requests that hold and drop registrations.
//! - [`crate::debounce::drain_fs_watch_events`] for the changes sent to them.

use crate::lsp::util;
use globset::{Glob, GlobMatcher};
use lsp_types::{
    notification::{DidChangeWatchedFiles, Notification},
    DidChangeWatchedFilesRegistrationOptions, FileChangeType, FileSystemWatcher, GlobPattern,
    OneOf, Registration, WatchKind,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One `workspace/didChangeWatchedFiles` registration of a language server,
/// with its globs compiled.
pub(crate) struct WatchedFilesRegistration {
    /// The id the server registered under, which its unregistration names.
    pub(crate) id: String,
    watchers: Vec<CompiledWatcher>,
}

/// One glob of a registration and the change kinds the server wants for it.
struct CompiledWatcher {
    matcher: GlobMatcher,
    /// The folder a relative pattern matches below, or `None` for a pattern
    /// that matches the whole absolute path.
    base: Option<PathBuf>,
    kind: WatchKind,
}

impl WatchedFilesRegistration {
    /// Whether the server asked to hear about a `change` to `path`.
    pub(crate) fn matches(&self, path: &Path, change: FileChangeType) -> bool {
        let wanted = watch_kind(change);
        self.watchers
            .iter()
            .any(|watcher| watcher.kind.contains(wanted) && watcher.matches(path))
    }
}

impl CompiledWatcher {
    fn matches(&self, path: &Path) -> bool {
        match &self.base {
            Some(base) => path
                .strip_prefix(base)
                .is_ok_and(|relative| self.matcher.is_match(relative)),
            None => self.matcher.is_match(path),
        }
    }
}

/// Compiles `registration` when it registers file watchers.
///
/// Returns `None` for a registration of any other method, and for options
/// that do not decode. A glob that does not compile, or a base that is not a
/// `file:` URI, is logged and skipped. The other watchers of the registration
/// stay in force.
pub(crate) fn compile(registration: &Registration) -> Option<WatchedFilesRegistration> {
    if registration.method != DidChangeWatchedFiles::METHOD {
        return None;
    }
    let options = DidChangeWatchedFilesRegistrationOptions::deserialize(
        registration.register_options.as_ref()?,
    )
    .ok()?;

    let watchers = options
        .watchers
        .into_iter()
        .filter_map(compile_watcher)
        .collect();
    Some(WatchedFilesRegistration {
        id: registration.id.clone(),
        watchers,
    })
}

fn compile_watcher(watcher: FileSystemWatcher) -> Option<CompiledWatcher> {
    let (pattern, base) = match watcher.glob_pattern {
        GlobPattern::String(pattern) => (pattern, None),
        GlobPattern::Relative(relative) => {
            let base_uri = match relative.base_uri {
                OneOf::Left(folder) => folder.uri,
                OneOf::Right(uri) => uri,
            };
            let Some(base) = util::lsp_uri_to_path(&base_uri) else {
                tracing::warn!(
                    target: "stoat::lsp",
                    base = base_uri.as_str(),
                    "file watcher base is not a file URI, skipping"
                );
                return None;
            };
            (relative.pattern, Some(base))
        },
    };

    let matcher = match Glob::new(&pattern) {
        Ok(glob) => glob.compile_matcher(),
        Err(err) => {
            tracing::warn!(
                target: "stoat::lsp",
                glob = %pattern,
                %err,
                "invalid file watcher glob, skipping"
            );
            return None;
        },
    };
    Some(CompiledWatcher {
        matcher,
        base,
        kind: watcher.kind.unwrap_or(WatchKind::all()),
    })
}

/// The [`WatchKind`] flag a watcher sets to hear about `change`.
fn watch_kind(change: FileChangeType) -> WatchKind {
    match change {
        FileChangeType::CREATED => WatchKind::Create,
        FileChangeType::CHANGED => WatchKind::Change,
        _ => WatchKind::Delete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture;
    use lsp_types::{RelativePattern, Uri};
    use std::str::FromStr;

    /// A string pattern matches the whole absolute path, at any depth.
    #[test]
    fn a_string_glob_matches_absolute_paths() {
        let rust = compiled(GlobPattern::String("**/*.rs".into()), None);

        assert_eq!(
            hits(
                &rust,
                FileChangeType::CREATED,
                &["/repo/src/a.rs", "/repo/Cargo.toml"]
            ),
            [true, false],
        );
    }

    /// A relative pattern matches only the paths below its base.
    #[test]
    fn a_relative_glob_matches_below_its_base() {
        let base_uri = OneOf::Right(Uri::from_str("file:///repo/sub").expect("base uri"));
        let manifests = compiled(
            GlobPattern::Relative(RelativePattern {
                base_uri,
                pattern: "*.toml".into(),
            }),
            None,
        );

        assert_eq!(
            hits(
                &manifests,
                FileChangeType::CHANGED,
                &["/repo/sub/Cargo.toml", "/repo/Cargo.toml"]
            ),
            [true, false],
        );
    }

    /// A watcher hears only the change kinds it asked for.
    #[test]
    fn a_create_only_watcher_ignores_changes() {
        let created = compiled(
            GlobPattern::String("**/*.rs".into()),
            Some(WatchKind::Create),
        );
        let path = Path::new("/repo/src/a.rs");

        assert_eq!(
            (
                created.matches(path, FileChangeType::CREATED),
                created.matches(path, FileChangeType::CHANGED),
            ),
            (true, false),
        );
    }

    /// The options of every other capability mean something else, so only a
    /// watched-files registration compiles.
    #[test]
    fn another_method_compiles_to_none() {
        let mut formatting = test_fixture::watched_files_registration(
            "w",
            GlobPattern::String("**/*.rs".into()),
            None,
        );
        formatting.method = "textDocument/formatting".into();

        assert!(compile(&formatting).is_none());
    }

    fn compiled(glob_pattern: GlobPattern, kind: Option<WatchKind>) -> WatchedFilesRegistration {
        let registration = test_fixture::watched_files_registration("w", glob_pattern, kind);
        compile(&registration).expect("a watched-files registration compiles")
    }

    /// Whether `registration` asks for `change` on each of `paths`.
    fn hits(
        registration: &WatchedFilesRegistration,
        change: FileChangeType,
        paths: &[&str],
    ) -> Vec<bool> {
        paths
            .iter()
            .map(|path| registration.matches(Path::new(path), change))
            .collect()
    }
}
