//! The two processes behind a detachable session.
//!
//! The server is a normal stoat whose fd 0 and fd 1 are a PTY slave it owns, so
//! every layer below it works unchanged against a terminal that never goes
//! away. A relay pumps the PTY master out to whichever client is attached, and
//! that client's frames back in.
//!
//! The client is a byte pipe between the real terminal and the socket. It owns
//! raw mode and reports the window size, and does nothing else.
//!
//! One command does both, so the same line starts a session and reattaches to
//! it. Last client wins: a new connection displaces the old one, which keeps a
//! forgotten client on a dead link from holding the session hostage.

use snafu::{whatever, ResultExt, Whatever};
use std::{
    ffi::OsString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use stoat::{
    attach::{self, Frame, FrameDecoder, REPLACED_EXIT, REPLACED_MESSAGE},
    host::{FsHost, LocalFs},
    tty,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// How long a freshly spawned server has to bind its socket.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the client retries the connect while waiting for that bind.
const START_POLL: Duration = Duration::from_millis(20);

/// How long the client's input thread waits on fd 0 before checking the window
/// size again.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Bytes moved per read in either direction.
const CHUNK: usize = 64 * 1024;

/// A frame tag plus the big-endian length that follows it, so a wire buffer
/// sized for one chunk never has to grow.
const FRAME_HEADER: usize = 5;

/// The running end of a detachable session, held by the process being attached
/// to.
///
/// Owns the PTY master the relay threads pump and the socket file they accept
/// on. [`Self::finish`] is what takes both down in the right order.
pub struct AttachServer {
    attached_rx: Option<UnboundedReceiver<()>>,
    master: Arc<File>,
    current: Arc<Mutex<Option<UnixStream>>>,
    path: PathBuf,
}

impl AttachServer {
    /// The channel the app listens on to hear that a client attached, taken
    /// once by the caller that installs it.
    pub fn attached_rx(&mut self) -> Option<UnboundedReceiver<()>> {
        self.attached_rx.take()
    }

    /// Send the last of the editor's output to the client, then close both the
    /// connection and the socket file.
    ///
    /// The drain matters. The UI thread writes its terminal-restore bytes on
    /// the way out, and they are still in the PTY when the editor's loop
    /// returns. A client closed before they cross is left in raw mode.
    pub fn finish(self) {
        let master_fd = self.master.as_raw_fd();
        // SAFETY: fcntl reads and sets this process's own descriptor flags.
        unsafe {
            let flags = libc::fcntl(master_fd, libc::F_GETFL);
            if flags != -1 {
                libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        let mut buf = [0u8; CHUNK];
        let mut wire = Vec::with_capacity(CHUNK + FRAME_HEADER);
        loop {
            // A `WouldBlock` error is the non-blocking read finding nothing
            // left, which ends the drain the way end of file does.
            let got = match (&*self.master).read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(got) => got,
            };
            wire.clear();
            attach::encode_bytes(&buf[..got], &mut wire);
            let mut held = self.current.lock().expect("attach client lock");
            let Some(stream) = held.as_mut() else {
                break;
            };
            if stream.write_all(&wire).is_err() {
                break;
            }
        }

        if let Some(stream) = self.current.lock().expect("attach client lock").take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let _ = LocalFs.remove_file(&self.path);
    }
}

/// Whether `s` names a session, for clap to reject a bad one at parse time.
pub fn parse_name(s: &str) -> Result<String, String> {
    if attach::valid_name(s) {
        return Ok(s.to_owned());
    }
    Err("a session name is 1 to 64 letters, digits, '-', or '_'".to_owned())
}

/// Attach to the session called `name`, starting it when nothing listens.
///
/// Exits the process with [`REPLACED_EXIT`] when another client took the
/// session over, since the shell that ran this wants to tell the two endings
/// apart.
pub fn attach_or_start(name: &str) -> Result<(), Whatever> {
    let path = attach::socket_path(name).whatever_context("resolve the attach socket path")?;

    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(err) => {
            // A crashed server leaves its socket file behind. Refusing to start
            // over one strands the name for good.
            if err.kind() == std::io::ErrorKind::ConnectionRefused {
                let _ = LocalFs.remove_file(&path);
            }
            spawn_server(name)?;
            wait_for_server(&path)?
        },
    };

    let code = run_client(stream)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Start the server for `name` as a session of its own.
///
/// It runs this same binary with this same argv, because rebuilding the
/// argument list drops every flag the user passed. `setsid` is what makes it
/// outlive the client. Without a session of its own it dies with the terminal
/// that started it, which is the one thing a detachable session must not do.
fn spawn_server(name: &str) -> Result<(), Whatever> {
    let exe = std::env::current_exe().whatever_context("locate this executable")?;
    let mut command = std::process::Command::new(exe);
    command
        .args(server_argv(std::env::args_os(), name))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // SAFETY: setsid is async-signal-safe and touches only this child's
    // session, which is what running between fork and exec requires.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    command
        .spawn()
        .whatever_context("start the attach server")?;
    Ok(())
}

/// This invocation's arguments with the attach request turned into a serve
/// request.
///
/// The program name and both spellings of `--attachable` go, and everything
/// else is kept, so the server starts on the same files, working directory, and
/// flags the user asked for.
fn server_argv(args: impl Iterator<Item = OsString>, name: &str) -> Vec<OsString> {
    let mut out = Vec::new();
    let mut drop_value = false;

    for arg in args.skip(1) {
        if drop_value {
            drop_value = false;
            continue;
        }
        if arg == "--attachable" {
            drop_value = true;
            continue;
        }
        if arg.to_string_lossy().starts_with("--attachable=") {
            continue;
        }
        out.push(arg);
    }

    out.push("--attach-serve".into());
    out.push(name.into());
    out
}

/// Connect to `path` until the server binds it.
fn wait_for_server(path: &std::path::Path) -> Result<UnixStream, Whatever> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => thread::sleep(START_POLL),
            Err(_) => whatever!("attach server did not start"),
        }
    }
}

/// Pipe this terminal to `stream` until one end goes away.
///
/// Returns the process's exit code, which is [`REPLACED_EXIT`] when the server
/// handed the session to another client.
fn run_client(stream: UnixStream) -> Result<i32, Whatever> {
    crossterm::terminal::enable_raw_mode().whatever_context("enter raw mode")?;

    let writer = stream
        .try_clone()
        .whatever_context("split the attach socket")?;
    thread::spawn(move || client_input(writer));

    let replaced = client_output(stream);

    let _ = crossterm::terminal::disable_raw_mode();
    if replaced {
        println!("{REPLACED_MESSAGE}");
    }
    Ok(client_exit_code(replaced))
}

/// What the client's process exits with.
fn client_exit_code(replaced: bool) -> i32 {
    if replaced {
        REPLACED_EXIT
    } else {
        0
    }
}

/// Send what the terminal produces, and its size whenever that changes.
///
/// The size is polled rather than caught from SIGWINCH because this thread
/// already wakes on a timeout. A signal handler must hand the news across
/// threads to reach the same place.
fn client_input(mut stream: UnixStream) {
    let mut buf = [0u8; CHUNK];
    let mut wire = Vec::with_capacity(CHUNK + FRAME_HEADER);
    let mut last: Option<libc::winsize> = None;

    loop {
        let size = tty::winsize();
        if let Some(ws) = size
            && last.is_none_or(|prev| !same_winsize(&prev, &ws))
        {
            last = Some(ws);
            wire.clear();
            attach::encode(
                &Frame::Winsize {
                    rows: ws.ws_row,
                    cols: ws.ws_col,
                    xpixel: ws.ws_xpixel,
                    ypixel: ws.ws_ypixel,
                },
                &mut wire,
            );
            if stream.write_all(&wire).is_err() {
                return;
            }
        }

        match tty::read_stdin(&mut buf, INPUT_POLL) {
            // The terminal went away, so this client has nothing left to send.
            Ok(0) if stdin_closed() => break,
            Ok(0) => {},
            Ok(n) => {
                wire.clear();
                attach::encode_bytes(&buf[..n], &mut wire);
                if stream.write_all(&wire).is_err() {
                    return;
                }
            },
            Err(_) => break,
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Write the server's output to this terminal until the socket ends.
///
/// Reports whether the server said another client took over.
fn client_output(mut stream: UnixStream) -> bool {
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; CHUNK];
    let mut replaced = false;

    while let Ok(got) = stream.read(&mut buf) {
        if got == 0 {
            break;
        }
        decoder.push(&buf[..got]);

        while let Some(frame) = decoder.next_frame() {
            match frame {
                Ok(Frame::Bytes(bytes)) => {
                    let mut stdout = std::io::stdout();
                    if stdout.write_all(&bytes).is_err() || stdout.flush().is_err() {
                        return replaced;
                    }
                },
                Ok(Frame::Replaced) => replaced = true,
                // A winsize from the server is not part of this direction, and
                // a decode error means the stream is desynchronized.
                Ok(Frame::Winsize { .. }) => {},
                Err(_) => return replaced,
            }
        }
    }
    replaced
}

/// Run this process's terminal on a PTY it owns, and relay it to clients.
///
/// fd 0 and fd 1 become the slave, so everything above them sees an ordinary
/// terminal that outlives every client. fd 2 is left alone, since the log
/// redirect already owns it.
pub fn serve(name: &str) -> Result<AttachServer, Whatever> {
    let path = attach::socket_path(name).whatever_context("resolve the attach socket path")?;

    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: openpty writes the two descriptors through the pointers and takes
    // no terminal settings, size, or name.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if opened != 0 {
        whatever!("open a pty for the attach server: {}", last_error());
    }
    // SAFETY: openpty just returned this descriptor and nothing else holds it,
    // so the `File` is its only owner and closes it once.
    let master = Arc::new(File::from(unsafe { OwnedFd::from_raw_fd(master) }));

    // SAFETY: each call names a descriptor this process just opened. The child
    // is a session leader with no controlling terminal, so TIOCSCTTY succeeds
    // and the editor's own process group owns the pty.
    unsafe {
        // macOS declares TIOCSCTTY narrower than the ioctl request type, so
        // the conversion is required there and a no-op on Linux.
        #[allow(clippy::useless_conversion)]
        libc::ioctl(slave, libc::TIOCSCTTY.into(), 0);
        libc::dup2(slave, 0);
        libc::dup2(slave, 1);
        if slave > 2 {
            libc::close(slave);
        }
    }

    let _ = LocalFs.remove_file(&path);
    if let Some(dir) = path.parent() {
        let _ = LocalFs.create_dir_all(dir);
    }
    let listener = UnixListener::bind(&path).whatever_context("bind the attach socket")?;

    let current: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
    let (attached_tx, attached_rx) = tokio::sync::mpsc::unbounded_channel();

    spawn_master_reader(master.clone(), current.clone());
    spawn_acceptor(listener, master.clone(), current.clone(), attached_tx);

    Ok(AttachServer {
        attached_rx: Some(attached_rx),
        master,
        current,
        path,
    })
}

/// Pump the editor's output to whichever client is attached.
///
/// Output produced while nobody is attached is dropped. The next client is told
/// to expect nothing, and the editor re-declares its whole terminal for it.
fn spawn_master_reader(master: Arc<File>, current: Arc<Mutex<Option<UnixStream>>>) {
    thread::Builder::new()
        .name("attach-out".to_owned())
        .spawn(move || {
            let mut buf = [0u8; CHUNK];
            let mut wire = Vec::with_capacity(CHUNK + FRAME_HEADER);
            loop {
                let got = match (&*master).read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(got) => got,
                };

                wire.clear();
                attach::encode_bytes(&buf[..got], &mut wire);

                let mut held = current.lock().expect("attach client lock");
                if let Some(stream) = held.as_mut()
                    && stream.write_all(&wire).is_err()
                {
                    *held = None;
                }
            }
        })
        .expect("spawn the attach output thread");
}

/// Take each connection as the session's one client, displacing the last.
fn spawn_acceptor(
    listener: UnixListener,
    master: Arc<File>,
    current: Arc<Mutex<Option<UnixStream>>>,
    attached_tx: UnboundedSender<()>,
) {
    thread::Builder::new()
        .name("attach-accept".to_owned())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let Ok(reader) = stream.try_clone() else {
                    continue;
                };

                {
                    let mut held = current.lock().expect("attach client lock");
                    if let Some(mut old) = held.take() {
                        let mut wire = Vec::new();
                        attach::encode(&Frame::Replaced, &mut wire);
                        let _ = old.write_all(&wire);
                        let _ = old.shutdown(std::net::Shutdown::Both);
                    }
                    *held = Some(stream);
                }

                spawn_client_reader(reader, master.clone(), current.clone(), attached_tx.clone());
            }
        })
        .expect("spawn the attach accept thread");
}

/// Feed one client's frames to the editor until that client goes.
fn spawn_client_reader(
    mut reader: UnixStream,
    master: Arc<File>,
    current: Arc<Mutex<Option<UnixStream>>>,
    attached_tx: UnboundedSender<()>,
) {
    thread::Builder::new()
        .name("attach-in".to_owned())
        .spawn(move || {
            let mut decoder = FrameDecoder::new();
            let mut buf = [0u8; CHUNK];
            let mut sized = false;

            while let Ok(got) = reader.read(&mut buf) {
                if got == 0 {
                    break;
                }
                decoder.push(&buf[..got]);

                while let Some(frame) = decoder.next_frame() {
                    match frame {
                        Ok(Frame::Bytes(bytes)) => {
                            let _ = (&*master).write_all(&bytes);
                        },
                        Ok(Frame::Winsize {
                            rows,
                            cols,
                            xpixel,
                            ypixel,
                        }) => {
                            set_winsize(&master, rows, cols, xpixel, ypixel);
                            // The first size of a connection is what says the
                            // grid this client brought, so the editor re-declares
                            // against it rather than the last client's.
                            if !sized {
                                sized = true;
                                let _ = attached_tx.send(());
                            }
                        },
                        Ok(Frame::Replaced) => {},
                        Err(_) => return,
                    }
                }
            }

            // Only when this is still the attached client. A displaced reader
            // reaching EOF must not clear the one that displaced it.
            let mut held = current.lock().expect("attach client lock");
            if held
                .as_ref()
                .is_some_and(|held| held.as_raw_fd() == reader.as_raw_fd())
            {
                *held = None;
            }
        })
        .expect("spawn the attach input thread");
}

/// Resize the pty, which signals the editor's process group in turn.
fn set_winsize(master: &File, rows: u16, cols: u16, xpixel: u16, ypixel: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: xpixel,
        ws_ypixel: ypixel,
    };
    // SAFETY: the ioctl reads one winsize through the pointer, which borrows a
    // live local for the call.
    unsafe {
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &raw const ws);
    }
}

fn same_winsize(a: &libc::winsize, b: &libc::winsize) -> bool {
    a.ws_row == b.ws_row
        && a.ws_col == b.ws_col
        && a.ws_xpixel == b.ws_xpixel
        && a.ws_ypixel == b.ws_ypixel
}

/// Whether fd 0 is at end of file. A read of zero alone leaves that
/// indistinguishable from a quiet wait.
fn stdin_closed() -> bool {
    let mut fds = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll reads and writes the one pollfd through the pointer and
    // touches nothing else. The struct is initialized above.
    let ready = unsafe { libc::poll(&raw mut fds, 1, 0) };
    ready > 0 && fds.revents & libc::POLLHUP != 0
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

#[cfg(test)]
mod tests {
    use super::{client_exit_code, server_argv};
    use std::ffi::OsString;
    use stoat::attach::REPLACED_EXIT;

    fn argv(args: &[&str]) -> Vec<String> {
        server_argv(args.iter().map(OsString::from), "box")
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The server runs on what the user asked for, so only the attach request
    /// itself is rewritten.
    #[test]
    fn server_argv_swaps_the_attach_request_and_keeps_the_rest() {
        assert_eq!(
            argv(&["stoat", "--attachable", "box", "-d", "/proj", "a.rs"]),
            ["-d", "/proj", "a.rs", "--attach-serve", "box"],
            "the separated spelling and its value both go",
        );
        assert_eq!(
            argv(&["stoat", "--attachable=box", "--continue"]),
            ["--continue", "--attach-serve", "box"],
            "and so does the joined one",
        );
        assert_eq!(
            argv(&["stoat"]),
            ["--attach-serve", "box"],
            "a bare launch serves a bare session",
        );
    }

    /// The shell that ran the client separates a replaced session from an
    /// editor that exited, since only the first leaves the session running.
    #[test]
    fn a_replaced_client_exits_apart_from_a_finished_one() {
        assert_eq!(client_exit_code(true), REPLACED_EXIT);
        assert_eq!(client_exit_code(false), 0);
    }
}
