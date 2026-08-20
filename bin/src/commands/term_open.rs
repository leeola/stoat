//! Forwarding a bare `stoat <files>` to the instance hosting this shell.
//!
//! A terminal pane's shell inherits `STOAT_AGENT_SOCK` and `STOAT_TERM_ID`,
//! which together name a running instance and the pane the shell sits in.
//! Typing `stoat foo` there means "show me this file", not "start a second
//! editor inside the one I am already looking at", so the open is sent to that
//! instance and this process exits.
//!
//! Every failure falls through instead of reporting. A dead parent or an env
//! pair copied into a surviving multiplexer both end in the ordinary nested
//! startup, where the worst outcome is the file opening twice.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

/// Open `files` in the instance hosting this shell, reporting whether it took
/// them.
///
/// `false` means this shell has no parent instance to reach, or reaching it
/// failed, and the caller starts its own editor as usual. `true` means the
/// files are on screen in the parent and this process has nothing left to do.
// The env reads are the blessed boundary. They hand their values straight to
// parent_target, which is pure and unit-tested.
#[allow(clippy::disallowed_methods)]
pub fn try_forward(files: &[PathBuf]) -> bool {
    let Some((socket, token)) = parent_target(
        std::env::var("STOAT_AGENT_SOCK").ok(),
        std::env::var("STOAT_TERM_ID").ok(),
    ) else {
        return false;
    };
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };

    let paths: Vec<PathBuf> = files.iter().map(|file| absolutize(&cwd, file)).collect();
    match send(&socket, &request_line(token, &paths)) {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(
                target: "stoat::bin",
                ?socket,
                "parent instance closed without opening the files; starting a nested session",
            );
            false
        },
        Err(err) => {
            tracing::warn!(
                target: "stoat::bin",
                %err,
                ?socket,
                "could not reach the parent instance; starting a nested session",
            );
            false
        },
    }
}

/// The parent instance's socket and this shell's terminal token, or `None` when
/// this shell is not a stoat terminal pane.
///
/// A token that does not parse is treated the same as an absent one. Nothing
/// else issues these variables, so a malformed pair means a stale environment
/// rather than a parent worth trying.
fn parent_target(sock: Option<String>, term_id: Option<String>) -> Option<(PathBuf, u64)> {
    let token = term_id?.parse::<u64>().ok()?;
    Some((PathBuf::from(sock?), token))
}

/// Resolve `path` against `base`, which an absolute `path` replaces outright.
///
/// The parent joins a relative path onto its own git root, which is the wrong
/// base whenever this shell has cd'd elsewhere, so the shell resolves its own
/// arguments first. Nothing is normalized. This is the same join the parent
/// itself does.
fn absolutize(base: &Path, path: &Path) -> PathBuf {
    base.join(path)
}

/// Build the newline-free open-in-term request line.
fn request_line(token: u64, paths: &[PathBuf]) -> String {
    serde_json::json!({
        "req": "open-in-term",
        "term": token,
        "paths": paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    })
    .to_string()
}

/// Send `line` to the socket and report whether the instance confirmed the open.
///
/// `Ok(false)` is a connection that ended without the reply, which is what a
/// parent exiting mid-request looks like.
fn send(socket: &Path, line: &str) -> std::io::Result<bool> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;

    for reply in BufReader::new(stream).lines() {
        if reply_is_opened(&reply?) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True when `line` is the instance's `opened` reply.
fn reply_is_opened(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    value.get("reply").and_then(|reply| reply.as_str()) == Some("opened")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_target_needs_both_a_socket_and_a_numeric_token() {
        assert_eq!(
            parent_target(Some("/run/a.sock".into()), Some("7".into())),
            Some((PathBuf::from("/run/a.sock"), 7)),
        );
        assert_eq!(parent_target(None, Some("7".into())), None);
        assert_eq!(parent_target(Some("/run/a.sock".into()), None), None);
        assert_eq!(
            parent_target(Some("/run/a.sock".into()), Some("not-a-token".into())),
            None,
            "a stale environment is no parent to try",
        );
    }

    #[test]
    fn absolutize_resolves_against_the_shells_own_cwd() {
        assert_eq!(
            absolutize(Path::new("/work/sub"), Path::new("a.rs")),
            PathBuf::from("/work/sub/a.rs"),
        );
        assert_eq!(
            absolutize(Path::new("/work/sub"), Path::new("/etc/hosts")),
            PathBuf::from("/etc/hosts"),
            "an already-absolute argument keeps its own root",
        );
        assert_eq!(
            absolutize(Path::new("/work/sub"), Path::new("../a.rs")),
            PathBuf::from("/work/sub/../a.rs"),
            "the parent resolves the join itself, so nothing is normalized here",
        );
    }

    #[test]
    fn request_line_carries_the_token_and_every_path() {
        let line = request_line(
            9,
            &[PathBuf::from("/work/a.rs"), PathBuf::from("/work/b.rs")],
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["req"].as_str(), Some("open-in-term"));
        assert_eq!(value["term"].as_u64(), Some(9));
        assert_eq!(
            value["paths"],
            serde_json::json!(["/work/a.rs", "/work/b.rs"]),
        );
    }

    /// Bind `socket`, then serve one connection on a thread: read the request
    /// line, write `replies`, and hand back exactly the bytes the client sent,
    /// terminator included.
    ///
    /// Bound before the thread starts, so a client that connects right away
    /// still finds a listener.
    ///
    /// The read is bounded because both sides are line-framed. A request that
    /// arrives without its terminator parks this thread while the client parks
    /// on the reply, so an unbounded read turns that regression into a hang.
    /// Returning the raw bytes is what makes the caller's assertion fail on the
    /// missing terminator instead.
    fn serve_once(socket: &Path, replies: &[&str]) -> std::thread::JoinHandle<String> {
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        let replies: Vec<String> = replies.iter().map(|reply| reply.to_string()).collect();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();

            let mut request = String::new();
            let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut request);
            for reply in replies {
                writeln!(stream, "{reply}").unwrap();
            }
            request
        })
    }

    #[test]
    fn a_forward_holds_only_when_the_instance_answers() {
        let dir = tempfile::tempdir().unwrap();

        let live = dir.path().join("live.sock");
        let served = serve_once(&live, &[r#"{"reply":"opened"}"#]);
        assert!(send(&live, "the-request").unwrap());
        assert_eq!(
            served.join().unwrap(),
            "the-request\n",
            "the instance reads lines, so the terminator is part of the request",
        );

        let quiet = dir.path().join("quiet.sock");
        let served = serve_once(&quiet, &[]);
        assert!(
            !send(&quiet, "the-request").unwrap(),
            "an instance that exits mid-request opened nothing",
        );
        served.join().unwrap();

        assert!(
            send(&dir.path().join("absent.sock"), "the-request").is_err(),
            "an unbound socket is no instance to reach",
        );
    }

    #[test]
    fn only_the_opened_reply_counts_as_forwarded() {
        assert!(reply_is_opened(r#"{"reply":"opened"}"#));
        assert!(!reply_is_opened(r#"{"reply":"editor-closed"}"#));
        assert!(!reply_is_opened(""));
        assert!(!reply_is_opened("not json"));
        assert!(!reply_is_opened(r#"{"hook":"stop"}"#));
    }
}
