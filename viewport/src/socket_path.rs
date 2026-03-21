use std::path::PathBuf;

/// Resolves the socket path for a given session identifier.
///
/// Uses `$TMPDIR` on macOS, `$XDG_RUNTIME_DIR` on Linux, falling back to `/tmp`.
pub fn socket_path(session_id: &str) -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
    };
    dir.join(format!("stoat-{session_id}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_contains_session_id() {
        let path = socket_path("abc123");
        assert_eq!(path.file_name().unwrap(), "stoat-abc123.sock");
    }
}
