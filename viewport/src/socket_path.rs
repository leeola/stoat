use std::path::PathBuf;
use stoat_host::EnvHost;

/// Resolves the socket path for a given session identifier.
///
/// Uses `$TMPDIR` on macOS, `$XDG_RUNTIME_DIR` on Linux, falling back to `/tmp`.
pub fn socket_path(env: &dyn EnvHost, session_id: &str) -> PathBuf {
    let dir = if cfg!(target_os = "macos") {
        env.var("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
    } else {
        env.var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
    };
    dir.join(format!("stoat-{session_id}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_host::FakeEnv;

    #[test]
    fn path_contains_session_id() {
        let env = FakeEnv::new();
        let path = socket_path(&env, "abc123");
        assert_eq!(path.file_name().unwrap(), "stoat-abc123.sock");
    }
}
