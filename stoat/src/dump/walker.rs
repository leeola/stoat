use super::DumpError;
use crate::host::FsHost;
use ignore::{
    gitignore::{Gitignore, GitignoreBuilder},
    Match,
};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

const GITIGNORE_FILE: &str = ".gitignore";

/// Walk the workspace tree at `root` through `fs`, returning a map from
/// workspace-relative path to file contents. Honors per-directory
/// `.gitignore` files (read through `fs`); skips ignored files /
/// directories. Top-level `.git/` and `.stoat/` directories bypass
/// gitignore checks so dump replay always has the repo metadata.
///
/// Out of scope: `.git/info/exclude` and the global gitignore. Adding
/// either requires reading state outside the workspace tree.
pub(crate) fn gather_workspace_files(
    fs: &dyn FsHost,
    root: &Path,
) -> Result<BTreeMap<PathBuf, Vec<u8>>, DumpError> {
    let mut out = BTreeMap::new();
    let mut chain = GitignoreChain::new();
    walk(fs, root, root, &mut chain, &mut out)?;
    Ok(out)
}

fn walk(
    fs: &dyn FsHost,
    root: &Path,
    dir: &Path,
    chain: &mut GitignoreChain,
    out: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), DumpError> {
    let pushed = read_gitignore(fs, dir, chain)?;

    let mut entries = fs.list_dir(dir)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    for entry in entries {
        let path = dir.join(entry.name.as_str());
        let rel = path
            .strip_prefix(root)
            .map_err(|e| DumpError::Io(io::Error::other(e.to_string())))?
            .to_path_buf();

        let force_include = is_force_included(&rel);

        if !force_include && chain.is_ignored(&path, entry.is_dir) {
            continue;
        }

        if entry.is_dir {
            walk(fs, root, &path, chain, out)?;
        } else {
            let mut buf = Vec::new();
            fs.read(&path, &mut buf)?;
            out.insert(rel, buf);
        }
    }

    if pushed {
        chain.pop();
    }
    Ok(())
}

fn read_gitignore(
    fs: &dyn FsHost,
    dir: &Path,
    chain: &mut GitignoreChain,
) -> Result<bool, DumpError> {
    let path = dir.join(GITIGNORE_FILE);
    if !fs.exists(&path) {
        return Ok(false);
    }
    let mut buf = Vec::new();
    fs.read(&path, &mut buf)?;
    let text = std::str::from_utf8(&buf)
        .map_err(|e| DumpError::Io(io::Error::other(format!(".gitignore not UTF-8: {e}"))))?;
    chain.push(dir.to_path_buf(), text)?;
    Ok(true)
}

fn is_force_included(rel: &Path) -> bool {
    super::FORCE_INCLUDE_DIRS
        .iter()
        .any(|top| rel.starts_with(top))
}

struct GitignoreChain {
    matchers: Vec<(PathBuf, Gitignore)>,
}

impl GitignoreChain {
    fn new() -> Self {
        Self {
            matchers: Vec::new(),
        }
    }

    fn push(&mut self, root: PathBuf, content: &str) -> Result<(), DumpError> {
        let mut builder = GitignoreBuilder::new(&root);
        for line in content.lines() {
            builder
                .add_line(None, line)
                .map_err(|e| DumpError::Io(io::Error::other(e.to_string())))?;
        }
        let gi = builder
            .build()
            .map_err(|e| DumpError::Io(io::Error::other(e.to_string())))?;
        self.matchers.push((root, gi));
        Ok(())
    }

    fn pop(&mut self) {
        self.matchers.pop();
    }

    /// Walks the matcher stack innermost-first; deeper gitignores
    /// override outer ones. Returns true when any matcher classifies
    /// `path` as ignored without a later whitelist override.
    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        for (root, gi) in self.matchers.iter().rev() {
            match gi.matched_path_or_any_parents(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {
                    let _ = root;
                    continue;
                },
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeFs;

    fn read_keys(map: &BTreeMap<PathBuf, Vec<u8>>) -> Vec<String> {
        map.keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn collects_plain_files() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/a.txt", "alpha");
        fs.insert_file("/ws/sub/b.txt", "beta");
        let out = gather_workspace_files(&fs, Path::new("/ws")).unwrap();
        assert_eq!(read_keys(&out), ["a.txt", "sub/b.txt"]);
        assert_eq!(out[Path::new("a.txt")], b"alpha");
        assert_eq!(out[Path::new("sub/b.txt")], b"beta");
    }

    #[test]
    fn root_gitignore_skips_matching_files() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/.gitignore", "ignored/\n");
        fs.insert_file("/ws/keep.txt", "ok");
        fs.insert_file("/ws/ignored/secret", "no");
        let out = gather_workspace_files(&fs, Path::new("/ws")).unwrap();
        let keys = read_keys(&out);
        assert!(keys.contains(&".gitignore".to_string()));
        assert!(keys.contains(&"keep.txt".to_string()));
        assert!(!keys.iter().any(|k| k.starts_with("ignored/")));
    }

    #[test]
    fn force_include_overrides_gitignore_for_git_and_stoat() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/.gitignore", ".git/\n.stoat/\n");
        fs.insert_file("/ws/.git/HEAD", "ref: refs/heads/main");
        fs.insert_file("/ws/.stoat/state.ron", "(state: ())");
        let out = gather_workspace_files(&fs, Path::new("/ws")).unwrap();
        let keys = read_keys(&out);
        assert!(keys.contains(&".git/HEAD".to_string()));
        assert!(keys.contains(&".stoat/state.ron".to_string()));
    }

    #[test]
    fn nested_gitignore_whitelist_overrides_outer_ignore() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/.gitignore", "*.log\n");
        fs.insert_file("/ws/sub/.gitignore", "!keep.log\n");
        fs.insert_file("/ws/skip.log", "no");
        fs.insert_file("/ws/sub/keep.log", "yes");
        fs.insert_file("/ws/sub/drop.log", "no");
        let out = gather_workspace_files(&fs, Path::new("/ws")).unwrap();
        let keys = read_keys(&out);
        assert!(!keys.contains(&"skip.log".to_string()));
        assert!(keys.contains(&"sub/keep.log".to_string()));
        assert!(!keys.contains(&"sub/drop.log".to_string()));
    }

    #[test]
    fn missing_gitignore_walks_everything() {
        let fs = FakeFs::new();
        fs.insert_file("/ws/a.txt", "");
        fs.insert_file("/ws/sub/b.txt", "");
        let out = gather_workspace_files(&fs, Path::new("/ws")).unwrap();
        assert_eq!(read_keys(&out), ["a.txt", "sub/b.txt"]);
    }
}
