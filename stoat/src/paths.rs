use crate::fs::Fs;
use std::path::{Path, PathBuf};

pub struct StoatPaths {
    pub stoat_dir: Option<PathBuf>,
    pub keymap_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
}

pub async fn discover(start_dir: &Path, fs: &dyn Fs) -> StoatPaths {
    let dir = match walk_ancestors(start_dir, fs).await {
        Some(d) => Some(d),
        None => system_config_dir(fs).await,
    };

    match dir {
        Some(d) => {
            if d.starts_with(start_dir) || start_dir.starts_with(d.parent().unwrap_or(&d)) {
                tracing::info!("found project .stoat directory: {}", d.display());
            } else {
                tracing::info!("using system config directory: {}", d.display());
            }
            paths_from_dir(&d, fs).await
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

async fn walk_ancestors(start_dir: &Path, fs: &dyn Fs) -> Option<PathBuf> {
    let mut current = Some(start_dir);
    while let Some(dir) = current {
        let candidate = dir.join(".stoat");
        if fs.is_dir(&candidate).await {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

async fn system_config_dir(fs: &dyn Fs) -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("stoat");
    if fs.is_dir(&dir).await {
        Some(dir)
    } else {
        None
    }
}

async fn paths_from_dir(dir: &Path, fs: &dyn Fs) -> StoatPaths {
    let keymap = dir.join("keymap.stcfg");
    let config = dir.join("config.toml");
    StoatPaths {
        stoat_dir: Some(dir.to_path_buf()),
        keymap_path: if fs.is_file(&keymap).await {
            Some(keymap)
        } else {
            None
        },
        config_path: if fs.is_file(&config).await {
            Some(config)
        } else {
            None
        },
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

        let paths = smol::block_on(discover(Path::new("/project"), &fs));
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
        smol::block_on(fs.create_dir_all(Path::new("/project/a/b/c"))).unwrap();

        let paths = smol::block_on(discover(Path::new("/project/a/b/c"), &fs));
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
        let paths = smol::block_on(discover(Path::new("/empty"), &fs));
        assert!(paths.stoat_dir.is_none());
    }

    #[test]
    fn partial_files_only_keymap() {
        let fs = FakeFs::new();
        fs.insert_file("/project/.stoat/keymap.stcfg", "# keymap");

        let paths = smol::block_on(discover(Path::new("/project"), &fs));
        assert!(paths.keymap_path.is_some());
        assert!(paths.config_path.is_none());
    }

    #[test]
    fn partial_files_only_config() {
        let fs = FakeFs::new();
        fs.insert_file("/project/.stoat/config.toml", "# config");

        let paths = smol::block_on(discover(Path::new("/project"), &fs));
        assert!(paths.keymap_path.is_none());
        assert!(paths.config_path.is_some());
    }

    #[test]
    fn empty_stoat_dir() {
        let fs = FakeFs::new();
        smol::block_on(fs.create_dir_all(Path::new("/project/.stoat"))).unwrap();

        let paths = smol::block_on(discover(Path::new("/project"), &fs));
        assert_eq!(
            paths.stoat_dir.as_deref(),
            Some(Path::new("/project/.stoat"))
        );
        assert!(paths.keymap_path.is_none());
        assert!(paths.config_path.is_none());
    }
}
