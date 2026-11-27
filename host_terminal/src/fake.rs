use crate::{SpawnArgs, TerminalHost, TerminalSession};
use async_trait::async_trait;
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub struct FakeTerminalSession {
    state: Mutex<FakeTerminalState>,
    read_tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    read_rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
}

struct FakeTerminalState {
    sent: Vec<Vec<u8>>,
    killed: bool,
    exit_code: Option<i32>,
    size: Option<(u16, u16)>,
    foreground_name: Option<String>,
}

impl Default for FakeTerminalSession {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTerminalSession {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            state: Mutex::new(FakeTerminalState {
                sent: Vec::new(),
                killed: false,
                exit_code: None,
                size: None,
                foreground_name: None,
            }),
            read_tx: Mutex::new(Some(tx)),
            read_rx: tokio::sync::Mutex::new(rx),
        }
    }

    pub fn push_output(&self, data: &[u8]) {
        if let Some(tx) = self
            .read_tx
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .as_ref()
        {
            let _ = tx.try_send(data.to_vec());
        }
    }

    /// Set the name returned by [`TerminalSession::foreground_process_name`].
    pub fn set_foreground_name(&self, name: impl Into<String>) {
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .foreground_name = Some(name.into());
    }

    /// Signal that the command finished with `exit_code`. Closes the read
    /// channel so a pending `read_chunk` returns `None`, and records the
    /// code for `try_wait`.
    pub fn finish(&self, exit_code: i32) {
        self.read_tx
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .take();
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .exit_code = Some(exit_code);
    }

    pub fn sent_bytes(&self) -> Vec<Vec<u8>> {
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .sent
            .clone()
    }

    pub fn sent_strings(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .sent
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect()
    }

    pub fn assert_sent(&self, index: usize, expected: &[u8]) {
        let state = self
            .state
            .lock()
            .expect("FakeTerminalSession lock poisoned");
        assert_eq!(
            state.sent.get(index).map(Vec::as_slice),
            Some(expected),
            "sent[{index}] mismatch"
        );
    }

    pub fn assert_sent_count(&self, count: usize) {
        let state = self
            .state
            .lock()
            .expect("FakeTerminalSession lock poisoned");
        assert_eq!(state.sent.len(), count, "send count mismatch");
    }

    pub fn was_killed(&self) -> bool {
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .killed
    }

    /// The last `(rows, cols)` passed to [`TerminalSession::resize`], or
    /// `None` if it has not been resized.
    pub fn last_size(&self) -> Option<(u16, u16)> {
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .size
    }
}

#[async_trait]
impl TerminalSession for FakeTerminalSession {
    async fn write(&self, data: &[u8]) -> io::Result<()> {
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .sent
            .push(data.to_vec());
        Ok(())
    }

    async fn read_chunk(&self) -> io::Result<Option<Vec<u8>>> {
        Ok(self.read_rx.lock().await.recv().await)
    }

    async fn kill(&self) -> io::Result<()> {
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .killed = true;
        Ok(())
    }

    async fn try_wait(&self) -> io::Result<Option<i32>> {
        Ok(self
            .state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .exit_code)
    }

    fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .size = Some((rows, cols));
        Ok(())
    }

    fn foreground_process_name(&self) -> Option<String> {
        self.state
            .lock()
            .expect("FakeTerminalSession lock poisoned")
            .foreground_name
            .clone()
    }
}

/// Factory fake that hands out boxes wrapping an `Arc<FakeTerminalSession>`,
/// so a test holding the `Arc` and the box that [`TerminalHost::spawn`]
/// returns observe the same underlying state.
///
/// A spawn takes its session from one of two sources. A session that
/// [`Self::push_session`] queued serves the next successful spawn, one spawn
/// each, in the order queued. With the queue empty, every spawn shares the
/// session of [`Self::new`]. A test that drives two processes apart queues a
/// session for each.
pub struct FakeTerminalHost {
    session: Arc<FakeTerminalSession>,
    spawns: Mutex<Vec<SpawnArgs>>,
    spawn_error: Mutex<Option<io::ErrorKind>>,
    /// The sessions that the next successful spawns return, front first.
    queued: Mutex<VecDeque<Arc<FakeTerminalSession>>>,
    /// The session of each successful spawn, in spawn order.
    handed: Mutex<Vec<Arc<FakeTerminalSession>>>,
}

impl FakeTerminalHost {
    pub fn new(session: Arc<FakeTerminalSession>) -> Self {
        Self {
            session,
            spawns: Mutex::new(Vec::new()),
            spawn_error: Mutex::new(None),
            queued: Mutex::new(VecDeque::new()),
            handed: Mutex::new(Vec::new()),
        }
    }

    /// The [`SpawnArgs`] of every [`TerminalHost::spawn`] call so far, in order,
    /// so a test can assert what program and arguments a spawn resolved to.
    pub fn spawns(&self) -> Vec<SpawnArgs> {
        self.spawns
            .lock()
            .expect("FakeTerminalHost lock poisoned")
            .clone()
    }

    /// Make the next [`TerminalHost::spawn`] fail with `kind`, as a missing
    /// program does. The failed spawn still shows in [`Self::spawns`].
    pub fn fail_next_spawn(&self, kind: io::ErrorKind) {
        *self
            .spawn_error
            .lock()
            .expect("FakeTerminalHost lock poisoned") = Some(kind);
    }

    /// Queue `session` for a later [`TerminalHost::spawn`] to return.
    ///
    /// The queue serves spawns in the order queued, one spawn each. A spawn
    /// that fails takes no session from it.
    pub fn push_session(&self, session: Arc<FakeTerminalSession>) {
        self.queued
            .lock()
            .expect("FakeTerminalHost lock poisoned")
            .push_back(session);
    }

    /// Take back the newest queued session, or `None` when the queue is
    /// empty.
    pub fn unqueue(&self) -> Option<Arc<FakeTerminalSession>> {
        self.queued
            .lock()
            .expect("FakeTerminalHost lock poisoned")
            .pop_back()
    }

    /// The session of each successful [`TerminalHost::spawn`] so far, in
    /// spawn order.
    pub fn sessions(&self) -> Vec<Arc<FakeTerminalSession>> {
        self.handed
            .lock()
            .expect("FakeTerminalHost lock poisoned")
            .clone()
    }
}

#[async_trait]
impl TerminalHost for FakeTerminalHost {
    async fn spawn(&self, args: SpawnArgs) -> io::Result<Box<dyn TerminalSession>> {
        self.spawns
            .lock()
            .expect("FakeTerminalHost lock poisoned")
            .push(args);
        if let Some(kind) = self
            .spawn_error
            .lock()
            .expect("FakeTerminalHost lock poisoned")
            .take()
        {
            return Err(io::Error::from(kind));
        }

        let session = self
            .queued
            .lock()
            .expect("FakeTerminalHost lock poisoned")
            .pop_front()
            .unwrap_or_else(|| self.session.clone());
        self.handed
            .lock()
            .expect("FakeTerminalHost lock poisoned")
            .push(session.clone());
        Ok(Box::new(ArcTerminalSession(session)))
    }
}

/// Trait-object bridge so [`FakeTerminalHost::spawn`] can hand out a
/// `Box<dyn TerminalSession>` while a test retains its own
/// `Arc<FakeTerminalSession>`. Both paths target the same state.
pub(crate) struct ArcTerminalSession(pub(crate) Arc<FakeTerminalSession>);

#[async_trait]
impl TerminalSession for ArcTerminalSession {
    async fn write(&self, data: &[u8]) -> io::Result<()> {
        self.0.write(data).await
    }

    async fn read_chunk(&self) -> io::Result<Option<Vec<u8>>> {
        self.0.read_chunk().await
    }

    async fn kill(&self) -> io::Result<()> {
        self.0.kill().await
    }

    async fn try_wait(&self) -> io::Result<Option<i32>> {
        self.0.try_wait().await
    }

    fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        self.0.resize(rows, cols)
    }

    fn foreground_process_name(&self) -> Option<String> {
        self.0.foreground_process_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn captures_writes() {
        rt().block_on(async {
            let fake = FakeTerminalSession::new();
            fake.write(b"hello").await.unwrap();
            fake.write(b"world").await.unwrap();
            assert_eq!(fake.sent_strings(), ["hello", "world"]);
            fake.assert_sent_count(2);
            fake.assert_sent(0, b"hello");
        });
    }

    #[test]
    fn tracks_kill() {
        rt().block_on(async {
            let fake = FakeTerminalSession::new();
            assert!(!fake.was_killed());
            fake.kill().await.unwrap();
            assert!(fake.was_killed());
        });
    }

    #[test]
    fn read_chunk_returns_pushed_data() {
        rt().block_on(async {
            let fake = FakeTerminalSession::new();
            fake.push_output(b"hello");
            let chunk = fake.read_chunk().await.unwrap();
            assert_eq!(chunk.as_deref(), Some(b"hello".as_slice()));
        });
    }

    #[test]
    fn records_resize_and_foreground_name() {
        let fake = FakeTerminalSession::new();
        fake.resize(30, 100).unwrap();
        assert_eq!(fake.last_size(), Some((30, 100)));
        fake.set_foreground_name("claude");
        assert_eq!(fake.foreground_process_name(), Some("claude".to_string()));
    }

    #[test]
    fn factory_spawn_shares_session_state() {
        rt().block_on(async {
            let session = Arc::new(FakeTerminalSession::new());
            let host = FakeTerminalHost::new(session.clone());
            let spawned = host
                .spawn(SpawnArgs {
                    program: "claude".into(),
                    args: vec![],
                    env: vec![],
                    env_remove: vec![],
                    cwd: "/tmp".into(),
                    width: 80,
                    rows: 24,
                })
                .await
                .unwrap();

            spawned.write(b"hi").await.unwrap();
            assert_eq!(session.sent_strings(), ["hi"]);

            session.finish(7);
            assert_eq!(spawned.try_wait().await.unwrap(), Some(7));
            assert_eq!(spawned.read_chunk().await.unwrap(), None);
        });
    }

    /// The arguments of a spawn of `sh`, which no test here reads.
    fn args() -> SpawnArgs {
        SpawnArgs {
            program: "sh".into(),
            args: vec![],
            env: vec![],
            env_remove: vec![],
            cwd: "/tmp".into(),
            width: 80,
            rows: 24,
        }
    }

    /// The session of each spawn of `host`, named `queued` or `shared` by the
    /// session it is.
    fn handed(
        host: &FakeTerminalHost,
        queued: &Arc<FakeTerminalSession>,
        shared: &Arc<FakeTerminalSession>,
    ) -> Vec<&'static str> {
        host.sessions()
            .iter()
            .map(
                |session| match (Arc::ptr_eq(session, queued), Arc::ptr_eq(session, shared)) {
                    (true, _) => "queued",
                    (_, true) => "shared",
                    _ => "other",
                },
            )
            .collect()
    }

    /// Output pushed on each session reaches the spawns of that session alone,
    /// so the first spawn and the two after it drive apart.
    #[test]
    fn a_queued_session_serves_one_spawn_and_the_shared_session_serves_the_rest() {
        rt().block_on(async {
            let shared = Arc::new(FakeTerminalSession::new());
            let queued = Arc::new(FakeTerminalSession::new());
            let host = FakeTerminalHost::new(shared.clone());
            host.push_session(queued.clone());
            let first = host.spawn(args()).await.unwrap();
            let second = host.spawn(args()).await.unwrap();
            let third = host.spawn(args()).await.unwrap();

            queued.push_output(b"queued");
            shared.push_output(b"shared");
            second.write(b"two").await.unwrap();
            third.write(b"three").await.unwrap();

            assert_eq!(
                (
                    first.read_chunk().await.unwrap(),
                    second.read_chunk().await.unwrap(),
                    queued.sent_strings(),
                    shared.sent_strings(),
                    handed(&host, &queued, &shared),
                ),
                (
                    Some(b"queued".to_vec()),
                    Some(b"shared".to_vec()),
                    Vec::<String>::new(),
                    vec!["two".to_owned(), "three".to_owned()],
                    vec!["queued", "shared", "shared"],
                ),
            );
        });
    }

    #[test]
    fn a_failed_spawn_keeps_the_queued_session() {
        rt().block_on(async {
            let shared = Arc::new(FakeTerminalSession::new());
            let queued = Arc::new(FakeTerminalSession::new());
            let host = FakeTerminalHost::new(shared.clone());
            host.push_session(queued.clone());
            host.fail_next_spawn(io::ErrorKind::NotFound);

            let failed = host
                .spawn(args())
                .await
                .map(|_| ())
                .map_err(|error| error.kind());
            host.spawn(args()).await.unwrap();

            assert_eq!(
                (failed, handed(&host, &queued, &shared)),
                (Err(io::ErrorKind::NotFound), vec!["queued"]),
            );
        });
    }

    #[test]
    fn an_unqueued_session_serves_no_spawn() {
        rt().block_on(async {
            let shared = Arc::new(FakeTerminalSession::new());
            let queued = Arc::new(FakeTerminalSession::new());
            let host = FakeTerminalHost::new(shared.clone());
            host.push_session(queued.clone());
            let taken = host.unqueue().map(|taken| Arc::ptr_eq(&taken, &queued));
            host.spawn(args()).await.unwrap();

            assert_eq!(
                (taken, handed(&host, &queued, &shared)),
                (Some(true), vec!["shared"]),
            );
        });
    }
}
