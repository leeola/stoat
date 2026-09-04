//! The `walkthrough-marks` fixture: a lint-rule table with an eight-stop tour
//! that puts every label and mark placement the layout makes on one screen.
//!
//! The source is chosen for its shapes rather than for what it computes. It
//! carries a five-arm match of short lines, a call taking two arguments, a
//! seven-arm match, a three-line struct literal, two match arms reaching past
//! column 110, a line past column 200, a lone closing brace at column 1, and a
//! last line worth naming. Each stop below focuses one of those, so the
//! placement it forces is reproducible rather than a screen someone has to
//! find.
//!
//! Between them the stops drive the fallbacks in `place_callouts`
//! (crate::walkthrough::slide): labels pushed off their row by
//! `LABEL_ROW_OFFSETS`, a label that finds no candidate at all, the marker
//! color cycle wrapping at six, a rect mark over a multi-line annotation, and a
//! focus long enough for soft wrap to spread over several display rows.

use crate::{
    fixture::{FixtureError, FixtureRepo},
    walkthrough::{Point, Range, Walkthrough},
};
use std::path::Path;

const CARGO: &str = r#"[package]
name = "fixture-walkthrough-marks"
version = "0.1.0"
edition = "2021"

[workspace]
"#;

const MAIN: &str = r#"mod rules;

use rules::Rule;

fn main() {
    let table = vec![
        Rule {
            name: "unused-import".to_string(),
            target: "src/lib.rs".to_string(),
        },
        Rule {
            name: "long-line".to_string(),
            target: "src/rules.rs".to_string(),
        },
    ];

    for finding in rules::check(&table) {
        println!("{} {}: {}", finding.code, finding.rule, finding.text);
    }

    println!("{}", rules::message("unused-import"));
}
"#;

const RULES: &str = r#"/// A rule as the table carries it: the name it is keyed by, and the path it
/// applies to.
pub struct Rule {
    pub name: String,
    pub target: String,
}

/// What a rule reports when it fires.
pub struct Finding {
    pub code: u16,
    pub rule: String,
    pub text: String,
}

/// How loudly a rule reports.
#[derive(Clone, Copy)]
pub enum Severity {
    Allow,
    Note,
    Warn,
    Deny,
}

/// The severity a rule reports at, by name.
pub fn severity(rule: &str) -> Severity {
    match rule {
        "unused-import" => Severity::Warn,
        "shadowed-binding" => Severity::Warn,
        "unreachable-arm" => Severity::Deny,
        "deprecated-call" => Severity::Note,
        _ => Severity::Allow,
    }
}

/// The stable code a rule reports under, by name.
pub fn code(rule: &str) -> u16 {
    match rule {
        "unused-import" => 101,
        "shadowed-binding" => 102,
        "unreachable-arm" => 103,
        "deprecated-call" => 104,
        "missing-doc" => 105,
        "long-line" => 106,
        _ => 0,
    }
}

/// What a rule says when it fires, written out for the reader rather than
/// abbreviated into a code.
pub fn message(rule: &str) -> &'static str {
    match rule {
        "unused-import" => "this import names something the file never reads, so deleting the line changes nothing the compiler can see",
        "shadowed-binding" => "a later binding of this name hides the earlier one, and every read below the shadow reaches the later value",
        _ => "this rule has no message of its own",
    }
}

/// Whether a rule reporting at `rule_level` is worth recording at all.
fn validate(rule_name: &str, rule_level: Severity) -> bool {
    !rule_name.is_empty() && !matches!(rule_level, Severity::Allow)
}

/// Every finding the table reports over `rules`, in table order.
pub fn check(rules: &[Rule]) -> Vec<Finding> {
    let mut findings = Vec::new();

    for rule in rules {
        let rule_name = rule.name.as_str();
        let rule_level = severity(rule_name);
        if !validate(rule_name, rule_level) {
            continue;
        }

        findings.push(Finding {
            code: code(rule_name),
            rule: rule_name.to_string(),
            text: message(rule_name).to_string(),
        });
    }

    findings
}

/// One line of prose about the table, left unwrapped on purpose.
pub const SUMMARY: &str = "The table is deliberately flat: a rule is a name, the name resolves to a severity and a code, and the message is looked up from the same name, so adding a rule is three arms and never a new concept.";

/// How many rules the table names.
pub const RULE_COUNT: usize = 6;
"#;

const NARRATION_CROWDED: &str = "\
Five annotations land on five consecutive rows, and each label is long enough
to wrap into a two-line box.

Only the first fits on its own row. The rest are pushed out one and two rows
by `LABEL_ROW_OFFSETS`, each keeping a connector back to the arm it names.
";

const NARRATION_PAIR: &str = "\
Both annotations start on the same row, so only one label can have it.

The first takes the row and reads as part of the line. The second drops one
row and draws a connector to say which mark it belongs to.
";

const NARRATION_SEVEN: &str = "\
Seven annotations, and the palette holds six marker colors.

The seventh wraps back to the first color, which is the only place the cycle
is visible at all.
";

const NARRATION_BLOCK: &str = "\
The focus is one line, so its mark is an ellipse.

The annotation below it covers three, so that one is boxed instead. Both marks
are on screen at once, which is the comparison this stop is for.
";

const NARRATION_NO_ROOM: &str = "\
Both arms run past column 110, and both labels want the space to their right.

In a narrow pane the first falls back to the left below the focus, and the
second finds no candidate anywhere and draws nothing at all.
";

const NARRATION_WRAPPED: &str = "\
The focus is one stored line and more than 200 columns wide.

Soft wrap spreads it over several display rows, so the mark measured from it
is a rectangle rather than the ellipse a one-line focus usually gets.
";

const NARRATION_EDGE: &str = "\
A lone closing brace at column 1, which is the narrowest thing a focus can be.

The mark still pads outward from the single cell it covers, so it has to stay
inside the pane rather than run off the left edge.
";

const NARRATION_LAST: &str = "\
The last line of the file, so there is nothing below it to scroll to.

The card has to sit above the focus here, which is the placement the earlier
stops never reach.
";

/// The five arms of `severity`, each on its own row and each under 60 columns.
const SEVERITY_ARMS: [&str; 5] = [
    "\"unused-import\" => Severity::Warn",
    "\"shadowed-binding\" => Severity::Warn",
    "\"unreachable-arm\" => Severity::Deny",
    "\"deprecated-call\" => Severity::Note",
    "_ => Severity::Allow",
];

/// Labels for [`SEVERITY_ARMS`], each long enough to wrap at `LABEL_WRAP` and
/// so tall enough to push the label below it off its own row.
const CROWDED_LABELS: [&str; 5] = [
    "an import nothing names is only ever a warning",
    "a rebound name warns exactly the way an unused one does",
    "an arm the match can never reach is denied outright",
    "a deprecated call is a note, so the build stays green",
    "every rule the table does not name is allowed by default",
];

/// The seven arms of `code`, one more than the palette has marker colors.
const CODE_ARMS: [&str; 7] = [
    "\"unused-import\" => 101",
    "\"shadowed-binding\" => 102",
    "\"unreachable-arm\" => 103",
    "\"deprecated-call\" => 104",
    "\"missing-doc\" => 105",
    "\"long-line\" => 106",
    "_ => 0,",
];

/// Labels for [`CODE_ARMS`], kept to one word so seven boxes fit beside the
/// arms they name and the color cycle is what stands out.
const CODE_LABELS: [&str; 7] = [
    "imports", "bindings", "arms", "calls", "docs", "lines", "unknown",
];

/// The two arms of `message`, both reaching past column 110.
const MESSAGE_ARMS: [&str; 2] = [
    "\"unused-import\" => \"this import names",
    "\"shadowed-binding\" => \"a later binding",
];

/// Build the fixture repository at `dest`.
pub(in crate::fixture) fn materialize(dest: &Path) -> Result<(), FixtureError> {
    let json = super::tour_json(&build());

    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            ("Cargo.toml", CARGO),
            ("src/main.rs", MAIN),
            ("src/rules.rs", RULES),
            (".stoat/walkthroughs/tour.json", &json),
        ],
    )?;
    Ok(())
}

/// The eight-stop tour the `walkthrough-marks` fixture commits.
fn build() -> Walkthrough {
    let mut tour = Walkthrough::new(
        "tour".to_string(),
        "Where the labels land".to_string(),
        None,
    );

    let rules = |range| super::location("src/rules.rs", RULES, range);

    let s1 = tour
        .add_stop(
            Some("Crowded labels".to_string()),
            NARRATION_CROWDED.to_string(),
            rules(super::block_of(
                RULES,
                SEVERITY_ARMS[0],
                SEVERITY_ARMS[SEVERITY_ARMS.len() - 1],
            )),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    for (needle, label) in SEVERITY_ARMS.iter().zip(CROWDED_LABELS) {
        annotate(&mut tour, &s1, needle, label);
    }

    let s2 = tour
        .add_stop(
            Some("Two on one row".to_string()),
            NARRATION_PAIR.to_string(),
            rules(super::line_of(RULES, "if !validate(")),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    annotate(
        &mut tour,
        &s2,
        "rule_name,",
        "the name the table is keyed by",
    );
    annotate(
        &mut tour,
        &s2,
        "rule_level)",
        "the severity that name resolved to",
    );

    let s3 = tour
        .add_stop(
            Some("Seven markers".to_string()),
            NARRATION_SEVEN.to_string(),
            rules(super::block_of(
                RULES,
                CODE_ARMS[0],
                CODE_ARMS[CODE_ARMS.len() - 1],
            )),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    for (needle, label) in CODE_ARMS.iter().zip(CODE_LABELS) {
        annotate(&mut tour, &s3, needle, label);
    }

    let s4 = tour
        .add_stop(
            Some("A block annotation".to_string()),
            NARRATION_BLOCK.to_string(),
            rules(super::line_of(RULES, "findings.push(Finding {")),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    super::annotate(
        &mut tour,
        &s4,
        None,
        RULES,
        super::block_of(RULES, "code: code(", "text: message("),
        "every field of the finding at once",
        "",
    );

    let s5 = tour
        .add_stop(
            Some("No room".to_string()),
            NARRATION_NO_ROOM.to_string(),
            rules(super::block_of(RULES, MESSAGE_ARMS[0], MESSAGE_ARMS[1])),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    annotate(
        &mut tour,
        &s5,
        MESSAGE_ARMS[0],
        "the message an unused import gets",
    );
    annotate(
        &mut tour,
        &s5,
        MESSAGE_ARMS[1],
        "the message a shadowed binding gets",
    );

    tour.add_stop(
        Some("A wrapped focus".to_string()),
        NARRATION_WRAPPED.to_string(),
        rules(super::line_of(RULES, "pub const SUMMARY")),
        None,
    )
    .expect("appending a stop cannot fail");

    tour.add_stop(
        Some("The left edge".to_string()),
        NARRATION_EDGE.to_string(),
        rules(closing_of(RULES, "pub fn check(")),
        None,
    )
    .expect("appending a stop cannot fail");

    tour.add_stop(
        Some("The last line".to_string()),
        NARRATION_LAST.to_string(),
        rules(super::line_of(RULES, "pub const RULE_COUNT")),
        None,
    )
    .expect("appending a stop cannot fail");

    tour
}

/// Attach an annotation over `needle`'s own bytes in `src/rules.rs`.
///
/// Every annotation in this tour names a span of the focus file, so the
/// cross-file and per-annotation-narration branches are left to the
/// `walkthrough` fixture and this one stays about placement.
fn annotate(tour: &mut Walkthrough, stop: &str, needle: &str, label: &str) {
    super::annotate(
        tour,
        stop,
        None,
        RULES,
        super::span_of(RULES, needle),
        label,
        "",
    );
}

/// The first line that is a lone `}` at column 1, at or after the line holding
/// `after`.
///
/// A closing brace is the one shape no needle names on its own, because every
/// block ends in one. A search forward from a line that is unique keeps the
/// range derived from the source rather than counted by hand.
fn closing_of(content: &str, after: &str) -> Range {
    let from = content
        .lines()
        .position(|line| line.contains(after))
        .unwrap_or_else(|| panic!("fixture source is missing {after:?}"));

    let index = from
        + content
            .lines()
            .skip(from)
            .position(|line| line == "}")
            .unwrap_or_else(|| panic!("nothing closes at column 1 below {after:?}"));

    let number = index as u32 + 1;
    Range {
        start: Point {
            line: number,
            col: 1,
        },
        end: Point {
            line: number,
            col: 1,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::RULES;
    use crate::{
        host::LocalFs,
        walkthrough::{self, store, Stop},
    };

    /// The tour is built from ranges derived out of the source consts, so a
    /// const gaining a line must not leave a range pointing at the wrong code.
    /// `validate` catches that, and this runs it against the materialized
    /// repository rather than against the builder's own idea of the text.
    #[test]
    fn marks_tour_validates_against_the_committed_sources() {
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

    /// Each stop stages one placement the layout has to make, and each is
    /// staged by a shape of the source rather than by the tour alone. An edit
    /// to `RULES` that flattens a shape leaves the stop pointing at code that
    /// no longer forces the placement, which is what these pin.
    #[test]
    fn marks_tour_stages_every_placement_shape() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();
        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");

        assert_eq!(tour.stops.len(), 8, "eight stops");

        let rows = |stop: &Stop| {
            (stop.focus.range.start.line..=stop.focus.range.end.line)
                .map(line_len)
                .collect::<Vec<_>>()
        };

        let s1 = &tour.stops[0];
        assert_eq!(
            s1.annotations
                .iter()
                .map(|a| a.range.start.line)
                .collect::<Vec<_>>(),
            (s1.focus.range.start.line..=s1.focus.range.end.line).collect::<Vec<_>>(),
            "one annotation per row of the focus, on consecutive rows",
        );
        assert!(
            s1.annotations.iter().all(|a| a.label.len() > 38),
            "every crowded label wraps past LABEL_WRAP into a two-line box",
        );

        let s2 = &tour.stops[1];
        assert_eq!(
            s2.annotations
                .iter()
                .map(|a| a.range.start.line)
                .collect::<Vec<_>>(),
            vec![s2.focus.range.start.line; 2],
            "both annotations start on the focus row, so one label moves",
        );

        assert_eq!(
            tour.stops[2].annotations.len(),
            7,
            "seven annotations, one past the six marker colors",
        );

        let s4 = &tour.stops[3];
        let block = &s4.annotations[0].range;
        assert_eq!(
            (
                s4.focus.range.start.line == s4.focus.range.end.line,
                block.end.line - block.start.line + 1,
            ),
            (true, 3),
            "a one-line focus is circled and its three-line annotation is boxed",
        );

        assert!(
            rows(&tour.stops[4]).iter().all(|len| *len > 110),
            "both arms of the no-room stop reach past column 110, got {:?}",
            rows(&tour.stops[4]),
        );

        assert_eq!(
            rows(&tour.stops[5])
                .iter()
                .filter(|len| **len > 200)
                .count(),
            1,
            "the wrapped focus is one stored line past column 200, got {:?}",
            rows(&tour.stops[5]),
        );

        let s7 = &tour.stops[6];
        assert_eq!(
            (s7.focus.snippet.as_str(), s7.focus.range.start.col),
            ("}", 1),
            "the left-edge focus is a lone closing brace in the first column",
        );

        assert_eq!(
            tour.stops[7].focus.range.start.line as usize,
            RULES.lines().count(),
            "the last stop focuses the last line of the file",
        );
    }

    /// The byte length of `RULES`' one-based line `line`.
    fn line_len(line: u32) -> usize {
        RULES
            .lines()
            .nth(line as usize - 1)
            .expect("the tour's ranges are derived from RULES")
            .len()
    }
}
