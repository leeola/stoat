//! Helix-parity goto-next/prev navigation for tree-sitter textobjects.
//!
//! Bound to `] f` / `[ f` (function) and `] t` / `[ t` (class) in the
//! `bracket_next` / `bracket_prev` modes. Each selection reads the active
//! buffer's textobjects query from its own cursor, filters the matches by
//! direction, and takes the whole object it lands on rather than the keyword
//! that opens it.
//!
//! Selection (`m a` / `m i`) lives in the sibling
//! [`crate::action_handlers::textobject`] module; this file is the
//! directional cousin (jump rather than expand-around).

use crate::{
    action_handlers::movement,
    app::{Stoat, UpdateEffect},
    pane::View,
};
use std::{cmp::Reverse, collections::HashMap};
use stoat_text::{Bias, Selection, SelectionGoal};

/// Object kinds the unimpaired menu steps between.
///
/// Each names a capture prefix in a language's textobjects query. A language
/// whose query lacks one answers no matches, so its motion is a no-op there
/// rather than an error.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NavKind {
    Function,
    Class,
    Parameter,
    Comment,
    Test,
    Entry,
    XmlElement,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NavDirection {
    Next,
    Prev,
}

impl NavKind {
    fn capture_name(self) -> &'static str {
        match self {
            NavKind::Function => "function.around",
            NavKind::Class => "class.around",
            NavKind::Parameter => "parameter.around",
            NavKind::Comment => "comment.around",
            NavKind::Test => "test.around",
            NavKind::Entry => "entry.around",
            NavKind::XmlElement => "xml-element.around",
        }
    }
}

pub(crate) fn goto_textobject(
    stoat: &mut Stoat,
    kind: NavKind,
    direction: NavDirection,
) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1).max(1);
    goto_textobject_impl(stoat, kind, direction, count)
}

/// [`goto_textobject`] with its count supplied rather than read from the
/// pending keypress, so a replay repeats the count the motion was made with.
pub(crate) fn goto_textobject_impl(
    stoat: &mut Stoat,
    kind: NavKind,
    direction: NavDirection,
    count: u32,
) -> UpdateEffect {
    stoat.last_motion = Some(crate::action_handlers::LastMotion::TsObject {
        kind,
        dir: direction,
        count,
    });
    let extend = stoat.in_select_mode();
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    let cursors: Vec<(usize, usize)> = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let tail = buffer_snapshot.resolve_anchor(&sel.tail());
                let head = buffer_snapshot.resolve_anchor(&sel.head());
                (sel.id, stoat_text::cursor_offset(rope, tail, head))
            })
            .collect()
    };

    // Every selection reads from its own cursor, so a multi-cursor set walks to
    // one object each rather than sharing whichever cursor happened to be
    // newest.
    // One index per layer, shared by every cursor of the press. The query
    // behind it is milliseconds on a large file, and it used to run once per
    // cursor per count step.
    let mut indexes: Vec<(*const stoat_language::Tree, ObjectIndex)> = Vec::new();
    let landings: HashMap<usize, std::ops::Range<usize>> = cursors
        .into_iter()
        .filter_map(|(id, cursor)| {
            let target = object_range(ws, buffer_id, &mut indexes, cursor, kind, direction, count)?;
            Some((id, target))
        })
        .collect();

    if landings.is_empty() {
        return UpdateEffect::None;
    }

    // The origin goes on the jumplist before the motion lands, so the jump back
    // returns to where the reader was reading rather than to the object.
    crate::action_handlers::jump::push_jump(stoat);

    let editor = crate::action_handlers::focused_editor_mut(stoat).expect("editor still exists");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    editor
        .selections
        .transform_resolved(buffer_snapshot, |sel, _head_offset, tail_offset| {
            let Some(target) = landings.get(&sel.id) else {
                return sel.clone();
            };

            let (start, end, reversed) = if extend {
                movement::extend_span(buffer_snapshot.rope(), tail_offset, target)
            } else {
                // The walk sets the direction, so a forward step leaves its
                // cursor at the object's end and a backward step at its start.
                // A repeat then carries on the way it went.
                (
                    target.start,
                    target.end,
                    matches!(direction, NavDirection::Prev),
                )
            };

            Selection {
                id: sel.id,
                start: buffer_snapshot.anchor_at(start, Bias::Right),
                end: buffer_snapshot.anchor_at(end, Bias::Left),
                reversed,
                goal: SelectionGoal::None,
            }
        });
    UpdateEffect::Redraw
}

/// Every object of one capture in one layer, ordered for both directions of
/// walk.
///
/// Built once per layer per press. The query used to run per cursor per count
/// step, and on a two-thousand-line file each run is milliseconds, so a
/// three-cursor press with a count of three paid for nine of them.
///
/// Two orderings rather than one, because a step reads whichever bound it
/// travels toward and a sort by the other bound puts the wrong object first. An
/// outer object starts before the one nested in it and ends after, so reading
/// the greatest start going backward reaches the inner object and steps over
/// the very one containing it.
struct ObjectIndex {
    /// Sorted by `(start, Reverse(end))`, so the first entry past a cursor is
    /// the nearest object ahead and a tie on the start goes to the longer of
    /// the two, which is the outer one.
    by_start: Vec<std::ops::Range<usize>>,
    /// Sorted by `(end, Reverse(start))`, the same rule against the other
    /// bound.
    by_end: Vec<std::ops::Range<usize>>,
}

impl ObjectIndex {
    /// Run `capture_name`'s patterns over the whole of `layer` and order the
    /// result both ways.
    fn build(
        layer: &stoat_language::SyntaxLayer,
        rope: &stoat_text::Rope,
        capture_name: &str,
    ) -> ObjectIndex {
        let Some(query) = layer.language.textobjects_query() else {
            return ObjectIndex {
                by_start: Vec::new(),
                by_end: Vec::new(),
            };
        };

        let mut by_start = stoat_language::collect_capture_ranges(
            query,
            layer.tree.root_node(),
            rope,
            capture_name,
            0..rope.len(),
        );
        by_start.sort_unstable_by_key(|r| (r.start, Reverse(r.end)));

        let mut by_end = by_start.clone();
        by_end.sort_unstable_by_key(|r| (r.end, Reverse(r.start)));

        ObjectIndex { by_start, by_end }
    }

    /// The nearest object starting after `at`, or `None` past the last one.
    fn next_after(&self, at: usize) -> Option<&std::ops::Range<usize>> {
        self.by_start
            .get(self.by_start.partition_point(|r| r.start <= at))
    }

    /// The nearest object ending before `at`, or `None` before the first one.
    fn prev_before(&self, at: usize) -> Option<&std::ops::Range<usize>> {
        let past = self.by_end.partition_point(|r| r.end < at);
        // The partition point names the first entry at or after `at`, so the
        // one before it is the last that ends strictly earlier. Sorting by
        // `(end, Reverse(start))` puts the longer of two objects sharing an end
        // first, so a tie is read off the front of the run rather than its
        // back.
        let run_end = self.by_end[..past].last()?.end;
        self.by_end[..past]
            .iter()
            .rev()
            .take_while(|r| r.end == run_end)
            .last()
    }
}
/// Byte range of the object `count` steps away from `cursor`, or `None` when
/// the walk runs out of objects before its first step.
///
/// `indexes` is filled as layers are met and shared across every cursor of the
/// press, keyed on the layer's tree. Cursors can sit in different layers, so
/// the index cannot simply be built once for the buffer.
///
/// The backward walk compares each object's end against the cursor rather than
/// its start, which is what steps past the object the cursor is already inside
/// instead of landing on that one again.
fn object_range(
    ws: &crate::workspace::Workspace,
    buffer_id: crate::buffer::BufferId,
    indexes: &mut Vec<(*const stoat_language::Tree, ObjectIndex)>,
    cursor: usize,
    kind: NavKind,
    direction: NavDirection,
    count: u32,
) -> Option<std::ops::Range<usize>> {
    let buffer = ws.buffers.get(buffer_id)?;
    let guard = buffer.read().ok()?;
    let rope = guard.rope();
    let len = rope.len();
    let syntax_map = ws.buffers.syntax_map(buffer_id)?;
    let snapshot = syntax_map.snapshot();

    let mut at = cursor;
    let mut found = None;
    for _ in 0..count {
        // Resolved per step because a step can cross into an injected region,
        // whose own grammar decides what an object is there.
        let Some(layer) = super::surround::deepest_layer_at(Some(snapshot), at) else {
            break;
        };
        let key = &layer.tree as *const _;
        let idx = match indexes.iter().position(|(seen, _)| *seen == key) {
            Some(idx) => idx,
            None => {
                indexes.push((key, ObjectIndex::build(layer, rope, kind.capture_name())));
                indexes.len() - 1
            },
        };

        let next = match direction {
            NavDirection::Next => indexes[idx].1.next_after(at),
            NavDirection::Prev => indexes[idx].1.prev_before(at),
        }
        .cloned();
        // A count reaching past the last object walks as far as it goes. Giving
        // up on the step that runs out throws away the ground already covered,
        // where the press asked to go as far as the objects allow.
        let Some(next) = next else { break };
        // An object reaching the end of the buffer is refused, and the next
        // candidate does not stand in for it, so a file with no trailing
        // newline offers no object for whatever closes it. The refusal comes
        // after the winner is chosen for that reason, which is where the
        // selection side of textobjects puts it too.
        if next.start >= len || next.end >= len {
            break;
        }
        // Both directions resume from the last byte of the object just taken.
        // Resuming a forward step at the object's start leaves everything
        // nested inside it still ahead, so the next step descends rather than
        // moving on.
        at = next.end.saturating_sub(1);
        found = Some(next);
    }
    found
}

#[cfg(test)]
mod tests {
    use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};
    use std::path::PathBuf;
    use stoat_action::{
        GotoNextClass, GotoNextComment, GotoNextEntry, GotoNextFunction, GotoNextParameter,
        GotoNextTest, GotoNextXmlElement, GotoPrevClass, GotoPrevFunction, OpenFile,
    };

    fn seed(h: &mut TestHarness, name: &str, contents: &str) -> PathBuf {
        let root = PathBuf::from("/textobject-nav-test");
        let path = root.join(name);
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.stoat.drive_background();
        let _ = h.stoat.render();
        h.settle();
        h.stoat.drive_background();
        let _ = h.stoat.render();
        h.settle();
        path
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        stoat_text::cursor_offset(
            buf_snap.rope(),
            buf_snap.resolve_anchor(&sel.tail()),
            buf_snap.resolve_anchor(&sel.head()),
        )
    }

    fn jump(h: &mut TestHarness, offset: usize) {
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, offset);
    }

    /// Text the primary selection covers, which is the object the motion
    /// landed on rather than the offset it starts at.
    fn selected<'s>(h: &mut TestHarness, src: &'s str) -> &'s str {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        &src[start..end]
    }

    /// The motion selects the whole object, not only the keyword that opens it.
    #[test]
    fn next_function_selects_whole_body() {
        let src = "fn alpha() {}\nfn beta() { 1 }\nfn gamma() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(selected(&mut h, src), "fn beta() { 1 }");
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(selected(&mut h, src), "fn gamma() {}");
    }

    /// The motion records where it left, so a jump back returns to the reading
    /// position rather than to the object.
    #[test]
    fn next_function_pushes_a_jump() {
        let src = "fn alpha() {}\nfn beta() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 3);
        let origin = cursor_offset(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_ne!(cursor_offset(&mut h), origin, "the motion moved");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(cursor_offset(&mut h), origin);
    }

    /// In select mode the motion grows the selection out to the object rather
    /// than replacing it.
    #[test]
    fn select_mode_next_function_extends_to_the_object() {
        let src = "fn alpha() {}\nfn beta() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);
        h.type_keys("v");

        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(
            selected(&mut h, src),
            "fn alpha() {}\nfn beta() {}",
            "the span reaches from where it started through the object",
        );
    }

    /// The anchor of a reversed selection is its right end, so extending
    /// forward releases everything to the left of it.
    #[test]
    fn select_mode_next_function_extends_from_a_reversed_anchor() {
        let src = "fn alpha() {}\nfn beta() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 6);
        h.type_keys("v h h h");
        assert_eq!(h.selection_spans(), vec![(3, 7, true)], "reversed to start");

        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(
            h.selection_spans(),
            vec![(7, 26, false)],
            "the span starts at the anchor, not at the head it just left",
        );
    }

    /// Each new object kind names a capture prefix the query already knows, so
    /// the menu reaches parameters, comments, tests, and entries.
    #[test]
    fn the_object_motions_reach_each_kind() {
        let src = "fn a() {}\n// note\n#[test]\nfn t(alpha: u8) { [77, 88]; }\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();

        for (action, expected) in [
            (&GotoNextComment as &dyn stoat_action::Action, "// note"),
            (&GotoNextParameter, "alpha: u8"),
            (&GotoNextEntry, "77"),
            (&GotoNextTest, "fn t(alpha: u8) { [77, 88]; }"),
        ] {
            jump(&mut h, 0);
            crate::action_handlers::dispatch(&mut h.stoat, action);
            assert_eq!(selected(&mut h, src), expected);
        }
    }

    /// A kind no shipped query captures answers nothing, which leaves its key a
    /// no-op rather than an error.
    ///
    /// The buffer holds objects a captured kind reaches, so the motion standing
    /// still says the capture is absent rather than the file is empty.
    #[test]
    fn an_uncaptured_object_kind_is_a_noop() {
        let src = "fn a() {}\nfn b() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);
        let before = cursor_offset(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextXmlElement);
        assert_eq!(cursor_offset(&mut h), before);
    }

    /// The whole menu is reachable from select mode, and a motion reached that
    /// way extends.
    ///
    /// The chord sits in a bracket submode when its second key arrives, so the
    /// motion asks whether the pane builds a selection rather than comparing
    /// the mode against `select` by name.
    #[test]
    fn the_select_bracket_menu_reaches_the_object_motions() {
        let src = "fn a() {}\nfn b() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);

        h.type_keys("v");
        h.type_keys("] f");
        assert_eq!(h.stoat.focused_mode(), "select", "and it returns to select");
        assert_eq!(
            selected(&mut h, src),
            "fn a() {}\nfn b() {}",
            "the span grew out to the object rather than replacing the selection",
        );
    }

    /// A count steps that many objects in one press.
    #[test]
    fn count_prefix_next_function() {
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);

        h.stoat.pending_count = Some(2);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(selected(&mut h, src), "fn gamma() {}");
    }

    /// Stepping back from inside a function reaches the one before it, not the
    /// one the cursor is already in.
    ///
    /// The backward walk compares each object's end against the cursor. The
    /// enclosing function's end is past the cursor, so it is skipped where a
    /// comparison on its start matches it.
    #[test]
    fn prev_function_from_inside_body() {
        let src = "fn alpha() {}\nfn beta() { 1 }\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.find("1").expect("body"));

        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevFunction);
        assert_eq!(selected(&mut h, src), "fn alpha() {}");
    }

    /// A counted step resumes past the object it reached, so the second step
    /// leaves the first object rather than descending into it.
    ///
    /// Everything nested inside the first function starts after the first
    /// function's own start, so resuming there leaves the inner one still
    /// ahead and the second step lands on it.
    #[test]
    fn count_prefix_next_function_steps_over_a_nested_one() {
        let src = "fn head() { 1 }\nfn outer() { fn inner() {} }\nfn tail() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.find('1').expect("body"));

        h.stoat.pending_count = Some(2);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(selected(&mut h, src), "fn tail() {}");
    }

    /// A count reaching past the last object walks as far as it goes and stops
    /// there, rather than abandoning the whole motion.
    ///
    /// A user pressing a large count means "as far as this goes", and giving
    /// up on the step that runs out throws away the ground already covered.
    #[test]
    fn count_prefix_past_the_last_function_keeps_the_last_reached() {
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);

        h.stoat.pending_count = Some(5);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(selected(&mut h, src), "fn gamma() {}");
    }

    /// A backward step lands on the object whose end is nearest, which for a
    /// nested pair is the one written around the other.
    ///
    /// Both functions above the cursor end before it, so both are candidates.
    /// The outer one starts first and ends last, so picking by start reaches
    /// the inner function and steps over the very object containing it.
    #[test]
    fn prev_function_takes_the_outer_of_a_nested_pair() {
        let src = "fn outer() { fn inner() {} }\nfn after() { 1 }\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.find('1').expect("body"));

        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevFunction);
        assert_eq!(selected(&mut h, src), "fn outer() { fn inner() {} }");
    }

    /// A forward step lands on the nearest start ahead, so a nested pair is
    /// entered at the outer function rather than the one written inside it.
    ///
    /// The backward step reads the far bound and the forward one the near
    /// bound, and only the backward rule changes which of a nested pair wins.
    /// This holds the forward answer steady across that.
    #[test]
    fn next_function_takes_the_nearest_start() {
        let src = "fn before() { 1 }\nfn outer() { fn inner() {} }\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.find('1').expect("body"));

        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(selected(&mut h, src), "fn outer() { fn inner() {} }");
    }

    /// Every cursor walks from where it stands, so three cursors reach three
    /// different objects. The index behind the walk is shared between them, and
    /// a shared index that answered from one cursor's position would land all
    /// three on the same object.
    #[test]
    fn each_cursor_steps_to_its_own_function() {
        let src = "fn one() { zz }\nfn two() {}\nfn three() { zz }\nfn four() {}\nfn five() { zz }\nfn six() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();

        // One cursor in every marked body, which is one in every other
        // function.
        h.type_keys("%");
        h.type_keys("s");
        h.type_text("zz");
        h.type_keys("Enter");
        assert_eq!(h.selection_spans().len(), 3, "one cursor per marked body");

        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);

        let landed: Vec<&str> = h
            .selection_spans()
            .into_iter()
            .map(|(start, end, _)| &src[start..end])
            .collect();
        assert_eq!(
            landed,
            ["fn two() {}", "fn four() {}", "fn six() {}"],
            "each cursor took the function after its own",
        );
    }

    /// Alt-. after a function jump repeats the jump, since the motion records
    /// itself the way a find does.
    #[test]
    fn repeat_last_motion_replays_a_function_jump() {
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RepeatLastMotion);
        assert_eq!(selected(&mut h, src), "fn gamma() {}");
    }

    /// An object running to the end of the buffer offers nothing to step to,
    /// the way `mi f` already refuses to select one.
    ///
    /// A file with no trailing newline ends inside its last function, so that
    /// function has no closing boundary to land on.
    #[test]
    fn next_function_refuses_an_object_at_the_buffer_end() {
        let src = "fn a() {}\nfn b() {}";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);

        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(
            cursor_offset(&mut h),
            before,
            "the last function is refused"
        );
    }

    /// A refused object is not replaced by the next candidate, so a nested one
    /// ending before the buffer does not stand in for the outer one.
    ///
    /// The refusal comes after the winner is chosen. Dropping the reaching
    /// objects from the candidates first leaves the inner function as the
    /// nearest start ahead, and the step then lands inside the very object the
    /// rule refuses.
    #[test]
    fn a_refused_object_does_not_fall_back_to_a_nested_one() {
        let src = "fn a() {}\nfn outer() { fn inner() {} }";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);

        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(
            cursor_offset(&mut h),
            before,
            "the inner function does not replace the refused outer one",
        );
    }

    #[test]
    fn next_function_no_op_after_last() {
        let src = "fn only() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        let after_last = src.len() - 1;
        jump(&mut h, after_last);
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn prev_function_jumps_backward() {
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.len());
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevFunction);
        let last = cursor_offset(&mut h);
        assert_eq!(&src[last..last + 9], "fn gamma(");
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevFunction);
        let mid = cursor_offset(&mut h);
        assert_eq!(&src[mid..mid + 8], "fn beta(");
    }

    #[test]
    fn prev_function_no_op_before_first() {
        let src = "fn only() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevFunction);
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn next_class_finds_struct_enum_trait_impl() {
        let src = "struct Foo {}\nenum Bar { A }\ntrait Baz {}\nimpl Foo {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextClass);
        assert_eq!(selected(&mut h, src), "enum Bar { A }");
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextClass);
        assert_eq!(selected(&mut h, src), "trait Baz {}");
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextClass);
        assert_eq!(selected(&mut h, src), "impl Foo {}");
    }

    #[test]
    fn prev_class_jumps_backward_through_definitions() {
        let src = "struct Foo {}\nenum Bar { A }\nimpl Foo {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.len());
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevClass);
        let last = cursor_offset(&mut h);
        assert!(src[last..].starts_with("impl Foo"), "{}", &src[last..]);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevClass);
        let mid = cursor_offset(&mut h);
        assert!(src[mid..].starts_with("enum Bar"), "{}", &src[mid..]);
    }

    #[test]
    fn json_buffer_with_no_textobjects_query_is_noop() {
        let src = "{\"a\": 1, \"b\": 2}\n";
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "data.json", src);
        h.settle();
        jump(&mut h, 0);
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(cursor_offset(&mut h), before);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevClass);
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn next_function_via_bracket_chord() {
        let src = "fn alpha() {}\nfn beta() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);
        h.type_keys("] f");
        assert_eq!(selected(&mut h, src), "fn beta() {}");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn prev_class_via_bracket_chord() {
        let src = "struct Foo {}\nstruct Bar {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.len());
        h.type_keys("[ t");
        let off = cursor_offset(&mut h);
        assert!(src[off..].starts_with("struct Bar"), "{}", &src[off..]);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }
}
