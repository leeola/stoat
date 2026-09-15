//! Helpers for textobject queries.
//!
//! `select_textobject_around` / `select_textobject_inner` need to find
//! the smallest tree-sitter capture (under a given name like
//! `function.around`) that contains the cursor. This module wraps the
//! query-cursor + rope-text-provider plumbing into a single function
//! so handlers in the `stoat` crate do not have to construct a
//! `QueryCursor` and `TextProvider` themselves.
//!
//! Pure tree-sitter logic only -- paragraph (line-based) textobjects
//! are handled in the `stoat` crate alongside the action handler.

use crate::highlight::{QueryCursorHandle, RopeTextProvider};
use std::{cmp::Reverse, ops::Range};
use stoat_text::Rope;
use tree_sitter::{Node, Query, QueryCursorOptions, QueryMatch, StreamingIterator};

/// Growths past a window's end that stop at a bound, before a growth takes the
/// rest of the layer.
///
/// A pattern that walks its enclosing list whatever the range, such as rust's
/// `#[test]` sequence, pays that list on every growth. Once a window has grown
/// this many times, its next growth takes the rest of the layer in one walk, so
/// a counted press pays the list a few times at most.
const BOUNDED_GROWTHS: u32 = 3;

/// The objects of one capture that start inside one stretch of a layer.
///
/// A goto press asks for the object past each of its cursors, once per count
/// step. A walk from every cursor pays for the same stretch of tree again, and a
/// pattern that walks its enclosing list pays that list on each walk. A window
/// walks a stretch once, answers every position inside it, and grows over new
/// bytes only when a position needs more.
///
/// Each answer is the object the whole layer's objects give: the nearest start
/// past the position, and the longer object of a tie, which is the outer one of
/// a nested pair.
///
/// See also:
/// - [`collect_capture_ranges`] for every object in a range at once, which a backward step reads.
pub struct ObjectWindow<'a> {
    query: &'a Query,
    root: Node<'a>,
    rope: &'a Rope,
    cap_idx: u32,
    /// The stretch `(lo, hi]`, where every object that starts in it is in
    /// `objects`. `None` before the first walk.
    stretch: Option<(usize, usize)>,
    /// No object starts past `hi`, because the stretch reaches the end of the
    /// layer.
    to_end: bool,
    /// Sorted by `(start, Reverse(end))`, so the first object past a position is
    /// its answer.
    objects: Vec<Range<usize>>,
    /// Growths past `hi` that stopped at a bound.
    bounded_growths: u32,
}

impl<'a> ObjectWindow<'a> {
    /// An empty window over the layer under `root`, or `None` when
    /// `capture_name` is unknown to `query`.
    pub fn new(
        query: &'a Query,
        root: Node<'a>,
        rope: &'a Rope,
        capture_name: &str,
    ) -> Option<Self> {
        Some(ObjectWindow {
            query,
            root,
            rope,
            cap_idx: query.capture_index_for_name(capture_name)?,
            stretch: None,
            to_end: false,
            objects: Vec::new(),
            bounded_growths: 0,
        })
    }

    /// The object a forward step from each of `positions` lands on, in the order
    /// given, or `None` for a position with no object past it.
    ///
    /// The window grows until it answers every position. A growth past its end
    /// walks far enough to hold `ahead` objects in a row past the last position,
    /// so the later steps of a counted press find their objects already inside.
    ///
    /// A call costs the bytes it adds to the window rather than the rest of the
    /// layer. A pattern that walks its enclosing list whatever the range, such as
    /// rust's `#[test]` sequence, also costs that list on each growth.
    pub fn next_after_each(
        &mut self,
        positions: &[usize],
        ahead: usize,
    ) -> Vec<Option<Range<usize>>> {
        self.next_after_each_with_options(positions, ahead, QueryCursorOptions::new())
    }

    /// [`Self::next_after_each`], with `options` on every walk, which lets a test
    /// count how far they went.
    fn next_after_each_with_options(
        &mut self,
        positions: &[usize],
        ahead: usize,
        mut options: QueryCursorOptions<'_>,
    ) -> Vec<Option<Range<usize>>> {
        let Some(&first) = positions.iter().min() else {
            return Vec::new();
        };

        let (lo, hi) = self.stretch.unwrap_or((first, first));
        if first < lo {
            let mut added = self.collect(first, lo, options.reborrow());
            added.append(&mut self.objects);
            self.objects = added;
        }
        let lo = lo.min(first);
        self.stretch = Some((lo, hi));

        if let Some(at) = positions
            .iter()
            .copied()
            .filter(|&at| !self.answers(at))
            .max()
        {
            self.grow_right(lo, hi, at, ahead, options);
        }
        positions
            .iter()
            .map(|&at| self.first_past(at).cloned())
            .collect()
    }

    /// Adds the objects past `hi` that answer `at` and every position before it.
    fn grow_right(
        &mut self,
        lo: usize,
        hi: usize,
        at: usize,
        ahead: usize,
        mut options: QueryCursorOptions<'_>,
    ) {
        // Matches arrive in the order they finish, not in the order they start.
        // In rust's query a parameter waits for its optional trailing comma, so
        // a parameter list inside its type finishes first. The objects the first
        // walk finds therefore only bound the stretch the second walk collects.
        let past = at.max(hi);
        let reach = if self.bounded_growths < BOUNDED_GROWTHS {
            self.bounded_growths += 1;
            bound_ahead(
                self.query,
                self.root,
                self.rope,
                self.cap_idx,
                past,
                ahead,
                options.reborrow(),
            )
        } else {
            Reach::End { none_past: false }
        };

        let (collect_to, end) = match reach {
            Reach::Bound(bound) => (bound, bound),
            Reach::End { none_past: true } => (past, self.rope.len()),
            Reach::End { none_past: false } => (self.rope.len(), self.rope.len()),
        };
        if collect_to > hi {
            let mut added = self.collect(hi, collect_to, options);
            self.objects.append(&mut added);
        }
        self.stretch = Some((lo, end));
        self.to_end = matches!(reach, Reach::End { .. });
    }

    /// The objects that start in `(from, to]`, sorted by `(start, Reverse(end))`.
    fn collect(
        &self,
        from: usize,
        to: usize,
        options: QueryCursorOptions<'_>,
    ) -> Vec<Range<usize>> {
        // A match whose union starts in the stretch has a root that overlaps
        // this range, or a first node whose parent overlaps it, so the walk meets
        // every such match. Past the range the walk starts no rooted pattern and
        // enters a subtree only where an unfinished match or a rootless pattern
        // needs it.
        let mut objects = collect_unions(
            self.query,
            self.root,
            self.rope,
            self.cap_idx,
            from..to + 1,
            options,
        );
        objects.retain(|object| from < object.start && object.start <= to);
        objects.sort_unstable_by_key(|object| (object.start, Reverse(object.end)));
        objects.dedup();
        objects
    }

    /// Whether the window holds the answer for `at`.
    ///
    /// The first object past `at` is the answer when it starts inside the
    /// stretch, because the true answer starts no later and every object that
    /// starts inside the stretch is known.
    fn answers(&self, at: usize) -> bool {
        let Some((lo, hi)) = self.stretch else {
            return false;
        };
        at >= lo && (self.to_end || self.first_past(at).is_some_and(|object| object.start <= hi))
    }

    fn first_past(&self, at: usize) -> Option<&Range<usize>> {
        self.objects
            .get(self.objects.partition_point(|object| object.start <= at))
    }
}

/// How far a growth past a window's end has to collect.
enum Reach {
    /// The start of the last object the growth asked for.
    Bound(usize),
    /// Fewer objects than asked for start past the growth's position, so the
    /// window reaches the end of the layer. When `none_past` is set, no object
    /// starts past that position at all, and only the gap before it holds new
    /// objects.
    End { none_past: bool },
}

/// Sorted, deduplicated byte ranges of every match's `capture_name` union
/// range, over the matches within `bytes`.
///
/// Backs goto-next/prev navigation (`] f` / `[ f` / `] t` / `[ t`), which
/// selects the whole object it lands on rather than only its opening keyword.
/// A caller seeking one direction passes only the bytes that direction
/// answers from. The range bounds which matches are visited, and each still
/// reports its own extents.
///
/// Returns an empty vector when `capture_name` is unknown to `query` or no
/// match yields a capture under that name.
pub fn collect_capture_ranges(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    bytes: Range<usize>,
) -> Vec<Range<usize>> {
    let Some(cap_idx) = query.capture_index_for_name(capture_name) else {
        return Vec::new();
    };
    let mut out = collect_unions(query, root, rope, cap_idx, bytes, QueryCursorOptions::new());
    out.sort_unstable_by_key(|r| (r.start, r.end));
    out.dedup();
    out
}

/// Sorted, deduplicated start byte offsets of every match's
/// `capture_name` union range, over the matches within `bytes`.
///
/// A caller seeking one direction passes only the bytes that direction
/// answers from. The range bounds which matches are visited, and each
/// still reports its own extents. Returns an empty vector when
/// `capture_name` is unknown to `query` or no match yields a
/// capture under that name.
///
/// See also:
/// - [`collect_capture_ranges`] for a caller that needs each match's extent rather than only where
///   it opens.
pub fn collect_capture_starts(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    bytes: Range<usize>,
) -> Vec<usize> {
    let mut out = Vec::new();
    let Some(cap_idx) = query.capture_index_for_name(capture_name) else {
        return out;
    };
    let provider = RopeTextProvider { rope };
    let mut cursor_h = QueryCursorHandle::new();
    cursor_h.set_byte_range(bytes);
    let mut matches = cursor_h.matches(query, root, provider);
    while let Some(m) = matches.next() {
        let mut union: Option<Range<usize>> = None;
        for cap in m.captures {
            if cap.index != cap_idx {
                continue;
            }
            let r = cap.node.byte_range();
            union = Some(match union {
                None => r,
                Some(u) => u.start.min(r.start)..u.end.max(r.end),
            });
        }
        if let Some(u) = union {
            out.push(u.start);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Smallest byte range under `capture_name` in `query` that contains
/// `cursor`. Returns `None` if `capture_name` is unknown to the query
/// or no matching capture brackets `cursor`.
///
/// Ties break toward the innermost match by capture length, which is
/// what a textobject selection wants. `rope` is needed for query
/// predicates (`#eq?`, `#match?`) that read node text.
pub fn find_smallest_capture_at(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    cursor: usize,
) -> Option<Range<usize>> {
    find_smallest_capture_scanning(query, root, rope, capture_name, cursor, true)
}

/// [`find_smallest_capture_at`], with the query restriction made optional so a
/// test can compare the restricted answer against the whole-file one.
fn find_smallest_capture_scanning(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    cursor: usize,
    restrict: bool,
) -> Option<Range<usize>> {
    let cap_idx = query.capture_index_for_name(capture_name)?;
    let provider = RopeTextProvider { rope };
    let mut cursor_h = QueryCursorHandle::new();
    if restrict {
        // Only a union bracketing the cursor can win, and every capture in a
        // match descends from the node the pattern matched, so that node covers
        // the union and the cursor with it. Matches elsewhere cannot answer and
        // need not be visited, even where the union spans the cursor while no
        // single capture node does.
        cursor_h.set_byte_range(cursor..cursor + 1);
    }
    let mut matches = cursor_h.matches(query, root, provider);
    let mut best: Option<Range<usize>> = None;
    while let Some(m) = matches.next() {
        let mut union: Option<Range<usize>> = None;
        for cap in m.captures {
            if cap.index != cap_idx {
                continue;
            }
            let r = cap.node.byte_range();
            union = Some(match union {
                None => r,
                Some(u) => u.start.min(r.start)..u.end.max(r.end),
            });
        }
        let Some(u) = union else { continue };
        if !(u.start <= cursor && cursor < u.end) {
            continue;
        }
        let len = u.end - u.start;
        match &best {
            Some(b) if (b.end - b.start) <= len => {},
            _ => best = Some(u),
        }
    }

    // A capture reaching the end of the buffer is dropped, and the next
    // smallest does not stand in for it. The winner is decided first and only
    // then refused, so a file with no trailing newline offers no object for
    // whatever closes it.
    best.filter(|u| u.start < rope.len() && u.end < rope.len())
}

/// How far past `past` the `ahead` objects in a row reach.
///
/// An object counts in the row when it starts at or past the end of the one
/// counted before it, which skips an object nested inside a counted one. The
/// first object counted is the first to finish past `past`, so the answer for
/// `past` starts no later than the bound.
fn bound_ahead(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    cap_idx: u32,
    past: usize,
    ahead: usize,
    options: QueryCursorOptions<'_>,
) -> Reach {
    let mut cursor_h = QueryCursorHandle::new();
    cursor_h.set_byte_range(past..rope.len());
    let mut matches =
        cursor_h.matches_with_options(query, root, RopeTextProvider { rope }, options);
    let mut counted = 0;
    let mut row_end = past + 1;
    while let Some(m) = matches.next() {
        let Some(union) = capture_union(m, cap_idx).filter(|u| u.start >= row_end) else {
            continue;
        };
        counted += 1;
        if counted >= ahead {
            return Reach::Bound(union.start);
        }
        row_end = union.end.max(union.start + 1);
    }
    Reach::End {
        none_past: counted == 0,
    }
}

/// The union of `cap_idx` captures of every match within `bytes`, unsorted.
fn collect_unions(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    cap_idx: u32,
    bytes: Range<usize>,
    options: QueryCursorOptions<'_>,
) -> Vec<Range<usize>> {
    let mut cursor_h = QueryCursorHandle::new();
    cursor_h.set_byte_range(bytes);
    let mut matches =
        cursor_h.matches_with_options(query, root, RopeTextProvider { rope }, options);
    let mut out = Vec::new();
    while let Some(m) = matches.next() {
        out.extend(capture_union(m, cap_idx));
    }
    out
}

/// Byte span covering every capture of `cap_idx` in `m`, or `None` when the
/// match holds no such capture.
fn capture_union(m: &QueryMatch<'_, '_>, cap_idx: u32) -> Option<Range<usize>> {
    m.captures
        .iter()
        .filter(|cap| cap.index == cap_idx)
        .map(|cap| cap.node.byte_range())
        .reduce(|union, r| union.start.min(r.start)..union.end.max(r.end))
}

#[cfg(test)]
mod tests {
    use super::{
        collect_capture_ranges, collect_capture_starts, collect_unions, find_smallest_capture_at,
        find_smallest_capture_scanning, ObjectWindow,
    };
    use crate::{Language, LanguageRegistry};
    use std::{
        cmp::Reverse,
        collections::HashMap,
        ops::{ControlFlow, Range},
        sync::Arc,
    };
    use stoat_text::Rope;
    use tree_sitter::{Parser, Query, QueryCursorOptions, QueryCursorState, Tree};

    fn lang(name: &str) -> Arc<Language> {
        LanguageRegistry::standard()
            .languages()
            .iter()
            .find(|l| l.name == name)
            .cloned()
            .expect("language registered")
    }

    fn parse(lang: &Language, src: &str) -> Tree {
        let mut parser = Parser::new();
        parser.set_language(&lang.grammar).expect("grammar");
        parser.parse(src, None).expect("parse")
    }

    /// Functions inside impl blocks and beside them, so a cursor in any one of
    /// them has matches above it, below it, and enclosing it. The restriction
    /// has to keep the last kind while skipping the others.
    ///
    /// The tail adds a comment run, a test function, entries, arguments, and
    /// fields, so every kind of capture answers somewhere.
    ///
    /// The parameter of `apply` holds a parameter list in its type. Every
    /// nested parameter finishes before the parameter around it, so a forward
    /// walk that stops at the first object to finish lands inside it. The list
    /// runs long, so the parameter around it finishes more than a hundred
    /// cursor steps later. A stop that reads the cursor's position from the
    /// progress callback therefore lands inside it too.
    fn nested_source() -> String {
        let mut src = String::from("struct A;\n\n");
        for block in 0..4 {
            src.push_str(&format!("impl A{block} {{\n"));
            for f in 0..4 {
                src.push_str(&format!(
                    "    fn m{block}_{f}(&self) -> u32 {{\n        let x = {f};\n        x + 1\n    }}\n"
                ));
            }
            src.push_str("}\n\n");
            src.push_str(&format!("fn free{block}() -> u32 {{\n    {block}\n}}\n\n"));
        }
        src.push_str(&format!(
            "fn apply(f: fn({}), x: u32) {{}}\n\n",
            vec!["u8"; 30].join(", ")
        ));
        src.push_str(
            "// one\n// two\n#[test]\nfn checks() {\n    let v = vec![1, 2];\n    call(v, [3, 4]);\n}\n\nstruct B {\n    a: u32,\n    b: u32,\n}\n",
        );
        src
    }

    /// Every entry point asks the query about a slice rather than the file,
    /// which is sound only if every match that answers still turns up. A press
    /// also asks only its kind's query, which is sound only if that query
    /// answers as the whole one does.
    #[test]
    fn restricting_the_query_finds_what_scanning_the_file_found() {
        let lang = lang("rust");
        let src = nested_source();
        let tree = parse(&lang, &src);
        let rope = Rope::from(src.as_str());
        let query = lang.textobjects_query().expect("textobjects query");
        let root = tree.root_node();
        let whole = 0..src.len();
        let names: Vec<&str> = query
            .capture_names()
            .iter()
            .copied()
            .filter(|name| !name.starts_with('_'))
            .collect();

        // Each name's objects over the whole file, sorted so that the first one
        // past a cursor has the nearest start and is the longer object of a tie.
        let ordered: Vec<Vec<Range<usize>>> = names
            .iter()
            .map(|name| {
                let mut ranges = collect_capture_ranges(query, root, &rope, name, whole.clone());
                ranges.sort_unstable_by_key(|r| (r.start, Reverse(r.end)));
                ranges
            })
            .collect();

        let mut answered = 0;
        let mut pruned = 0;
        let mut answered_by_kind: HashMap<&str, usize> = HashMap::new();
        for cursor in 0..src.len() {
            if !src.is_char_boundary(cursor) {
                continue;
            }

            for name in ["function.around", "function.inside", "class.around"] {
                let restricted =
                    find_smallest_capture_scanning(query, root, &rope, name, cursor, true);
                let whole_file =
                    find_smallest_capture_scanning(query, root, &rope, name, cursor, false);
                assert_eq!(restricted, whole_file, "{name} at offset {cursor}");
                answered += usize::from(whole_file.is_some());
            }

            for (name, ranges) in names.iter().zip(&ordered) {
                let kind = lang
                    .textobject_query_for(name)
                    .unwrap_or_else(|| panic!("{name} has a kind query"));
                let by_kind = find_smallest_capture_at(kind, root, &rope, name, cursor);
                let by_whole = find_smallest_capture_at(query, root, &rope, name, cursor);
                assert_eq!(by_kind, by_whole, "{name} by its kind at offset {cursor}");
                *answered_by_kind.entry(name).or_default() += usize::from(by_whole.is_some());

                let mut window = ObjectWindow::new(kind, root, &rope, name).expect("window");
                assert_eq!(
                    window.next_after_each(&[cursor], 1),
                    [ranges.iter().find(|r| r.start > cursor).cloned()],
                    "next {name} from offset {cursor}"
                );
            }

            // What the caller keeps out of each direction's window, against what
            // it kept when the starts came from the whole file.
            let full = collect_capture_starts(query, root, &rope, "function.around", whole.clone());
            let forward =
                collect_capture_starts(query, root, &rope, "function.around", cursor..src.len());
            let backward =
                collect_capture_starts(query, root, &rope, "function.around", 0..cursor + 1);
            assert_eq!(
                forward.iter().copied().find(|&s| s > cursor),
                full.iter().copied().find(|&s| s > cursor),
                "next function from offset {cursor}"
            );
            assert_eq!(
                backward.iter().copied().rev().find(|&s| s < cursor),
                full.iter().copied().rev().find(|&s| s < cursor),
                "prev function from offset {cursor}"
            );
            pruned += usize::from(forward.len() < full.len() || backward.len() < full.len());
        }

        assert!(
            answered > 100,
            "the fixture has to put the cursor inside plenty of captures, or the \
             comparison above was None against None: {answered}"
        );
        assert!(
            pruned > 100,
            "and the windows have to actually drop matches, or they were the whole \
             file and nothing was restricted: {pruned}"
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| answered_by_kind.get(*name).copied().unwrap_or(0) == 0)
                .collect::<Vec<_>>(),
            Vec::<&&str>::new(),
            "every capture answers somewhere, or its kind comparison was None against None",
        );
    }

    /// A window answers every cursor of every count step as the whole file's
    /// objects do, whether the cursors spread out, cluster, or stand alone.
    ///
    /// Each step moves every cursor to the last byte of the object it reached,
    /// the way a counted press resumes. Later steps read what earlier growths
    /// left in the window and grow it where they run past. The last call asks
    /// from the start of the file, which grows a window with a later start the
    /// other way.
    #[test]
    fn a_window_answers_each_press_as_the_index_does() {
        let lang = lang("rust");
        let src = nested_source();
        let tree = parse(&lang, &src);
        let rope = Rope::from(src.as_str());
        let root = tree.root_node();
        let cursor_sets = [
            (0..src.len()).step_by(37).collect(),
            (400..440).collect(),
            vec![src.len() / 2],
            vec![0],
        ];

        for name in [
            "function.around",
            "class.around",
            "parameter.around",
            "comment.around",
            "test.around",
            "entry.around",
        ] {
            let query = lang.textobject_query_for(name).expect("kind query");
            let mut ordered = collect_capture_ranges(query, root, &rope, name, 0..src.len());
            ordered.sort_unstable_by_key(|r| (r.start, Reverse(r.end)));
            let indexed = |at: usize| ordered.iter().find(|r| r.start > at).cloned();

            for cursors in &cursor_sets {
                for count in [1, 2, 5, 40] {
                    let mut window = ObjectWindow::new(query, root, &rope, name).expect("window");
                    let mut positions: Vec<usize> = cursors.clone();
                    for step in 0..count {
                        let answers = window.next_after_each(&positions, count - step);
                        let expected: Vec<_> = positions.iter().map(|&at| indexed(at)).collect();
                        assert_eq!(
                            answers,
                            expected,
                            "{name}, step {step} of {count} from {} cursors",
                            cursors.len()
                        );
                        positions = answers.into_iter().flatten().map(|r| r.end - 1).collect();
                    }
                    assert_eq!(
                        window.next_after_each(&[0], 1),
                        [indexed(0)],
                        "{name}, a later call from the start of the file"
                    );
                }
            }
        }
    }

    /// A window walks the stretch its cursors cover rather than the whole tree.
    ///
    /// The query cursor runs its progress callback once per hundred steps, so
    /// the count of runs measures how much of the tree a walk visited. The
    /// fixture holds a thousand functions and one struct at its end, so the
    /// class past any cursor lies at the far end.
    #[test]
    fn a_window_walks_only_the_stretch_its_cursors_cover() {
        let lang = lang("rust");
        let body = "    let a = 1;\n".repeat(60);
        let mut src: String = (0..1000)
            .map(|i| format!("fn f{i}() {{\n{body}}}\n"))
            .collect();
        src.push_str("struct Tail {}\n");
        let tree = parse(&lang, &src);
        let rope = Rope::from(src.as_str());
        let root = tree.root_node();
        let counting = |name: &str, walk: &mut dyn FnMut(&Query, QueryCursorOptions<'_>)| {
            let query = lang.textobject_query_for(name).expect("kind query");
            let mut runs = 0;
            let mut count = |_: &QueryCursorState| {
                runs += 1;
                ControlFlow::Continue(())
            };
            walk(
                query,
                QueryCursorOptions::new().progress_callback(&mut count),
            );
            runs
        };
        let step = |name: &str, positions: &[usize]| {
            let mut answers = Vec::new();
            let runs = counting(name, &mut |query, options| {
                let mut window = ObjectWindow::new(query, root, &rope, name).expect("window");
                answers = window.next_after_each_with_options(positions, 1, options);
            });
            (answers, runs)
        };

        let whole = counting("function.around", &mut |query, options| {
            let cap_idx = query
                .capture_index_for_name("function.around")
                .expect("capture");
            collect_unions(query, root, &rope, cap_idx, 0..src.len(), options);
        });
        let three_functions = whole * 3 / 1000;
        let second = src.find("fn f1(").expect("second function");
        let third = src.find("fn f2(").expect("third function");

        let (answers, runs) = step("function.around", &[0]);
        assert_eq!(
            answers,
            [Some(second..third - 1)],
            "one step lands on the second function"
        );
        assert!(
            runs < three_functions,
            "one step ran the callback {runs} times, where three functions take {three_functions}"
        );

        let mut landed = Vec::new();
        let runs = counting("function.around", &mut |query, mut options| {
            let mut window =
                ObjectWindow::new(query, root, &rope, "function.around").expect("window");
            let mut at = 0;
            for step in 0..40 {
                let next = window
                    .next_after_each_with_options(&[at], 40 - step, options.reborrow())
                    .remove(0)
                    .expect("a function ahead");
                at = next.end - 1;
                landed.push(next.start);
            }
        });
        let starts: Vec<usize> = (1..=40)
            .map(|i| src.find(&format!("fn f{i}(")).expect("function"))
            .collect();
        assert_eq!(
            landed, starts,
            "forty steps land on the next forty functions"
        );
        let hundred_functions = whole * 100 / 1000;
        assert!(
            runs < hundred_functions,
            "forty steps ran the callback {runs} times, where a hundred functions take \
             {hundred_functions}"
        );

        let cluster: Vec<usize> = (10..second).step_by(97).collect();
        let (answers, runs) = step("function.around", &cluster);
        assert_eq!(
            answers,
            vec![Some(second..third - 1); cluster.len()],
            "every cursor in the first function lands on the second"
        );
        assert!(
            runs < three_functions,
            "a cluster ran the callback {runs} times, where three functions take {three_functions}"
        );

        let (answers, runs) = step("parameter.around", &[0]);
        assert_eq!(answers, [None], "no function takes a parameter");
        assert!(
            runs < whole + whole / 10,
            "a step with nothing ahead ran the callback {runs} times, where a whole walk takes \
             {whole}"
        );

        let tail = src.find("struct Tail").expect("tail struct");
        let spread: Vec<usize> = (0..200).map(|i| i * tail / 200).collect();
        let (answers, runs) = step("class.around", &spread);
        assert_eq!(
            answers,
            vec![Some(tail..src.len() - 1); spread.len()],
            "every cursor lands on the struct at the end"
        );
        assert!(
            runs <= 2 * whole,
            "200 cursors ran the callback {runs} times, where a whole walk takes {whole}"
        );
    }
}
