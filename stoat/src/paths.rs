use std::path::{Path, PathBuf};

pub struct StoatPaths {
    pub stoat_dir: Option<PathBuf>,
    pub keymap_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
}

pub fn discover(start_dir: &Path) -> StoatPaths {
    let dir = walk_ancestors(start_dir).or_else(system_config_dir);

    match dir {
        Some(d) => {
            if d.starts_with(start_dir) || start_dir.starts_with(d.parent().unwrap_or(&d)) {
                tracing::info!("found project .stoat directory: {}", d.display());
            } else {
                tracing::info!("using system config directory: {}", d.display());
            }
            paths_from_dir(&d)
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

fn walk_ancestors(start_dir: &Path) -> Option<PathBuf> {
    let mut current = Some(start_dir);
    while let Some(dir) = current {
        let candidate = dir.join(".stoat");
        if candidate.is_dir() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

fn system_config_dir() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("stoat");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

fn paths_from_dir(dir: &Path) -> StoatPaths {
    let keymap = dir.join("keymap.stcfg");
    let config = dir.join("config.toml");
    StoatPaths {
        stoat_dir: Some(dir.to_path_buf()),
        keymap_path: keymap.is_file().then_some(keymap),
        config_path: config.is_file().then_some(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn finds_stoat_dir_in_cwd() {
        let tmp = tempdir().unwrap();
        let stoat_dir = tmp.path().join(".stoat");
        fs::create_dir(&stoat_dir).unwrap();
        fs::write(stoat_dir.join("keymap.stcfg"), "# keymap").unwrap();
        fs::write(stoat_dir.join("config.toml"), "# config").unwrap();

        let paths = discover(tmp.path());
        assert_eq!(paths.stoat_dir.as_deref(), Some(stoat_dir.as_path()));
        assert_eq!(
            paths.keymap_path.as_deref(),
            Some(stoat_dir.join("keymap.stcfg").as_path())
        );
        assert_eq!(
            paths.config_path.as_deref(),
            Some(stoat_dir.join("config.toml").as_path())
        );
    }

    #[test]
    fn finds_stoat_dir_in_ancestor() {
        let tmp = tempdir().unwrap();
        let stoat_dir = tmp.path().join(".stoat");
        fs::create_dir(&stoat_dir).unwrap();
        fs::write(stoat_dir.join("keymap.stcfg"), "# keymap").unwrap();

        let child = tmp.path().join("a").join("b").join("c");
        fs::create_dir_all(&child).unwrap();

        let paths = discover(&child);
        assert_eq!(paths.stoat_dir.as_deref(), Some(stoat_dir.as_path()));
        assert_eq!(
            paths.keymap_path.as_deref(),
            Some(stoat_dir.join("keymap.stcfg").as_path())
        );
        assert!(paths.config_path.is_none());
    }

    #[test]
    fn no_stoat_dir_found() {
        let tmp = tempdir().unwrap();
        let paths = discover(tmp.path());
        // System config dir may or may not exist; stoat_dir could be Some or None.
        // The key invariant: no crash, and if stoat_dir is set it's a real directory.
        if let Some(ref dir) = paths.stoat_dir {
            assert!(dir.is_dir());
        }
    }

    #[test]
    fn partial_files_only_keymap() {
        let tmp = tempdir().unwrap();
        let stoat_dir = tmp.path().join(".stoat");
        fs::create_dir(&stoat_dir).unwrap();
        fs::write(stoat_dir.join("keymap.stcfg"), "# keymap").unwrap();

        let paths = discover(tmp.path());
        assert!(paths.keymap_path.is_some());
        assert!(paths.config_path.is_none());
    }

    #[test]
    fn partial_files_only_config() {
        let tmp = tempdir().unwrap();
        let stoat_dir = tmp.path().join(".stoat");
        fs::create_dir(&stoat_dir).unwrap();
        fs::write(stoat_dir.join("config.toml"), "# config").unwrap();

        let paths = discover(tmp.path());
        assert!(paths.keymap_path.is_none());
        assert!(paths.config_path.is_some());
    }

    #[test]
    fn empty_stoat_dir() {
        let tmp = tempdir().unwrap();
        let stoat_dir = tmp.path().join(".stoat");
        fs::create_dir(&stoat_dir).unwrap();

        let paths = discover(tmp.path());
        assert_eq!(paths.stoat_dir.as_deref(), Some(stoat_dir.as_path()));
        assert!(paths.keymap_path.is_none());
        assert!(paths.config_path.is_none());
    }
}
