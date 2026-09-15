use crate::{
    badge::{Anchor, Badge, BadgeSource, BadgeState},
    render::walkthrough::SlideParts,
    walkthrough::{Annotation, Stop, Walkthrough},
};

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
    /// What the last frame declared for the stop the reader is on.
    ///
    /// Held so a step has something to un-draw. Replaced every frame, so it
    /// always names the parts as they were last seen rather than as they were
    /// first drawn.
    pub(crate) last_parts: SlideParts,
    /// Where this run's mark ids start.
    ///
    /// The terminal latches a mark's timing when its id first appears, so a
    /// second tour opened while the first's strokes are still settling must not
    /// reuse its ids. A base per run is what keeps them apart.
    pub(crate) id_base: u32,
}

/// Where walkthrough mark ids start.
///
/// High enough that nothing else an emitter declares lands in the same range by
/// accident, since a collision would have two things sharing one clock.
pub(crate) const ID_SPACE: u32 = 0x5700_0000;

/// Ids one stop reserves, which caps its drawn annotations at 60.
///
/// A slide with more marks than that is unreadable long before it runs out of
/// room, so the cap costs nothing and keeps a stop's ids contiguous.
pub(crate) const STOP_ID_STRIDE: u32 = 0x100;

/// The parts of a slide, in the order their ids run.
///
/// Derived from the stop and the part rather than allocated, so a re-emitted
/// scene comes out with the same ids and no state has to remember them.
pub(crate) mod part {
    pub(crate) const FOCUS_MARK: u32 = 0;
    pub(crate) const CARD: u32 = 1;
    pub(crate) const FOCUS_LINK: u32 = 2;
    /// The first annotation's three ids, which then repeat every three.
    pub(crate) const ANNOTATION_BASE: u32 = 3;
    pub(crate) const ANNOTATION_STRIDE: u32 = 3;
}

impl WalkthroughRun {
    /// Start `walkthrough` at its first stop, or `None` when it has none.
    ///
    /// Refusing an empty tour here is what lets [`Self::current_stop`] always
    /// have a stop to answer with.
    ///
    /// `run` distinguishes this run's mark ids from an earlier run's, so a
    /// tour opened while another's strokes are still settling does not restart
    /// them. See [`Self::id_base`].
    pub(crate) fn new(walkthrough: Walkthrough, run: u32) -> Option<Self> {
        (!walkthrough.stops.is_empty()).then_some(Self {
            walkthrough,
            stop_idx: 0,
            annotation_idx: None,
            last_parts: SlideParts::default(),
            id_base: ID_SPACE + (run << 16),
        })
    }

    /// The id of `part` for the stop the reader is on.
    ///
    /// See [`part`] for what the offsets name.
    pub(crate) fn part_id(&self, part: u32) -> u32 {
        self.id_base + self.stop_idx as u32 * STOP_ID_STRIDE + part
    }

    /// The mark, connector, and label ids of annotation `index`.
    pub(crate) fn annotation_ids(&self, index: usize) -> (u32, u32, u32) {
        let base = part::ANNOTATION_BASE + part::ANNOTATION_STRIDE * index as u32;
        (
            self.part_id(base),
            self.part_id(base + 1),
            self.part_id(base + 2),
        )
    }

    /// How many of a stop's annotations fit its reserved ids.
    pub(crate) fn drawable_annotations(&self) -> usize {
        let room = STOP_ID_STRIDE - part::ANNOTATION_BASE;
        (room / part::ANNOTATION_STRIDE) as usize
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

    /// Project the run into its workspace badge.
    ///
    /// The slug names which tour is open, since a workspace stores several, and
    /// the count says how far through it the reader has come. The status line
    /// says both on arrival, but a status message expires where a tour does not.
    pub(crate) fn badge(&self) -> Badge {
        let (at, stops) = self.progress();
        Badge {
            source: BadgeSource::Walkthrough,
            anchor: Anchor::BottomRight,
            state: BadgeState::Active,
            label: format!("{} {at}/{stops}", self.walkthrough.slug),
            detail: None,
        }
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

    /// Move `delta` places along the tour read as one sequence, clamped at both
    /// ends.
    ///
    /// That sequence runs a stop's focus, then each of its annotations, then
    /// the next stop's focus. It is the finest unit the tour has, which is what
    /// a reader walking it point by point wants under one gesture.
    ///
    /// Returns whether it moved.
    pub(crate) fn step_linear(&mut self, delta: i32) -> bool {
        let mut moved = false;
        for _ in 0..delta.unsigned_abs() {
            moved |= match delta > 0 {
                true => self.forward_one(),
                false => self.backward_one(),
            };
        }
        moved
    }

    /// The annotation after this one, or the next stop's focus past the last.
    fn forward_one(&mut self) -> bool {
        self.step_annotation(1) || self.step(1)
    }

    /// The exact inverse of [`Self::forward_one`].
    ///
    /// A step back off a focus lands on the previous stop's *last* annotation,
    /// since that is the point a forward step came from. A landing on its focus
    /// instead skips every annotation on the way back.
    fn backward_one(&mut self) -> bool {
        if self.annotation_idx.is_some() {
            return self.step_annotation(-1);
        }
        if !self.step(-1) {
            return false;
        }

        self.annotation_idx = self.current_stop().annotations.len().checked_sub(1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::WalkthroughRun;
    use crate::{
        badge::{Anchor, BadgeState},
        walkthrough::{Location, Point, Range, Walkthrough},
    };
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
        WalkthroughRun::new(walkthrough, 0)
    }

    fn annotate(run: &mut WalkthroughRun, stop: &str, label: &str) {
        run.walkthrough
            .add_annotation(
                stop,
                None,
                Range {
                    start: Point { line: 1, col: 1 },
                    end: Point { line: 1, col: 1 },
                },
                "x".to_owned(),
                label.to_owned(),
                String::new(),
            )
            .expect("the stop exists");
    }

    /// Where a walk has reached, as the stop and which annotation of how many.
    fn position(run: &WalkthroughRun) -> (String, Option<(usize, usize)>) {
        (run.current_stop().id.clone(), run.annotation_progress())
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
            annotate(&mut run, "s1", label);
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
        annotate(&mut run, "s1", "l");
        run.step_annotation(1);

        assert!(run.step(1));
        assert_eq!(
            run.annotation_progress(),
            None,
            "the annotations belonged to the stop just left",
        );
    }

    /// The tour reads as one sequence of attention points, so a walk forward
    /// and straight back retraces exactly the points it visited.
    #[test]
    fn a_linear_walk_visits_every_point_and_retraces_it() {
        let mut run = run(3).expect("three stops");
        for label in ["one", "two"] {
            annotate(&mut run, "s2", label);
        }

        let mut forward = vec![position(&run)];
        while run.step_linear(1) {
            forward.push(position(&run));
        }
        assert_eq!(
            forward,
            [
                ("s1".to_owned(), None),
                ("s2".to_owned(), None),
                ("s2".to_owned(), Some((1, 2))),
                ("s2".to_owned(), Some((2, 2))),
                ("s3".to_owned(), None),
            ],
            "each stop's focus heads its own annotations",
        );

        let mut backward = vec![position(&run)];
        while run.step_linear(-1) {
            backward.push(position(&run));
        }
        forward.reverse();
        assert_eq!(backward, forward, "backward exactly inverts forward");
    }

    #[test]
    fn a_linear_walk_clamps_at_both_ends() {
        let mut run = run(2).expect("two stops");
        annotate(&mut run, "s1", "l");

        assert!(
            run.step_linear(9),
            "a clamped walk that still moves has moved"
        );
        assert_eq!(position(&run), ("s2".to_owned(), None));
        assert!(!run.step_linear(1), "there is nothing past the last point");

        assert!(run.step_linear(-9));
        assert_eq!(position(&run), ("s1".to_owned(), None));
        assert!(!run.step_linear(-1), "there is nothing before the first");
    }

    /// With nothing between two focuses, the linear walk is the stop walk.
    #[test]
    fn a_linear_walk_over_an_unannotated_tour_steps_stops() {
        let mut run = run(3).expect("three stops");

        assert!(run.step_linear(1));
        assert_eq!(position(&run), ("s2".to_owned(), None));

        assert!(run.step_linear(-1));
        assert_eq!(position(&run), ("s1".to_owned(), None));
    }

    /// The badge is what says a tour is open at all, so it names which one and
    /// tracks every step through it.
    #[test]
    fn the_badge_names_the_tour_and_the_stop() {
        let mut run = run(3).expect("three stops");
        assert_eq!(run.badge().label, "tour 1/3");
        assert_eq!(run.badge().anchor, Anchor::BottomRight);
        assert_eq!(run.badge().state, BadgeState::Active);

        run.step(9);
        assert_eq!(run.badge().label, "tour 3/3");
    }

    #[test]
    fn a_stop_with_no_annotations_never_leaves_its_focus() {
        let mut run = run(1).expect("one stop");
        assert!(!run.step_annotation(1));
        assert_eq!(run.annotation_progress(), None);
    }
}
