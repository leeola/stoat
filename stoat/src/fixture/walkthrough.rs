//! Shared scaffolding for the `walkthrough-` family of fixtures.
//!
//! Each fixture below commits a tour under `.stoat/walkthroughs/tour.json`
//! next to the sources it names, and stages one group of the player's
//! surfaces. They share this module's range helpers so no fixture pins a
//! hand-counted line or column: every range is derived from the same text the
//! commit carries, and a source const gaining a line moves the range with it.

use crate::walkthrough::{self, Location, Point, Range, Walkthrough};
use std::path::PathBuf;

pub(super) mod card;
pub(super) mod catalog;
pub(super) mod columns;
pub(super) mod drift;
pub(super) mod marks;
pub(super) mod tour;
pub(super) mod trail;

/// How long a driven tour rests on a stop before stepping to the next.
///
/// The slide draws in about two seconds, and the narration card is two
/// sentences, so this leaves a reader time to finish both.
const STOP_DWELL_MS: u32 = 4000;

/// How long a driven tour rests on an annotation before stepping past it.
///
/// An annotation is a label over a range rather than a fresh screen, so it
/// needs less than a stop.
const ANNOTATION_DWELL_MS: u32 = 2500;

/// The `--inputs` script that opens `tour` and walks every stop and annotation
/// in it, pausing on each one long enough to read.
///
/// The script is derived from the tour rather than written beside it, so a
/// fixture that gains a stop or an annotation drives the new one with no edit
/// here.
pub(super) fn stepping_inputs(tour: &Walkthrough) -> String {
    let mut script = format!(":walkthrough {}<Enter><Space>W", tour.slug);

    for (index, stop) in tour.stops.iter().enumerate() {
        for _ in &stop.annotations {
            script.push_str(&format!("<Wait:{ANNOTATION_DWELL_MS}>a"));
        }
        if index + 1 < tour.stops.len() {
            script.push_str(&format!("<Wait:{STOP_DWELL_MS}>n"));
        }
    }

    script
}

/// Returns the tour serialized the way the store writes it to disk.
///
/// `store::save` rewrites `git_head` from the workspace, and a fixture's tour
/// file is committed alongside the sources it names, so there is no earlier
/// commit to record. The serialization is otherwise the same, trailing
/// newline included.
pub(super) fn tour_json(tour: &Walkthrough) -> String {
    let mut json = serde_json::to_string_pretty(tour).expect("the tour serializes");
    json.push('\n');
    json
}

/// The whole line `needle` first appears on, as an inclusive range.
///
/// This helper and [`span_of`] exist so no range in a walkthrough fixture is
/// a hand-counted column. A range written by hand goes stale the moment a const
/// above it gains a line, and `validate` is the only thing that notices.
pub(super) fn line_of(content: &str, needle: &str) -> Range {
    let (index, line) = content
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(needle))
        .unwrap_or_else(|| panic!("fixture source is missing {needle:?}"));

    let number = index as u32 + 1;
    Range {
        start: Point {
            line: number,
            col: 1,
        },
        // Inclusive, so the last byte of the line is the last column. An empty
        // line has no byte to name, so it takes column 1.
        end: Point {
            line: number,
            col: line.len().max(1) as u32,
        },
    }
}

/// Just `needle`'s own bytes, on the first line holding it.
pub(super) fn span_of(content: &str, needle: &str) -> Range {
    let (index, line) = content
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(needle))
        .unwrap_or_else(|| panic!("fixture source is missing {needle:?}"));

    let start = line
        .find(needle)
        .expect("the line was found by this needle")
        + 1;
    Range {
        start: Point {
            line: index as u32 + 1,
            col: start as u32,
        },
        end: Point {
            line: index as u32 + 1,
            col: (start + needle.len() - 1) as u32,
        },
    }
}

/// Whole lines, from the one holding `first` through the one holding `last`.
pub(super) fn block_of(content: &str, first: &str, last: &str) -> Range {
    let head = line_of(content, first);
    let tail = line_of(content, last);
    Range {
        start: head.start,
        end: tail.end,
    }
}

/// A [`Location`] over `range` in `path`, with the bytes it covers captured.
pub(super) fn location(path: &str, content: &str, range: Range) -> Location {
    Location {
        path: PathBuf::from(path),
        range,
        snippet: walkthrough::snippet_for(content, range)
            .expect("fixture ranges are derived from the content they name"),
    }
}

/// Attach an annotation whose snippet is captured from `content`.
///
/// `path` is [`None`] for the stop's own file and [`Some`] for a cross-file
/// annotation, which is the distinction the player reads to decide whether to
/// open another buffer.
///
/// An empty `narration` leaves the stop's own narration standing while the
/// reader is on this annotation.
pub(super) fn annotate(
    tour: &mut Walkthrough,
    stop: &str,
    path: Option<&str>,
    content: &str,
    range: Range,
    label: &str,
    narration: &str,
) {
    let snippet = walkthrough::snippet_for(content, range)
        .expect("fixture ranges are derived from the content they name");
    tour.add_annotation(
        stop,
        path.map(PathBuf::from),
        range,
        snippet,
        label.to_string(),
        narration.to_string(),
    )
    .expect("the stop was just added");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_inputs_walks_every_stop_and_annotation() {
        let content = "fn first() {}\nfn second() {}\n";
        let mut tour = Walkthrough::new("tour".to_string(), "Two stops".to_string(), None);

        for needle in ["fn first", "fn second"] {
            tour.add_stop(
                None,
                String::new(),
                location("src/main.rs", content, line_of(content, needle)),
                None,
                None,
            )
            .expect("the tour accepts a stop");
        }

        annotate(
            &mut tour,
            "s1",
            None,
            content,
            span_of(content, "first"),
            "first",
            "",
        );

        assert_eq!(
            stepping_inputs(&tour),
            ":walkthrough tour<Enter><Space>W<Wait:2500>a<Wait:4000>n",
        );
    }
}
