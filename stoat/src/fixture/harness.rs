//! Headless driver for a real [`Stoat`] instance, no terminal.
//!
//! Integration tests need to exercise the whole app the way a person would
//! (open a review, place the cursor, trigger hover) but without a tty. This
//! harness constructs a production [`Stoat`] with real hosts on a current-thread
//! Tokio runtime and drives it purely over the same event and render channels
//! the terminal front-end uses. Input goes in as [`Event`]s and rendered frames
//! come back as [`RenderFrame`]s, so the harness observes exactly what the app
//! paints.
//!
//! The session agent socket is also bound, so a script can query live LSP
//! status, diagnostics, and hover over the same protocol the `stoat query` CLI
//! speaks.

use crate::{
    host::LocalFsWatcher,
    input_parse::{self, InputParseError},
    run,
    ui::RenderFrame,
    Settings, Stoat,
};
use crossterm::event::Event;
use ratatui::buffer::Buffer;
use serde::Serialize;
use serde_json::Value;
use snafu::{OptionExt, ResultExt, Snafu};
use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use stoat_scheduler::TokioScheduler;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    runtime::{Builder, Runtime},
    sync::{mpsc, watch, Notify},
    time::{self, Instant},
};

const DEFAULT_COLS: u16 = 80;

const DEFAULT_ROWS: u16 = 24;

const EVENT_CHANNEL_CAP: usize = 64;

/// Gap between driven keys, matching the terminal front-end's inter-key pacing
/// so each keystroke's effects settle before the next arrives.
const INTER_KEY_DELAY: Duration = Duration::from_millis(20);

/// How long [`Handle::query`] waits for the session socket to appear. The socket
/// is bound by a task the run loop spawns, so the first query races that bind.
const QUERY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

const QUERY_RETRY_DELAY: Duration = Duration::from_millis(20);

/// Failure driving a [`LiveHarness`].
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum HarnessError {
    #[snafu(display("failed to build the harness tokio runtime"))]
    RuntimeBuild {
        source: io::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("failed to set up the session agent socket"))]
    SessionSocket {
        source: io::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("failed to parse the input sequence: {source}"))]
    ParseInput {
        source: InputParseError,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("the harness event channel is closed"))]
    SendEvent {
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("timed out after {timeout:?} waiting for a matching frame"))]
    FrameTimeout {
        timeout: Duration,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("the render channel closed before a matching frame"))]
    RenderClosed {
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("agent query socket I/O failed"))]
    QuerySocket {
        source: io::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("failed to encode or decode a query message: {source}"))]
    Json {
        source: serde_json::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

/// A structured session query, serialized to the newline-delimited JSON the
/// session agent socket accepts. Mirrors the `stoat query` wire form; `line`
/// and `col` are zero-based LSP UTF-16 positions.
#[derive(Serialize)]
#[serde(tag = "req", rename_all = "kebab-case")]
pub enum Query {
    LspStatus,
    Diagnostics {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    Hover {
        path: PathBuf,
        line: u32,
        col: u32,
    },
}

/// A real [`Stoat`] wired to drive over channels instead of a terminal.
///
/// Build one with [`Self::open`], then call [`Self::run`] with a script that
/// receives a [`Handle`] to send keys, await frames, and query the session.
pub struct LiveHarness {
    event_tx: mpsc::Sender<Event>,
    event_rx: Option<mpsc::Receiver<Event>>,
    render_tx: Option<watch::Sender<Option<RenderFrame>>>,
    render_rx: watch::Receiver<Option<RenderFrame>>,
    shutdown: Arc<Notify>,
    socket_path: PathBuf,
    stoat: Stoat,
    _scheduler: Arc<TokioScheduler>,
    rt: Runtime,
}

impl LiveHarness {
    /// Construct a headless [`Stoat`] rooted at `root` with production hosts.
    ///
    /// LSP auto-spawn is enabled and the session agent socket is bound, so a
    /// script can drive real language-server interactions. The filesystem
    /// watcher is best-effort. A failure to install it is logged rather than
    /// fatal, so the harness still runs where the platform watcher is
    /// unavailable.
    pub fn open(root: &Path, settings: Settings) -> Result<Self, HarnessError> {
        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .context(RuntimeBuildSnafu)?;
        let scheduler = Arc::new(TokioScheduler::new(rt.handle().clone()));

        let mut stoat = Stoat::new(scheduler.executor(), settings, root.to_path_buf());
        stoat.set_lsp_auto_spawn(true);
        stoat.set_diff_warm_auto(true);
        match LocalFsWatcher::new() {
            Ok(watcher) => stoat.set_fs_watch_host(Arc::new(watcher)),
            Err(err) => tracing::warn!(
                target: "stoat::fixture",
                %err,
                "LocalFsWatcher init failed; harness runs without fs watching",
            ),
        }

        let uid = stoat.active_workspace().uid();
        {
            // Enter the runtime so the socket-serving task the call spawns has a
            // reactor to bind on, regardless of how the executor schedules.
            let _guard = rt.enter();
            stoat.serve_term_session(uid).context(SessionSocketSnafu)?;
        }
        let socket_path = run::agent_socket_path(uid).context(SessionSocketSnafu)?;

        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let (render_tx, render_rx) = watch::channel(None);
        let shutdown = stoat.shutdown_handle();

        Ok(Self {
            event_tx,
            event_rx: Some(event_rx),
            render_tx: Some(render_tx),
            render_rx,
            shutdown,
            socket_path,
            stoat,
            _scheduler: scheduler,
            rt,
        })
    }

    /// Run the event loop and `script` concurrently to completion, returning the
    /// script's output.
    ///
    /// An initial resize is delivered before the script so frames begin to flow.
    /// When the script future finishes, the loop is shut down, so a script need
    /// not call [`Handle::shutdown`] itself. Panics if the event loop returns an
    /// error, or if called more than once.
    pub fn run<F, Fut, T>(&mut self, script: F) -> T
    where
        F: FnOnce(Handle) -> Fut,
        Fut: Future<Output = T>,
    {
        let event_rx = self
            .event_rx
            .take()
            .expect("LiveHarness::run may be called only once");
        let render_tx = self
            .render_tx
            .take()
            .expect("LiveHarness::run may be called only once");
        let handle = Handle {
            event_tx: self.event_tx.clone(),
            render_rx: self.render_rx.clone(),
            shutdown: self.shutdown.clone(),
            socket_path: self.socket_path.clone(),
        };
        let auto_shutdown = self.shutdown.clone();

        let stoat = &mut self.stoat;
        let rt = &self.rt;
        rt.block_on(async move {
            let driver = async move {
                handle
                    .send_event(Event::Resize(DEFAULT_COLS, DEFAULT_ROWS))
                    .await
                    .ok();
                let output = script(handle).await;
                auto_shutdown.notify_one();
                output
            };
            let (run_result, output) = tokio::join!(stoat.run(event_rx, render_tx), driver);
            run_result.expect("stoat event loop returned an error");
            output
        })
    }
}

/// Driver-side handle passed to a [`LiveHarness::run`] script.
pub struct Handle {
    event_tx: mpsc::Sender<Event>,
    render_rx: watch::Receiver<Option<RenderFrame>>,
    shutdown: Arc<Notify>,
    socket_path: PathBuf,
}

impl Handle {
    /// Parse `keys` in the Helix/vim-style grammar and feed them as key events,
    /// pausing [`INTER_KEY_DELAY`] between keys so effects settle.
    pub async fn send_keys(&self, keys: &str) -> Result<(), HarnessError> {
        let parsed = input_parse::parse_input_sequence(keys).context(ParseInputSnafu)?;
        for key in parsed {
            self.send_event(Event::Key(key)).await?;
            time::sleep(INTER_KEY_DELAY).await;
        }
        Ok(())
    }

    /// Wait until a rendered frame's text satisfies `predicate`, returning that
    /// text. The frame text is the trimmed grid content, matching the terminal
    /// snapshot form. Fails on `timeout` or if the app stops rendering.
    pub async fn await_frame<P>(
        &mut self,
        predicate: P,
        timeout: Duration,
    ) -> Result<String, HarnessError>
    where
        P: Fn(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(text) = self.current_frame_text()
                && predicate(&text)
            {
                return Ok(text);
            }
            match time::timeout_at(deadline, self.render_rx.changed()).await {
                Ok(Ok(())) => {},
                Ok(Err(_)) => return RenderClosedSnafu.fail(),
                Err(_) => return FrameTimeoutSnafu { timeout }.fail(),
            }
        }
    }

    /// Send `request` over the session agent socket and return the JSON reply.
    ///
    /// Retries connecting for [`QUERY_CONNECT_TIMEOUT`] because the socket is
    /// bound by a task the run loop spawns, so an early query races that bind.
    pub async fn query(&self, request: &Query) -> Result<Value, HarnessError> {
        let line = serde_json::to_string(request).context(JsonSnafu)?;

        let mut stream = self.connect().await?;
        stream
            .write_all(line.as_bytes())
            .await
            .context(QuerySocketSnafu)?;
        stream.write_all(b"\n").await.context(QuerySocketSnafu)?;

        let mut reply = String::new();
        BufReader::new(stream)
            .read_line(&mut reply)
            .await
            .context(QuerySocketSnafu)?;
        serde_json::from_str(&reply).context(JsonSnafu)
    }

    /// Ask the event loop to quit at its next turn.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    async fn send_event(&self, event: Event) -> Result<(), HarnessError> {
        self.event_tx.send(event).await.ok().context(SendEventSnafu)
    }

    fn current_frame_text(&mut self) -> Option<String> {
        let frame = self.render_rx.borrow_and_update();
        frame.as_ref().map(|f| frame_to_text(&f.buffer))
    }

    async fn connect(&self) -> Result<UnixStream, HarnessError> {
        let deadline = Instant::now() + QUERY_CONNECT_TIMEOUT;
        loop {
            match UnixStream::connect(&self.socket_path).await {
                Ok(stream) => return Ok(stream),
                Err(err) => {
                    if Instant::now() >= deadline {
                        return Err(err).context(QuerySocketSnafu);
                    }
                    time::sleep(QUERY_RETRY_DELAY).await;
                },
            }
        }
    }
}

/// Flatten a rendered grid to trimmed text, matching the snapshot form: each
/// row is the concatenated cell symbols with trailing blanks trimmed, and
/// trailing blank rows are dropped.
fn frame_to_text(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut lines: Vec<String> = Vec::with_capacity(area.height as usize);
    for y in area.y..area.y + area.height {
        let mut line = String::with_capacity(area.width as usize);
        for x in area.x..area.x + area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        lines.push(line.trim_end().to_string());
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::materialize;

    #[test]
    fn renders_a_frame_over_the_channels() {
        let dir = tempfile::tempdir().unwrap();
        materialize("basic-diff", dir.path()).unwrap();

        let mut harness = LiveHarness::open(dir.path(), Settings::default()).unwrap();
        harness.run(|mut handle| async move {
            handle
                .await_frame(|_| true, Duration::from_secs(5))
                .await
                .expect("the harness should render at least one frame");
        });
    }
}
