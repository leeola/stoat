use crate::walkthrough::{Stop, Walkthrough};

/// A stored walkthrough being played, and which stop the reader is on.
///
/// The whole walkthrough is held rather than read back per step, so a tour
/// plays the same from start to finish even while the file behind it is edited.
/// Drift against the *source* files is a separate question, reported per jump.
pub(crate) struct WalkthroughRun {
    pub(crate) walkthrough: Walkthrough,
    stop_idx: usize,
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
        })
    }

    pub(crate) fn current_stop(&self) -> &Stop {
        &self.walkthrough.stops[self.stop_idx]
    }

    /// Which stop of how many, counted the way a reader says it.
    pub(crate) fn progress(&self) -> (usize, usize) {
        (self.stop_idx + 1, self.walkthrough.stops.len())
    }

    /// Move `delta` stops, clamped at both ends.
    ///
    /// Returns whether it moved, so a step off either end reports that the tour
    /// is over rather than wrapping around to the other end of it.
    pub(crate) fn step(&mut self, delta: i32) -> bool {
        let max = (self.walkthrough.stops.len() - 1) as i32;
        let next = (self.stop_idx as i32 + delta).clamp(0, max) as usize;
        let moved = next != self.stop_idx;
        self.stop_idx = next;
        moved
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
}
