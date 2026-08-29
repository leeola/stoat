//! Hands the whole window to a plain `stoat` on a remote host, over ssh or
//! over mosh.
//!
//! One session is on screen at a time. While the remote runs, the local stoat
//! is a byte pipe. The input thread reads fd 0 raw and writes it to the remote
//! PTY, and the PTY's output reaches stdout untouched. Under ssh the remote
//! stoat therefore handshakes with the local stoatty directly, since its
//! `hello` reaches stoatty and the ident reply rides fd 0 back through the
//! pipe. Under mosh it does not: mosh-server's emulator drops APC, so the
//! remote runs plain and its handshake resolves on the DSR probe.
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
use serde::{Deserialize, Serialize};
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

/// Which program carries the remote session.
///
/// The two differ in how the remote command is spelled and in how the escape
/// key is disabled, not in how the window is handed over. One passthrough
/// therefore serves both, and this is the only thing it branches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Transport {
    Ssh,
    Mosh,
}

/// The remote host a workspace's window was last handed to and did not close
/// cleanly.
///
/// A reopen of that workspace reconnects to this, so it holds everything
/// [`connect`] needs to run again: which program carries the session, where it
/// goes, and what the remote editor was told to open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteTarget {
    pub transport: Transport,
    pub host: String,
    pub args: Vec<String>,
}

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
    /// A newly attached terminal needs identifying, and only the thread that
    /// owns fd 0 runs the probe. It reverts to [`Self::Idle`] once the answer
    /// is on its way to the app.
    Handshake,
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
        transport: Transport,
        host: String,
        program: String,
        args: Vec<String>,
    },
    /// The remote owns the screen. The session itself lives in the slot and in
    /// the reader task, which is what keeps the PTY open, so only the output
    /// tail belongs here.
    Active {
        transport: Transport,
        tail: VecDeque<u8>,
    },
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

    /// Ask the input thread to identify the terminal on fd 0 again.
    pub(crate) fn rehandshake(&self) {
        *self.state.lock().expect("passthrough slot lock") = SlotState::Handshake;
    }
}

impl Transport {
    /// The local program, which is also the word every status message uses for
    /// the session.
    fn name(self) -> &'static str {
        match self {
            Transport::Ssh => "ssh",
            Transport::Mosh => "mosh",
        }
    }
}

impl Passthrough {
    fn transport(&self) -> Transport {
        match self {
            Passthrough::Pending { transport, .. } | Passthrough::Active { transport, .. } => {
                *transport
            },
        }
    }

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

/// Drop everything this session declared to the terminal and forget it was
/// declared.
///
/// Every image, pool, minimap strip, and APC component goes, both emitters
/// forget what they last sent, and the zoom claim is released. The next frame
/// therefore re-declares from scratch.
///
/// Two things need that. A handoff gives the terminal to a remote that knows
/// nothing of what is on it, and an attach brings a terminal that holds nothing
/// this session sent. Every drop is a no-op against a terminal that was never
/// declared to, so an attach calls this without checking.
pub(crate) fn retire_terminal_state(stoat: &mut Stoat) {
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

/// Hand the window to a stoat on `host`, or refuse and say why.
///
/// A `None` host reconnects to the workspace's stored target, and refuses when
/// it has none. `transport` is the caller's regardless, so a reconnect reaches
/// the same remote over whichever link was asked for.
///
/// The terminal state goes first. Every image, pool, minimap strip, and APC
/// component is dropped, and both emitters forget what they last sent. The
/// remote drives the same terminal from here, and nothing tells it what is
/// already on screen.
pub(crate) fn connect(
    stoat: &mut Stoat,
    transport: Transport,
    host: Option<&str>,
    args: &[String],
) -> UpdateEffect {
    // No host reconnects to where this workspace last went. The typed command
    // still picks the link, so a `:mosh` after an `:ssh` reaches the same
    // remote over the other one.
    let (host, args) = match host {
        Some(host) => (host.to_owned(), args.to_vec()),
        None => match stoat.active_workspace().remote.clone() {
            Some(target) => (target.host, target.args),
            None => {
                stoat.set_status("no remote to reconnect to");
                return UpdateEffect::Redraw;
            },
        },
    };

    if stoat.passthrough.is_some() {
        stoat.set_status("a remote session is already active");
        return UpdateEffect::Redraw;
    }
    if stoat.passthrough_link.is_none() {
        stoat.set_status(format!("{} needs the terminal UI", transport.name()));
        return UpdateEffect::Redraw;
    }
    if stoat.active_workspace().panes.has_windowed_panes() || !stoat.aux_windows.is_empty() {
        stoat.set_status(format!(
            "reattach detached panes before :{}",
            transport.name()
        ));
        return UpdateEffect::Redraw;
    }

    if stoat.stoatty {
        retire_terminal_state(stoat);
    }

    let program = stoat
        .settings
        .ssh_program
        .as_deref()
        .unwrap_or("stoat")
        .to_owned();
    let target = RemoteTarget {
        transport,
        host,
        args,
    };
    stoat.passthrough = Some(Passthrough::Pending {
        transport,
        host: target.host.clone(),
        program,
        args: target.args.clone(),
    });

    // Recorded and saved before the remote starts, so a crash while it owns the
    // screen still leaves the workspace something to reconnect to.
    stoat.active_workspace_mut().remote = Some(target);
    stoat.save_workspace(stoat.active_workspace);

    if let Some(link) = &stoat.passthrough_link {
        link.slot.arm();
    }
    UpdateEffect::None
}

/// Start over against a terminal that just attached.
///
/// A client that attaches brings a terminal holding nothing this session sent,
/// and it is a different stoatty, or none at all. So everything declared is
/// retired, the identity resets to what a fresh session starts with, the UI
/// thread re-enters the alternate screen, and the input thread identifies the
/// new terminal.
///
/// No effect while a remote owns the screen. That session's own client is what
/// was replaced, and this process is a byte pipe with nothing declared.
///
/// Returns [`UpdateEffect::None`] because the frame that repaints comes from the
/// input thread's size report after the handshake, against the new terminal's
/// grid rather than the old one's.
pub(crate) fn terminal_replaced(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.passthrough.is_some() {
        return UpdateEffect::None;
    }

    retire_terminal_state(stoat);
    stoat.stoatty = false;
    stoat.stoatty_protocol = 0;
    stoat.terminal_reported = false;

    if let Some(link) = &stoat.passthrough_link {
        let _ = link.ui_tx.send(UiControl::Resume);
        link.slot.rehandshake();
    }
    UpdateEffect::None
}

/// Hand the window back to the active workspace's stored target, once both the
/// workspace and the terminal are ready.
///
/// Three points complete that pair, and each one calls here: the restore and
/// the switch that arm a workspace, and the terminal's report of the startup
/// handshake. Whichever lands last is the one that connects, and the rest
/// return without doing anything.
///
/// The terminal report is the gate because a handoff before it races the
/// report's own zoom claim and theme colors onto the remote's screen.
pub(crate) fn reconnect_when_ready(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat.terminal_reported || !stoat.remote_pending {
        return UpdateEffect::None;
    }
    stoat.remote_pending = false;

    let Some(target) = stoat.active_workspace().remote.clone() else {
        return UpdateEffect::None;
    };
    connect(stoat, target.transport, Some(&target.host), &target.args)
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
    let (transport, host, program, args) = match stoat.passthrough.take() {
        Some(Passthrough::Pending {
            transport,
            host,
            program,
            args,
        }) => (transport, host, program, args),
        other => {
            stoat.passthrough = other;
            return UpdateEffect::None;
        },
    };

    // Nothing is unset, so the inherited TERM reaches the remote. An empty
    // MOSH_ESCAPE_KEY disables mosh's Ctrl-^ the way `-e none` disables ssh's
    // `~`, and the remote editor must see both bytes.
    let spawn_args = SpawnArgs {
        program: transport.name().to_owned(),
        args: match transport {
            Transport::Ssh => ssh_argv(&host, &program, &args),
            Transport::Mosh => mosh_argv(
                &host,
                &program,
                &args,
                stoat.settings.mosh_server.as_deref(),
            ),
        },
        env: match transport {
            Transport::Ssh => Vec::new(),
            Transport::Mosh => vec![("MOSH_ESCAPE_KEY".to_owned(), String::new())],
        },
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
            stoat.set_status(format!("{} failed to start: {err}", transport.name()));
            return abandon(stoat);
        },
        None => {
            stoat.set_status(format!("{} failed to start", transport.name()));
            return abandon(stoat);
        },
    };

    let session: Arc<dyn TerminalSession> = Arc::from(session);
    stoat.passthrough = Some(Passthrough::Active {
        transport,
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

    // A clean exit is the user closing the remote on purpose. Every other
    // ending is a dropped link, and the target stays for the next reopen.
    if exit_status == Some(0) {
        stoat.active_workspace_mut().remote = None;
        stoat.save_workspace(stoat.active_workspace);
    } else {
        let code = match exit_status {
            Some(code) => code.to_string(),
            None => "no status".to_owned(),
        };
        let name = ended.as_ref().map_or("remote", |p| p.transport().name());
        let message = match ended.as_ref().and_then(Passthrough::last_line) {
            Some(line) => format!("{name} exited ({code}): {line}"),
            None => format!("{name} exited ({code})"),
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

/// The argument list for the local `mosh` process.
///
/// `--server` names the remote `mosh-server` binary, which mosh starts through
/// a login shell whose PATH often misses it. `--` comes before the host because
/// the `mosh` script parses its options with permute on. Without it, a remote
/// argument that starts with `-` reads as a mosh option.
///
/// The program and its arguments stay separate and unquoted. The script
/// single-quotes every server argument itself and mosh-server execs the program
/// directly, so a `~` reaches the remote literally and never resolves.
pub(crate) fn mosh_argv(
    host: &str,
    program: &str,
    args: &[String],
    server: Option<&str>,
) -> Vec<String> {
    let mut argv = Vec::with_capacity(args.len() + 4);
    if let Some(server) = server {
        argv.push(format!("--server={server}"));
    }
    argv.push("--".to_owned());
    argv.push(host.to_owned());
    argv.push(program.to_owned());
    argv.extend(args.iter().cloned());
    argv
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
    use super::{last_line_of, mosh_argv, shell_quote, ssh_argv};

    #[test]
    fn ssh_argv_joins_the_remote_command_and_quotes_only_what_needs_it() {
        assert_eq!(
            ssh_argv("foo", "stoat", &["~/proj".to_owned(), "a b".to_owned()]),
            vec!["-e", "none", "-t", "foo", "stoat ~/proj 'a b'"],
        );
    }

    #[test]
    fn mosh_argv_keeps_the_remote_command_in_separate_unquoted_entries() {
        assert_eq!(
            mosh_argv(
                "box",
                "/opt/stoat",
                &["~/proj".to_owned(), "a b".to_owned()],
                Some("/opt/mosh-server"),
            ),
            vec![
                "--server=/opt/mosh-server",
                "--",
                "box",
                "/opt/stoat",
                "~/proj",
                "a b",
            ],
        );
        assert_eq!(
            mosh_argv("box", "stoat", &[], None),
            vec!["--", "box", "stoat"],
            "an unset server leaves mosh its own lookup",
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
