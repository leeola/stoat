//! The `walkthrough-card` fixture: a text report builder with an eight-stop
//! tour that pushes the narration card into every placement and content shape.
//!
//! The card is the one surface no committed tour reaches the whole of.
//! `place_card` (crate::walkthrough::slide) tries four positions and the last
//! three need code wide or tall enough to block the ones before them, and
//! `card_width` clamps at a floor, a ceiling, and half the frame with nothing
//! to drive it to any of them. The source here is written for those bounds: a
//! six-line block and a fifty-line block whose every line runs past column 110,
//! and three one-line functions short enough to leave the whole pane free.
//!
//! The content shapes are the tour's own. A narration of one word, one of
//! 70-column lines around an unbreakable token, one of sixty lines, one holding
//! every markdown construct the renderer styles, three annotations that each
//! narrate for themselves, and a titled stop that says nothing at all.

use crate::{
    fixture::{FixtureError, FixtureRepo},
    walkthrough::{Range, Walkthrough},
};
use std::path::Path;

const CARGO: &str = r#"[package]
name = "fixture-walkthrough-card"
version = "0.1.0"
edition = "2021"

[workspace]
"#;

const MAIN: &str = r#"mod report;

use report::Row;

fn main() {
    let rows = vec![
        Row {
            label: "requests.total".to_string(),
            count: 10007,
            bytes: 4_194_304,
        },
        Row {
            label: "cache.accepted".to_string(),
            count: 20014,
            bytes: 1_048_576,
        },
    ];

    print!("{}", report::render(&rows));
    println!("{}", report::summarize(&rows));
}
"#;

const REPORT: &str = r#"use std::fmt::Write;

/// The fixed header every report opens with.
fn header(out: &mut String) {
    writeln!(out, "{:<40}{:>12}{:>14}  {}", "metric", "count", "bytes", "what the counter measures, in the operator's own words").unwrap();
    writeln!(out, "{:<40}{:>12}{:>14}  {}", "------", "-----", "-----", "------------------------------------------------------").unwrap();
    writeln!(out, "{:<40}{:>12}{:>14}  {}", "generated", "-", "-", "one line per counter, in the fixed order the table declares").unwrap();
    writeln!(out, "{:<40}{:>12}{:>14}  {}", "scope", "-", "-", "the whole process, since start, with no window and no decay").unwrap();
    writeln!(out, "{:<40}{:>12}{:>14}  {}", "reset", "-", "-", "never, so a difference between two reports is the interval").unwrap();
    writeln!(out, "{:<40}{:>12}{:>14}  {}", "units", "events", "octets", "counts are events and bytes are octets on the wire, not in memory").unwrap();
}

/// One counter as the report carries it.
pub struct Row {
    pub label: String,
    pub count: u64,
    pub bytes: u64,
}

/// Every row of the report, rendered in table order.
pub fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    header(&mut out);

    for row in rows {
        writeln!(out, "{:<40}{:>12}{:>14}", row.label, row.count, row.bytes).unwrap();
    }

    out
}

/// The counters the report knows, in the order it prints them.
pub const TEMPLATE: [&str; 50] = [
    "  requests.total                               10007   counted at the edge before any routing decision was taken",
    "  requests.accepted                            20014   counted after the router picked a backend and before the write",
    "  requests.rejected                            30021   counted once per attempt, so a retry adds one to this and to the parent",
    "  requests.retried                             40028   counted on the way out, after the response body was fully written",
    "  requests.timed_out                           50035   counted when the deadline elapsed with no byte written either way",
    "  routing.total                                60042   counted at the edge before any routing decision was taken",
    "  routing.accepted                             70049   counted after the router picked a backend and before the write",
    "  routing.rejected                             80056   counted once per attempt, so a retry adds one to this and to the parent",
    "  routing.retried                              90063   counted on the way out, after the response body was fully written",
    "  routing.timed_out                           100070   counted when the deadline elapsed with no byte written either way",
    "  cache.total                                 110077   counted at the edge before any routing decision was taken",
    "  cache.accepted                              120084   counted after the router picked a backend and before the write",
    "  cache.rejected                              130091   counted once per attempt, so a retry adds one to this and to the parent",
    "  cache.retried                               140098   counted on the way out, after the response body was fully written",
    "  cache.timed_out                             150105   counted when the deadline elapsed with no byte written either way",
    "  upstream.total                              160112   counted at the edge before any routing decision was taken",
    "  upstream.accepted                           170119   counted after the router picked a backend and before the write",
    "  upstream.rejected                           180126   counted once per attempt, so a retry adds one to this and to the parent",
    "  upstream.retried                            190133   counted on the way out, after the response body was fully written",
    "  upstream.timed_out                          200140   counted when the deadline elapsed with no byte written either way",
    "  tls.total                                   210147   counted at the edge before any routing decision was taken",
    "  tls.accepted                                220154   counted after the router picked a backend and before the write",
    "  tls.rejected                                230161   counted once per attempt, so a retry adds one to this and to the parent",
    "  tls.retried                                 240168   counted on the way out, after the response body was fully written",
    "  tls.timed_out                               250175   counted when the deadline elapsed with no byte written either way",
    "  storage.total                               260182   counted at the edge before any routing decision was taken",
    "  storage.accepted                            270189   counted after the router picked a backend and before the write",
    "  storage.rejected                            280196   counted once per attempt, so a retry adds one to this and to the parent",
    "  storage.retried                             290203   counted on the way out, after the response body was fully written",
    "  storage.timed_out                           300210   counted when the deadline elapsed with no byte written either way",
    "  index.total                                 310217   counted at the edge before any routing decision was taken",
    "  index.accepted                              320224   counted after the router picked a backend and before the write",
    "  index.rejected                              330231   counted once per attempt, so a retry adds one to this and to the parent",
    "  index.retried                               340238   counted on the way out, after the response body was fully written",
    "  index.timed_out                             350245   counted when the deadline elapsed with no byte written either way",
    "  queue.total                                 360252   counted at the edge before any routing decision was taken",
    "  queue.accepted                              370259   counted after the router picked a backend and before the write",
    "  queue.rejected                              380266   counted once per attempt, so a retry adds one to this and to the parent",
    "  queue.retried                               390273   counted on the way out, after the response body was fully written",
    "  queue.timed_out                             400280   counted when the deadline elapsed with no byte written either way",
    "  render.total                                410287   counted at the edge before any routing decision was taken",
    "  render.accepted                             420294   counted after the router picked a backend and before the write",
    "  render.rejected                             430301   counted once per attempt, so a retry adds one to this and to the parent",
    "  render.retried                              440308   counted on the way out, after the response body was fully written",
    "  render.timed_out                            450315   counted when the deadline elapsed with no byte written either way",
    "  session.total                               460322   counted at the edge before any routing decision was taken",
    "  session.accepted                            470329   counted after the router picked a backend and before the write",
    "  session.rejected                            480336   counted once per attempt, so a retry adds one to this and to the parent",
    "  session.retried                             490343   counted on the way out, after the response body was fully written",
    "  session.timed_out                           500350   counted when the deadline elapsed with no byte written either way",
];

pub fn total(rows: &[Row]) -> u64 { rows.iter().map(|row| row.count).sum() }

pub fn widest(rows: &[Row]) -> usize { rows.iter().map(|row| row.label.len()).max().unwrap_or(0) }

pub fn bytes(rows: &[Row]) -> u64 { rows.iter().map(|row| row.bytes).sum() }

/// One line about the whole report, for a log the operator tails.
pub fn summarize(rows: &[Row]) -> String {
    let count = total(rows);
    let width = widest(rows);
    let volume = bytes(rows);
    format!("{count} events over {} rows, {width} wide, {volume} bytes", rows.len())
}
"#;

/// The first and last lines of the six-line header block, whose every line
/// runs past column 110 and so blocks the card's right margin.
const HEADER_FIRST: &str = "\"metric\", \"count\"";
const HEADER_LAST: &str = "\"units\", \"events\"";

/// The first and last rows of the fifty-line table, long enough and tall
/// enough that no candidate but the last one fits.
const TEMPLATE_FIRST: &str = "requests.total";
const TEMPLATE_LAST: &str = "session.timed_out";

const NARRATION_BELOW: &str = "\
Every line here runs past column 110, so the layout finds the right margin
blocked and drops the card under the block instead.

The labels and the connector follow it there, which is where the placement
shows.
";

const NARRATION_OVER: &str = "\
Fifty long rows leave no margin beside them, no room below them, and none
above them either.

The last candidate is what makes the placement total: the card goes back to
the margin and over the code, because a covered line beats a stop with
nowhere to put its narration.
";

const NARRATION_DONE: &str = "Done.";

const NARRATION_WIDE: &str = "\
Every counter is named for the thing it counts rather than the place it is
counted, which is how a name reaches seventy columns before its description
has even started:

requests_accepted_by_route_and_method_and_status_family_total

A token that long has nowhere to break, and the wrap never splits a word, so
the name runs past the card and is clipped at its edge.
";

const NARRATION_TALL: &str = "\
The report prints one line per counter, and the counters are fixed. Every
group below reports the same five leaves, in the same order, so a reader
who has learned one group has learned all ten:

- `requests`
  - total
  - accepted
  - rejected
  - retried
  - timed out
- `routing`
  - total
  - accepted
  - rejected
  - retried
  - timed out
- `cache`
  - total
  - accepted
  - rejected
  - retried
  - timed out
- `upstream`
  - total
  - accepted
  - rejected
  - retried
  - timed out
- `tls`
  - total
  - accepted
  - rejected
  - retried
  - timed out
- `storage`
  - total
  - accepted
  - rejected
  - retried
  - timed out
- `index`
  - total
  - accepted
  - rejected
  - retried
  - timed out
- `queue`
  - total
  - accepted
  - rejected
  - retried
  - timed out
- `render`
  - total
  - accepted
  - rejected
  - retried
  - timed out
- `session`
  - total
  - accepted
  - rejected
  - retried
  - timed out
";

const NARRATION_MARKDOWN: &str = r#"# The report format

The table is **fixed**: every counter is *always* printed, even at zero, so
two reports line up row for row. Nothing is ~~omitted~~ and nothing folded.

---

Each row is one counter, and the columns read left to right:

- the metric name, written `group.leaf`
  - the group names the subsystem
  - the leaf names one of the five outcomes
- the count, then the bytes

Read a report back with the widths it was written at:

```rust
let rows = report::TEMPLATE;
println!("{}", rows.len());
```

The column list is in the [format notes](https://example.invalid).
"#;

const NARRATION_ANNOTATED: &str = "\
Three calls, three annotations, and every one of them narrates for itself.

Stepping onto an annotation swaps the card for that annotation's own, headed
by its label and its place in the three rather than by the stop's title.
";

const NARRATION_TOTAL: &str = "\
The count column is a plain sum. Nothing weights a row or drops one for
being zero.
";

const NARRATION_WIDEST: &str = "\
The width is measured over the labels alone, so a long count never widens
the first column.
";

const NARRATION_BYTES: &str = "\
Bytes are counted on the wire, after framing and before compression, which
is the one point every layer agrees on a number.

That choice is why the column rarely matches what a client reports:

- a proxy that re-frames the body changes the count
- a compressor running after this point does not
- a retry counts twice, once per attempt
- a connection that dies mid-body counts what was written

The report says nothing about which of those happened. It is a counter and
not a trace, and a number that needs a story is a number the operator wants
to be reading somewhere else.
";

/// Build the fixture repository at `dest`.
pub(in crate::fixture) fn materialize(dest: &Path) -> Result<(), FixtureError> {
    let json = super::tour_json(&build());

    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            ("Cargo.toml", CARGO),
            ("src/main.rs", MAIN),
            ("src/report.rs", REPORT),
            (".stoat/walkthroughs/tour.json", &json),
        ],
    )?;
    Ok(())
}

/// The eight-stop tour the `walkthrough-card` fixture commits.
fn build() -> Walkthrough {
    let mut tour = Walkthrough::new(
        "tour".to_string(),
        "Where the narration goes".to_string(),
        None,
    );

    stop(
        &mut tour,
        "Card below",
        NARRATION_BELOW,
        super::block_of(REPORT, HEADER_FIRST, HEADER_LAST),
    );
    stop(
        &mut tour,
        "Card over code",
        NARRATION_OVER,
        super::block_of(REPORT, TEMPLATE_FIRST, TEMPLATE_LAST),
    );
    stop(
        &mut tour,
        "Narrowest card",
        NARRATION_DONE,
        super::line_of(REPORT, "pub fn total(rows"),
    );
    stop(
        &mut tour,
        "Widest card",
        NARRATION_WIDE,
        super::line_of(REPORT, "pub fn widest(rows"),
    );
    stop(
        &mut tour,
        "Tallest card",
        NARRATION_TALL,
        super::line_of(REPORT, "pub fn bytes(rows"),
    );
    stop(
        &mut tour,
        "Every markdown shape",
        NARRATION_MARKDOWN,
        super::line_of(REPORT, "pub fn render(rows"),
    );

    let s7 = stop(
        &mut tour,
        "Annotation cards",
        NARRATION_ANNOTATED,
        super::block_of(REPORT, "pub fn summarize(rows", "format!(\"{count} events"),
    );
    for (needle, label, narration) in [
        ("total(rows)", "the count column", NARRATION_TOTAL),
        ("widest(rows)", "the label column", NARRATION_WIDEST),
        ("bytes(rows)", "the bytes column", NARRATION_BYTES),
    ] {
        super::annotate(
            &mut tour,
            &s7,
            None,
            REPORT,
            super::span_of(REPORT, needle),
            label,
            narration,
        );
    }

    // A title with nothing under it, so the card comes down and the status
    // line is the only thing still naming where the reader is.
    tour.add_stop(
        Some("Nothing to say".to_string()),
        String::new(),
        super::location("src/main.rs", MAIN, super::line_of(MAIN, "fn main() {")),
        None,
    )
    .expect("appending a stop cannot fail");

    tour
}

/// Append a stop over `range` of `src/report.rs`, returning its id.
fn stop(tour: &mut Walkthrough, title: &str, narration: &str, range: Range) -> String {
    tour.add_stop(
        Some(title.to_string()),
        narration.to_string(),
        super::location("src/report.rs", REPORT, range),
        None,
    )
    .expect("appending a stop cannot fail")
    .id
    .clone()
}

#[cfg(test)]
mod tests {
    use super::REPORT;
    use crate::{
        host::LocalFs,
        walkthrough::{self, store, Stop},
    };

    /// Every marker the markdown stop has to carry, one per construct
    /// `render_markdown` gives a style of its own.
    const MARKDOWN_MARKERS: [&str; 9] = [
        "# The report format",
        "**fixed**",
        "*always*",
        "~~omitted~~",
        "](https://example.invalid)",
        "\n---\n",
        "  - the group names",
        "`group.leaf`",
        "```rust",
    ];

    /// The tour is built from ranges derived out of the source consts, so a
    /// const gaining a line must not leave a range pointing at the wrong code.
    /// `validate` catches that, and this runs it against the materialized
    /// repository rather than against the builder's own idea of the text.
    #[test]
    fn card_tour_validates_against_the_committed_sources() {
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

    /// Each stop drives the card to one placement or one content shape, and
    /// half of them are driven by the source rather than by the narration. An
    /// edit that shortens a block or a narration leaves the stop pointing
    /// somewhere valid while the shape it was staging is gone.
    #[test]
    fn card_tour_stages_every_card_shape() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();
        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");

        assert_eq!(tour.stops.len(), 8, "eight stops");

        let rows = |stop: &Stop| {
            (stop.focus.range.start.line..=stop.focus.range.end.line)
                .map(line_len)
                .collect::<Vec<_>>()
        };

        let blocking = rows(&tour.stops[0]);
        assert_eq!(
            (blocking.len(), blocking.iter().all(|len| *len > 110)),
            (6, true),
            "the card-below block is six lines, none of them leaving a margin, got {blocking:?}",
        );

        let covering = rows(&tour.stops[1]);
        assert_eq!(
            (covering.len() >= 50, covering.iter().all(|len| *len > 110)),
            (true, true),
            "the card-over block is fifty long lines, so no candidate but the last fits, got {} lines",
            covering.len(),
        );

        assert_eq!(
            tour.stops[2].narration, "Done.",
            "one word, so the card clamps to CARD_MIN_WIDTH",
        );

        assert!(
            tour.stops[3]
                .narration
                .split_whitespace()
                .any(|token| token.chars().count() > 60),
            "one token is too long to break at, so wrap_styled breaks it mid-word",
        );

        let tall = tour.stops[4].narration.lines().count();
        assert!(
            tall >= 60,
            "the narration outruns any frame, so the height clamps, got {tall} lines",
        );

        let markdown = &tour.stops[5].narration;
        assert_eq!(
            MARKDOWN_MARKERS
                .iter()
                .filter(|marker| !markdown.contains(*marker))
                .collect::<Vec<_>>(),
            Vec::<&&str>::new(),
            "every markdown construct the renderer styles is present",
        );

        let annotations = &tour.stops[6].annotations;
        assert_eq!(
            (
                annotations.len(),
                annotations.iter().all(|a| !a.narration.is_empty()),
                annotations[2].narration.lines().count() >= 12,
            ),
            (3, true, true),
            "three annotations narrate for themselves and the last outgrows its stop",
        );

        let last = &tour.stops[7];
        assert_eq!(
            (last.title.as_deref(), last.narration.as_str()),
            (Some("Nothing to say"), ""),
            "a titled stop with nothing to say takes the card down",
        );
    }

    /// The byte length of `REPORT`'s one-based line `line`.
    fn line_len(line: u32) -> usize {
        REPORT
            .lines()
            .nth(line as usize - 1)
            .expect("the tour's ranges are derived from REPORT")
            .len()
    }
}
