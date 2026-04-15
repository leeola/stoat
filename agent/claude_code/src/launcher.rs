//! [`ClaudeCodeHost`] implementation that spawns real [`ClaudeCode`]
//! subprocesses. Stoat registers one of these at startup; each call to
//! [`ClaudeCodeHost::new_session`] launches a fresh subprocess using the
//! launcher's configured defaults.

use crate::{ClaudeCode, SessionConfig};
use async_trait::async_trait;
use std::{io, sync::Arc};
use stoat::host::{ClaudeCodeHost, ClaudeCodeSession};
use stoat_log::TextProtoLog;

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
        let resume_existing = self.default_config.session_id.is_some();
        let session_id = self
            .default_config
            .session_id
            .unwrap_or_else(uuid::Uuid::new_v4);

        let (tx_log, rx_log) = open_session_logs(session_id);

        let mut config = self.default_config.clone();
        config.session_id = Some(session_id);

        let mut builder = ClaudeCode::builder().with_config(config);
        if let (Some(tx), Some(rx)) = (tx_log, rx_log) {
            builder = builder.with_text_proto_logs(tx, rx);
        }
        let claude_code = if resume_existing {
            builder.resume().await
        } else {
            builder.create_new().await
        }
        .map_err(io::Error::other)?;
        Ok(Box::new(claude_code))
    }
}

fn open_session_logs(
    session_id: uuid::Uuid,
) -> (Option<Arc<TextProtoLog>>, Option<Arc<TextProtoLog>>) {
    let dir = match stoat_log::log_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("resolve log dir for Claude protocol logs: {e}");
            return (None, None);
        },
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("create log dir {}: {e}", dir.display());
        return (None, None);
    }
    let tx_path = dir.join(format!("claude-{session_id}.tx.jsonl"));
    let rx_path = dir.join(format!("claude-{session_id}.rx.jsonl"));
    let tx = open_log(&tx_path);
    let rx = open_log(&rx_path);
    (tx, rx)
}

fn open_log(path: &std::path::Path) -> Option<Arc<TextProtoLog>> {
    match TextProtoLog::create_at(path) {
        Ok(log) => Some(Arc::new(log)),
        Err(e) => {
            tracing::warn!("open protocol log {}: {e}", path.display());
            None
        },
    }
}
