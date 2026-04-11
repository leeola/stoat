//! [`ClaudeCodeFactory`] implementation that spawns real [`ClaudeCode`]
//! subprocesses. Stoat registers one of these at startup; each call to
//! [`ClaudeCodeFactory::create`] launches a fresh subprocess using the
//! launcher's configured defaults.

use crate::{ClaudeCode, SessionConfig};
use async_trait::async_trait;
use std::{io, sync::Arc};
use stoat::host::{ClaudeCodeFactory, ClaudeCodeHost};

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
impl ClaudeCodeFactory for ClaudeCodeLauncher {
    async fn create(&self) -> io::Result<Arc<dyn ClaudeCodeHost>> {
        let claude_code = ClaudeCode::new(self.default_config.clone())
            .await
            .map_err(io::Error::other)?;
        Ok(Arc::new(claude_code) as Arc<dyn ClaudeCodeHost>)
    }
}
