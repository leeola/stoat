//! The walkthrough format and the pure operations over it.
//!
//! A walkthrough is a guided tour of a codebase. It is an ordered list of
//! stops, each naming one range of one file, a markdown narration of what that
//! code does, and labeled annotations over ranges in that file or in another
//! beside it. It is authored out of band and stored as one JSON file per
//! walkthrough, so the tour survives the session that wrote it and travels with
//! the repository.
//!
//! Every range carries the bytes it covered when it was captured. Code moves,
//! and a stored range on its own then points at the wrong lines with nothing to
//! say so. Comparing the captured snippet against what the file holds now turns
//! that drift into a reported finding rather than a wrong tour.
//!
//! Nothing here touches the filesystem. [`validate`] takes a reader closure, so
//! the whole module is exercised against in-memory content and the store layer
//! owns the IO.

use serde::{Deserialize, Serialize};
use snafu::{Location as ErrorLocation, Snafu};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

pub(crate) mod run;
pub(crate) mod slide;
pub mod store;

/// One authored walkthrough, the whole contents of its JSON file.
///
/// Field order is the wire order.
///
/// The two counters only ever grow. A removed stop's id is never handed out
/// again, so a note or an annotation that refers to `s3` never quietly resolves
/// to a different stop after an edit.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Walkthrough {
    /// Filename stem the walkthrough is stored and addressed under.
    pub slug: String,
    pub title: String,
    /// Commit the stops were captured against, when the workspace had one.
    ///
    /// A reader that finds the repository on a different commit knows the tour
    /// was written elsewhere before any snippet is compared.
    pub git_head: Option<String>,
    /// Id to assign the next stop, as `s<N>`.
    pub next_stop_id: u32,
    /// Id to assign the next annotation, as `a<N>`, shared across every stop.
    pub next_annotation_id: u32,
    pub stops: Vec<Stop>,
}

/// One step of the tour, being a place to look and what to say about it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Stop {
    /// `s<N>`, assigned once and never reused.
    pub id: String,
    pub title: Option<String>,
    /// Markdown narration shown alongside the focused code.
    pub narration: String,
    /// The file and range this stop is about.
    pub focus: Location,
    pub annotations: Vec<Annotation>,
}

/// A labeled range, in its stop's focus file or in one beside it.
///
/// Most annotations call out part of the code the stop already put on screen,
/// so [`Self::path`] stays `None` and the range reads against the focus file.
/// A stop that walks between two files sets it, which is what lets one slide
/// name a caller and its callee at once.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Annotation {
    /// `a<N>`, assigned once and never reused.
    pub id: String,
    /// Workspace-relative, and `None` for the stop's own focus file.
    ///
    /// Absent from the stored form while `None`, so a same-file annotation
    /// writes the same bytes it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub range: Range,
    /// Bytes [`Self::range`] covered when the annotation was captured.
    pub snippet: String,
    pub label: String,
    /// Markdown narration the player's card shows while the reader is on this
    /// annotation. An empty one leaves the stop's narration standing.
    ///
    /// Absent from the stored form while empty, so an annotation written before
    /// narration existed writes the same bytes it always did.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub narration: String,
}

/// A range of one file, with the bytes it covered when captured.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Location {
    /// Workspace-relative, so a walkthrough survives being cloned elsewhere.
    pub path: PathBuf,
    pub range: Range,
    /// Bytes [`Self::range`] covered when the location was captured.
    pub snippet: String,
}

/// An inclusive span between two points.
///
/// Both ends are inclusive, so the shortest range covers one byte and an empty
/// selection has no representation. That is what lets a snippet always be
/// non-empty, and so always be worth comparing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Range {
    pub start: Point,
    pub end: Point,
}

/// A position in a file, counted the way an editor reports it.
///
/// [`Self::line`] is 1-based. [`Self::col`] is a 1-based **byte** offset within
/// that line, not a character or column count, so it lands on the same place a
/// byte-oriented tool reports without any width arithmetic.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Point {
    pub line: u32,
    pub col: u32,
}

/// Which stop names the field an edit leaves alone and which it replaces.
///
/// Every field is `None` by default, so a caller sets only what it changes.
/// [`Self::title`] nests two layers because a stop's title is itself optional:
/// the outer `None` leaves the title untouched, and `Some(None)` clears it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StopEdit {
    pub title: Option<Option<String>>,
    pub narration: Option<String>,
    pub focus: Option<Location>,
}

/// The fields an annotation edit replaces, each `None` leaving what is there.
///
/// [`Self::path`] nests two layers the way [`StopEdit::title`] does, since the
/// path is itself optional. The outer `None` leaves it alone. `Some(None)`
/// returns the annotation to its stop's focus file.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct AnnotationEdit {
    pub path: Option<Option<PathBuf>>,
    pub range: Option<Range>,
    pub snippet: Option<String>,
    pub label: Option<String>,
    pub narration: Option<String>,
}

/// Where [`Walkthrough::move_stop`] puts the stop it moves.
///
/// An enum rather than three optional parameters, so naming two destinations at
/// once does not compile instead of failing at run time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MoveTarget<'a> {
    Before(&'a str),
    After(&'a str),
    Last,
}

/// Failure operating on a walkthrough or reading a range out of a file.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum WalkthroughError {
    #[snafu(display("no stop '{id}'"))]
    UnknownStop {
        id: String,
        #[snafu(implicit)]
        location: ErrorLocation,
    },

    #[snafu(display("no annotation '{id}' on stop '{stop}'"))]
    UnknownAnnotation {
        stop: String,
        id: String,
        #[snafu(implicit)]
        location: ErrorLocation,
    },

    #[snafu(display("line {line} is past the {lines} lines of content"))]
    LineOutOfBounds {
        line: u32,
        lines: usize,
        #[snafu(implicit)]
        location: ErrorLocation,
    },

    #[snafu(display("column {col} is past the {bytes} bytes of line {line}"))]
    ColumnOutOfBounds {
        line: u32,
        col: u32,
        bytes: usize,
        #[snafu(implicit)]
        location: ErrorLocation,
    },

    #[snafu(display("range ends before it starts"))]
    RangeInverted {
        #[snafu(implicit)]
        location: ErrorLocation,
    },

    #[snafu(display("range splits the character at byte {offset}"))]
    SplitCharacter {
        offset: usize,
        #[snafu(implicit)]
        location: ErrorLocation,
    },
}

/// What [`validate`] found wrong with one range.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Finding {
    pub stop: String,
    /// The annotation at fault, or `None` when the stop's own focus is.
    pub annotation: Option<String>,
    pub kind: FindingKind,
    pub detail: String,
}

/// Whether a finding means the range no longer reads at all, or reads something
/// other than what it captured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FindingKind {
    /// The file is gone, or the range does not fit the content.
    Error,
    /// The range still fits, but no longer covers what was captured.
    Stale,
}

impl Walkthrough {
    /// An empty walkthrough with both id counters at their first value.
    pub fn new(slug: String, title: String, git_head: Option<String>) -> Walkthrough {
        Walkthrough {
            slug,
            title,
            git_head,
            next_stop_id: 1,
            next_annotation_id: 1,
            stops: Vec::new(),
        }
    }

    /// Add a stop, either before the stop `before` names or at the end.
    ///
    /// Returns the stop, whose `id` the counter just assigned.
    pub fn add_stop(
        &mut self,
        title: Option<String>,
        narration: String,
        focus: Location,
        before: Option<&str>,
    ) -> Result<&Stop, WalkthroughError> {
        let at = match before {
            Some(id) => self.stop_index(id)?,
            None => self.stops.len(),
        };

        let stop = Stop {
            id: format!("s{}", self.next_stop_id),
            title,
            narration,
            focus,
            annotations: Vec::new(),
        };
        self.next_stop_id += 1;

        self.stops.insert(at, stop);
        Ok(&self.stops[at])
    }

    /// Replace the fields `edit` names on the stop `id`, leaving the rest.
    pub fn edit_stop(&mut self, id: &str, edit: StopEdit) -> Result<&Stop, WalkthroughError> {
        let at = self.stop_index(id)?;
        let stop = &mut self.stops[at];

        if let Some(title) = edit.title {
            stop.title = title;
        }
        if let Some(narration) = edit.narration {
            stop.narration = narration;
        }
        if let Some(focus) = edit.focus {
            stop.focus = focus;
        }

        Ok(stop)
    }

    /// Remove the stop `id` and hand it back, along with its annotations.
    pub fn remove_stop(&mut self, id: &str) -> Result<Stop, WalkthroughError> {
        let at = self.stop_index(id)?;
        Ok(self.stops.remove(at))
    }

    /// Move the stop `id` to `to`, keeping every other stop in order.
    pub fn move_stop(&mut self, id: &str, to: MoveTarget<'_>) -> Result<(), WalkthroughError> {
        let from = self.stop_index(id)?;
        let stop = self.stops.remove(from);

        // The anchor is resolved after the removal, so its index already
        // accounts for the hole the moved stop left behind.
        let at = match to {
            MoveTarget::Before(anchor) => self.stop_index(anchor),
            MoveTarget::After(anchor) => self.stop_index(anchor).map(|at| at + 1),
            MoveTarget::Last => Ok(self.stops.len()),
        };

        match at {
            Ok(at) => {
                self.stops.insert(at, stop);
                Ok(())
            },
            Err(error) => {
                // Put the stop back where it was, so a bad anchor leaves the
                // walkthrough as it found it.
                self.stops.insert(from, stop);
                Err(error)
            },
        }
    }

    /// Add an annotation to the stop `stop`, appended after its existing ones.
    ///
    /// A `path` of `None` puts the annotation in the stop's focus file.
    ///
    /// Returns the annotation, whose `id` the counter just assigned.
    pub fn add_annotation(
        &mut self,
        stop: &str,
        path: Option<PathBuf>,
        range: Range,
        snippet: String,
        label: String,
        narration: String,
    ) -> Result<&Annotation, WalkthroughError> {
        let at = self.stop_index(stop)?;

        let annotation = Annotation {
            id: format!("a{}", self.next_annotation_id),
            path,
            range,
            snippet,
            label,
            narration,
        };
        self.next_annotation_id += 1;

        let annotations = &mut self.stops[at].annotations;
        annotations.push(annotation);
        Ok(annotations.last().expect("just pushed"))
    }

    /// Replace the fields `edit` names on annotation `id` of stop `stop`.
    pub fn edit_annotation(
        &mut self,
        stop: &str,
        id: &str,
        edit: AnnotationEdit,
    ) -> Result<&Annotation, WalkthroughError> {
        let (stop_at, annotation_at) = self.annotation_index(stop, id)?;
        let annotation = &mut self.stops[stop_at].annotations[annotation_at];

        if let Some(path) = edit.path {
            annotation.path = path;
        }
        if let Some(range) = edit.range {
            annotation.range = range;
        }
        if let Some(snippet) = edit.snippet {
            annotation.snippet = snippet;
        }
        if let Some(label) = edit.label {
            annotation.label = label;
        }
        if let Some(narration) = edit.narration {
            annotation.narration = narration;
        }

        Ok(annotation)
    }

    /// Remove annotation `id` from stop `stop` and hand it back.
    pub fn remove_annotation(
        &mut self,
        stop: &str,
        id: &str,
    ) -> Result<Annotation, WalkthroughError> {
        let (stop_at, annotation_at) = self.annotation_index(stop, id)?;
        Ok(self.stops[stop_at].annotations.remove(annotation_at))
    }

    fn stop_index(&self, id: &str) -> Result<usize, WalkthroughError> {
        self.stops
            .iter()
            .position(|stop| stop.id == id)
            .ok_or_else(|| UnknownStopSnafu { id }.build())
    }

    fn annotation_index(&self, stop: &str, id: &str) -> Result<(usize, usize), WalkthroughError> {
        let stop_at = self.stop_index(stop)?;
        let annotation_at = self.stops[stop_at]
            .annotations
            .iter()
            .position(|annotation| annotation.id == id)
            .ok_or_else(|| UnknownAnnotationSnafu { stop, id }.build())?;
        Ok((stop_at, annotation_at))
    }
}

/// The bytes of `content` that `range` covers, both ends inclusive.
///
/// This is the one place a [`Range`] turns into text, so capture and validation
/// agree by construction. Whatever this extracts at capture time is exactly what
/// a later call compares against.
///
/// Errors rather than clamping or panicking when the range does not fit. A line
/// or column past the end, an end before its start, and a column landing inside
/// a multi-byte character are all reported, since each means the range names
/// something the content does not have.
///
/// No column reaches the newline that ends a line, so a range on an empty line
/// has no valid form.
pub fn snippet_for(content: &str, range: Range) -> Result<String, WalkthroughError> {
    let start = byte_offset(content, range.start)?;
    let end = byte_offset(content, range.end)?;

    if end < start {
        return RangeInvertedSnafu.fail();
    }

    // The end point names the last byte to keep, and slicing is exclusive.
    // Stepping past it lands on the next character's first byte, which `get`
    // rejects if the step landed inside one.
    let after_end = end + 1;
    content
        .get(start..after_end)
        .map(str::to_owned)
        .ok_or_else(|| SplitCharacterSnafu { offset: after_end }.build())
}

/// Every range in `walkthrough` whose file, bounds, or captured bytes no longer
/// hold, in stop then annotation order.
///
/// `read` returns a file's current content, or `None` when it is gone. Taking a
/// reader rather than a path keeps this pure, so the same call validates a
/// working tree, a commit, or a fixture.
///
/// A focus file that fails to read yields one finding for the stop, and none
/// for the annotations that share it. Their ranges point into that same file,
/// so a repeat of the failure per annotation buries the one fact that matters.
/// An annotation naming its own file is checked either way, and reports its own
/// read failure against itself.
///
/// An empty result means every stop still points at what it was written
/// against.
pub fn validate(walkthrough: &Walkthrough, read: &dyn Fn(&Path) -> Option<String>) -> Vec<Finding> {
    let mut findings = Vec::new();

    for stop in &walkthrough.stops {
        let focus = read(&stop.focus.path);

        match &focus {
            Some(content) => {
                if let Some(finding) = check(content, stop.focus.range, &stop.focus.snippet) {
                    findings.push(Finding {
                        stop: stop.id.clone(),
                        annotation: None,
                        ..finding
                    });
                }
            },
            None => findings.push(Finding {
                stop: stop.id.clone(),
                annotation: None,
                kind: FindingKind::Error,
                detail: format!("cannot read {}", stop.focus.path.display()),
            }),
        }

        for annotation in &stop.annotations {
            let content = match &annotation.path {
                Some(path) => match read(path) {
                    Some(content) => Cow::Owned(content),
                    None => {
                        findings.push(Finding {
                            stop: stop.id.clone(),
                            annotation: Some(annotation.id.clone()),
                            kind: FindingKind::Error,
                            detail: format!("cannot read {}", path.display()),
                        });
                        continue;
                    },
                },
                None => match &focus {
                    Some(content) => Cow::Borrowed(content.as_str()),
                    None => continue,
                },
            };

            if let Some(finding) = check(&content, annotation.range, &annotation.snippet) {
                findings.push(Finding {
                    stop: stop.id.clone(),
                    annotation: Some(annotation.id.clone()),
                    ..finding
                });
            }
        }
    }

    findings
}

/// The finding one range against `content` produces, or `None` when it still
/// covers `captured`.
///
/// The returned finding carries placeholder ids, which [`validate`] fills in
/// from whichever range it checked.
fn check(content: &str, range: Range, captured: &str) -> Option<Finding> {
    let blank = |kind, detail| Finding {
        stop: String::new(),
        annotation: None,
        kind,
        detail,
    };

    match snippet_for(content, range) {
        Err(error) => Some(blank(FindingKind::Error, error.to_string())),
        Ok(found) if found != captured => Some(blank(
            FindingKind::Stale,
            format!("captured {captured:?}, found {found:?}"),
        )),
        Ok(_) => None,
    }
}

/// The absolute byte offset `point` names in `content`.
///
/// A trailing newline ends the last line rather than opening an empty one, so
/// the line count matches what an editor shows. A `\r\n` line ends at its `\r`,
/// which puts the terminator out of every column's reach.
fn byte_offset(content: &str, point: Point) -> Result<usize, WalkthroughError> {
    let body = content.strip_suffix('\n').unwrap_or(content);
    let mut offset = 0;
    let mut lines = 0;

    for (index, line) in body.split('\n').enumerate() {
        lines = index as u32 + 1;
        if lines == point.line {
            let text = line.strip_suffix('\r').unwrap_or(line);
            let bytes = text.len();

            // A column is 1-based and names a byte to keep, so the last valid
            // one is the line's length. Zero names nothing at all.
            if point.col == 0 || point.col as usize > bytes {
                return ColumnOutOfBoundsSnafu {
                    line: point.line,
                    col: point.col,
                    bytes,
                }
                .fail();
            }

            return Ok(offset + point.col as usize - 1);
        }

        // Past the newline `split` consumed, so the next line's first byte.
        offset += line.len() + 1;
    }

    LineOutOfBoundsSnafu {
        line: point.line,
        lines: lines as usize,
    }
    .fail()
}

#[cfg(test)]
mod tests {
    use super::{
        snippet_for, validate, Annotation, AnnotationEdit, Finding, FindingKind, Location,
        MoveTarget, Point, Range, StopEdit, Walkthrough,
    };
    use std::path::{Path, PathBuf};

    fn point(line: u32, col: u32) -> Point {
        Point { line, col }
    }

    fn range(start: (u32, u32), end: (u32, u32)) -> Range {
        Range {
            start: point(start.0, start.1),
            end: point(end.0, end.1),
        }
    }

    fn location(path: &str, range: Range, snippet: &str) -> Location {
        Location {
            path: PathBuf::from(path),
            range,
            snippet: snippet.to_owned(),
        }
    }

    /// A walkthrough with `count` stops, each focused on its own file.
    fn with_stops(count: u32) -> Walkthrough {
        let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
        for index in 1..=count {
            walkthrough
                .add_stop(
                    None,
                    format!("stop {index}"),
                    location(&format!("src/{index}.rs"), range((1, 1), (1, 1)), "x"),
                    None,
                )
                .expect("append needs no anchor");
        }
        walkthrough
    }

    fn ids(walkthrough: &Walkthrough) -> Vec<&str> {
        walkthrough
            .stops
            .iter()
            .map(|stop| stop.id.as_str())
            .collect()
    }

    #[test]
    fn a_walkthrough_round_trips_through_json() {
        let mut original = with_stops(1);
        original.git_head = Some("abc123".to_owned());
        original.stops[0].title = Some("Entry point".to_owned());
        original
            .add_annotation(
                "s1",
                None,
                range((1, 1), (1, 1)),
                "x".to_owned(),
                "here".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: Walkthrough = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed, original);
    }

    /// Every walkthrough stored before annotations named files must read back
    /// the same, and keep writing the same bytes it always did.
    #[test]
    fn a_same_file_annotation_stores_no_path() {
        let mut walkthrough = with_stops(1);
        walkthrough
            .add_annotation(
                "s1",
                None,
                range((1, 1), (1, 1)),
                "x".to_owned(),
                "here".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let json = serde_json::to_string(&walkthrough).expect("serialize");
        assert!(
            !json.contains("\"path\":null"),
            "the field is absent: {json}"
        );

        let parsed: Walkthrough = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.stops[0].annotations[0].path, None);
    }

    /// Every walkthrough stored before annotations carried narration must read
    /// back the same, and keep writing the same bytes it always did.
    #[test]
    fn an_unnarrated_annotation_stores_no_narration() {
        let mut walkthrough = with_stops(1);
        walkthrough
            .add_annotation(
                "s1",
                None,
                range((1, 1), (1, 1)),
                "x".to_owned(),
                "here".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let json = serde_json::to_string(&walkthrough.stops[0].annotations[0]).expect("serialize");
        assert!(!json.contains("narration"), "the field is absent: {json}");

        let parsed: Annotation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.narration, "");
    }

    #[test]
    fn an_annotation_narration_round_trips_through_json() {
        let mut walkthrough = with_stops(1);
        walkthrough
            .add_annotation(
                "s1",
                None,
                range((1, 1), (1, 1)),
                "x".to_owned(),
                "here".to_owned(),
                "the **why**".to_owned(),
            )
            .expect("s1 exists");

        let json = serde_json::to_string(&walkthrough).expect("serialize");
        let parsed: Walkthrough = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed, walkthrough);
        assert_eq!(parsed.stops[0].annotations[0].narration, "the **why**");
    }

    #[test]
    fn an_annotation_path_survives_an_edit_back_to_none() {
        let mut walkthrough = with_stops(1);
        walkthrough
            .add_annotation(
                "s1",
                Some(PathBuf::from("other.rs")),
                range((1, 1), (1, 1)),
                "x".to_owned(),
                "l".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let edited = walkthrough
            .edit_annotation(
                "s1",
                "a1",
                AnnotationEdit {
                    path: Some(Some(PathBuf::from("third.rs"))),
                    ..AnnotationEdit::default()
                },
            )
            .expect("a1 exists");
        assert_eq!(edited.path.as_deref(), Some(Path::new("third.rs")));

        let cleared = walkthrough
            .edit_annotation(
                "s1",
                "a1",
                AnnotationEdit {
                    path: Some(None),
                    ..AnnotationEdit::default()
                },
            )
            .expect("a1 exists");
        assert_eq!(cleared.path, None, "an inner None returns it to the focus");
    }

    /// Ids outlive the items that held them. A note or an annotation naming s2
    /// must never come to mean a different stop.
    #[test]
    fn a_removed_id_is_never_handed_out_again() {
        let mut walkthrough = with_stops(2);
        walkthrough.remove_stop("s2").expect("s2 exists");

        let added = walkthrough
            .add_stop(
                None,
                "third".to_owned(),
                location("src/3.rs", range((1, 1), (1, 1)), "x"),
                None,
            )
            .expect("append needs no anchor");
        assert_eq!(added.id, "s3");

        walkthrough
            .add_annotation(
                "s1",
                None,
                range((1, 1), (1, 1)),
                "x".to_owned(),
                "one".to_owned(),
                String::new(),
            )
            .expect("s1 exists");
        walkthrough
            .remove_annotation("s1", "a1")
            .expect("a1 exists");
        let added = walkthrough
            .add_annotation(
                "s1",
                None,
                range((1, 1), (1, 1)),
                "x".to_owned(),
                "two".to_owned(),
                String::new(),
            )
            .expect("s1 exists");
        assert_eq!(added.id, "a2");
    }

    #[test]
    fn add_stop_inserts_before_the_named_stop() {
        let mut walkthrough = with_stops(2);
        walkthrough
            .add_stop(
                None,
                "middle".to_owned(),
                location("src/m.rs", range((1, 1), (1, 1)), "x"),
                Some("s2"),
            )
            .expect("s2 exists");

        assert_eq!(ids(&walkthrough), ["s1", "s3", "s2"]);
    }

    #[test]
    fn move_stop_reaches_every_target() {
        let mut walkthrough = with_stops(3);

        walkthrough
            .move_stop("s3", MoveTarget::Before("s1"))
            .expect("s1 exists");
        assert_eq!(ids(&walkthrough), ["s3", "s1", "s2"]);

        walkthrough
            .move_stop("s3", MoveTarget::After("s1"))
            .expect("s1 exists");
        assert_eq!(ids(&walkthrough), ["s1", "s3", "s2"]);

        walkthrough
            .move_stop("s1", MoveTarget::Last)
            .expect("last needs no anchor");
        assert_eq!(ids(&walkthrough), ["s3", "s2", "s1"]);
    }

    /// A failed move must not eat the stop it lifted out.
    #[test]
    fn a_move_to_an_unknown_anchor_leaves_the_order_alone() {
        let mut walkthrough = with_stops(3);

        let error = walkthrough.move_stop("s2", MoveTarget::Before("s9"));
        assert!(error.is_err(), "s9 does not exist");
        assert_eq!(ids(&walkthrough), ["s1", "s2", "s3"]);
    }

    #[test]
    fn edits_replace_only_the_named_fields() {
        let mut walkthrough = with_stops(1);
        walkthrough
            .edit_stop(
                "s1",
                StopEdit {
                    title: Some(Some("Named".to_owned())),
                    ..StopEdit::default()
                },
            )
            .expect("s1 exists");
        assert_eq!(walkthrough.stops[0].title.as_deref(), Some("Named"));
        assert_eq!(
            walkthrough.stops[0].narration, "stop 1",
            "narration untouched"
        );

        walkthrough
            .edit_stop(
                "s1",
                StopEdit {
                    title: Some(None),
                    ..StopEdit::default()
                },
            )
            .expect("s1 exists");
        assert_eq!(walkthrough.stops[0].title, None, "an inner None clears it");

        walkthrough
            .add_annotation(
                "s1",
                None,
                range((1, 1), (1, 1)),
                "x".to_owned(),
                "old".to_owned(),
                String::new(),
            )
            .expect("s1 exists");
        walkthrough
            .edit_annotation(
                "s1",
                "a1",
                AnnotationEdit {
                    label: Some("new".to_owned()),
                    ..AnnotationEdit::default()
                },
            )
            .expect("a1 exists");
        let annotation = &walkthrough.stops[0].annotations[0];
        assert_eq!(
            (annotation.label.as_str(), annotation.snippet.as_str()),
            ("new", "x")
        );
    }

    #[test]
    fn unknown_ids_are_errors() {
        let mut walkthrough = with_stops(1);

        assert!(walkthrough.remove_stop("s9").is_err(), "unknown stop");
        assert!(
            walkthrough.edit_stop("s9", StopEdit::default()).is_err(),
            "unknown stop"
        );
        assert!(
            walkthrough.remove_annotation("s1", "a9").is_err(),
            "unknown annotation"
        );
        assert!(
            walkthrough
                .add_annotation(
                    "s9",
                    None,
                    range((1, 1), (1, 1)),
                    "x".to_owned(),
                    "l".to_owned(),
                    String::new(),
                )
                .is_err(),
            "unknown stop"
        );
    }

    const CONTENT: &str = "one\ntwo\nthree\n";

    #[test]
    fn snippet_for_covers_both_ends_inclusively() {
        assert_eq!(
            snippet_for(CONTENT, range((1, 1), (1, 1))).ok().as_deref(),
            Some("o")
        );
        assert_eq!(
            snippet_for(CONTENT, range((1, 1), (1, 3))).ok().as_deref(),
            Some("one")
        );
        assert_eq!(
            snippet_for(CONTENT, range((1, 1), (3, 5))).ok().as_deref(),
            Some("one\ntwo\nthree"),
            "a range spans the newlines between its lines"
        );
        assert_eq!(
            snippet_for(CONTENT, range((3, 5), (3, 5))).ok().as_deref(),
            Some("e"),
            "the last byte of the last line is reachable"
        );
    }

    #[test]
    fn snippet_for_rejects_a_range_the_content_does_not_have() {
        assert!(
            snippet_for(CONTENT, range((4, 1), (4, 1))).is_err(),
            "no line 4"
        );
        assert!(
            snippet_for(CONTENT, range((1, 4), (1, 4))).is_err(),
            "line 1 has 3 bytes"
        );
        assert!(
            snippet_for(CONTENT, range((1, 0), (1, 1))).is_err(),
            "columns start at 1"
        );
        assert!(
            snippet_for(CONTENT, range((2, 1), (1, 1))).is_err(),
            "end before start"
        );
        assert!(
            snippet_for("é\n", range((1, 1), (1, 1))).is_err(),
            "a column inside a character splits it"
        );
        assert_eq!(
            snippet_for("é\n", range((1, 1), (1, 2))).ok().as_deref(),
            Some("é"),
            "and the whole character is reachable"
        );
    }

    /// A trailing newline ends the last line rather than opening an empty one,
    /// so the line count matches what an editor shows.
    #[test]
    fn a_trailing_newline_opens_no_line() {
        assert!(
            snippet_for("one\n", range((2, 1), (2, 1))).is_err(),
            "no line 2"
        );
        assert_eq!(
            snippet_for("one\ntwo", range((2, 1), (2, 3)))
                .ok()
                .as_deref(),
            Some("two")
        );
    }

    fn reads(files: &[(&'static str, &'static str)]) -> impl Fn(&Path) -> Option<String> {
        let files: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(path, content)| (PathBuf::from(path), (*content).to_owned()))
            .collect();
        move |path| {
            files
                .iter()
                .find(|(known, _)| known == path)
                .map(|(_, content)| content.clone())
        }
    }

    #[test]
    fn validate_passes_a_walkthrough_that_still_matches() {
        let mut walkthrough = Walkthrough::new("t".to_owned(), "T".to_owned(), None);
        walkthrough
            .add_stop(
                None,
                "n".to_owned(),
                location("a.rs", range((1, 1), (1, 3)), "one"),
                None,
            )
            .expect("append");
        walkthrough
            .add_annotation(
                "s1",
                None,
                range((2, 1), (2, 3)),
                "two".to_owned(),
                "l".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let found = validate(&walkthrough, &reads(&[("a.rs", CONTENT)]));
        assert_eq!(found, Vec::new());
    }

    #[test]
    fn validate_reports_a_missing_file_once_per_stop() {
        let mut walkthrough = Walkthrough::new("t".to_owned(), "T".to_owned(), None);
        walkthrough
            .add_stop(
                None,
                "n".to_owned(),
                location("gone.rs", range((1, 1), (1, 3)), "one"),
                None,
            )
            .expect("append");
        walkthrough
            .add_annotation(
                "s1",
                None,
                range((2, 1), (2, 3)),
                "two".to_owned(),
                "l".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let found = validate(&walkthrough, &reads(&[]));
        assert_eq!(
            found,
            vec![Finding {
                stop: "s1".to_owned(),
                annotation: None,
                kind: FindingKind::Error,
                detail: "cannot read gone.rs".to_owned(),
            }],
            "the annotations point into the same unreadable file",
        );
    }

    #[test]
    fn validate_separates_an_unreadable_range_from_a_drifted_one() {
        let mut walkthrough = Walkthrough::new("t".to_owned(), "T".to_owned(), None);
        walkthrough
            .add_stop(
                None,
                "n".to_owned(),
                location("a.rs", range((9, 1), (9, 1)), "one"),
                None,
            )
            .expect("append");
        walkthrough
            .add_annotation(
                "s1",
                None,
                range((2, 1), (2, 3)),
                "TWO".to_owned(),
                "l".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let found = validate(&walkthrough, &reads(&[("a.rs", CONTENT)]));
        let seen: Vec<(&str, Option<&str>, FindingKind)> = found
            .iter()
            .map(|finding| {
                (
                    finding.stop.as_str(),
                    finding.annotation.as_deref(),
                    finding.kind,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("s1", None, FindingKind::Error),
                ("s1", Some("a1"), FindingKind::Stale),
            ],
            "a range past the end is an error, a changed one is stale",
        );
        assert!(
            found[1].detail.contains("TWO") && found[1].detail.contains("two"),
            "a stale detail shows both what was captured and what is there: {}",
            found[1].detail,
        );
    }

    #[test]
    fn validate_reads_an_annotation_against_the_file_it_names() {
        let mut walkthrough = Walkthrough::new("t".to_owned(), "T".to_owned(), None);
        walkthrough
            .add_stop(
                None,
                "n".to_owned(),
                location("a.rs", range((1, 1), (1, 3)), "one"),
                None,
            )
            .expect("append");
        walkthrough
            .add_annotation(
                "s1",
                Some(PathBuf::from("b.rs")),
                range((1, 1), (1, 3)),
                "far".to_owned(),
                "l".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let found = validate(
            &walkthrough,
            &reads(&[("a.rs", CONTENT), ("b.rs", "far\naway\n")]),
        );
        assert_eq!(found, Vec::new(), "each range met its own file");
    }

    #[test]
    fn validate_blames_the_annotation_for_its_own_unreadable_file() {
        let mut walkthrough = Walkthrough::new("t".to_owned(), "T".to_owned(), None);
        walkthrough
            .add_stop(
                None,
                "n".to_owned(),
                location("a.rs", range((1, 1), (1, 3)), "one"),
                None,
            )
            .expect("append");
        walkthrough
            .add_annotation(
                "s1",
                Some(PathBuf::from("gone.rs")),
                range((1, 1), (1, 3)),
                "far".to_owned(),
                "l".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let found = validate(&walkthrough, &reads(&[("a.rs", CONTENT)]));
        assert_eq!(
            found,
            vec![Finding {
                stop: "s1".to_owned(),
                annotation: Some("a1".to_owned()),
                kind: FindingKind::Error,
                detail: "cannot read gone.rs".to_owned(),
            }],
            "the stop's own focus still reads, so only the annotation is at fault",
        );
    }

    /// A stop whose focus is gone says so once, and still reports the
    /// annotations that point somewhere else.
    #[test]
    fn validate_checks_a_cross_file_annotation_under_a_missing_focus() {
        let mut walkthrough = Walkthrough::new("t".to_owned(), "T".to_owned(), None);
        walkthrough
            .add_stop(
                None,
                "n".to_owned(),
                location("gone.rs", range((1, 1), (1, 3)), "one"),
                None,
            )
            .expect("append");
        walkthrough
            .add_annotation(
                "s1",
                None,
                range((2, 1), (2, 3)),
                "two".to_owned(),
                "same".to_owned(),
                String::new(),
            )
            .expect("s1 exists");
        walkthrough
            .add_annotation(
                "s1",
                Some(PathBuf::from("a.rs")),
                range((1, 1), (1, 3)),
                "NOPE".to_owned(),
                "cross".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        let found = validate(&walkthrough, &reads(&[("a.rs", CONTENT)]));
        let seen: Vec<(Option<&str>, FindingKind)> = found
            .iter()
            .map(|finding| (finding.annotation.as_deref(), finding.kind))
            .collect();
        assert_eq!(
            seen,
            [(None, FindingKind::Error), (Some("a2"), FindingKind::Stale)],
            "a1 shares the unreadable focus, a2 has a file of its own",
        );
    }
}
