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
//!    container drills down to its changed statement. A longer run first peels each pair at its
//!    ends that shares at least four subtrees unique on their own side, and each peeled pair
//!    recurses the same way. Every other non-empty run is a section.
//!
//! After this pass, every node carries a [`ChangeKind`] in a side [`ChangeMap`], and the changed
//! sections are collected in [`PreprocessResult::sections`]. The Dijkstra search then runs over one
//! section at a time rather than the whole file.
//!
//! The pass follows `split_unchanged` in `references/difftastic/src/diff/unchanged.rs`, but
//! sibling matching runs imara-diff's histogram over child [`ContentId`]s rather than `wu_diff`.
//! The peel is `split_mostly_unchanged_toplevel`, which difftastic applies to the top level alone
//! and this pass applies to every run.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    content_id::ContentId,
    fx::FxBuildHasher,
};
use imara_diff::{Algorithm, Diff, InternedInput, Interner};
use std::{
    cell::OnceCell,
    collections::{HashMap, HashSet},
};

/// The count of subtrees, each unique on its own side, that two unmatched
/// sibling lists must share to recurse as a pair before the search.
///
/// The value is difftastic's. A unique subtree that both lists hold is a
/// landmark that pins them together, and four of them separate a list that
/// kept most of its body from two lists that share only boilerplate.
const MOSTLY_UNCHANGED_MIN_COMMON_CHILDREN: usize = 4;

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
    /// The `(lhs, rhs)` node pairs matched whole, each a subtree equal on both
    /// sides.
    ///
    /// The change maps say which nodes are unchanged but not what each one
    /// matched, which a consumer aligning the two sides by content needs.
    pub pairs: Vec<(SyntaxId, SyntaxId)>,
}

/// How many times each content id occurs in one arena.
type IdCounts = HashMap<ContentId, u32, FxBuildHasher>;

/// The content id counts of both arenas, which tell [`is_mostly_unchanged_pair`]
/// which subtrees are unique on their own side.
///
/// The counts form on the first read, once per [`mark_unchanged`]. Only a run
/// with members on both sides and two or more on one side reads them, so a
/// diff whose runs all pair one to one never pays for the pass over both
/// arenas.
#[derive(Default)]
struct Uniqueness {
    counts: OnceCell<(IdCounts, IdCounts)>,
}

impl Uniqueness {
    /// The lhs and rhs counts, built from the two arenas on the first call.
    fn counts(
        &self,
        lhs_arena: &SyntaxArena<'_>,
        rhs_arena: &SyntaxArena<'_>,
    ) -> &(IdCounts, IdCounts) {
        self.counts
            .get_or_init(|| (count_ids(lhs_arena), count_ids(rhs_arena)))
    }
}

/// Run the three-phase preprocessing pass on `(lhs_root, rhs_root)`,
/// returning a [`PreprocessResult`] that tags every node either
/// [`ChangeKind::Unchanged`] or [`ChangeKind::Pending`].
pub fn mark_unchanged(
    lhs_arena: &SyntaxArena<'_>,
    lhs_root: SyntaxId,
    rhs_arena: &SyntaxArena<'_>,
    rhs_root: SyntaxId,
) -> PreprocessResult {
    let mut result = PreprocessResult {
        lhs_changes: ChangeMap::with_len(lhs_arena.len()),
        rhs_changes: ChangeMap::with_len(rhs_arena.len()),
        sections: Vec::new(),
        pairs: Vec::new(),
    };

    // Top-level: roots are always lists in practice (they wrap the
    // whole document). If they share content_ids, every descendant is
    // unchanged and we can return early.
    if lhs_arena.get(lhs_root).content_id() == rhs_arena.get(rhs_root).content_id() {
        match_subtrees(lhs_arena, rhs_arena, lhs_root, rhs_root, &mut result);
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
        &Uniqueness::default(),
        &mut result,
    );

    result
}

/// Pair two child lists via shrink-then-LCS, marking matched subtrees
/// [`ChangeKind::Unchanged`] and collecting the changed runs between anchors
/// into `out.sections`.
///
/// The unmatched run between two consecutive anchors is a changed section. A
/// run of exactly one same-kind [`Syntax::List`] on each side recurses into
/// its children instead, so a small edit inside a container drills down to the
/// changed statement rather than emitting the whole container. A longer run
/// first peels the mostly unchanged pairs at its ends, as [`emit_run`]
/// describes. Every other non-empty run goes to `out.sections` for the
/// per-section Dijkstra search.
fn pair_children(
    lhs_arena: &SyntaxArena<'_>,
    rhs_arena: &SyntaxArena<'_>,
    lhs: &[SyntaxId],
    rhs: &[SyntaxId],
    uniqueness: &Uniqueness,
    out: &mut PreprocessResult,
) {
    // Phase 1a: shrink common prefix.
    let mut prefix = 0usize;
    while prefix < lhs.len()
        && prefix < rhs.len()
        && lhs_arena.get(lhs[prefix]).content_id() == rhs_arena.get(rhs[prefix]).content_id()
    {
        match_subtrees(lhs_arena, rhs_arena, lhs[prefix], rhs[prefix], out);
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
        match_subtrees(lhs_arena, rhs_arena, lhs_id, rhs_id, out);
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
        match_subtrees(
            lhs_arena,
            rhs_arena,
            lhs_mid[lhs_idx],
            rhs_mid[rhs_idx],
            out,
        );
    }

    // Phase 3: each unmatched run between consecutive anchors (and before the
    // first / after the last) goes to emit_run. A sentinel anchor past the end
    // of both middles closes the final run.
    let mut lhs_cursor = 0usize;
    let mut rhs_cursor = 0usize;
    let boundaries = anchors
        .iter()
        .copied()
        .chain(std::iter::once((lhs_mid.len(), rhs_mid.len())));
    for (lhs_anchor, rhs_anchor) in boundaries {
        let lhs_run = &lhs_mid[lhs_cursor..lhs_anchor];
        let rhs_run = &rhs_mid[rhs_cursor..rhs_anchor];
        emit_run(lhs_arena, rhs_arena, lhs_run, rhs_run, uniqueness, out);
        lhs_cursor = lhs_anchor + 1;
        rhs_cursor = rhs_anchor + 1;
    }
}

/// Recurse into one unmatched run between two anchors, or emit it as
/// sections.
///
/// A run of one member per side goes to [`emit_pair`]. A longer run first
/// peels, from its front and then from its back, each pair that
/// [`is_mostly_unchanged_pair`] accepts, and sends each peeled pair to
/// [`emit_pair`]. A middle of one member per side goes to [`emit_pair`] too,
/// and any other non-empty middle is one section.
///
/// The peel keeps two edited neighbors apart, so the search runs inside each
/// of them and never over the pair as one section.
fn emit_run(
    lhs_arena: &SyntaxArena<'_>,
    rhs_arena: &SyntaxArena<'_>,
    lhs_run: &[SyntaxId],
    rhs_run: &[SyntaxId],
    uniqueness: &Uniqueness,
    out: &mut PreprocessResult,
) {
    if lhs_run.len() == 1 && rhs_run.len() == 1 {
        emit_pair(
            lhs_arena, rhs_arena, lhs_run[0], rhs_run[0], uniqueness, out,
        );
        return;
    }

    let peels = |lhs, rhs| is_mostly_unchanged_pair(lhs_arena, rhs_arena, lhs, rhs, uniqueness);
    let shorter = lhs_run.len().min(rhs_run.len());
    let lead = (0..shorter)
        .take_while(|&k| peels(lhs_run[k], rhs_run[k]))
        .count();
    let lhs_last = |k: usize| lhs_run[lhs_run.len() - 1 - k];
    let rhs_last = |k: usize| rhs_run[rhs_run.len() - 1 - k];
    let trail = (0..shorter - lead)
        .take_while(|&k| peels(lhs_last(k), rhs_last(k)))
        .count();

    for k in 0..lead {
        emit_pair(
            lhs_arena, rhs_arena, lhs_run[k], rhs_run[k], uniqueness, out,
        );
    }

    let lhs_middle = &lhs_run[lead..lhs_run.len() - trail];
    let rhs_middle = &rhs_run[lead..rhs_run.len() - trail];
    if lhs_middle.len() == 1 && rhs_middle.len() == 1 {
        emit_pair(
            lhs_arena,
            rhs_arena,
            lhs_middle[0],
            rhs_middle[0],
            uniqueness,
            out,
        );
    } else if !lhs_middle.is_empty() || !rhs_middle.is_empty() {
        out.sections
            .push((lhs_middle.to_vec(), rhs_middle.to_vec()));
    }

    for k in (0..trail).rev() {
        emit_pair(
            lhs_arena,
            rhs_arena,
            lhs_last(k),
            rhs_last(k),
            uniqueness,
            out,
        );
    }
}

/// Recurse into `lhs_id` and `rhs_id` when they are two lists of one kind, or
/// emit the pair as a one-member section.
fn emit_pair(
    lhs_arena: &SyntaxArena<'_>,
    rhs_arena: &SyntaxArena<'_>,
    lhs_id: SyntaxId,
    rhs_id: SyntaxId,
    uniqueness: &Uniqueness,
    out: &mut PreprocessResult,
) {
    if !recurse_singleton_lists(lhs_arena, rhs_arena, lhs_id, rhs_id, uniqueness, out) {
        out.sections.push((vec![lhs_id], vec![rhs_id]));
    }
}

/// Whether `lhs` and `rhs` are lists that share at least
/// [`MOSTLY_UNCHANGED_MIN_COMMON_CHILDREN`] subtrees, each unique on its own
/// side.
///
/// A shared subtree counts once, and the walk skips the nodes under it, so a
/// matched parent does not count again through its children. The counts span
/// each whole arena, so a subtree repeated anywhere in its file pins nothing.
fn is_mostly_unchanged_pair(
    lhs_arena: &SyntaxArena<'_>,
    rhs_arena: &SyntaxArena<'_>,
    lhs: SyntaxId,
    rhs: SyntaxId,
    uniqueness: &Uniqueness,
) -> bool {
    if !matches!(
        (lhs_arena.get(lhs), rhs_arena.get(rhs)),
        (Syntax::List(_), Syntax::List(_))
    ) {
        return false;
    }
    let (lhs_counts, rhs_counts) = uniqueness.counts(lhs_arena, rhs_arena);
    let unique = |counts: &IdCounts, id: ContentId| counts.get(&id) == Some(&1);

    let mut rhs_unique: HashSet<ContentId, FxBuildHasher> = HashSet::default();
    let mut stack = vec![rhs];
    while let Some(id) = stack.pop() {
        let node = rhs_arena.get(id);
        if unique(rhs_counts, node.content_id()) {
            rhs_unique.insert(node.content_id());
        }
        if let Syntax::List(list) = node {
            stack.extend(list.children.iter().copied());
        }
    }

    let mut shared = 0;
    let mut stack = vec![lhs];
    while let Some(id) = stack.pop() {
        let node = lhs_arena.get(id);
        let content = node.content_id();
        if unique(lhs_counts, content) && rhs_unique.contains(&content) {
            shared += 1;
            if shared >= MOSTLY_UNCHANGED_MIN_COMMON_CHILDREN {
                return true;
            }
        } else if let Syntax::List(list) = node {
            stack.extend(list.children.iter().copied());
        }
    }
    false
}

/// Recurse into a one-node-each run when both nodes are lists of the same
/// kind, splitting their children into sub-sections. Returns `true` when it
/// recursed, `false` when the caller should emit the pair as one section.
///
/// This is difftastic's singleton-list rule. A lone modified container -- a
/// function whose body changed, say -- descends so only its changed statements
/// become sections rather than the whole body.
fn recurse_singleton_lists(
    lhs_arena: &SyntaxArena<'_>,
    rhs_arena: &SyntaxArena<'_>,
    lhs_id: SyntaxId,
    rhs_id: SyntaxId,
    uniqueness: &Uniqueness,
    out: &mut PreprocessResult,
) -> bool {
    if let (Syntax::List(lhs_list), Syntax::List(rhs_list)) =
        (lhs_arena.get(lhs_id), rhs_arena.get(rhs_id))
        && lhs_list.kind == rhs_list.kind
    {
        let lhs_grand = lhs_list.children.clone();
        let rhs_grand = rhs_list.children.clone();
        pair_children(
            lhs_arena, rhs_arena, &lhs_grand, &rhs_grand, uniqueness, out,
        );
        true
    } else {
        false
    }
}

fn count_ids(arena: &SyntaxArena<'_>) -> IdCounts {
    let mut counts = IdCounts::default();
    for i in 0..arena.len() {
        *counts
            .entry(arena.get(SyntaxId(i)).content_id())
            .or_default() += 1;
    }
    counts
}

/// Mark the subtrees `lhs_id` and `rhs_id` [`ChangeKind::Unchanged`] and record
/// them as a pair in `out.pairs`.
///
/// One call does both, so no matched subtree goes unrecorded.
fn match_subtrees(
    lhs_arena: &SyntaxArena<'_>,
    rhs_arena: &SyntaxArena<'_>,
    lhs_id: SyntaxId,
    rhs_id: SyntaxId,
    out: &mut PreprocessResult,
) {
    mark_subtree(
        lhs_arena,
        lhs_id,
        &mut out.lhs_changes,
        ChangeKind::Unchanged,
    );
    mark_subtree(
        rhs_arena,
        rhs_id,
        &mut out.rhs_changes,
        ChangeKind::Unchanged,
    );
    out.pairs.push((lhs_id, rhs_id));
}

/// Mark `id` and every transitive descendant in the same arena as `kind`.
pub(crate) fn mark_subtree(
    arena: &SyntaxArena<'_>,
    id: SyntaxId,
    changes: &mut ChangeMap,
    kind: ChangeKind,
) {
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        changes.mark(current, kind);
        if let Syntax::List(list) = arena.get(current) {
            stack.extend(list.children.iter().copied());
        }
    }
}

fn list_children(arena: &SyntaxArena<'_>, id: SyntaxId) -> Vec<SyntaxId> {
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
/// [`LcsPairSink`] reconstructs the in-order pairs from them. No slider
/// heuristic runs, since a content id carries no indentation to place a
/// region by.
///
/// Histogram may under-match the true longest common subsequence on
/// degenerate repeats. The per-section Dijkstra search recovers those, so
/// the diff output is unchanged.
fn lcs_pairs(
    lhs_arena: &SyntaxArena<'_>,
    rhs_arena: &SyntaxArena<'_>,
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

    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut sink = LcsPairSink {
        lhs_cursor: 0,
        rhs_cursor: 0,
        pairs: Vec::new(),
    };
    for hunk in diff.hunks() {
        sink.emit_equal(hunk.before.start, hunk.after.start);
        sink.lhs_cursor = hunk.before.end;
        sink.rhs_cursor = hunk.after.end;
    }
    sink.emit_equal(lhs.len() as u32, rhs.len() as u32);
    sink.pairs
}

/// Reconstructs [`lcs_pairs`]' matched anchors from imara-diff's change
/// hunks. imara-diff reports the differing runs. The equal runs between them
/// are the matches, and they advance in lockstep on both sides, so each
/// equal token yields one `(lhs_idx, rhs_idx)` pair.
struct LcsPairSink {
    lhs_cursor: u32,
    rhs_cursor: u32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, structural_diff::lower_tree, LanguageRegistry};
    use std::ops::Range;

    fn rust_lang() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    fn lower(source: &str) -> (SyntaxArena<'_>, SyntaxId) {
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        lower_tree(&tree, source)
    }

    fn count_unchanged(arena: &SyntaxArena<'_>, root: SyntaxId, changes: &ChangeMap) -> usize {
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

    fn count_total(arena: &SyntaxArena<'_>, root: SyntaxId) -> usize {
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
    fn adjacent_changed_siblings_sharing_unique_subtrees_recurse_separately() {
        let lhs = "fn a() { a1(); a2(); a3(); a4(); old_a(); }\n\
                   fn b() { b1(); b2(); b3(); b4(); old_b(); }";
        let rhs = "fn a() { a1(); a2(); a3(); a4(); new_a(); }\n\
                   fn b() { b1(); b2(); b3(); b4(); new_b(); }";

        assert_eq!(
            section_texts(lhs, rhs),
            [
                (vec!["old_a"], vec!["new_a"]),
                (vec!["old_b"], vec!["new_b"])
            ],
            "each function recurses on its own down to its changed call",
        );
    }

    #[test]
    fn adjacent_changed_siblings_sharing_nothing_stay_one_section() {
        let lhs = "fn a() { x(); }\nfn b() { y(); }";
        let rhs = "fn c() { z(); }\nfn d() { w(); }";

        assert_eq!(
            section_texts(lhs, rhs),
            [(
                vec!["fn a() { x(); }", "fn b() { y(); }"],
                vec!["fn c() { z(); }", "fn d() { w(); }"],
            )],
            "with no unique subtree in common, the run stays one section",
        );
    }

    #[test]
    fn a_pair_peels_from_four_shared_unique_subtrees() {
        let three = "fn a() { a1(); a2(); old_a(); }\nfn b() { b1(); b2(); old_b(); }";
        let four = "fn a() { a1(); a2(); a3(); old_a(); }\nfn b() { b1(); b2(); b3(); old_b(); }";

        let shapes = [three, four].map(|lhs| {
            let rhs = lhs.replace("old_", "new_");
            section_texts(lhs, &rhs)
                .iter()
                .map(|(lhs_members, rhs_members)| (lhs_members.len(), rhs_members.len()))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            shapes,
            [vec![(2, 2)], vec![(1, 1), (1, 1)]],
            "a name and two calls stay one section, and a name and three calls peel",
        );
    }

    /// The sections that [`mark_unchanged`] leaves for `lhs` against `rhs`, as
    /// the texts of their members.
    fn section_texts<'a>(lhs: &'a str, rhs: &'a str) -> Vec<(Vec<&'a str>, Vec<&'a str>)> {
        let (lhs_arena, lhs_root) = lower(lhs);
        let (rhs_arena, rhs_root) = lower(rhs);
        let result = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);

        let texts = |arena: &SyntaxArena<'_>, source: &'a str, ids: &[SyntaxId]| -> Vec<&'a str> {
            ids.iter().map(|id| &source[span(arena, *id)]).collect()
        };
        result
            .sections
            .iter()
            .map(|(l, r)| (texts(&lhs_arena, lhs, l), texts(&rhs_arena, rhs, r)))
            .collect()
    }

    /// The byte span from the first atom under `id` to the last.
    fn span(arena: &SyntaxArena<'_>, id: SyntaxId) -> Range<usize> {
        match arena.get(id) {
            Syntax::Atom(atom) => atom.byte_range.clone(),
            Syntax::List(list) => {
                let first = list.children.first().expect("a lowered list has children");
                let last = list.children.last().expect("a lowered list has children");
                span(arena, *first).start..span(arena, *last).end
            },
        }
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
        let mk = |arena: &mut SyntaxArena<'_>, name: &str| {
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
        let mk = |arena: &mut SyntaxArena<'_>, name: &str| {
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
