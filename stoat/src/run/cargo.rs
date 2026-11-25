//! A reading of cargo's output, the report that a Parsed Cmd's block shows.
//!
//! Everything here is pure code over whole lines, with no I/O. A report is a
//! fold of one `push_line` over the lines that a [`LineFeed`] cuts from the
//! output, so a test reads a sample of cargo's text the way a run reads its
//! output.

use super::kind::{CargoKind, RunKind};
use crate::location_picker::{self, ParsedLocation};

/// The output of a `cargo test` run of one binary with three tests, one of
/// which passes, one fails, and one is ignored.
#[cfg(test)]
pub(crate) const ONE_FAILURE: &str = concat!(
    "running 3 tests\n",
    "test render::chrome::tests::zoomed_clamps ... ok\n",
    "test render::chrome::tests::zoomed_negative_steps ... FAILED\n",
    "test render::chrome::tests::slow_case ... ignored\n",
    "\n",
    "failures:\n",
    "\n",
    "---- render::chrome::tests::zoomed_negative_steps stdout ----\n",
    "\n",
    "thread 'render::chrome::tests::zoomed_negative_steps' (48213) panicked at ",
    "stoat/src/render/chrome.rs:412:9:\n",
    "assertion `left == right` failed\n",
    "  left: 18\n",
    " right: 16\n",
    "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n",
    "\n",
    "\n",
    "failures:\n",
    "    render::chrome::tests::zoomed_negative_steps\n",
    "\n",
    "test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; ",
    "finished in 0.02s\n",
);

/// The output of a `cargo test` run of three binaries, the unit tests, an
/// integration test, and the doc-tests, every test of which passes.
#[cfg(test)]
pub(crate) const THREE_BINARIES: &str = concat!(
    "running 2 tests\n",
    "test run::tests::first ... ok\n",
    "test run::tests::second ... ok\n",
    "\n",
    "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; ",
    "finished in 0.01s\n",
    "\n",
    "     Running tests/cli.rs (target/debug/deps/cli-3c4d)\n",
    "\n",
    "running 1 test\n",
    "test starts ... ok\n",
    "\n",
    "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; ",
    "finished in 0.00s\n",
    "\n",
    "   Doc-tests stoat\n",
    "\n",
    "running 0 tests\n",
    "\n",
    "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; ",
    "finished in 0.00s\n",
);

/// [`ONE_FAILURE`] with its failure's block printed tight, with no blank line
/// after its `----` line and no thread id on its panic line.
#[cfg(test)]
const NO_THREAD_ID: &str = concat!(
    "running 3 tests\n",
    "test render::chrome::tests::zoomed_clamps ... ok\n",
    "test render::chrome::tests::zoomed_negative_steps ... FAILED\n",
    "test render::chrome::tests::slow_case ... ignored\n",
    "\n",
    "failures:\n",
    "\n",
    "---- render::chrome::tests::zoomed_negative_steps stdout ----\n",
    "thread 'render::chrome::tests::zoomed_negative_steps' panicked at ",
    "stoat/src/render/chrome.rs:412:9:\n",
    "assertion `left == right` failed\n",
    "  left: 18\n",
    " right: 16\n",
    "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n",
    "\n",
    "\n",
    "failures:\n",
    "    render::chrome::tests::zoomed_negative_steps\n",
    "\n",
    "test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; ",
    "finished in 0.02s\n",
);

/// What a Cmd's output reports, for a kind whose output the block reads.
#[derive(Debug, Hash)]
pub(crate) enum CmdReport {
    /// The report of a `cargo test` run.
    Test(TestReport),
}

/// The counts and the failures of a `cargo test` run, as its lines arrive.
#[derive(Debug, Default, Hash)]
pub(crate) struct TestReport {
    /// The test binaries that started, each with its `running N tests` line.
    pub(crate) binaries: u32,
    /// The tests that the binaries that started said they would run.
    pub(crate) total: u32,
    passed: u32,
    failed: u32,
    ignored: u32,
    /// The `test result:` lines read, one for each binary that finished.
    summaries: u32,
    /// The passed, failed, and ignored sums of the `test result:` lines.
    summed: (u32, u32, u32),
    /// The failed tests, in the order their result lines arrived.
    pub(crate) failures: Vec<TestFailure>,
    /// The failure block that the arriving lines belong to.
    block: FailureBlock,
}

/// One failed test of a `cargo test` run, as its result line and its block of
/// output name it.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct TestFailure {
    /// The test's path, as its result line names it.
    pub(crate) name: String,
    /// Where the test panicked, as its block's first panic line names it.
    pub(crate) location: Option<ParsedLocation>,
    /// The panic message, its lines trimmed and joined by two spaces.
    pub(crate) message: Option<String>,
}

/// Where the reading of a failure's block of output stands.
///
/// Cargo prints the output of each failed test after the results, in a block
/// that a `---- <name> stdout ----` line opens and a `failures:` line ends. The
/// block's first panic line names where the test panicked, and the lines after
/// it hold the panic message.
#[derive(Debug, Default, Hash)]
enum FailureBlock {
    /// The lines belong to no block, or to one whose message has ended.
    #[default]
    Closed,
    /// The block of the failure at this index is open, and its panic line has
    /// not arrived.
    Opened(usize),
    /// The lines are the panic message of the failure at this index.
    Message(usize),
}

/// The bytes of a Cmd's output, cut into whole lines.
#[derive(Debug, Default)]
pub(crate) struct LineFeed {
    /// The bytes of the line that has not ended yet.
    pending: Vec<u8>,
    /// Whether the last line ended at a carriage return, so that a line feed
    /// straight after it ends nothing more.
    after_return: bool,
}

impl CmdReport {
    /// The report that a Cmd of `kind` keeps, or `None` for a kind whose
    /// output the block shows as it prints.
    pub(crate) fn for_kind(kind: RunKind) -> Option<Self> {
        match kind {
            RunKind::Parsed(CargoKind::Test) => Some(CmdReport::Test(TestReport::default())),
            _ => None,
        }
    }

    /// Read one line of the Cmd's output into the report.
    pub(crate) fn push_line(&mut self, line: &str) {
        match self {
            CmdReport::Test(report) => report.push_line(line),
        }
    }
}

impl TestReport {
    /// The result lines read so far, one for each test that finished.
    pub(crate) fn done(&self) -> u32 {
        self.passed + self.failed + self.ignored
    }

    /// The passed, failed, and ignored counts of the run.
    ///
    /// They are the sums of the `test result:` lines when every binary that
    /// started printed one, since those count the tests of the whole run.
    /// Otherwise they are the result lines read so far.
    pub(crate) fn totals(&self) -> (u32, u32, u32) {
        if self.summaries > 0 && self.summaries == self.binaries {
            self.summed
        } else {
            (self.passed, self.failed, self.ignored)
        }
    }

    /// The location of the first failure whose panic line named one.
    pub(crate) fn first_location(&self) -> Option<&ParsedLocation> {
        self.failures
            .iter()
            .find_map(|failure| failure.location.as_ref())
    }

    /// Read one line of the run's output into the report.
    ///
    /// A `running N tests` line starts a binary of N tests. A `test result:`
    /// line ends one with its sums. Any other line that starts with `test `
    /// and holds ` ... ` is the result of one test. The outcome after its
    /// last ` ... ` reads `ok` or `bench` for a pass, `FAILED` for a failure,
    /// and `ignored` for an ignored test. A failure adds a [`TestFailure`] of
    /// its name when none has it.
    ///
    /// A `---- <name> stdout ----` line opens the block of the failure it
    /// names, and a line that starts with `failures:` closes it. The block's
    /// first line that starts with `thread '` and holds `panicked at ` gives
    /// the failure's location, read from the text after `panicked at `. The
    /// lines after it are the message, up to a blank line or a line that
    /// starts with `note:` or `stack backtrace:`. Every other line counts for
    /// nothing.
    fn push_line(&mut self, line: &str) {
        if let Some(count) = running_count(line) {
            self.total += count;
            self.binaries += 1;
            return;
        }
        if let Some(rest) = line.strip_prefix("test result:") {
            let (passed, failed, ignored) = result_counts(rest);
            self.summaries += 1;
            self.summed.0 += passed;
            self.summed.1 += failed;
            self.summed.2 += ignored;
            return;
        }
        if line.starts_with("failures:") {
            self.block = FailureBlock::Closed;
            return;
        }
        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" stdout ----"))
        {
            self.block = self
                .failures
                .iter()
                .position(|failure| failure.name == name)
                .map_or(FailureBlock::Closed, FailureBlock::Opened);
            return;
        }
        match self.block {
            FailureBlock::Closed => self.push_result_line(line),
            FailureBlock::Opened(index) => self.push_block_line(index, line),
            FailureBlock::Message(index) => self.push_message_line(index, line),
        }
    }

    /// Read a line outside every failure block, which counts when it is the
    /// result of one test.
    fn push_result_line(&mut self, line: &str) {
        let Some((name, outcome)) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.rsplit_once(" ... "))
        else {
            return;
        };
        if outcome.starts_with("ok") || outcome.starts_with("bench") {
            self.passed += 1;
        } else if outcome.starts_with("FAILED") {
            self.failed += 1;
            if !self.failures.iter().any(|failure| failure.name == name) {
                self.failures.push(TestFailure {
                    name: name.to_owned(),
                    location: None,
                    message: None,
                });
            }
        } else if outcome.starts_with("ignored") {
            self.ignored += 1;
        }
    }

    /// Read a line of the open block of failure `index` before its panic
    /// line, where the test's captured output counts for nothing.
    fn push_block_line(&mut self, index: usize, line: &str) {
        let Some((_, at)) = line
            .strip_prefix("thread '")
            .and_then(|rest| rest.split_once("panicked at "))
        else {
            return;
        };
        self.failures[index].location = location_picker::parse_location_line(at);
        self.block = FailureBlock::Message(index);
    }

    /// Read a line of the panic message of failure `index`, or end the message
    /// at a line that ends it.
    fn push_message_line(&mut self, index: usize, line: &str) {
        let text = line.trim();
        if text.is_empty() || line.starts_with("note:") || line.starts_with("stack backtrace:") {
            self.block = FailureBlock::Closed;
            return;
        }
        let failure = &mut self.failures[index];
        match &mut failure.message {
            Some(message) => {
                message.push_str("  ");
                message.push_str(text);
            },
            None => failure.message = Some(text.to_owned()),
        }
    }
}

impl LineFeed {
    /// Cut `bytes` into lines and hand each line that they end to `sink`, its
    /// escape sequences dropped, holding back the bytes of a line that has
    /// not ended.
    ///
    /// A carriage return ends a line as a line feed does, since cargo redraws
    /// its progress line with one, and a line feed straight after a carriage
    /// return ends nothing more. A line that is not UTF-8 reads lossily.
    pub(crate) fn push(&mut self, bytes: &[u8], sink: &mut dyn FnMut(&str)) {
        for &byte in bytes {
            match byte {
                b'\n' if self.after_return => self.after_return = false,
                b'\r' | b'\n' => {
                    sink(&strip_ansi(&String::from_utf8_lossy(&self.pending)));
                    self.pending.clear();
                    self.after_return = byte == b'\r';
                },
                _ => {
                    self.pending.push(byte);
                    self.after_return = false;
                },
            }
        }
    }
}

/// The count of a binary's `running N tests` line, or `None` for another
/// line.
fn running_count(line: &str) -> Option<u32> {
    let (count, noun) = line.strip_prefix("running ")?.split_once(' ')?;
    if noun != "test" && noun != "tests" {
        return None;
    }
    count.parse().ok()
}

/// The passed, failed, and ignored counts of a `test result:` line, read from
/// the text after its colon.
fn result_counts(rest: &str) -> (u32, u32, u32) {
    let mut counts = (0, 0, 0);
    for part in rest.split(';') {
        let mut words = part.split_whitespace().rev();
        let (Some(label), Some(count)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(count) = count.parse() else {
            continue;
        };
        match label {
            "passed" => counts.0 = count,
            "failed" => counts.1 = count,
            "ignored" => counts.2 = count,
            _ => {},
        }
    }
    counts
}

/// `text` without its CSI and OSC escape sequences.
///
/// A CSI sequence runs from ESC `[` to its final byte, in 0x40 to 0x7E, and an
/// OSC sequence from ESC `]` to BEL or ESC `\`. Cargo colors its status words
/// and its test outcomes with them when it is asked for color.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for ch in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&ch) {
                        break;
                    }
                }
            },
            Some(']') => {
                while let Some(ch) = chars.next() {
                    if ch == '\x07' {
                        break;
                    }
                    if ch == '\x1b' && chars.next_if_eq(&'\\').is_some() {
                        break;
                    }
                }
            },
            _ => {},
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        strip_ansi, LineFeed, ParsedLocation, TestFailure, TestReport, NO_THREAD_ID, ONE_FAILURE,
        THREE_BINARIES,
    };

    /// The report of `text`, folded over the lines that a [`LineFeed`] cuts
    /// from it.
    fn parse_test_output(text: &str) -> TestReport {
        let mut report = TestReport::default();
        LineFeed::default().push(text.as_bytes(), &mut |line| report.push_line(line));
        report
    }

    /// The lines that a [`LineFeed`] hands on from `chunks`, fed in order.
    fn lines(chunks: &[&str]) -> Vec<String> {
        let mut feed = LineFeed::default();
        let mut lines = Vec::new();
        for chunk in chunks {
            feed.push(chunk.as_bytes(), &mut |line| lines.push(line.to_owned()));
        }
        lines
    }

    #[test]
    fn a_test_run_counts_each_result_line() {
        let report = parse_test_output(ONE_FAILURE);
        assert_eq!(
            (
                report.binaries,
                report.total,
                report.done(),
                report.totals()
            ),
            (1, 3, 3, (1, 1, 1)),
        );
    }

    #[test]
    fn several_binaries_add_up() {
        let report = parse_test_output(THREE_BINARIES);
        assert_eq!(
            (report.binaries, report.total, report.totals()),
            (3, 3, (3, 0, 0)),
        );
    }

    /// The totals of a run are the sums of its binaries' `test result:` lines
    /// once every binary printed one, since those count tests that printed no
    /// line of their own. Before that they are the result lines read so far.
    #[test]
    fn totals_take_the_sums_once_every_binary_printed_them() {
        let finished = parse_test_output(
            "running 3 tests\ntest a ... ok\ntest result: ok. 3 passed; 0 failed; 0 ignored\n",
        );
        let running = parse_test_output("running 2 tests\ntest a ... FAILED\n");
        assert_eq!(
            [finished.totals(), running.totals()],
            [(3, 0, 0), (0, 1, 0)]
        );
    }

    /// A `test result:` line starts with `test ` but reads as a binary's sums,
    /// so it counts no test as done.
    #[test]
    fn a_test_result_line_is_no_result_of_a_test() {
        let report = parse_test_output(
            "test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n",
        );
        assert_eq!((report.done(), report.summed), (0, (2, 0, 1)));
    }

    /// The failure that [`ONE_FAILURE`] reports, as its block names it.
    fn chrome_failure() -> TestFailure {
        TestFailure {
            name: "render::chrome::tests::zoomed_negative_steps".to_owned(),
            location: Some(ParsedLocation {
                path: "stoat/src/render/chrome.rs".to_owned(),
                line: 412,
                column: 9,
                text: String::new(),
            }),
            message: Some("assertion `left == right` failed  left: 18  right: 16".to_owned()),
        }
    }

    #[test]
    fn a_failure_carries_its_panic_location_and_message() {
        assert_eq!(parse_test_output(ONE_FAILURE).failures, [chrome_failure()]);
    }

    #[test]
    fn a_panic_line_reads_with_or_without_a_thread_id() {
        assert_eq!(parse_test_output(NO_THREAD_ID).failures, [chrome_failure()]);
    }

    /// A message ends at a blank line or at a backtrace, so the output after
    /// it and a backtrace's frames join no message.
    #[test]
    fn a_message_ends_at_a_blank_line_or_a_backtrace() {
        let report = parse_test_output(concat!(
            "test a ... FAILED\n",
            "test b ... FAILED\n",
            "---- a stdout ----\n",
            "thread 'a' panicked at src/a.rs:3:5:\n",
            "first\n",
            "stack backtrace:\n",
            "   0: rust_begin_unwind\n",
            "---- b stdout ----\n",
            "thread 'b' panicked at src/b.rs:7:5:\n",
            "second\n",
            "\n",
            "later output\n",
        ));
        let messages: Vec<_> = report
            .failures
            .iter()
            .map(|failure| failure.message.as_deref())
            .collect();
        assert_eq!(messages, [Some("first"), Some("second")]);
    }

    /// A `failures:` line closes a block that no panic line read, such as a
    /// `should_panic` test's, so the results of a later binary still count.
    #[test]
    fn a_failures_line_closes_a_block_with_no_panic() {
        let report = parse_test_output(concat!(
            "running 1 test\n",
            "test a ... FAILED\n",
            "---- a stdout ----\n",
            "note: test did not panic as expected\n",
            "failures:\n",
            "running 1 test\n",
            "test b ... FAILED\n",
        ));
        let names: Vec<_> = report
            .failures
            .iter()
            .map(|failure| failure.name.as_str())
            .collect();
        assert_eq!((report.totals(), names), ((0, 2, 0), vec!["a", "b"]));
    }

    #[test]
    fn a_failed_result_with_no_panic_block_keeps_its_name_alone() {
        assert_eq!(
            parse_test_output("test a ... FAILED\n").failures,
            [TestFailure {
                name: "a".to_owned(),
                location: None,
                message: None,
            }],
        );
    }

    #[test]
    fn colored_result_lines_parse_as_plain_ones() {
        let report = parse_test_output("running 1 test\ntest a ... \x1b[32mok\x1b[0m\n");
        assert_eq!((report.done(), report.totals()), (1, (1, 0, 0)));
    }

    /// A carriage return ends a line, a line feed after one ends nothing more,
    /// and the bytes after the last end wait for theirs.
    #[test]
    fn line_feed_ends_a_line_at_a_carriage_return_and_at_a_line_feed() {
        let text = "a\rb\r\nc\n\nd";
        assert_eq!(
            [lines(&[text]), lines(&[text, "\n"])],
            [vec!["a", "b", "c", ""], vec!["a", "b", "c", "", "d"]],
        );
    }

    /// A line split across chunks reads whole, as does a carriage return and a
    /// line feed that arrive in two chunks.
    #[test]
    fn line_feed_joins_a_line_split_across_chunks() {
        assert_eq!(
            lines(&["test a ..", ". ok\r", "\ntest b ... ok\n"]),
            ["test a ... ok", "test b ... ok"],
        );
    }

    #[test]
    fn strip_ansi_drops_csi_and_osc_sequences() {
        assert_eq!(
            [
                strip_ansi("\x1b[1m\x1b[32m   Compiling\x1b[0m stoat"),
                strip_ansi("\x1b]0;title\x07done"),
                strip_ansi("\x1b]8;;http://x\x1b\\link"),
            ],
            ["   Compiling stoat", "done", "link"],
        );
    }
}
