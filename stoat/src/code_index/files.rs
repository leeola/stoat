//! The record of the path behind each file id of the code graph.

use codegraph::FileId;
use std::{
    collections::{BTreeSet, HashMap},
    ops::Bound,
    path::{Path, PathBuf},
};

/// The workspace-relative path of each file the code graph holds.
///
/// The graph keys files by a one-way hash of the path, so this record is the
/// only way back from a symbol to its file. The paths also stay in order, so
/// the files below a directory are one range.
///
/// See also:
/// - [`Self::below`] for that range, which the fs-watch drain reads.
#[derive(Default)]
pub(crate) struct IndexedFiles {
    by_id: HashMap<FileId, PathBuf>,
    /// The paths of `by_id`, in order.
    ordered: BTreeSet<PathBuf>,
}

impl IndexedFiles {
    pub(crate) fn insert(&mut self, file: FileId, path: PathBuf) {
        if self.by_id.get(&file) == Some(&path) {
            return;
        }
        if let Some(replaced) = self.by_id.insert(file, path.clone()) {
            self.ordered.remove(&replaced);
        }
        self.ordered.insert(path);
    }

    pub(crate) fn get(&self, file: &FileId) -> Option<&PathBuf> {
        self.by_id.get(file)
    }

    pub(crate) fn remove(&mut self, file: &FileId) {
        if let Some(path) = self.by_id.remove(file) {
            self.ordered.remove(&path);
        }
    }

    /// Returns the recorded paths strictly below `dir`, in path order.
    ///
    /// `dir` is workspace-relative, as the recorded paths are. A directory that
    /// moves away raises one watch event, on the directory, and this range is
    /// how [`crate::debounce::drain_fs_watch_events`] finds the files it held.
    pub(crate) fn below(&self, dir: &Path) -> impl Iterator<Item = &PathBuf> {
        self.ordered
            .range::<Path, _>((Bound::Excluded(dir), Bound::Unbounded))
            .take_while(move |path| path.starts_with(dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_index::build;

    /// A sibling whose name extends the directory's name sorts beside the
    /// subtree and stays out, as do the directory itself and a removed file.
    #[test]
    fn below_lists_the_recorded_subtree() {
        let mut files = IndexedFiles::default();
        for path in [
            "src/foo",
            "src/foo/a.rs",
            "src/foo/gone.rs",
            "src/foo/sub/b.rs",
            "src/foo.rs",
            "src/foobar/c.rs",
        ] {
            files.insert(build::file_id(path), PathBuf::from(path));
        }
        files.remove(&build::file_id("src/foo/gone.rs"));

        assert_eq!(
            files.below(Path::new("src/foo")).collect::<Vec<_>>(),
            [Path::new("src/foo/a.rs"), Path::new("src/foo/sub/b.rs")],
        );
    }
}
