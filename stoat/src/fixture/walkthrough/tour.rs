//! The `walkthrough` fixture: a four-file cargo crate with a six-stop tour
//! committed over it.
//!
//! The tour is the baseline the player is developed against. It stages a
//! one-line focus, a many-line focus, a step that glides more than a screen,
//! several annotations in one file, a cross-file annotation, and an untitled
//! stop with no narration.
//!
//! The sources are shared: the other `walkthrough-` fixtures commit the same
//! crate and vary only the tour over it, so a reader who has learned this code
//! once reads every fixture in the family.

use crate::{
    fixture::{FixtureError, FixtureRepo},
    walkthrough::Walkthrough,
};
use std::path::Path;

pub(in crate::fixture) const CARGO: &str = r#"[package]
name = "fixture-walkthrough"
version = "0.1.0"
edition = "2021"

[workspace]
"#;

pub(in crate::fixture) const MAIN: &str = r#"mod config;
mod handler;
mod server;

use server::Server;
use std::path::PathBuf;

fn main() {
    let path = PathBuf::from("config.toml");
    let config = config::load(&path);
    let server = Server::new(config);
    server.run();
}
"#;

pub(in crate::fixture) const CONFIG: &str = r#"use std::path::Path;

/// Everything the server reads before it binds.
pub struct Config {
    pub addr: String,
    pub workers: usize,
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            addr: "127.0.0.1:8080".to_string(),
            workers: 4,
            verbose: false,
        }
    }
}

pub fn load(path: &Path) -> Config {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut config = Config::default();

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        apply(&mut config, key.trim(), value.trim());
    }

    config
}

fn apply(config: &mut Config, key: &str, value: &str) {
    match key {
        "addr" => config.addr = value.to_string(),
        "workers" => config.workers = value.parse().unwrap_or(4),
        "verbose" => config.verbose = value == "true",
        _ => {},
    }
}
"#;

pub(in crate::fixture) const SERVER: &str = r#"use crate::config::Config;
use crate::handler;

/// A parsed request, as the accept loop hands it on.
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// What the handler decided, ready to write back.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn not_found() -> Response {
        Response {
            status: 404,
            body: Vec::new(),
        }
    }

    pub fn ok(body: Vec<u8>) -> Response {
        Response { status: 200, body }
    }
}

/// Counts the accept loop keeps, reported on shutdown.
#[derive(Default)]
pub struct Metrics {
    pub served: u64,
    pub refused: u64,
    pub bytes_out: u64,
}

impl Metrics {
    fn record(&mut self, response: &Response) {
        if response.status >= 400 {
            self.refused += 1;
        } else {
            self.served += 1;
        }
        self.bytes_out += response.body.len() as u64;
    }

    fn summary(&self) -> String {
        format!(
            "{} served, {} refused, {} bytes",
            self.served, self.refused, self.bytes_out
        )
    }
}

/// One bound listener and the configuration it was built from.
pub struct Server {
    config: Config,
    connections: usize,
    metrics: Metrics,
    draining: bool,
}

impl Server {
    pub fn new(config: Config) -> Server {
        Server {
            config,
            connections: 0,
            metrics: Metrics::default(),
            draining: false,
        }
    }

    /// Accept forever, handing each request to the dispatcher.
    ///
    /// The loop owns no threads of its own. Each request is served to
    /// completion before the next is read, which is what keeps the
    /// connection count honest.
    pub fn run(&mut self) {
        loop {
            let Some(request) = self.accept() else {
                break;
            };

            self.connections += 1;
            let response = self.dispatch(request);
            self.metrics.record(&response);
            self.write(response);

            if self.connections >= self.config.workers {
                self.drain();
            }
        }

        self.shutdown();
    }

    /// The next request, or `None` once the listener is closed.
    ///
    /// A draining server refuses new work rather than queueing it, so the
    /// count the metrics report is the count that was actually served.
    fn accept(&mut self) -> Option<Request> {
        if self.draining || self.connections > 1024 {
            return None;
        }

        Some(Request {
            method: "GET".to_string(),
            path: "/health".to_string(),
            body: Vec::new(),
        })
    }

    fn write(&mut self, response: Response) {
        if self.config.verbose {
            eprintln!("-> {} ({} bytes)", response.status, response.body.len());
        }
    }

    fn drain(&mut self) {
        self.connections = 0;
    }

    fn shutdown(&mut self) {
        self.draining = true;
        if self.config.verbose {
            eprintln!("shutting down: {}", self.metrics.summary());
        }
    }

    /// Route a request to whoever handles it.
    ///
    /// Everything below the health check goes to the one handler, which is
    /// where the interesting work happens.
    fn dispatch(&mut self, request: Request) -> Response {
        if request.path == "/health" {
            return Response::ok(b"ok".to_vec());
        }

        handler::handle(&request)
    }
}
"#;

pub(in crate::fixture) const HANDLER: &str = r#"use crate::server::{Request, Response};

/// Serve one request, by method.
///
/// Every arm answers on its own. There is no shared fallthrough, so adding a
/// method means adding an arm rather than editing one.
pub fn handle(request: &Request) -> Response {
    match request.method.as_str() {
        "GET" => read(request),
        "POST" => write(request),
        "DELETE" => remove(request),
        _ => Response::not_found(),
    }
}

fn read(request: &Request) -> Response {
    Response::ok(request.path.as_bytes().to_vec())
}

fn write(request: &Request) -> Response {
    Response::ok(request.body.clone())
}

fn remove(_request: &Request) -> Response {
    Response {
        status: 204,
        body: Vec::new(),
    }
}
"#;

/// Narration for the tour's first stop. Carries a list and inline code, so the
/// card has more than one paragraph shape to lay out.
const NARRATION_MAIN: &str = "\
`main` does three things and then hands over:

- reads the config file off disk
- builds a `Server` from it
- runs the accept loop until it stops

Everything after this point is one of those three.
";

/// Narration for the annotation over the config call. The only sub-step of the
/// tour that speaks for itself, so the card has both branches to lay out.
const NARRATION_LOAD: &str = "\
The path is relative to the working directory, so a server started from
elsewhere reads a different file.

A missing one is not an error, which is what the next stop is about.
";

/// Narration carrying a fenced code block, the widest thing a card has to fit.
const NARRATION_CONFIG: &str = "\
Loading is deliberately forgiving. A missing file, a blank line, or a key
nobody recognizes all leave the defaults in place:

```
addr = 0.0.0.0:9000
workers = 8
```

Only a line with an `=` is even looked at.
";

const NARRATION_RUN: &str = "\
The accept loop is single-threaded on purpose. Each request is served to
completion before the next is read, so `connections` counts what was really
handled rather than what was queued.
";

const NARRATION_DISPATCH: &str = "\
Dispatch is the fork in the road. `/health` answers here and goes no further;
everything else leaves this file.
";

const NARRATION_HANDLE: &str = "\
One arm per method, with no shared fallthrough. Adding a method means adding
an arm, which is why the unknown case is the last thing here rather than the
first.
";

/// Build the fixture repository at `dest`.
pub(in crate::fixture) fn materialize(dest: &Path) -> Result<(), FixtureError> {
    commit(dest)?;
    Ok(())
}

/// Commit the crate and its tour, returning the repository so a caller can
/// stage working-tree changes on top of the committed state.
pub(in crate::fixture) fn commit(dest: &Path) -> Result<FixtureRepo, FixtureError> {
    let json = super::tour_json(&build());

    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            ("Cargo.toml", CARGO),
            ("src/main.rs", MAIN),
            ("src/config.rs", CONFIG),
            ("src/server.rs", SERVER),
            ("src/handler.rs", HANDLER),
            (".stoat/walkthroughs/tour.json", &json),
        ],
    )?;
    Ok(repo)
}

/// The six-stop tour the `walkthrough` fixture commits.
///
/// Built through the crate's own API rather than written as a JSON literal, so
/// every range is derived from the same text the commit carries. A literal
/// pins byte offsets that the sources then drift from silently.
pub(in crate::fixture) fn build() -> Walkthrough {
    let mut tour = Walkthrough::new(
        "tour".to_string(),
        "How a request is served".to_string(),
        None,
    );

    // A one-line focus, so the mark is an ellipse.
    let s1 = tour
        .add_stop(
            Some("Entry point".to_string()),
            NARRATION_MAIN.to_string(),
            super::location("src/main.rs", MAIN, super::line_of(MAIN, "fn main() {")),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    super::annotate(
        &mut tour,
        &s1,
        None,
        MAIN,
        super::span_of(MAIN, "config::load(&path)"),
        "reads the file, or falls back to defaults",
        NARRATION_LOAD,
    );
    super::annotate(
        &mut tour,
        &s1,
        None,
        MAIN,
        super::span_of(MAIN, "Server::new(config)"),
        "binds nothing yet; that waits for run",
        "",
    );

    // A many-line focus, so the mark is a rect. One label wraps.
    let s2 = tour
        .add_stop(
            Some("Loading the config".to_string()),
            NARRATION_CONFIG.to_string(),
            super::location(
                "src/config.rs",
                CONFIG,
                super::block_of(CONFIG, "pub fn load(path: &Path)", "    config"),
            ),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    super::annotate(
        &mut tour,
        &s2,
        None,
        CONFIG,
        super::span_of(CONFIG, "unwrap_or_default()"),
        "a missing file is not an error",
        "",
    );
    super::annotate(
        &mut tour,
        &s2,
        None,
        CONFIG,
        super::span_of(CONFIG, "line.split_once('=')"),
        "a line without an equals sign is skipped entirely, which is how blank \
         lines and comments both fall through without a case of their own",
        "",
    );
    super::annotate(
        &mut tour,
        &s2,
        None,
        CONFIG,
        super::span_of(CONFIG, "apply(&mut config"),
        "one key at a time",
        "",
    );

    // These two sit at opposite ends of one file, so stepping between them
    // glides more than a screen.
    let s3 = tour
        .add_stop(
            Some("The accept loop".to_string()),
            NARRATION_RUN.to_string(),
            super::location(
                "src/server.rs",
                SERVER,
                super::line_of(SERVER, "pub fn run(&mut self)"),
            ),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    super::annotate(
        &mut tour,
        &s3,
        None,
        SERVER,
        super::span_of(SERVER, "self.dispatch(request)"),
        "every request goes through here",
        "",
    );

    tour.add_stop(
        Some("Dispatch".to_string()),
        NARRATION_DISPATCH.to_string(),
        super::location(
            "src/server.rs",
            SERVER,
            super::line_of(SERVER, "fn dispatch(&mut self"),
        ),
        None,
    )
    .expect("appending a stop cannot fail");

    // One annotation per match arm, plus one pointing back at server.rs.
    let s5 = tour
        .add_stop(
            Some("Handling a request".to_string()),
            NARRATION_HANDLE.to_string(),
            super::location(
                "src/handler.rs",
                HANDLER,
                super::line_of(HANDLER, "pub fn handle(request: &Request)"),
            ),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    for (needle, label) in [
        ("\"GET\" => read(request)", "reads the path back"),
        ("\"POST\" => write(request)", "echoes the body"),
        ("\"DELETE\" => remove(request)", "answers 204, with no body"),
        ("_ => Response::not_found()", "anything else is a 404"),
    ] {
        super::annotate(
            &mut tour,
            &s5,
            None,
            HANDLER,
            super::span_of(HANDLER, needle),
            label,
            "",
        );
    }
    super::annotate(
        &mut tour,
        &s5,
        Some("src/server.rs"),
        SERVER,
        super::span_of(SERVER, "handler::handle(&request)"),
        "this is the call that got here",
        "",
    );

    // No title and nothing to say, so the tour title stands in and the
    // narration card has nothing to show.
    tour.add_stop(
        None,
        String::new(),
        super::location("src/main.rs", MAIN, super::span_of(MAIN, "server.run()")),
        None,
    )
    .expect("appending a stop cannot fail");

    tour
}

#[cfg(test)]
mod tests {
    use crate::{
        host::LocalFs,
        walkthrough::{self, store},
    };
    use std::path::Path;

    /// The tour is built from ranges derived out of the source consts, so a
    /// const gaining a line must not leave a range pointing at the wrong code.
    /// `validate` is what would catch that, and this runs it against the
    /// materialized repository rather than against the builder's own idea of
    /// the text.
    #[test]
    fn walkthrough_tour_validates_against_the_committed_sources() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();

        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");
        let drift = walkthrough::validate(&tour, &store::workspace_reader(&LocalFs, dir.path()));
        assert_eq!(
            drift,
            Vec::new(),
            "every range in the tour still covers what it captured",
        );
    }

    /// The tour exists to give the walkthrough player every shape it has to
    /// handle. Each assertion here pins one of those shapes, so a later edit to
    /// the sources that flattens one is caught rather than quietly narrowing
    /// what the fixture stages.
    #[test]
    fn walkthrough_tour_stages_every_shape_the_player_handles() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();
        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");

        assert_eq!(tour.stops.len(), 6, "six stops");

        let s1 = &tour.stops[0];
        assert_eq!(
            s1.focus.snippet, "fn main() {",
            "the first focus is one line, so the mark is an ellipse",
        );
        assert_eq!(
            (
                s1.annotations[0].narration.is_empty(),
                s1.annotations[1].narration.is_empty(),
            ),
            (false, true),
            "one annotation narrates for itself and its sibling leaves the \
             stop's card up",
        );

        let s2 = &tour.stops[1];
        let s2_lines = s2.focus.range.end.line - s2.focus.range.start.line + 1;
        assert!(
            s2_lines > 10,
            "the second focus spans a block, so the mark is a rect, got {s2_lines} lines",
        );
        assert!(
            s2.annotations.iter().any(|a| a.label.len() > 60),
            "one label is long enough to wrap in its box",
        );

        let s4 = &tour.stops[3];
        assert!(
            s4.focus.range.start.line > 100,
            "the fourth stop sits far enough down server.rs that stepping to it \
             glides more than a screen, got line {}",
            s4.focus.range.start.line,
        );

        let s5 = &tour.stops[4];
        assert_eq!(
            s5.annotations.len(),
            5,
            "one annotation per match arm, plus the cross-file one",
        );
        assert_eq!(
            s5.annotations
                .iter()
                .filter(|a| a.path.as_deref() == Some(Path::new("src/server.rs")))
                .count(),
            1,
            "exactly one annotation points outside the stop's own file",
        );

        let s6 = &tour.stops[5];
        assert_eq!(
            (s6.title.as_deref(), s6.narration.as_str()),
            (None, ""),
            "the last stop falls back to the tour title and shows no narration",
        );
    }
}
