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
use std::cmp::Reverse;
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
    let landings: Vec<(usize, std::ops::Range<usize>)> = cursors
        .into_iter()
        .filter_map(|(id, cursor)| {
            let target = object_range(ws, buffer_id, cursor, kind, direction, count)?;
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
            let Some((_, target)) = landings.iter().find(|(id, _)| *id == sel.id) else {
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

/// Byte range of the object `count` steps away from `cursor`, or `None` when
/// the walk runs out of objects before its first step.
///
/// The backward walk compares each object's end against the cursor rather than
/// its start, which is what steps past the object the cursor is already inside
/// instead of landing on that one again.
fn object_range(
    ws: &crate::workspace::Workspace,
    buffer_id: crate::buffer::BufferId,
    cursor: usize,
    kind: NavKind,
    direction: NavDirection,
    count: u32,
) -> Option<std::ops::Range<usize>> {
    let mut at = cursor;
    let mut found = None;
    for _ in 0..count {
        let ranges =
            collect_capture_ranges_for_buffer(ws, buffer_id, at, kind.capture_name(), direction);
        // Each step travels toward one bound, so that bound picks the winner:
        // the nearest start ahead, or the nearest end behind. A tie on it goes
        // to the longer object, which is the outer one of a nested pair.
        //
        // The list arrives sorted by start, which is the wrong key going back.
        // An outer object starts before the one nested in it and ends after,
        // so reading the greatest start reaches the inner object and steps
        // over the very one containing it.
        let next = match direction {
            NavDirection::Next => ranges
                .iter()
                .filter(|r| r.start > at)
                .min_by_key(|r| (r.start, Reverse(r.end))),
            NavDirection::Prev => ranges
                .iter()
                .filter(|r| r.end < at)
                .max_by_key(|r| (r.end, Reverse(r.start))),
        }
        .cloned()?;
        at = match direction {
            NavDirection::Next => next.start,
            NavDirection::Prev => next.end.saturating_sub(1),
        };
        found = Some(next);
    }
    found
}

fn collect_capture_ranges_for_buffer(
    ws: &crate::workspace::Workspace,
    buffer_id: crate::buffer::BufferId,
    cursor: usize,
    capture_name: &str,
    direction: NavDirection,
) -> Vec<std::ops::Range<usize>> {
    let Some(syntax_map) = ws.buffers.syntax_map(buffer_id) else {
        return Vec::new();
    };
    let snapshot = syntax_map.snapshot();
    let layer = snapshot
        .iter_layers()
        .fold(None::<&stoat_language::SyntaxLayer>, |acc, layer| {
            let start = layer.start_offset as usize;
            let end = layer.end_offset as usize;
            if start <= cursor && end >= cursor {
                match acc {
                    Some(prev) if prev.depth >= layer.depth => acc,
                    _ => Some(layer),
                }
            } else {
                acc
            }
        });
    let Some(layer) = layer else {
        return Vec::new();
    };
    let Some(query) = layer.language.textobjects_query() else {
        return Vec::new();
    };
    let Some(buffer) = ws.buffers.get(buffer_id) else {
        return Vec::new();
    };
    let Ok(guard) = buffer.read() else {
        return Vec::new();
    };
    // Only objects past the cursor answer a forward seek, and only ones before
    // it answer a backward one, so the other side of the buffer need not be
    // visited. The enclosing matches each half also returns are dropped by the
    // caller's comparison, as they were when the whole file was scanned.
    let bytes = match direction {
        NavDirection::Next => cursor..guard.rope().len(),
        NavDirection::Prev => 0..cursor + 1,
    };
    stoat_language::collect_capture_ranges(
        query,
        layer.tree.root_node(),
        guard.rope(),
        capture_name,
        bytes,
    )
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
