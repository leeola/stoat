//! Agent->client terminal requests: terminal/create, terminal/output,
//! terminal/wait_for_exit, terminal/kill, and terminal/release. Commands
//! run through the injected [`TerminalHost`]; a [`TerminalRegistry`] keeps
//! each running terminal by the id `create` assigns so the later requests
//! can reach it.

use crate::rpc::{error, method_not_found, parse_params, INTERNAL_ERROR, INVALID_PARAMS};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use stoat::host::{SpawnArgs, TerminalHost, TerminalSession};
use stoat_agent_claude_code::jsonrpc::{IncomingRequest, RpcError};
use stoat_scheduler::{Executor, Task};
use tokio::sync::watch;

pub(crate) const TERMINAL_CREATE: &str = "terminal/create";
pub(crate) const TERMINAL_OUTPUT: &str = "terminal/output";
pub(crate) const TERMINAL_WAIT_FOR_EXIT: &str = "terminal/wait_for_exit";
pub(crate) const TERMINAL_KILL: &str = "terminal/kill";
pub(crate) const TERMINAL_RELEASE: &str = "terminal/release";

const DEFAULT_WIDTH: u16 = 80;

pub(crate) fn is_terminal_method(method: &str) -> bool {
    matches!(
        method,
        TERMINAL_CREATE
            | TERMINAL_OUTPUT
            | TERMINAL_WAIT_FOR_EXIT
            | TERMINAL_KILL
            | TERMINAL_RELEASE
    )
}

/// Dispatch one terminal request, then respond. Spawned (it may await a
/// command's exit), so it owns its inputs.
pub(crate) async fn handle_terminal_request(
    req: IncomingRequest,
    terminal_host: Arc<dyn TerminalHost>,
    registry: TerminalRegistry,
    executor: Executor,
    cwd: String,
) {
    let params = req.params.as_ref();
    let response = match req.method.as_str() {
        TERMINAL_CREATE => create(params, &terminal_host, &registry, &executor, &cwd).await,
        TERMINAL_OUTPUT => output(params, &registry),
        TERMINAL_WAIT_FOR_EXIT => wait_for_exit(params, &registry).await,
        TERMINAL_KILL => kill(params, &registry).await,
        TERMINAL_RELEASE => release(params, &registry),
        other => Err(method_not_found(other)),
    };
    let _ = req.respond(response);
}

async fn create(
    params: Option<&Value>,
    terminal_host: &Arc<dyn TerminalHost>,
    registry: &TerminalRegistry,
    executor: &Executor,
    cwd: &str,
) -> Result<Value, RpcError> {
    let params: CreateParams = parse_params(params)?;
    let args = SpawnArgs {
        program: params.command,
        args: params.args,
        env: params.env.into_iter().map(|e| (e.name, e.value)).collect(),
        cwd: params.cwd.map_or_else(|| PathBuf::from(cwd), PathBuf::from),
        width: DEFAULT_WIDTH,
    };
    let session = terminal_host
        .spawn(args)
        .await
        .map_err(|source| error(INTERNAL_ERROR, source.to_string()))?;
    let id = registry.insert(AcpTerminal::spawn(session.into(), executor));
    Ok(json!({ "terminalId": id }))
}

fn output(params: Option<&Value>, registry: &TerminalRegistry) -> Result<Value, RpcError> {
    let terminal = lookup(params, registry)?;
    Ok(json!({
        "output": terminal.output_string(),
        "truncated": false,
        "exitStatus": terminal.exit_code().map(exit_status),
    }))
}

async fn wait_for_exit(
    params: Option<&Value>,
    registry: &TerminalRegistry,
) -> Result<Value, RpcError> {
    let terminal = lookup(params, registry)?;
    let code = terminal.wait().await;
    Ok(json!({ "exitStatus": exit_status_opt(code) }))
}

async fn kill(params: Option<&Value>, registry: &TerminalRegistry) -> Result<Value, RpcError> {
    let terminal = lookup(params, registry)?;
    terminal
        .session
        .kill()
        .await
        .map_err(|source| error(INTERNAL_ERROR, source.to_string()))?;
    Ok(Value::Null)
}

fn release(params: Option<&Value>, registry: &TerminalRegistry) -> Result<Value, RpcError> {
    let params: IdParams = parse_params(params)?;
    registry.remove(&params.terminal_id);
    Ok(Value::Null)
}

fn lookup(
    params: Option<&Value>,
    registry: &TerminalRegistry,
) -> Result<Arc<AcpTerminal>, RpcError> {
    let params: IdParams = parse_params(params)?;
    registry.get(&params.terminal_id).ok_or_else(|| {
        error(
            INVALID_PARAMS,
            format!("unknown terminal: {}", params.terminal_id),
        )
    })
}

fn exit_status(code: i32) -> Value {
    json!({ "exitCode": code })
}

fn exit_status_opt(code: Option<i32>) -> Value {
    json!({ "exitCode": code })
}

/// One running agent terminal: the session plus a read loop that
/// accumulates its output and captures the exit code at EOF. The
/// `finished` watch flips to `true` once the read loop ends, after the
/// exit code is stored, so [`Self::wait`] never misses the transition.
pub(crate) struct AcpTerminal {
    session: Arc<dyn TerminalSession>,
    output: Arc<Mutex<Vec<u8>>>,
    exit: Arc<Mutex<Option<i32>>>,
    finished: watch::Receiver<bool>,
    _reader: Task<()>,
}

impl AcpTerminal {
    fn spawn(session: Arc<dyn TerminalSession>, executor: &Executor) -> Arc<Self> {
        let output = Arc::new(Mutex::new(Vec::new()));
        let exit = Arc::new(Mutex::new(None));
        let (finished_tx, finished) = watch::channel(false);
        let reader = executor.spawn({
            let session = Arc::clone(&session);
            let output = Arc::clone(&output);
            let exit = Arc::clone(&exit);
            async move {
                let mut buf = [0u8; 4096];
                loop {
                    match session.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => output
                            .lock()
                            .expect("output mutex")
                            .extend_from_slice(&buf[..n]),
                    }
                }
                if let Ok(Some(code)) = session.try_wait().await {
                    *exit.lock().expect("exit mutex") = Some(code);
                }
                let _ = finished_tx.send(true);
            }
        });
        Arc::new(Self {
            session,
            output,
            exit,
            finished,
            _reader: reader,
        })
    }

    fn output_string(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().expect("output mutex")).into_owned()
    }

    fn exit_code(&self) -> Option<i32> {
        *self.exit.lock().expect("exit mutex")
    }

    /// Resolve once the command has exited, with its exit code.
    async fn wait(&self) -> Option<i32> {
        let mut finished = self.finished.clone();
        while !*finished.borrow() {
            if finished.changed().await.is_err() {
                break;
            }
        }
        self.exit_code()
    }
}

/// Running agent terminals keyed by the id assigned on create.
#[derive(Clone)]
pub(crate) struct TerminalRegistry {
    terminals: Arc<Mutex<HashMap<String, Arc<AcpTerminal>>>>,
    next_id: Arc<AtomicU64>,
}

impl TerminalRegistry {
    pub(crate) fn new() -> Self {
        Self {
            terminals: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    fn insert(&self, terminal: Arc<AcpTerminal>) -> String {
        let id = format!("term-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        self.terminals
            .lock()
            .expect("terminals mutex")
            .insert(id.clone(), terminal);
        id
    }

    fn get(&self, id: &str) -> Option<Arc<AcpTerminal>> {
        self.terminals
            .lock()
            .expect("terminals mutex")
            .get(id)
            .cloned()
    }

    fn remove(&self, id: &str) {
        self.terminals.lock().expect("terminals mutex").remove(id);
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateParams {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<EnvVar>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EnvVar {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdParams {
    terminal_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat::host::FakeTerminalSession;
    use stoat_scheduler::TokioScheduler;

    fn executor() -> Executor {
        Arc::new(TokioScheduler::new(tokio::runtime::Handle::current())).executor()
    }

    #[tokio::test]
    async fn accumulates_output_and_captures_exit() {
        let executor = executor();
        let fake = Arc::new(FakeTerminalSession::new());
        let terminal = AcpTerminal::spawn(fake.clone(), &executor);

        fake.push_output(b"hello ");
        fake.push_output(b"world");
        fake.finish(3);

        assert_eq!(terminal.wait().await, Some(3));
        assert_eq!(terminal.output_string(), "hello world");
        assert_eq!(terminal.exit_code(), Some(3));
    }

    #[tokio::test]
    async fn registry_inserts_and_removes() {
        let executor = executor();
        let registry = TerminalRegistry::new();
        let fake = Arc::new(FakeTerminalSession::new());

        let id = registry.insert(AcpTerminal::spawn(fake, &executor));
        assert_eq!(id, "term-1");
        assert!(registry.get(&id).is_some());

        registry.remove(&id);
        assert!(registry.get(&id).is_none());
    }
}
