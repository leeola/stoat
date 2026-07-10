//! Unchanged-region preprocessing for the structural diff.
//!
//! Difftastic's diff is Dijkstra over an edit graph; the search space is
//! enormous on real files, so before running the search a preprocessing
//! pass tags every node that is *trivially* unchanged. The preprocessing
//! cost is `O(n)` (plus a histogram diff over top-level [`ContentId`]s
//! that runs in `O(n+m)` memory); the savings are substantial because
//! the Dijkstra walk only sees the novel regions.
//!
//! Over each list of children, three steps:
//!
//! 1. **Shrink at endpoints.** Walk both sides from the front, pairing children with matching
//!    `ContentId`, and stop on the first mismatch. Repeat from the back. The matched ranges are
//!    tagged [`ChangeKind::Unchanged`] recursively.
//!
//! 2. **LCS anchors.** Over the remaining middle, diff child `ContentId`s to recover the matched
//!    subsequence. Each match is an anchor, marked unchanged along with its subtree.
//!
//! 3. **Emit sections.** The unmatched run between two consecutive anchors is a changed section. A
//!    lone same-kind list on each side recurses into its children instead, so a small edit inside a
//!    container drills down to its changed statement. Every other non-empty run is a section.
//!
//! After this pass, every node carries a [`ChangeKind`] in a side [`ChangeMap`], and the changed
//! sections are collected in [`PreprocessResult::sections`]. The Dijkstra search then runs over one
//! section at a time rather than the whole file.
//!
//! Reference: `references/difftastic/src/diff/unchanged.rs` (`split_unchanged`). The algorithm is
//! the same, but sibling matching runs imara-diff's histogram over child [`ContentId`]s rather
//! than `wu_diff`.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    content_id::ContentId,
};
use imara_diff::{
    intern::{InternedInput, Interner},
    Algorithm, Sink,
};
use std::ops::Range;

/// What the preprocessing pass concluded about a node. The diff search
/// only walks `Pending` nodes; `Unchanged` and `Moved` are terminal tags
/// that downstream passes respect as already-paired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Bytes are byte-for-byte unchanged on the other side, in the same
    /// relative position. Paired by either the shrink-from-endpoints pass,
    /// the LCS-over-siblings pass, or the Dijkstra search.
    Unchanged,
    /// Preprocessing was unable to pair this node; the diff algorithm
    /// (or a later pass like [`super::moves::find_moves`]) must decide.
    Pending,
    /// Bytes are byte-for-byte equal on the other side but at a different
    /// relative position. Paired by the post-Dijkstra move pass; the
    /// provenance metadata lives in a side table owned by that pass.
    Moved,
}

/// Side-table mapping [`SyntaxId`] -> [`ChangeKind`]. Backed by a dense
/// `Vec` indexed by [`SyntaxId`] for O(1) access with no hashing.
#[derive(Clone, Debug, Default)]
pub struct ChangeMap {
    data: Vec<ChangeKind>,
}

impl ChangeMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_len(n: usize) -> Self {
        Self {
            data: vec![ChangeKind::Pending; n],
        }
    }

    pub fn get(&self, id: SyntaxId) -> ChangeKind {
        self.data.get(id.0).copied().unwrap_or(ChangeKind::Pending)
    }

    pub fn mark(&mut self, id: SyntaxId, kind: ChangeKind) {
        if id.0 >= self.data.len() {
            self.data.resize(id.0 + 1, ChangeKind::Pending);
        }
        self.data[id.0] = kind;
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Result of [`mark_unchanged`]. Holds one [`ChangeMap`] per side plus the
/// changed sections the caller runs the Dijkstra search over.
///
/// A section is a `(lhs_run, rhs_run)` pair of consecutive unmatched children
/// between two unchanged anchors. The search runs per section rather than over
/// the whole file, so a small edit only explores its own region. A section
/// empty on one side is a pure insertion or deletion the caller leaves for the
/// change collector.
#[derive(Clone, Debug, Default)]
pub struct PreprocessResult {
    pub lhs_changes: ChangeMap,
    pub rhs_changes: ChangeMap,
    pub sections: Vec<(Vec<SyntaxId>, Vec<SyntaxId>)>,
}

/// Run the three-phase preprocessing pass on `(lhs_root, rhs_root)`,
/// returning a [`PreprocessResult`] that tags every node either
/// [`ChangeKind::Unchanged`] or [`ChangeKind::Pending`].
pub fn mark_unchanged(
    lhs_arena: &SyntaxArena,
    lhs_root: SyntaxId,
    rhs_arena: &SyntaxArena,
    rhs_root: SyntaxId,
) -> PreprocessResult {
    let mut result = PreprocessResult {
        lhs_changes: ChangeMap::with_len(lhs_arena.len()),
        rhs_changes: ChangeMap::with_len(rhs_arena.len()),
        sections: Vec::new(),
    };

    // Top-level: roots are always lists in practice (they wrap the
    // whole document). If they share content_ids, every descendant is
    // unchanged and we can return early.
    if lhs_arena.get(lhs_root).content_id() == rhs_arena.get(rhs_root).content_id() {
        mark_subtree(
            lhs_arena,
            lhs_root,
            &mut result.lhs_changes,
            ChangeKind::Unchanged,
        );
        mark_subtree(
            rhs_arena,
            rhs_root,
            &mut result.rhs_changes,
            ChangeKind::Unchanged,
        );
        return result;
    }

    // The roots differ structurally. Compare their children.
    let lhs_children = list_children(lhs_arena, lhs_root);
    let rhs_children = list_children(rhs_arena, rhs_root);
    pair_children(
        lhs_arena,
        rhs_arena,
        &lhs_children,
        &rhs_children,
        &mut result.lhs_changes,
        &mut result.rhs_changes,
        &mut result.sections,
    );

    result
}

/// Pair two child lists via shrink-then-LCS, marking matched subtrees
/// [`ChangeKind::Unchanged`] and collecting the changed runs between anchors
/// into `sections`.
///
/// The unmatched run between two consecutive anchors is a changed section. A
/// run of exactly one same-kind [`Syntax::List`] on each side recurses into
/// its children instead, so a small edit inside a container drills down to the
/// changed statement rather than emitting the whole container. Every other
/// non-empty run is pushed to `sections` for the per-section Dijkstra search.
fn pair_children(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs: &[SyntaxId],
    rhs: &[SyntaxId],
    lhs_changes: &mut ChangeMap,
    rhs_changes: &mut ChangeMap,
    sections: &mut Vec<(Vec<SyntaxId>, Vec<SyntaxId>)>,
) {
    // Phase 1a: shrink common prefix.
    let mut prefix = 0usize;
    while prefix < lhs.len()
        && prefix < rhs.len()
        && lhs_arena.get(lhs[prefix]).content_id() == rhs_arena.get(rhs[prefix]).content_id()
    {
        mark_subtree(lhs_arena, lhs[prefix], lhs_changes, ChangeKind::Unchanged);
        mark_subtree(rhs_arena, rhs[prefix], rhs_changes, ChangeKind::Unchanged);
        prefix += 1;
    }

    // Phase 1b: shrink common suffix.
    let mut suffix = 0usize;
    while prefix + suffix < lhs.len()
        && prefix + suffix < rhs.len()
        && lhs_arena.get(lhs[lhs.len() - 1 - suffix]).content_id()
            == rhs_arena.get(rhs[rhs.len() - 1 - suffix]).content_id()
    {
        let lhs_id = lhs[lhs.len() - 1 - suffix];
        let rhs_id = rhs[rhs.len() - 1 - suffix];
        mark_subtree(lhs_arena, lhs_id, lhs_changes, ChangeKind::Unchanged);
        mark_subtree(rhs_arena, rhs_id, rhs_changes, ChangeKind::Unchanged);
        suffix += 1;
    }

    let lhs_mid = &lhs[prefix..lhs.len() - suffix];
    let rhs_mid = &rhs[prefix..rhs.len() - suffix];

    // Phase 2: LCS anchors over the middle. Matched pairs are byte-for-byte
    // equal, so mark them and their descendants Unchanged.
    let anchors = if !lhs_mid.is_empty() && !rhs_mid.is_empty() {
        lcs_pairs(lhs_arena, rhs_arena, lhs_mid, rhs_mid)
    } else {
        Vec::new()
    };
    for &(lhs_idx, rhs_idx) in &anchors {
        mark_subtree(
            lhs_arena,
            lhs_mid[lhs_idx],
            lhs_changes,
            ChangeKind::Unchanged,
        );
        mark_subtree(
            rhs_arena,
            rhs_mid[rhs_idx],
            rhs_changes,
            ChangeKind::Unchanged,
        );
    }

    // Phase 3: the unmatched run between each consecutive pair of anchors
    // (and before the first / after the last) is a changed section. A
    // sentinel anchor past the end of both middles closes the final run.
    let mut lhs_cursor = 0usize;
    let mut rhs_cursor = 0usize;
    let boundaries = anchors
        .iter()
        .copied()
        .chain(std::iter::once((lhs_mid.len(), rhs_mid.len())));
    for (lhs_anchor, rhs_anchor) in boundaries {
        let lhs_run = &lhs_mid[lhs_cursor..lhs_anchor];
        let rhs_run = &rhs_mid[rhs_cursor..rhs_anchor];
        if !lhs_run.is_empty() || !rhs_run.is_empty() {
            let recursed = lhs_run.len() == 1
                && rhs_run.len() == 1
                && recurse_singleton_lists(
                    lhs_arena,
                    rhs_arena,
                    lhs_run[0],
                    rhs_run[0],
                    lhs_changes,
                    rhs_changes,
                    sections,
                );
            if !recursed {
                sections.push((lhs_run.to_vec(), rhs_run.to_vec()));
            }
        }
        lhs_cursor = lhs_anchor + 1;
        rhs_cursor = rhs_anchor + 1;
    }
}

/// Recurse into a one-node-each run when both nodes are lists of the same
/// kind, splitting their children into sub-sections. Returns `true` when it
/// recursed, `false` when the caller should emit the pair as one section.
///
/// This is difftastic's singleton-list rule. A lone modified container -- a
/// function whose body changed, say -- descends so only its changed statements
/// become sections rather than the whole body.
fn recurse_singleton_lists(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_id: SyntaxId,
    rhs_id: SyntaxId,
    lhs_changes: &mut ChangeMap,
    rhs_changes: &mut ChangeMap,
    sections: &mut Vec<(Vec<SyntaxId>, Vec<SyntaxId>)>,
) -> bool {
    if let (Syntax::List(lhs_list), Syntax::List(rhs_list)) =
        (lhs_arena.get(lhs_id), rhs_arena.get(rhs_id))
        && lhs_list.kind == rhs_list.kind
    {
        let lhs_grand = lhs_list.children.clone();
        let rhs_grand = rhs_list.children.clone();
        pair_children(
            lhs_arena,
            rhs_arena,
            &lhs_grand,
            &rhs_grand,
            lhs_changes,
            rhs_changes,
            sections,
        );
        true
    } else {
        false
    }
}

/// Mark `id` and every transitive descendant in the same arena as `kind`.
fn mark_subtree(arena: &SyntaxArena, id: SyntaxId, changes: &mut ChangeMap, kind: ChangeKind) {
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        changes.mark(current, kind);
        if let Syntax::List(list) = arena.get(current) {
            stack.extend(list.children.iter().copied());
        }
    }
}

fn list_children(arena: &SyntaxArena, id: SyntaxId) -> Vec<SyntaxId> {
    match arena.get(id) {
        Syntax::List(l) => l.children.clone(),
        Syntax::Atom(_) => Vec::new(),
    }
}

/// Matched index pairs `(lhs_idx, rhs_idx)` in source order over the
/// children's [`ContentId`]s: the anchors that pin the changed sections.
///
/// Interns each side's `ContentId` sequence and runs imara-diff's histogram
/// algorithm in `O(n+m)` memory. The matched anchors are the equal regions
/// between the change hunks, which advance in lockstep on both sides, so
/// [`LcsPairSink`] reconstructs the in-order pairs from them.
///
/// Histogram may under-match the true longest common subsequence on
/// degenerate repeats. The per-section Dijkstra search recovers those, so
/// the diff output is unchanged.
fn lcs_pairs(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs: &[SyntaxId],
    rhs: &[SyntaxId],
) -> Vec<(usize, usize)> {
    let mut input: InternedInput<ContentId> = InternedInput {
        before: Vec::new(),
        after: Vec::new(),
        interner: Interner::new(lhs.len() + rhs.len()),
    };
    input.update_before(lhs.iter().map(|&id| lhs_arena.get(id).content_id()));
    input.update_after(rhs.iter().map(|&id| rhs_arena.get(id).content_id()));

    imara_diff::diff(
        Algorithm::Histogram,
        &input,
        LcsPairSink {
            lhs_cursor: 0,
            rhs_cursor: 0,
            lhs_len: lhs.len() as u32,
            rhs_len: rhs.len() as u32,
            pairs: Vec::new(),
        },
    )
}

/// Reconstructs [`lcs_pairs`]' matched anchors from imara-diff's change
/// hunks. imara-diff reports the differing runs. The equal runs between them
/// are the matches, and they advance in lockstep on both sides, so each
/// equal token yields one `(lhs_idx, rhs_idx)` pair.
struct LcsPairSink {
    lhs_cursor: u32,
    rhs_cursor: u32,
    lhs_len: u32,
    rhs_len: u32,
    pairs: Vec<(usize, usize)>,
}

impl LcsPairSink {
    /// Emit one pair per token in the equal run from the current cursors up
    /// to `lhs_end`/`rhs_end`. The two run lengths are equal by construction.
    /// Taking `min` pairs only as far as the shorter run, so a broken
    /// invariant truncates the pairing rather than misaligning it.
    fn emit_equal(&mut self, lhs_end: u32, rhs_end: u32) {
        let len = (lhs_end - self.lhs_cursor).min(rhs_end - self.rhs_cursor);
        for k in 0..len {
            self.pairs.push((
                (self.lhs_cursor + k) as usize,
                (self.rhs_cursor + k) as usize,
            ));
        }
    }
}

impl Sink for LcsPairSink {
    type Out = Vec<(usize, usize)>;

    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        self.emit_equal(before.start, after.start);
        self.lhs_cursor = before.end;
        self.rhs_cursor = after.end;
    }

    fn finish(mut self) -> Self::Out {
        self.emit_equal(self.lhs_len, self.rhs_len);
        self.pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, structural_diff::lower_tree, LanguageRegistry};

    fn rust_lang() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    fn lower(source: &str) -> (SyntaxArena, SyntaxId) {
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        lower_tree(&tree, source)
    }

    fn count_unchanged(arena: &SyntaxArena, root: SyntaxId, changes: &ChangeMap) -> usize {
        let mut stack = vec![root];
        let mut count = 0usize;
        while let Some(id) = stack.pop() {
            if changes.get(id) == ChangeKind::Unchanged {
                count += 1;
            }
            if let Syntax::List(l) = arena.get(id) {
                stack.extend(l.children.iter().copied());
            }
        }
        count
    }

    fn count_total(arena: &SyntaxArena, root: SyntaxId) -> usize {
        let mut stack = vec![root];
        let mut count = 0usize;
        while let Some(id) = stack.pop() {
            count += 1;
            if let Syntax::List(l) = arena.get(id) {
                stack.extend(l.children.iter().copied());
            }
        }
        count
    }

    #[test]
    fn identical_inputs_mark_every_node_unchanged() {
        let source = "fn main() { let x = 1; }";
        let (lhs_arena, lhs_root) = lower(source);
        let (rhs_arena, rhs_root) = lower(source);
        let result = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);
        let lhs_total = count_total(&lhs_arena, lhs_root);
        let rhs_total = count_total(&rhs_arena, rhs_root);
        assert_eq!(
            count_unchanged(&lhs_arena, lhs_root, &result.lhs_changes),
            lhs_total
        );
        assert_eq!(
            count_unchanged(&rhs_arena, rhs_root, &result.rhs_changes),
            rhs_total
        );
    }

    #[test]
    fn appended_function_leaves_first_unchanged() {
        // The first function is identical; the second is novel.
        let lhs = "fn main() { let x = 1; }";
        let rhs = "fn main() { let x = 1; }\nfn extra() {}";
        let (lhs_arena, lhs_root) = lower(lhs);
        let (rhs_arena, rhs_root) = lower(rhs);
        let result = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);

        // The lhs side has a single top-level function; it must be
        // marked unchanged because the rhs has the same first function.
        let lhs_unchanged = count_unchanged(&lhs_arena, lhs_root, &result.lhs_changes);
        let lhs_total = count_total(&lhs_arena, lhs_root);
        assert!(
            lhs_unchanged > lhs_total / 2,
            "lhs has {lhs_unchanged}/{lhs_total} unchanged; expected the first function tree"
        );

        // On the rhs side at least the first function's nodes should
        // be unchanged. The second function and the surrounding
        // source_file container are novel.
        let rhs_unchanged = count_unchanged(&rhs_arena, rhs_root, &result.rhs_changes);
        assert!(
            rhs_unchanged >= lhs_unchanged - 1,
            "rhs unchanged count {rhs_unchanged} should match the lhs's preserved subtree"
        );
    }

    #[test]
    fn fully_distinct_inputs_have_only_pending_at_root_level() {
        // Different identifiers mean different content_ids; only nodes
        // that happen to share structure (e.g. empty parameter list) get
        // tagged unchanged. The root source_file is always pending.
        let lhs = "fn alpha() {}";
        let rhs = "fn beta() {}";
        let (lhs_arena, lhs_root) = lower(lhs);
        let (rhs_arena, rhs_root) = lower(rhs);
        let result = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);
        assert_eq!(result.lhs_changes.get(lhs_root), ChangeKind::Pending);
        assert_eq!(result.rhs_changes.get(rhs_root), ChangeKind::Pending);
    }

    #[test]
    fn lcs_pairs_in_order() {
        // Trivial sanity check on the LCS routine: input [A, B, C] and
        // [A, X, B] should produce [(0,0), (1,2)].
        let mut arena = SyntaxArena::new();
        use crate::structural_diff::{
            arena::{Atom, Syntax},
            ContentId,
        };
        let mk = |arena: &mut SyntaxArena, name: &str| {
            let id = ContentId::for_atom("ident", name);
            arena.alloc(Syntax::Atom(Atom {
                kind: "ident",
                byte_range: 0..0,
                content: "",
                content_id: id,
                next_sibling: None,
            }))
        };
        let lhs_a = mk(&mut arena, "A");
        let lhs_b = mk(&mut arena, "B");
        let lhs_c = mk(&mut arena, "C");
        let rhs_a = mk(&mut arena, "A");
        let rhs_x = mk(&mut arena, "X");
        let rhs_b = mk(&mut arena, "B");
        let pairs = lcs_pairs(
            &arena,
            &arena,
            &[lhs_a, lhs_b, lhs_c],
            &[rhs_a, rhs_x, rhs_b],
        );
        assert_eq!(pairs, vec![(0, 0), (1, 2)]);
    }

    #[test]
    fn large_input_pairs_all_but_the_perturbation() {
        // Two 5000-element sequences differing at a single index. The DP
        // table this replaced would allocate a 25M-cell matrix. Histogram
        // stays linear in memory and pairs everything but the changed node.
        use crate::structural_diff::{
            arena::{Atom, Syntax},
            ContentId,
        };
        let mut arena = SyntaxArena::new();
        let mk = |arena: &mut SyntaxArena, name: &str| {
            arena.alloc(Syntax::Atom(Atom {
                kind: "ident",
                byte_range: 0..0,
                content: "",
                content_id: ContentId::for_atom("ident", name),
                next_sibling: None,
            }))
        };
        let n = 5000usize;
        let mut lhs = Vec::with_capacity(n);
        let mut rhs = Vec::with_capacity(n);
        for i in 0..n {
            lhs.push(mk(&mut arena, &i.to_string()));
            let rhs_name = if i == 2500 {
                "novel".to_string()
            } else {
                i.to_string()
            };
            rhs.push(mk(&mut arena, &rhs_name));
        }
        let pairs = lcs_pairs(&arena, &arena, &lhs, &rhs);
        assert_eq!(pairs.len(), n - 1);
        assert_eq!(pairs.first(), Some(&(0, 0)));
        assert_eq!(pairs.last(), Some(&(n - 1, n - 1)));
    }
}
