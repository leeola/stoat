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
            out.push(u);
        }
    }
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

/// Byte range of the `capture_name` object that starts nearest past `after`,
/// or `None` when no object starts past it.
///
/// A tie on the start goes to the longer object, which is the outer one of a
/// nested pair. That is the object a forward goto step (`] f`) lands on.
///
/// A step costs the part of the tree between `after` and that object rather
/// than the rest of the file. A pattern that walks its enclosing list whatever
/// the range, such as rust's `#[test]` sequence, still costs that list.
///
/// See also:
/// - [`collect_capture_ranges`] for every object in a range at once, which a backward step and a
///   press over many cursors read.
pub fn find_next_capture_after(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    after: usize,
) -> Option<Range<usize>> {
    find_next_capture_with_options(
        query,
        root,
        rope,
        capture_name,
        after,
        QueryCursorOptions::new(),
    )
}

/// [`find_next_capture_after`], with `options` on both of its walks, which
/// lets a test count how far they went.
fn find_next_capture_with_options(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    after: usize,
    mut options: QueryCursorOptions<'_>,
) -> Option<Range<usize>> {
    let cap_idx = query.capture_index_for_name(capture_name)?;

    // Matches arrive in the order they finish, not in the order they start. In
    // rust's query a parameter waits for its optional trailing comma, so a
    // parameter list inside its type finishes first. The first object past
    // `after` therefore only bounds the answer.
    let first = {
        let mut cursor_h = QueryCursorHandle::new();
        cursor_h.set_byte_range(after..rope.len());
        let mut matches = cursor_h.matches_with_options(
            query,
            root,
            RopeTextProvider { rope },
            options.reborrow(),
        );
        loop {
            let m = matches.next()?;
            if let Some(union) = capture_union(m, cap_idx).filter(|u| u.start > after) {
                break union;
            }
        }
    };

    // A match whose union starts past `after` and at or before the bound has a
    // root that overlaps this range, or a first node whose parent overlaps it,
    // so the second walk meets every such match. Past the bound the walk starts
    // no rooted pattern and enters a subtree only where an unfinished match or
    // a rootless pattern needs it.
    let mut cursor_h = QueryCursorHandle::new();
    cursor_h.set_byte_range(after..first.start + 1);
    let mut matches =
        cursor_h.matches_with_options(query, root, RopeTextProvider { rope }, options);
    let mut best = first;
    while let Some(m) = matches.next() {
        let Some(union) = capture_union(m, cap_idx).filter(|u| u.start > after) else {
            continue;
        };
        if (union.start, Reverse(union.end)) < (best.start, Reverse(best.end)) {
            best = union;
        }
    }
    Some(best)
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
        collect_capture_ranges, collect_capture_starts, find_next_capture_after,
        find_next_capture_with_options, find_smallest_capture_at, find_smallest_capture_scanning,
    };
    use crate::{Language, LanguageRegistry};
    use std::{
        cmp::Reverse,
        collections::HashMap,
        ops::{ControlFlow, Range},
        sync::Arc,
    };
    use stoat_text::Rope;
    use tree_sitter::{Parser, QueryCursorOptions, QueryCursorState, Tree};

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

                assert_eq!(
                    find_next_capture_after(kind, root, &rope, name, cursor),
                    ranges.iter().find(|r| r.start > cursor).cloned(),
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

    /// A forward step walks the tree only as far as the object it lands on,
    /// not over the functions past it.
    ///
    /// The query cursor runs its progress callback once per hundred steps, so
    /// the count of runs measures how much of the tree a walk visited. The
    /// fixture holds no class, so the class walk visits the whole tree.
    #[test]
    fn a_forward_step_walks_only_to_the_object_it_lands_on() {
        let lang = lang("rust");
        let body = "    let a = 1;\n".repeat(60);
        let src: String = (0..1000)
            .map(|i| format!("fn f{i}() {{\n{body}}}\n"))
            .collect();
        let tree = parse(&lang, &src);
        let rope = Rope::from(src.as_str());
        let walk = |name: &str| {
            let query = lang.textobject_query_for(name).expect("kind query");
            let mut runs = 0;
            let mut count = |_: &QueryCursorState| {
                runs += 1;
                ControlFlow::Continue(())
            };
            let options = QueryCursorOptions::new().progress_callback(&mut count);
            let found =
                find_next_capture_with_options(query, tree.root_node(), &rope, name, 0, options);
            (found, runs)
        };

        let (found, step_runs) = walk("function.around");
        let (no_class, whole_runs) = walk("class.around");
        let second = src.find("fn f1(").expect("second function");
        let third = src.find("fn f2(").expect("third function");
        assert_eq!(
            found,
            Some(second..third - 1),
            "the step lands on the second function"
        );
        assert_eq!(no_class, None, "the class walk finds nothing");
        assert!(
            step_runs < whole_runs * 3 / 1000,
            "the step ran the callback {step_runs} times, where three of the thousand \
             functions take {} of the {whole_runs} runs",
            whole_runs * 3 / 1000
        );
    }
}
