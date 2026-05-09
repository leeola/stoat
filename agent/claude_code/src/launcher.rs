//! [`ClaudeCodeHost`] implementation that spawns real [`ClaudeCode`]
//! subprocesses. Stoat registers one of these at startup; each call to
//! [`ClaudeCodeHost::new_session`] launches a fresh subprocess using the
//! launcher's configured defaults.

use crate::{ClaudeCode, SessionConfig};
use async_trait::async_trait;
use std::{io, sync::Arc};
use stoat::host::{ClaudeCodeHost, ClaudeCodeSession, FsHost, PermissionCallback};
use stoat_log::TextProtoLog;
use stoat_scheduler::Executor;

pub struct ClaudeCodeLauncher {
    default_config: SessionConfig,
    fs_host: Arc<dyn FsHost>,
    executor: Executor,
    permission_callback: Option<Arc<dyn PermissionCallback>>,
}

impl std::fmt::Debug for ClaudeCodeLauncher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeLauncher")
            .field("default_config", &self.default_config)
            .field(
                "has_permission_callback",
                &self.permission_callback.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl ClaudeCodeLauncher {
    pub fn new(fs_host: Arc<dyn FsHost>, executor: Executor) -> Self {
        Self {
            default_config: SessionConfig::default(),
            fs_host,
            executor,
            permission_callback: None,
        }
    }

    pub fn with_config(
        default_config: SessionConfig,
        fs_host: Arc<dyn FsHost>,
        executor: Executor,
    ) -> Self {
        Self {
            default_config,
            fs_host,
            executor,
            permission_callback: None,
        }
    }

    /// Install a permission callback that gates every spawned session.
    /// When set, sessions are launched with the control protocol's
    /// permission-prompt routing and each tool invocation flows
    /// through `callback.can_use_tool` before execution.
    pub fn with_permission_callback(mut self, callback: Arc<dyn PermissionCallback>) -> Self {
        self.permission_callback = Some(callback);
        self
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

        let (tx_log, rx_log) = open_session_logs(session_id, &*self.fs_host);

        let mut config = self.default_config.clone();
        config.session_id = Some(session_id);

        let mut builder = ClaudeCode::builder(self.executor.clone()).with_config(config);
        if let (Some(tx), Some(rx)) = (tx_log, rx_log) {
            builder = builder.with_text_proto_logs(tx, rx);
        }
        if let Some(callback) = self.permission_callback.clone() {
            builder = builder.permission_callback(callback);
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
    fs: &dyn FsHost,
) -> (Option<Arc<TextProtoLog>>, Option<Arc<TextProtoLog>>) {
    let dir = match stoat_log::log_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("resolve log dir for Claude protocol logs: {e}");
            return (None, None);
        },
    };
    if let Err(e) = fs.create_dir_all(&dir) {
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
