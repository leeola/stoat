use crate::host::terminal::{open_local_pty, SpawnArgs, TerminalHost, TerminalSession};
use async_trait::async_trait;
use std::io;

/// Production [`TerminalHost`] that spawns sessions backed by
/// the OS PTY via `portable_pty`. Stateless -- spawn each
/// session independently.
pub struct LocalTerminalHost;

#[async_trait]
impl TerminalHost for LocalTerminalHost {
    async fn spawn(&self, args: SpawnArgs) -> io::Result<Box<dyn TerminalSession>> {
        let session = open_local_pty(args)?;
        Ok(Box::new(session))
    }
}
