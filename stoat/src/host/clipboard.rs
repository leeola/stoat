use crate::host::EnvHost;
use std::io;

/// System clipboard write surface.
///
/// Production code routes clipboard writes through this trait so tests
/// can install [`crate::host::FakeClipboard`] without leaking into the
/// real OS clipboard. UTF-8-only by design: callers serialize into a
/// `&str` before invoking [`Self::set`].
pub trait ClipboardHost: Send + Sync {
    /// Replaces the system clipboard contents with `text`.
    fn set(&self, text: &str) -> io::Result<()>;

    /// Returns the current clipboard contents. `Ok(None)` covers the
    /// "no clipboard available" case (headless servers, CI without a
    /// display server) so callers fall back to a no-op rather than
    /// surface the platform error. The default impl returns
    /// `Ok(None)` for hosts without a real backing store.
    fn get(&self) -> io::Result<Option<String>> {
        Ok(None)
    }

    /// Forwards `text` to the host terminal as an OSC 52 set-clipboard
    /// escape so SSH sessions reach the user's local clipboard. The
    /// default impl is a no-op for hosts without a stdout-bound
    /// terminal (the test [`crate::host::FakeClipboard`] overrides
    /// this to record the emit).
    fn osc52_emit(&self, text: &str) -> io::Result<()> {
        let _ = text;
        Ok(())
    }
}

/// Reports whether the running session should emit OSC 52 clipboard
/// forwarding alongside [`ClipboardHost::set`]. Returns true when an
/// SSH session is detected via `$SSH_CONNECTION` or `$SSH_TTY` and no
/// parent multiplexer is announced via `$TMUX` or `$ZELLIJ` (in mux
/// nesting the parent owns clipboard forwarding).
pub fn osc52_should_emit(env: &dyn EnvHost) -> bool {
    let in_ssh = env.var("SSH_CONNECTION").is_some() || env.var("SSH_TTY").is_some();
    let in_mux = env.var("TMUX").is_some() || env.var("ZELLIJ").is_some();
    in_ssh && !in_mux
}

/// No-op [`ClipboardHost`] used when no real clipboard is needed (or
/// available). Logs the would-be write at trace level and returns
/// success so call sites can ignore the absence of a real clipboard.
pub struct NoopClipboard;

impl ClipboardHost for NoopClipboard {
    fn set(&self, text: &str) -> io::Result<()> {
        tracing::trace!(
            target: "stoat::host::clipboard",
            len = text.len(),
            "clipboard set ignored (NoopClipboard)"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeEnv;

    #[test]
    fn should_emit_in_ssh_without_mux() {
        let env = FakeEnv::new();
        env.set("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
        assert!(osc52_should_emit(&env));
    }

    #[test]
    fn should_emit_in_ssh_via_ssh_tty() {
        let env = FakeEnv::new();
        env.set("SSH_TTY", "/dev/pts/0");
        assert!(osc52_should_emit(&env));
    }

    #[test]
    fn should_skip_in_tmux() {
        let env = FakeEnv::new();
        env.set("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
        env.set("TMUX", "/tmp/tmux-1000/default,1234,0");
        assert!(!osc52_should_emit(&env));
    }

    #[test]
    fn should_skip_in_zellij() {
        let env = FakeEnv::new();
        env.set("SSH_TTY", "/dev/pts/0");
        env.set("ZELLIJ", "0");
        assert!(!osc52_should_emit(&env));
    }

    #[test]
    fn should_skip_locally() {
        let env = FakeEnv::new();
        assert!(!osc52_should_emit(&env));
    }
}
