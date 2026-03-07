use crate::fs::Fs;
use std::path::{Path, PathBuf};

pub struct StoatPaths {
    pub stoat_dir: Option<PathBuf>,
    pub keymap_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
}

pub fn discover(start_dir: &Path, fs: &dyn Fs) -> StoatPaths {
    let dir = walk_ancestors(start_dir, fs).or_else(|| system_config_dir(fs));

    match dir {
        Some(d) => {
            if d.starts_with(start_dir) || start_dir.starts_with(d.parent().unwrap_or(&d)) {
                tracing::info!("found project .stoat directory: {}", d.display());
            } else {
                tracing::info!("using system config directory: {}", d.display());
            }
            paths_from_dir(&d, fs)
        },
        None => {
            tracing::debug!("no .stoat directory found");
            StoatPaths {
                stoat_dir: None,
                keymap_path: None,
                config_path: None,
            }
        },
    }
}

fn walk_ancestors(start_dir: &Path, fs: &dyn Fs) -> Option<PathBuf> {
    let mut current = Some(start_dir);
    while let Some(dir) = current {
        let candidate = dir.join(".stoat");
        if fs.is_dir(&candidate) {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

fn system_config_dir(fs: &dyn Fs) -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("stoat");
    if fs.is_dir(&dir) {
        Some(dir)
    } else {
        None
    }
}

fn paths_from_dir(dir: &Path, fs: &dyn Fs) -> StoatPaths {
    let keymap = dir.join("keymap.stcfg");
    let config = dir.join("config.toml");
    StoatPaths {
        stoat_dir: Some(dir.to_path_buf()),
        keymap_path: fs.is_file(&keymap).then_some(keymap),
        config_path: fs.is_file(&config).then_some(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FakeFs;

    #[test]
    fn finds_stoat_dir_in_cwd() {
        let fs = FakeFs::new();
        fs.insert_file("/project/.stoat/keymap.stcfg", "# keymap");
        fs.insert_file("/project/.stoat/config.toml", "# config");

        let paths = discover(Path::new("/project"), &fs);
        assert_eq!(
            paths.stoat_dir.as_deref(),
            Some(Path::new("/project/.stoat"))
        );
        assert_eq!(
            paths.keymap_path.as_deref(),
            Some(Path::new("/project/.stoat/keymap.stcfg"))
        );
        assert_eq!(
            paths.config_path.as_deref(),
            Some(Path::new("/project/.stoat/config.toml"))
        );
    }

    #[test]
    fn finds_stoat_dir_in_ancestor() {
        let fs = FakeFs::new();
        fs.insert_file("/project/.stoat/keymap.stcfg", "# keymap");
        fs.create_dir_all(Path::new("/project/a/b/c")).unwrap();

        let paths = discover(Path::new("/project/a/b/c"), &fs);
        assert_eq!(
            paths.stoat_dir.as_deref(),
            Some(Path::new("/project/.stoat"))
        );
        assert_eq!(
            paths.keymap_path.as_deref(),
            Some(Path::new("/project/.stoat/keymap.stcfg"))
        );
        assert!(paths.config_path.is_none());
    }

    #[test]
    fn no_stoat_dir_found() {
        let fs = FakeFs::new();
        let paths = discover(Path::new("/empty"), &fs);
        assert!(paths.stoat_dir.is_none());
    }

    #[test]
    fn partial_files_only_keymap() {
        let fs = FakeFs::new();
        fs.insert_file("/project/.stoat/keymap.stcfg", "# keymap");

        let paths = discover(Path::new("/project"), &fs);
        assert!(paths.keymap_path.is_some());
        assert!(paths.config_path.is_none());
    }

    #[test]
    fn partial_files_only_config() {
        let fs = FakeFs::new();
        fs.insert_file("/project/.stoat/config.toml", "# config");

        let paths = discover(Path::new("/project"), &fs);
        assert!(paths.keymap_path.is_none());
        assert!(paths.config_path.is_some());
    }

    #[test]
    fn empty_stoat_dir() {
        let fs = FakeFs::new();
        fs.create_dir_all(Path::new("/project/.stoat")).unwrap();

        let paths = discover(Path::new("/project"), &fs);
        assert_eq!(
            paths.stoat_dir.as_deref(),
            Some(Path::new("/project/.stoat"))
        );
        assert!(paths.keymap_path.is_none());
        assert!(paths.config_path.is_none());
    }
}
