//! Hands the whole window to a plain `stoat` on a remote host over ssh.
//!
//! One session is on screen at a time. While the remote runs, the local stoat
//! is a byte pipe. The input thread reads fd 0 raw and writes it to the ssh
//! PTY, and the PTY's output reaches stdout untouched. The remote stoat
//! therefore handshakes with the local stoatty directly, since its `hello`
//! reaches stoatty and the ident reply rides fd 0 back through the pipe.
//!
//! Nothing tells the remote what this terminal already holds, so the local
//! emitters forget their terminal-side tracking before the handoff and rebuild
//! it from scratch on return.

use crate::{
    apc_emit,
    app::{Stoat, UpdateEffect},
    host::terminal::{SpawnArgs, TerminalSession},
    run,
};
use futures::FutureExt;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use stoatty_protocol::command::{encode_minimap_drop_into, encode_reset_into, MinimapDropCommand};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Remote output kept for the exit report.
///
/// The remote prints its errors inside the alternate screen, which its own exit
/// tears down, so the text is gone by the time anyone reads the status bar.
/// This much of the tail always covers the last line.
const TAIL_BYTES: usize = 4096;

/// What the app tells the UI thread to do while a remote session owns the
/// screen.
pub enum UiControl {
    /// Remote bytes to write to stdout as they are, framing included.
    Raw(Vec<u8>),
    /// The remote exited. Re-enter the alternate screen and repaint.
    Resume,
}

/// The passthrough state the input thread reads to decide what fd 0 feeds.
///
/// Shared between the app and that thread, which is why it is behind a lock
/// rather than owned by either. The app moves it through the states and the
/// input thread only observes.
pub struct PassthroughSlot {
    state: Mutex<SlotState>,
    ack_tx: UnboundedSender<()>,
}

#[derive(Clone)]
pub enum SlotState {
    Idle,
    Pending,
    Active(Arc<dyn TerminalSession>),
}

/// The app's ends of the passthrough plumbing, installed by the bin layer.
///
/// A headless or embedded run installs none, which is what makes `:ssh` refuse
/// there. With no UI thread to hand the terminal to there is nothing to pass
/// through.
pub struct PassthroughLink {
    pub slot: Arc<PassthroughSlot>,
    pub ui_tx: UnboundedSender<UiControl>,
    pub ack_rx: UnboundedReceiver<()>,
}

/// A remote session, before and after its process exists.
pub(crate) enum Passthrough {
    /// The terminal is torn down and the slot armed, waiting for the input
    /// thread to confirm it stopped parsing fd 0.
    Pending {
        host: String,
        program: String,
        args: Vec<String>,
    },
    /// The remote owns the screen. The session itself lives in the slot and in
    /// the reader task, which is what keeps the PTY open, so only the output
    /// tail belongs here.
    Active { tail: VecDeque<u8> },
}

impl PassthroughSlot {
    /// A slot in its idle state, plus the receiver the app waits on for the
    /// input thread's ack.
    pub fn new() -> (Arc<PassthroughSlot>, UnboundedReceiver<()>) {
        let (ack_tx, ack_rx) = unbounded_channel();
        let slot = PassthroughSlot {
            state: Mutex::new(SlotState::Idle),
            ack_tx,
        };
        (Arc::new(slot), ack_rx)
    }

    /// Whether fd 0 feeds the editor, a buffer, or the remote session.
    ///
    /// Handed back by value because the input thread acts on the answer outside
    /// the lock. A guard held across a blocking read parks the app the moment
    /// it tries to advance the state.
    pub fn state(&self) -> SlotState {
        self.state.lock().expect("passthrough slot lock").clone()
    }

    /// Report that fd 0 is no longer parsed as terminal input.
    ///
    /// The spawn waits for this rather than starting ssh straight away. A warm
    /// ControlMaster plus a fast remote delivers the ident inside the input
    /// thread's poll interval, and an ident parsed by crossterm is garbage keys.
    pub fn ack(&self) {
        let _ = self.ack_tx.send(());
    }

    pub(crate) fn arm(&self) {
        *self.state.lock().expect("passthrough slot lock") = SlotState::Pending;
    }

    pub(crate) fn engage(&self, session: Arc<dyn TerminalSession>) {
        *self.state.lock().expect("passthrough slot lock") = SlotState::Active(session);
    }

    pub(crate) fn release(&self) {
        *self.state.lock().expect("passthrough slot lock") = SlotState::Idle;
    }
}

impl Passthrough {
    fn push_tail(&mut self, data: &[u8]) {
        let Passthrough::Active { tail, .. } = self else {
            return;
        };
        tail.extend(data.iter().copied());
        while tail.len() > TAIL_BYTES {
            tail.pop_front();
        }
    }

    fn last_line(&self) -> Option<String> {
        let Passthrough::Active { tail, .. } = self else {
            return None;
        };
        let bytes: Vec<u8> = tail.iter().copied().collect();
        last_line_of(&bytes)
    }
}

/// Hand the window to a stoat on `host`, or refuse and say why.
///
/// The terminal state goes first. Every image, pool, minimap strip, and APC
/// component is dropped, and both emitters forget what they last sent. The
/// remote drives the same terminal from here, and nothing tells it what is
/// already on screen.
pub(crate) fn connect(stoat: &mut Stoat, host: &str, args: &[String]) -> UpdateEffect {
    if stoat.passthrough.is_some() {
        stoat.set_status("an ssh session is already active");
        return UpdateEffect::Redraw;
    }
    if stoat.passthrough_link.is_none() {
        stoat.set_status("ssh needs the terminal UI");
        return UpdateEffect::Redraw;
    }
    if stoat.active_workspace().panes.has_windowed_panes() || !stoat.aux_windows.is_empty() {
        stoat.set_status("reattach detached panes before :ssh");
        return UpdateEffect::Redraw;
    }

    if stoat.stoatty {
        crate::image_emit::emit_drop_all_images(stoat);
        stoat.images.forget();

        let mut out = Vec::new();
        stoat.smooth_scroll.drop_all(&mut out);
        for (_, content) in std::mem::take(&mut stoat.minimap_content) {
            encode_minimap_drop_into(
                &mut out,
                &MinimapDropCommand {
                    content_id: content.content_id(),
                },
            );
        }
        encode_reset_into(&mut out);
        if let Some(apc_tx) = stoat.apc_tx.clone() {
            let _ = apc_tx.send(out);
        }

        stoat.apc_scene.clear();
        stoat.apc_scene.forget_flushed();

        stoat.zoom_claimed = false;
        apc_emit::emit_zoom_capture(stoat, false);
        apc_emit::emit_reset_default_colors(stoat);
    }

    let program = stoat
        .settings
        .ssh_program
        .as_deref()
        .unwrap_or("stoat")
        .to_owned();
    stoat.passthrough = Some(Passthrough::Pending {
        host: host.to_owned(),
        program,
        args: args.to_vec(),
    });
    if let Some(link) = &stoat.passthrough_link {
        link.slot.arm();
    }
    UpdateEffect::None
}

/// Await the input thread's ack, or park forever when no link is installed.
///
/// A `None` link has no receiver to wait on, and an arm that returns
/// immediately spins the run loop.
pub(crate) async fn ack_recv(link: &mut Option<PassthroughLink>) -> Option<()> {
    match link {
        Some(link) => link.ack_rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Spawn the armed session now that fd 0 is no longer parsed as input.
pub(crate) fn spawn_armed(stoat: &mut Stoat) -> UpdateEffect {
    let (host, program, args) = match stoat.passthrough.take() {
        Some(Passthrough::Pending {
            host,
            program,
            args,
        }) => (host, program, args),
        other => {
            stoat.passthrough = other;
            return UpdateEffect::None;
        },
    };

    // `env` stays empty so the inherited TERM reaches the remote.
    let spawn_args = SpawnArgs {
        program: "ssh".to_owned(),
        args: ssh_argv(&host, &program, &args),
        env: Vec::new(),
        env_remove: Vec::new(),
        cwd: stoat.active_workspace().git_root.clone(),
        width: stoat.size.width,
        rows: stoat.size.height,
    };

    // The local terminal host opens the PTY synchronously, so the spawn future
    // is ready on first poll.
    let spawned = stoat.terminal_host.spawn(spawn_args).now_or_never();
    let session = match spawned {
        Some(Ok(session)) => session,
        Some(Err(err)) => {
            stoat.set_status(format!("ssh failed to start: {err}"));
            return abandon(stoat);
        },
        None => {
            stoat.set_status("ssh failed to start");
            return abandon(stoat);
        },
    };

    let session: Arc<dyn TerminalSession> = Arc::from(session);
    stoat.passthrough = Some(Passthrough::Active {
        tail: VecDeque::new(),
    });
    if let Some(link) = &stoat.passthrough_link {
        link.slot.engage(session.clone());
    }
    run::spawn_ssh_reader(&stoat.executor, session, stoat.pty_tx.clone());
    UpdateEffect::None
}

/// Give up on a spawn that never produced a session.
///
/// The forgotten emitter caches make the next frame re-declare everything, so a
/// failed spawn needs no undo beyond releasing the slot.
fn abandon(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(link) = &stoat.passthrough_link {
        link.slot.release();
    }
    stoat.passthrough = None;
    UpdateEffect::Redraw
}

/// Pass one chunk of remote output to the UI thread, keeping the tail.
pub(crate) fn forward_output(stoat: &mut Stoat, data: Vec<u8>) -> UpdateEffect {
    if let Some(passthrough) = &mut stoat.passthrough {
        passthrough.push_tail(&data);
    }
    if let Some(link) = &stoat.passthrough_link {
        let _ = link.ui_tx.send(UiControl::Raw(data));
    }
    UpdateEffect::None
}

/// Take the window back after the remote stoat exited.
pub(crate) fn finish(stoat: &mut Stoat, exit_status: Option<i32>) -> UpdateEffect {
    let ended = stoat.passthrough.take();
    if let Some(link) = &stoat.passthrough_link {
        link.slot.release();
        let _ = link.ui_tx.send(UiControl::Resume);
    }

    stoat.zoom_claimed = false;
    stoat.sync_zoom_claim();
    apc_emit::emit_theme_default_colors(stoat);

    if exit_status != Some(0) {
        let code = match exit_status {
            Some(code) => code.to_string(),
            None => "no status".to_owned(),
        };
        let message = match ended.as_ref().and_then(Passthrough::last_line) {
            Some(line) => format!("ssh exited ({code}): {line}"),
            None => format!("ssh exited ({code})"),
        };
        stoat.set_status(message);
    }
    UpdateEffect::Redraw
}

/// The argument list for the local `ssh` process.
///
/// `-e none` because ssh arms its `~` escape on a tty session, and the remote
/// editor must see that byte. `-t` because the remote stoat needs a PTY. The
/// remote program and its arguments join into one string because sshd hands the
/// command to the remote login shell whole.
pub(crate) fn ssh_argv(host: &str, program: &str, args: &[String]) -> Vec<String> {
    let mut remote = shell_quote(program);
    for arg in args {
        remote.push(' ');
        remote.push_str(&shell_quote(arg));
    }
    vec![
        "-e".to_owned(),
        "none".to_owned(),
        "-t".to_owned(),
        host.to_owned(),
        remote,
    ]
}

/// Quote `arg` for the remote login shell, leaving it bare when it needs no
/// quoting.
///
/// Bare matters beyond brevity. A quoted `~/proj` reaches the remote as a
/// literal tilde and the path never resolves, so the bare set is every byte a
/// shell passes through unchanged, tilde included.
fn shell_quote(arg: &str) -> String {
    const SAFE: &str = "_./~@:=+,-";
    if arg.is_empty() {
        return "''".to_owned();
    }
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || SAFE.contains(c))
    {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// The last non-empty line in `bytes`, for the exit report.
///
/// Lossy UTF-8 because the tail is cut at a fixed byte count and lands mid
/// codepoint as often as not, and a mangled character still reads better than
/// no error text at all.
fn last_line_of(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .split('\n')
        .map(|line| line.trim_end_matches('\r').trim())
        .rfind(|line| !line.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{last_line_of, shell_quote, ssh_argv};

    #[test]
    fn ssh_argv_joins_the_remote_command_and_quotes_only_what_needs_it() {
        assert_eq!(
            ssh_argv("foo", "stoat", &["~/proj".to_owned(), "a b".to_owned()]),
            vec!["-e", "none", "-t", "foo", "stoat ~/proj 'a b'"],
        );
    }

    #[test]
    fn shell_quote_keeps_a_tilde_bare_and_escapes_a_quote() {
        assert_eq!(shell_quote("~/proj"), "~/proj");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn last_line_reports_the_remote_error() {
        assert_eq!(
            last_line_of(b"x\r\nssh: connect refused\r\n"),
            Some("ssh: connect refused".to_owned()),
        );
        assert_eq!(last_line_of(b"\r\n \r\n"), None);
    }
}
