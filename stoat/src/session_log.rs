use std::path::PathBuf;

/// The log file this session writes.
pub(crate) struct SessionLog {
    /// Where the file is, which is what `:logs` opens.
    pub(crate) path: PathBuf,
}

impl SessionLog {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}
