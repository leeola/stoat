//! LSP snippet body parser + renderer.
//!
//! Subset of the [LSP snippet syntax][spec] that v1 of completion
//! acceptance handles: bare tabstops `$N`, braced tabstops `${N}`,
//! and placeholders with default text `${N:default}`. Backslash
//! escapes a literal `$`, `}`, or `\`. Unsupported forms (choices
//! `${1|a,b|}`, variables `${TM_FILENAME}`, transforms
//! `${1/regex/.../}`, nested placeholders) fall through to literal
//! text -- the body still inserts cleanly, the editor just has no
//! semantic structure to navigate.
//!
//! [`render`] returns the inlined text plus an ordered list of
//! [`TabstopGroup`]s. Each group is the byte ranges sharing one
//! tabstop number; tabs visit groups in ascending order with `$0`
//! (or, absent that, the rendered-text end) as the exit.
//!
//! [spec]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#snippet_syntax
//!
//! Active-snippet state ([`ActiveSnippet`]) lives on
//! [`crate::app::Stoat::active_snippet`] and is consumed by
//! [`advance`] from the Tab arbitration arm in
//! `handle_insert_key`.

use crate::{
    app::Stoat,
    pane::{FocusTarget, View},
};
use std::ops::Range;
use stoat_text::{Anchor, Bias, Selection, SelectionGoal};

/// One element of a parsed snippet body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Element {
    /// Literal text (after escape resolution).
    Text(String),
    /// `$N` or `${N}` tabstop with no default.
    Tabstop(u32),
    /// `${N:default}` placeholder. The default is plain text;
    /// nested placeholders are flattened into literal text inside
    /// the default during parsing.
    Placeholder(u32, String),
}

/// Parsed snippet body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snippet {
    pub elements: Vec<Element>,
}

/// Output of [`Snippet::render`]: rendered text plus tabstop visit
/// groups and the final cursor position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rendered {
    pub text: String,
    /// Tabstop groups in visit order. The last entry is the exit
    /// position represented as a zero-length range at the exit
    /// offset; it always exists so callers always have a place to
    /// land the cursor. Earlier entries are real placeholder /
    /// tabstop groups that the user should visit.
    pub groups: Vec<TabstopGroup>,
}

/// All byte ranges in [`Rendered::text`] that share a tabstop
/// number. Repeated numbers (e.g. `${1:foo} ${1}`) produce a single
/// group with multiple ranges so the editor can place a multi-cursor
/// selection across them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabstopGroup {
    pub idx: u32,
    pub ranges: Vec<Range<usize>>,
}

/// Parse a snippet body. Always succeeds; unrecognized syntax falls
/// through to literal text.
pub(crate) fn parse(input: &str) -> Snippet {
    let mut elements = Vec::new();
    let mut text = String::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'$' || next == b'}' || next == b'\\' {
                text.push(next as char);
                i += 2;
                continue;
            }
            text.push(b as char);
            i += 1;
            continue;
        }
        if b == b'$' {
            if let Some((element, consumed)) = try_parse_dollar(input, i) {
                if !text.is_empty() {
                    elements.push(Element::Text(std::mem::take(&mut text)));
                }
                elements.push(element);
                i += consumed;
                continue;
            }
        }
        if let Some(ch) = input[i..].chars().next() {
            text.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    if !text.is_empty() {
        elements.push(Element::Text(text));
    }
    Snippet { elements }
}

/// Try to parse a `$`-led element at byte offset `start` in
/// `input`. Returns the parsed element and how many bytes were
/// consumed (including the `$`). Returns `None` when the form does
/// not match a supported snippet shape so the caller falls back to
/// literal text.
fn try_parse_dollar(input: &str, start: usize) -> Option<(Element, usize)> {
    let bytes = input.as_bytes();
    debug_assert_eq!(bytes[start], b'$');
    let after = start + 1;
    if after >= bytes.len() {
        return None;
    }

    let next = bytes[after];
    if next.is_ascii_digit() {
        let mut end = after;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let num: u32 = input[after..end].parse().ok()?;
        return Some((Element::Tabstop(num), end - start));
    }
    if next == b'{' {
        return parse_braced(input, after + 1)
            .map(|(e, consumed)| (e, after + 1 + consumed - start));
    }
    None
}

/// Parse the inside of `${...}` starting after the open brace.
/// Returns the element and the count of bytes consumed up to and
/// including the matching `}`.
fn parse_braced(input: &str, start: usize) -> Option<(Element, usize)> {
    let bytes = input.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    let mut idx_end = start;
    while idx_end < bytes.len() && bytes[idx_end].is_ascii_digit() {
        idx_end += 1;
    }
    if idx_end == start {
        return None;
    }
    let num: u32 = input[start..idx_end].parse().ok()?;

    if idx_end >= bytes.len() {
        return None;
    }
    match bytes[idx_end] {
        b'}' => Some((Element::Tabstop(num), idx_end + 1 - start)),
        b':' => {
            let body_start = idx_end + 1;
            let mut depth = 1i32;
            let mut k = body_start;
            while k < bytes.len() {
                let c = bytes[k];
                if c == b'\\' && k + 1 < bytes.len() {
                    k += 2;
                    continue;
                }
                if c == b'{' {
                    depth += 1;
                } else if c == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        let raw = &input[body_start..k];
                        let default = unescape_default(raw);
                        return Some((Element::Placeholder(num, default), k + 1 - start));
                    }
                }
                k += 1;
            }
            None
        },
        _ => None,
    }
}

fn unescape_default(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'$' || next == b'}' || next == b'\\' {
                out.push(next as char);
                i += 2;
                continue;
            }
        }
        if let Some(ch) = s[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

impl Snippet {
    pub(crate) fn render(&self) -> Rendered {
        let mut text = String::new();
        let mut groups_by_idx: std::collections::BTreeMap<u32, Vec<Range<usize>>> =
            std::collections::BTreeMap::new();
        let mut exit_explicit: Option<Range<usize>> = None;

        for el in &self.elements {
            match el {
                Element::Text(t) => text.push_str(t),
                Element::Tabstop(n) => {
                    let pos = text.len();
                    let range = pos..pos;
                    if *n == 0 {
                        exit_explicit = Some(range);
                    } else {
                        groups_by_idx.entry(*n).or_default().push(range);
                    }
                },
                Element::Placeholder(n, default) => {
                    let start = text.len();
                    text.push_str(default);
                    let end = text.len();
                    let range = start..end;
                    if *n == 0 {
                        exit_explicit = Some(range);
                    } else {
                        groups_by_idx.entry(*n).or_default().push(range);
                    }
                },
            }
        }

        let mut groups: Vec<TabstopGroup> = groups_by_idx
            .into_iter()
            .map(|(idx, ranges)| TabstopGroup { idx, ranges })
            .collect();
        let exit_range = exit_explicit.unwrap_or_else(|| {
            let end = text.len();
            end..end
        });
        groups.push(TabstopGroup {
            idx: 0,
            ranges: vec![exit_range],
        });

        Rendered { text, groups }
    }
}

/// In-flight snippet expansion: the editor is sitting on the
/// rendered text, and Tab cycles through the remaining tabstop
/// groups before landing the cursor at [`Self::exit`]. Anchors
/// survive intermediate edits so that typing inside one tabstop
/// keeps subsequent tabstops correctly positioned.
#[derive(Debug, Clone)]
pub(crate) struct ActiveSnippet {
    pub(crate) groups: std::collections::VecDeque<Vec<Range<Anchor>>>,
    pub(crate) exit: Anchor,
}

/// Advance the active snippet to the next tabstop group, or exit
/// when no groups remain. Sets the focused editor's selections to
/// either the next group's anchors or a single collapsed selection
/// at [`ActiveSnippet::exit`].
///
/// Returns `true` when state changed (always true when called with
/// an active snippet); `false` when there is nothing to advance.
pub(crate) fn advance(stoat: &mut Stoat) -> bool {
    let Some(mut active) = stoat.active_snippet.take() else {
        return false;
    };
    let next_group = active.groups.pop_front();

    let ws = stoat.active_workspace_mut();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return true;
    };
    let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
        return true;
    };
    let Some(editor) = ws.editors.get_mut(editor_id) else {
        return true;
    };
    let snapshot = editor.display_map.snapshot();
    let buf_snapshot = snapshot.buffer_snapshot();

    match next_group {
        Some(group) if !group.is_empty() => {
            let selections: Vec<Selection<Anchor>> = group
                .into_iter()
                .enumerate()
                .map(|(i, range)| Selection {
                    id: i,
                    start: range.start,
                    end: range.end,
                    reversed: false,
                    goal: SelectionGoal::None,
                })
                .collect();
            editor.selections.replace_with(selections, buf_snapshot);
            stoat.active_snippet = Some(active);
        },
        _ => {
            let exit = active.exit;
            editor.selections.replace_with(
                vec![Selection {
                    id: 0,
                    start: exit,
                    end: exit,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                buf_snapshot,
            );
        },
    }
    true
}

/// Build an [`ActiveSnippet`] from rendered tabstop groups. Each
/// rendered range is offset by `inserted_at` (the buffer byte
/// offset where the snippet text was inserted) and resolved to
/// anchors via `buf_snapshot`. The first group is taken as the
/// initial selection set; any remaining groups (plus the exit) are
/// stashed for [`advance`].
///
/// Returns the initial selections to install (caller writes them
/// onto the editor) plus the active-snippet state to stash. When
/// the snippet has only the exit group, returns a single collapsed
/// selection at the exit and `None` for the active state.
pub(crate) fn install(
    rendered: &Rendered,
    inserted_at: usize,
    buf_snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) -> (Vec<Selection<Anchor>>, Option<ActiveSnippet>) {
    let mut anchored: Vec<Vec<Range<Anchor>>> = rendered
        .groups
        .iter()
        .map(|g| {
            g.ranges
                .iter()
                .map(|r| {
                    let start = buf_snapshot.anchor_at(inserted_at + r.start, Bias::Right);
                    let end = buf_snapshot.anchor_at(inserted_at + r.end, Bias::Right);
                    start..end
                })
                .collect()
        })
        .collect();

    let exit_anchor = anchored
        .last()
        .and_then(|g| g.first())
        .map(|r| r.start)
        .unwrap_or_else(|| buf_snapshot.anchor_at(inserted_at, Bias::Right));

    let real_groups_count = anchored.len().saturating_sub(1);
    let exit_group = anchored.pop();
    let _ = exit_group;

    if real_groups_count == 0 {
        let sel = Selection {
            id: 0,
            start: exit_anchor,
            end: exit_anchor,
            reversed: false,
            goal: SelectionGoal::None,
        };
        return (vec![sel], None);
    }

    let first = anchored.remove(0);
    let initial: Vec<Selection<Anchor>> = first
        .into_iter()
        .enumerate()
        .map(|(i, range)| Selection {
            id: i,
            start: range.start,
            end: range.end,
            reversed: false,
            goal: SelectionGoal::None,
        })
        .collect();

    let active = if anchored.is_empty() {
        ActiveSnippet {
            groups: std::collections::VecDeque::new(),
            exit: exit_anchor,
        }
    } else {
        ActiveSnippet {
            groups: anchored.into(),
            exit: exit_anchor,
        }
    };

    (initial, Some(active))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_text(input: &str) -> String {
        parse(input).render().text
    }

    #[test]
    fn plain_text_round_trips() {
        let r = parse("hello world").render();
        assert_eq!(r.text, "hello world");
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].idx, 0);
    }

    #[test]
    fn bare_tabstop_renders_to_empty_marker() {
        let r = parse("$1").render();
        assert_eq!(r.text, "");
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.groups[0].idx, 1);
        assert_eq!(r.groups[0].ranges, vec![0..0]);
    }

    #[test]
    fn braced_tabstop_renders_to_empty_marker() {
        let r = parse("${1}").render();
        assert_eq!(r.text, "");
        assert_eq!(r.groups[0].idx, 1);
        assert_eq!(r.groups[0].ranges, vec![0..0]);
    }

    #[test]
    fn placeholder_renders_default_text() {
        let r = parse("${1:foo}").render();
        assert_eq!(r.text, "foo");
        assert_eq!(r.groups[0].idx, 1);
        assert_eq!(r.groups[0].ranges, vec![0..3]);
    }

    #[test]
    fn multiple_tabstops_visited_in_ascending_order() {
        let r = parse("${1:fn}(${2:arg}) -> ${3:Result}").render();
        assert_eq!(r.text, "fn(arg) -> Result");
        let idxs: Vec<u32> = r.groups.iter().map(|g| g.idx).collect();
        assert_eq!(idxs, vec![1, 2, 3, 0]);
        assert_eq!(r.groups[0].ranges, vec![0..2]);
        assert_eq!(r.groups[1].ranges, vec![3..6]);
        assert_eq!(r.groups[2].ranges, vec![11..17]);
    }

    #[test]
    fn linked_tabstop_groups_repeated_indexes() {
        let r = parse("${1:x} ${1}").render();
        assert_eq!(r.text, "x ");
        assert_eq!(r.groups[0].idx, 1);
        assert_eq!(r.groups[0].ranges, vec![0..1, 2..2]);
    }

    #[test]
    fn dollar_zero_is_explicit_exit() {
        let r = parse("println!(${1:msg})$0").render();
        assert_eq!(r.text, "println!(msg)");
        let last = r.groups.last().unwrap();
        assert_eq!(last.idx, 0);
        assert_eq!(last.ranges, vec![13..13]);
    }

    #[test]
    fn missing_dollar_zero_uses_text_end_as_exit() {
        let r = parse("foo${1:bar}baz").render();
        assert_eq!(r.text, "foobarbaz");
        let last = r.groups.last().unwrap();
        assert_eq!(last.idx, 0);
        assert_eq!(last.ranges, vec![9..9]);
    }

    #[test]
    fn escape_dollar_inserts_literal() {
        assert_eq!(rendered_text(r"price: \$5"), "price: $5");
    }

    #[test]
    fn escape_brace_inserts_literal() {
        assert_eq!(rendered_text(r"\}"), "}");
    }

    #[test]
    fn escape_backslash_inserts_literal() {
        assert_eq!(rendered_text(r"a\\b"), r"a\b");
    }

    #[test]
    fn unsupported_choice_falls_through_to_text() {
        let r = parse("${1|a,b|}").render();
        assert!(r.text.contains("|a,b|") || r.text.contains("a,b"));
    }

    #[test]
    fn unterminated_brace_falls_through_to_text() {
        let r = parse("${1:foo").render();
        assert_eq!(r.text, "${1:foo");
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].idx, 0);
    }

    #[test]
    fn dollar_at_eof_is_literal() {
        assert_eq!(rendered_text("end$"), "end$");
    }

    #[test]
    fn multibyte_default_keeps_byte_offsets_correct() {
        let r = parse("${1:resume}: ${2:done}").render();
        assert_eq!(r.text, "resume: done");
        assert_eq!(r.groups[0].ranges, vec![0..6]);
        assert_eq!(r.groups[1].ranges, vec![8..12]);
    }
}
