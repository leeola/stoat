//! [`ClaudeCodeHost`] implementation that spawns real [`ClaudeCode`]
//! subprocesses. Stoat registers one of these at startup; each call to
//! [`ClaudeCodeHost::new_session`] launches a fresh subprocess using the
//! launcher's configured defaults.

use crate::{ClaudeCode, SessionConfig};
use async_trait::async_trait;
use std::io;
use stoat::host::{ClaudeCodeHost, ClaudeCodeSession};

#[derive(Debug, Default)]
pub struct ClaudeCodeLauncher {
    default_config: SessionConfig,
}

impl ClaudeCodeLauncher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(default_config: SessionConfig) -> Self {
        Self { default_config }
    }
}

#[async_trait]
impl ClaudeCodeHost for ClaudeCodeLauncher {
    async fn new_session(&self) -> io::Result<Box<dyn ClaudeCodeSession>> {
        let claude_code = ClaudeCode::new(self.default_config.clone())
            .await
            .map_err(io::Error::other)?;
        Ok(Box::new(claude_code))
    }
}
