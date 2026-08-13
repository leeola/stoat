use crate::walkthrough::{Annotation, Stop, Walkthrough};

/// A stored walkthrough being played, and where in it the reader is.
///
/// The whole walkthrough is held rather than read back per step, so a tour
/// plays the same from start to finish even while the file behind it is edited.
/// Drift against the *source* files is a separate question, reported per jump.
pub(crate) struct WalkthroughRun {
    pub(crate) walkthrough: Walkthrough,
    stop_idx: usize,
    /// Which of the current stop's annotations the reader is on, or `None` for
    /// the stop's own focus.
    ///
    /// The focus and the annotations form one sequence to walk, with the focus
    /// at its head. That is what gives a step away from the focus somewhere to
    /// have come from, and a step back somewhere to return to.
    annotation_idx: Option<usize>,
}

impl WalkthroughRun {
    /// Start `walkthrough` at its first stop, or `None` when it has none.
    ///
    /// Refusing an empty tour here is what lets [`Self::current_stop`] always
    /// have a stop to answer with.
    pub(crate) fn new(walkthrough: Walkthrough) -> Option<Self> {
        (!walkthrough.stops.is_empty()).then_some(Self {
            walkthrough,
            stop_idx: 0,
            annotation_idx: None,
        })
    }

    pub(crate) fn current_stop(&self) -> &Stop {
        &self.walkthrough.stops[self.stop_idx]
    }

    /// The annotation the reader is on, or `None` while they are on the stop's
    /// own focus.
    pub(crate) fn current_annotation(&self) -> Option<&Annotation> {
        self.current_stop().annotations.get(self.annotation_idx?)
    }

    /// Which stop of how many, counted the way a reader says it.
    pub(crate) fn progress(&self) -> (usize, usize) {
        (self.stop_idx + 1, self.walkthrough.stops.len())
    }

    /// Which annotation of how many, or `None` while on the stop's focus.
    pub(crate) fn annotation_progress(&self) -> Option<(usize, usize)> {
        Some((
            self.annotation_idx? + 1,
            self.current_stop().annotations.len(),
        ))
    }

    /// Move `delta` stops, clamped at both ends.
    ///
    /// Returns whether it moved, so a step off either end reports that the tour
    /// is over rather than wrapping around to the other end of it.
    ///
    /// A stop that moves lands on its own focus. The annotations belonged to
    /// the stop just left, and the reader arrives at a new one the same way
    /// they arrive by opening the tour.
    pub(crate) fn step(&mut self, delta: i32) -> bool {
        let max = (self.walkthrough.stops.len() - 1) as i32;
        let next = (self.stop_idx as i32 + delta).clamp(0, max) as usize;
        let moved = next != self.stop_idx;
        self.stop_idx = next;
        if moved {
            self.annotation_idx = None;
        }
        moved
    }

    /// Move `delta` places along the current stop's focus and annotations,
    /// clamped at both ends.
    ///
    /// Returns whether it moved. The focus heads the sequence, so a backward
    /// step off the first annotation returns to it rather than stopping short.
    pub(crate) fn step_annotation(&mut self, delta: i32) -> bool {
        let count = self.current_stop().annotations.len() as i32;
        let at = self.annotation_idx.map_or(0, |idx| idx as i32 + 1);

        let next = (at + delta).clamp(0, count);
        self.annotation_idx = (next > 0).then(|| (next - 1) as usize);
        next != at
    }
}

#[cfg(test)]
mod tests {
    use super::WalkthroughRun;
    use crate::walkthrough::{Location, Point, Range, Walkthrough};
    use std::path::PathBuf;

    fn run(stops: u32) -> Option<WalkthroughRun> {
        let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
        for index in 1..=stops {
            walkthrough
                .add_stop(
                    None,
                    format!("stop {index}"),
                    Location {
                        path: PathBuf::from(format!("{index}.rs")),
                        range: Range {
                            start: Point { line: 1, col: 1 },
                            end: Point { line: 1, col: 1 },
                        },
                        snippet: "x".to_owned(),
                    },
                    None,
                )
                .expect("append needs no anchor");
        }
        WalkthroughRun::new(walkthrough)
    }

    #[test]
    fn an_empty_walkthrough_has_no_run() {
        assert!(run(0).is_none(), "there is no stop to start on");
    }

    #[test]
    fn stepping_clamps_at_both_ends() {
        let mut run = run(3).expect("three stops");
        assert_eq!(run.progress(), (1, 3));

        assert!(run.step(1));
        assert_eq!(run.current_stop().id, "s2");

        assert!(run.step(-5), "a clamped step that still moves has moved");
        assert_eq!(run.progress(), (1, 3));
        assert!(!run.step(-1), "there is nothing before the first stop");

        assert!(run.step(9));
        assert_eq!(run.progress(), (3, 3), "a long step lands on the last stop");
        assert!(!run.step(1), "there is nothing past the last stop");
    }

    /// A stop's focus heads the sequence its annotations continue, so both ends
    /// of that walk are reachable and neither runs past.
    #[test]
    fn annotation_stepping_starts_and_ends_on_the_focus() {
        let mut run = run(1).expect("one stop");
        for label in ["one", "two"] {
            run.walkthrough
                .add_annotation(
                    "s1",
                    None,
                    Range {
                        start: Point { line: 1, col: 1 },
                        end: Point { line: 1, col: 1 },
                    },
                    "x".to_owned(),
                    label.to_owned(),
                )
                .expect("s1 exists");
        }

        assert_eq!(run.annotation_progress(), None, "the focus comes first");
        assert!(!run.step_annotation(-1), "nothing precedes the focus");

        assert!(run.step_annotation(1));
        assert_eq!(run.annotation_progress(), Some((1, 2)));
        assert_eq!(
            run.current_annotation().map(|a| a.label.as_str()),
            Some("one")
        );

        assert!(run.step_annotation(5));
        assert_eq!(
            run.annotation_progress(),
            Some((2, 2)),
            "clamped at the last"
        );
        assert!(
            !run.step_annotation(1),
            "nothing follows the last annotation"
        );

        assert!(run.step_annotation(-9));
        assert_eq!(
            run.annotation_progress(),
            None,
            "a step back reaches the focus"
        );
    }

    #[test]
    fn a_stop_step_returns_to_the_new_stops_focus() {
        let mut run = run(2).expect("two stops");
        run.walkthrough
            .add_annotation(
                "s1",
                None,
                Range {
                    start: Point { line: 1, col: 1 },
                    end: Point { line: 1, col: 1 },
                },
                "x".to_owned(),
                "l".to_owned(),
            )
            .expect("s1 exists");
        run.step_annotation(1);

        assert!(run.step(1));
        assert_eq!(
            run.annotation_progress(),
            None,
            "the annotations belonged to the stop just left",
        );
    }

    #[test]
    fn a_stop_with_no_annotations_never_leaves_its_focus() {
        let mut run = run(1).expect("one stop");
        assert!(!run.step_annotation(1));
        assert_eq!(run.annotation_progress(), None);
    }
}
