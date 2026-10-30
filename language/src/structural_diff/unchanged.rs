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
//!    subsequence. Each match is an anchor, marked unchanged along with its subtree. In a middle
//!    whose two node counts multiply to at most 2,000, a matched atom anchors nothing and stays in
//!    its section, so one shared operator or comma does not split a small edit apart.
//!
//! 3. **Emit sections.** The unmatched run between two consecutive anchors is a changed section. A
//!    lone same-kind list on each side recurses into its children instead, so a small edit inside a
//!    container drills down to its changed statement. A longer run first peels each pair at its
//!    ends that shares at least four subtrees unique on their own side. A middle too large to
//!    search whole then pairs its members by the equal subtrees they share. Each peeled or shared
//!    pair recurses the same way, and every other non-empty run is a section.
//!
//! After this pass, every node carries a [`ChangeKind`] in a side [`ChangeMap`], and the changed
//! sections are collected in [`PreprocessResult::sections`]. The Dijkstra search then runs over one
//! section at a time rather than the whole file.
//!
//! The pass follows `split_unchanged` in `references/difftastic/src/diff/unchanged.rs`, but
//! sibling matching runs imara-diff's histogram over child [`ContentId`]s rather than `wu_diff`.
//! The peel is `split_mostly_unchanged_toplevel`, which difftastic applies to the top level alone
//! and this pass applies to every run. The pairing is the bottom-up phase of
//! `references/omnidiff/src/diff/solve_bottom_up_propagation.rs`, applied to one sibling run.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    content_id::ContentId,
    fx::FxBuildHasher,
    moves::MAX_AMBIGUITY,
};
use imara_diff::{Algorithm, Diff, InternedInput, Interner};
use smallvec::SmallVec;
use std::{
    cell::OnceCell,
    collections::{HashMap, HashSet},
};

/// The product of a middle's node counts, lhs times rhs, at or under which a
/// matched atom anchors no split.
///
/// One shared atom splits a small edit apart. The `||` of a left-nested chain
/// splits `[a, ||, b]` against `[a || b, ||, c]` into sections that pair `a`
/// with `a || b` and `b` with `c`, so the two `b` land in different sections
/// and the search marks each one novel. Inside its section, the atom pairs
/// wherever the search finds the cheapest alignment.
///
/// The bound is a product because the search costs about two vertices per node
/// of the product, as [`PAIRING_THRESHOLD`] records. So a middle at the budget
/// searches about 4,000 vertices, a few milliseconds. A large middle needs its
/// atoms. With no budget, the comma between two changed top-level pairs of
/// json-ipfs-ipfs-desktop-only-update-version-strings stops splitting them, and
/// one section of 198,000 nodes a side passes the vertex cap.
///
/// A reordered list pays for the rule. In `{a::B, C, D}` against `{D, C, a::B}`,
/// the search keeps the larger `a::B` and marks `C` and the commas, where the
/// anchors kept `C` and the commas and marked the two names that moved.
const ATOM_ANCHOR_BUDGET: u64 = 2_000;

/// The count of subtrees, each unique on its own side, that two unmatched
/// sibling lists must share to recurse as a pair before the search.
///
/// The value is difftastic's. A unique subtree that both lists hold is a
/// landmark that pins them together, and four of them separate a list that
/// kept most of its body from two lists that share only boilerplate.
const MOSTLY_UNCHANGED_MIN_COMMON_CHILDREN: usize = 4;

/// The product of a run's node counts, lhs times rhs, above which the run pairs
/// its members by shared content before the search.
///
/// The search costs about two vertices per node of the product and about one
/// microsecond per vertex. Over the fixture corpus, a product of 15,000 took
/// 29,000 vertices and 10 ms, 60,000 took 82,000 and 34 ms, and 151,000 took
/// 334,000 and 190 ms. A run under the threshold therefore searches whole near
/// 100 ms and keeps the optimal alignment. A larger run grows toward
/// [`super::dijkstra::DEFAULT_GRAPH_LIMIT`], where the search gives up on the
/// whole section.
const PAIRING_THRESHOLD: u64 = 50_000;

/// The atom leaves a subtree needs to weigh in [`pair_by_shared_content`], so a
/// `;` or a `)` that every member holds weighs nothing.
const PAIRING_MIN_LEAVES: u32 = 3;

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
    /// The `(lhs, rhs)` list pairs the pass recursed into, each two lists of
    /// one kind that differ, whose children it paired against each other.
    ///
    /// They stay apart from [`Self::pairs`], because only a pair matched whole
    /// pairs its children one for one. A consumer that climbs from a matched
    /// token through the structure around it reads both.
    pub entered: Vec<(SyntaxId, SyntaxId)>,
}

/// How many times each content id occurs in one arena.
type IdCounts = HashMap<ContentId, u32, FxBuildHasher>;

/// The node count and the atom leaf count of each subtree of one arena, as
/// [`SyntaxArena::subtree_counts`] gives them.
type SubtreeCounts = Vec<(u32, u32)>;

/// The facts about both arenas that the anchor and run handling read, lhs
/// first.
///
/// Each table forms on its first read, once per [`mark_unchanged`]. A diff
/// whose middles hold no atom anchor and whose runs all pair one to one never
/// pays for the passes over both arenas.
#[derive(Default)]
struct ArenaFacts {
    /// The occurrences of each content id, which tell
    /// [`is_mostly_unchanged_pair`] which subtrees are unique on their side.
    ///
    /// It takes a hash per node, and only a run with members on both sides and
    /// two or more on one side reads it.
    ids: OnceCell<(IdCounts, IdCounts)>,
    /// The node and atom leaf counts of each subtree.
    ///
    /// They size a middle for [`ATOM_ANCHOR_BUDGET`] and a run for
    /// [`PAIRING_THRESHOLD`], and weigh what [`pair_by_shared_content`] finds.
    /// Most diffs hold an atom anchor somewhere, so most build them, in one
    /// pass with no hashing.
    sizes: OnceCell<(SubtreeCounts, SubtreeCounts)>,
}

impl ArenaFacts {
    fn ids(&self, lhs_arena: &SyntaxArena, rhs_arena: &SyntaxArena) -> &(IdCounts, IdCounts) {
        self.ids
            .get_or_init(|| (count_ids(lhs_arena), count_ids(rhs_arena)))
    }

    fn sizes(
        &self,
        lhs_arena: &SyntaxArena,
        rhs_arena: &SyntaxArena,
    ) -> &(SubtreeCounts, SubtreeCounts) {
        self.sizes
            .get_or_init(|| (lhs_arena.subtree_counts(), rhs_arena.subtree_counts()))
    }
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
        pairs: Vec::new(),
        entered: Vec::new(),
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
        &ArenaFacts::default(),
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
/// first peels the mostly unchanged pairs at its ends, and a large middle pairs
/// its members by shared content, as [`emit_run`] describes. Every other
/// non-empty run goes to `out.sections` for the per-section Dijkstra search.
fn pair_children(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs: &[SyntaxId],
    rhs: &[SyntaxId],
    facts: &ArenaFacts,
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
        let pairs = lcs_pairs(lhs_arena, rhs_arena, lhs_mid, rhs_mid);
        splitting_anchors(lhs_arena, rhs_arena, lhs_mid, rhs_mid, pairs, facts)
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
        emit_run(lhs_arena, rhs_arena, lhs_run, rhs_run, facts, out);
        lhs_cursor = lhs_anchor + 1;
        rhs_cursor = rhs_anchor + 1;
    }
}

/// The pairs of `pairs` that anchor a split of the two middles.
///
/// Every list pair anchors. An atom pair anchors only when the node counts of
/// the two middles, the anchors counted too, multiply past
/// [`ATOM_ANCHOR_BUDGET`]. Under the budget, the atom stays in its section for
/// the search to pair.
fn splitting_anchors(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_mid: &[SyntaxId],
    rhs_mid: &[SyntaxId],
    pairs: Vec<(usize, usize)>,
    facts: &ArenaFacts,
) -> Vec<(usize, usize)> {
    let is_atom =
        |&(lhs, _): &(usize, usize)| matches!(lhs_arena.get(lhs_mid[lhs]), Syntax::Atom(_));
    // Every member holds a node, so member counts that multiply past the budget
    // put the node counts past it too, and the size table stays unread.
    let members = lhs_mid.len() as u64 * rhs_mid.len() as u64;
    if members > ATOM_ANCHOR_BUDGET || !pairs.iter().any(is_atom) {
        return pairs;
    }
    let (lhs_sizes, rhs_sizes) = facts.sizes(lhs_arena, rhs_arena);
    if node_total(lhs_sizes, lhs_mid) * node_total(rhs_sizes, rhs_mid) > ATOM_ANCHOR_BUDGET {
        return pairs;
    }
    pairs.into_iter().filter(|pair| !is_atom(pair)).collect()
}

/// Recurse into one unmatched run between two anchors, or emit it as
/// sections.
///
/// A run of one member per side goes to [`emit_pair`]. A longer run first
/// peels, from its front and then from its back, each pair that
/// [`is_mostly_unchanged_pair`] accepts, and sends each peeled pair to
/// [`emit_pair`]. A middle of one member per side goes to [`emit_pair`] too.
/// A middle that [`pairs_by_content`] accepts goes to [`emit_paired`], and any
/// other non-empty middle is one section.
///
/// The peel and the pairing keep edited members apart, so the search runs
/// inside each member and never over the whole run as one section.
fn emit_run(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_run: &[SyntaxId],
    rhs_run: &[SyntaxId],
    facts: &ArenaFacts,
    out: &mut PreprocessResult,
) {
    if lhs_run.len() == 1 && rhs_run.len() == 1 {
        emit_pair(lhs_arena, rhs_arena, lhs_run[0], rhs_run[0], facts, out);
        return;
    }

    let peels = |lhs, rhs| is_mostly_unchanged_pair(lhs_arena, rhs_arena, lhs, rhs, facts);
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
        emit_pair(lhs_arena, rhs_arena, lhs_run[k], rhs_run[k], facts, out);
    }

    let lhs_middle = &lhs_run[lead..lhs_run.len() - trail];
    let rhs_middle = &rhs_run[lead..rhs_run.len() - trail];
    if lhs_middle.len() == 1 && rhs_middle.len() == 1 {
        emit_pair(
            lhs_arena,
            rhs_arena,
            lhs_middle[0],
            rhs_middle[0],
            facts,
            out,
        );
    } else if pairs_by_content(lhs_arena, rhs_arena, lhs_middle, rhs_middle, facts) {
        emit_paired(lhs_arena, rhs_arena, lhs_middle, rhs_middle, facts, out);
    } else if !lhs_middle.is_empty() || !rhs_middle.is_empty() {
        out.sections
            .push((lhs_middle.to_vec(), rhs_middle.to_vec()));
    }

    for k in (0..trail).rev() {
        emit_pair(lhs_arena, rhs_arena, lhs_last(k), rhs_last(k), facts, out);
    }
}

/// Recurse into `lhs_id` and `rhs_id` when they are two lists of one kind, or
/// emit the pair as a one-member section.
fn emit_pair(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_id: SyntaxId,
    rhs_id: SyntaxId,
    facts: &ArenaFacts,
    out: &mut PreprocessResult,
) {
    if !recurse_singleton_lists(lhs_arena, rhs_arena, lhs_id, rhs_id, facts, out) {
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
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs: SyntaxId,
    rhs: SyntaxId,
    facts: &ArenaFacts,
) -> bool {
    if !matches!(
        (lhs_arena.get(lhs), rhs_arena.get(rhs)),
        (Syntax::List(_), Syntax::List(_))
    ) {
        return false;
    }
    let (lhs_counts, rhs_counts) = facts.ids(lhs_arena, rhs_arena);
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

/// Whether a run pairs its members by shared content before the search.
///
/// The run needs two members or more on each side, and node counts that
/// multiply past [`PAIRING_THRESHOLD`]. A lone member on one side sometimes
/// wraps or unwraps several members of the other side, as when nested `if let`
/// statements become one let chain. Pairing it with one of them strands the
/// rest as additions, while the search descends into the wrapper and aligns
/// them all. A run that fails the member count reads no facts.
fn pairs_by_content(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_run: &[SyntaxId],
    rhs_run: &[SyntaxId],
    facts: &ArenaFacts,
) -> bool {
    if lhs_run.len() < 2 || rhs_run.len() < 2 {
        return false;
    }
    let (lhs_sizes, rhs_sizes) = facts.sizes(lhs_arena, rhs_arena);
    node_total(lhs_sizes, lhs_run) * node_total(rhs_sizes, rhs_run) > PAIRING_THRESHOLD
}

/// The nodes of the subtrees of `run`, by the counts in `sizes`.
fn node_total(sizes: &[(u32, u32)], run: &[SyntaxId]) -> u64 {
    run.iter().map(|id| u64::from(sizes[id.0].0)).sum()
}

/// Emit `lhs_run` against `rhs_run` split at the pairs that
/// [`pair_by_shared_content`] finds, or as one section when it finds none.
///
/// Each pair goes to [`emit_pair`], and each stretch between two pairs goes
/// through [`emit_run`] again.
fn emit_paired(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_run: &[SyntaxId],
    rhs_run: &[SyntaxId],
    facts: &ArenaFacts,
    out: &mut PreprocessResult,
) {
    let pairs = pair_by_shared_content(lhs_arena, rhs_arena, lhs_run, rhs_run, facts);
    if pairs.is_empty() {
        out.sections.push((lhs_run.to_vec(), rhs_run.to_vec()));
        return;
    }

    let (mut lhs_from, mut rhs_from) = (0, 0);
    for (i, j) in pairs {
        let (lhs_stretch, rhs_stretch) = (&lhs_run[lhs_from..i], &rhs_run[rhs_from..j]);
        emit_run(lhs_arena, rhs_arena, lhs_stretch, rhs_stretch, facts, out);
        emit_pair(lhs_arena, rhs_arena, lhs_run[i], rhs_run[j], facts, out);
        (lhs_from, rhs_from) = (i + 1, j + 1);
    }
    let (lhs_stretch, rhs_stretch) = (&lhs_run[lhs_from..], &rhs_run[rhs_from..]);
    emit_run(lhs_arena, rhs_arena, lhs_stretch, rhs_stretch, facts, out);
}

/// The pairs of an lhs and an rhs member of a run that share the most content,
/// in order, as `(lhs index, rhs index)`.
///
/// A pair weighs the atom leaves of the equal subtrees that its two members
/// share. Only a subtree of at least [`PAIRING_MIN_LEAVES`] leaves counts,
/// and only when at most [`MAX_AMBIGUITY`] members hold it on each side, so
/// punctuation and boilerplate repeated across the run weigh nothing. A shared
/// subtree counts once, and the walk skips the nodes under it.
///
/// The heaviest pairs of two lists of one kind win in turn, each member at
/// most once. The result is their heaviest chain whose rhs indices rise with
/// the lhs indices, so the run splits into stretches in order.
fn pair_by_shared_content(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_run: &[SyntaxId],
    rhs_run: &[SyntaxId],
    facts: &ArenaFacts,
) -> Vec<(usize, usize)> {
    let (lhs_sizes, rhs_sizes) = facts.sizes(lhs_arena, rhs_arena);
    let lhs_holders = holders(lhs_arena, lhs_run, lhs_sizes);
    let rhs_holders = holders(rhs_arena, rhs_run, rhs_sizes);
    let held_by = |id: ContentId| {
        let lhs_count = lhs_holders.get(&id).map_or(0, SmallVec::len);
        rhs_holders
            .get(&id)
            .filter(|rhs| lhs_count <= MAX_AMBIGUITY && rhs.len() <= MAX_AMBIGUITY)
    };

    let mut weights: HashMap<(usize, usize), u64, FxBuildHasher> = HashMap::default();
    for (i, &member) in lhs_run.iter().enumerate() {
        let mut stack = vec![member];
        while let Some(id) = stack.pop() {
            let leaves = lhs_sizes[id.0].1;
            if leaves < PAIRING_MIN_LEAVES {
                continue;
            }
            let node = lhs_arena.get(id);
            if let Some(rhs_members) = held_by(node.content_id()) {
                for &j in rhs_members {
                    *weights.entry((i, j as usize)).or_default() += u64::from(leaves);
                }
            } else if let Syntax::List(list) = node {
                stack.extend(list.children.iter().copied());
            }
        }
    }

    let mut candidates: Vec<(u64, usize, usize)> = weights
        .into_iter()
        .map(|((i, j), weight)| (weight, i, j))
        .collect();
    candidates.sort_unstable_by(|a, b| b.cmp(a));

    let (mut lhs_free, mut rhs_free) = (vec![true; lhs_run.len()], vec![true; rhs_run.len()]);
    let mut taken: Vec<(usize, usize, u64)> = Vec::new();
    for (weight, i, j) in candidates {
        if lhs_free[i]
            && rhs_free[j]
            && same_kind_lists(lhs_arena.get(lhs_run[i]), rhs_arena.get(rhs_run[j]))
        {
            lhs_free[i] = false;
            rhs_free[j] = false;
            taken.push((i, j, weight));
        }
    }
    taken.sort_unstable();
    heaviest_rising_chain(&taken)
}

/// Each content id under the members of `run` whose subtree holds at least
/// [`PAIRING_MIN_LEAVES`] leaves, with the indices of the members that hold
/// it, each once and in order.
///
/// A node under the minimum is skipped with all of its descendants, because
/// no subtree inside it reaches the minimum either.
fn holders(
    arena: &SyntaxArena,
    run: &[SyntaxId],
    sizes: &[(u32, u32)],
) -> HashMap<ContentId, SmallVec<[u32; 4]>, FxBuildHasher> {
    let mut holders: HashMap<ContentId, SmallVec<[u32; 4]>, FxBuildHasher> = HashMap::default();
    for (index, &member) in run.iter().enumerate() {
        let index = index as u32;
        let mut stack = vec![member];
        while let Some(id) = stack.pop() {
            if sizes[id.0].1 < PAIRING_MIN_LEAVES {
                continue;
            }
            let node = arena.get(id);
            let members = holders.entry(node.content_id()).or_default();
            if members.last() != Some(&index) {
                members.push(index);
            }
            if let Syntax::List(list) = node {
                stack.extend(list.children.iter().copied());
            }
        }
    }
    holders
}

fn same_kind_lists(lhs: &Syntax, rhs: &Syntax) -> bool {
    matches!((lhs, rhs), (Syntax::List(lhs), Syntax::List(rhs)) if lhs.kind == rhs.kind)
}

/// The heaviest chain of `pairs`, given as `(lhs index, rhs index, weight)`
/// sorted by lhs index, whose rhs indices strictly rise, as
/// `(lhs index, rhs index)`.
///
/// Each pair looks back over every earlier pair, quadratic in the pair count,
/// which stays small because a run holds at most a few thousand members.
fn heaviest_rising_chain(pairs: &[(usize, usize, u64)]) -> Vec<(usize, usize)> {
    let mut best: Vec<(u64, Option<usize>)> = Vec::with_capacity(pairs.len());
    for (k, &(_, j, weight)) in pairs.iter().enumerate() {
        let before = (0..k)
            .filter(|&p| pairs[p].1 < j)
            .max_by_key(|&p| best[p].0);
        best.push(match before {
            Some(p) => (best[p].0 + weight, Some(p)),
            None => (weight, None),
        });
    }

    let mut chain = Vec::new();
    let mut at = (0..pairs.len()).max_by_key(|&k| best[k].0);
    while let Some(k) = at {
        chain.push((pairs[k].0, pairs[k].1));
        at = best[k].1;
    }
    chain.reverse();
    chain
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
    facts: &ArenaFacts,
    out: &mut PreprocessResult,
) -> bool {
    if let (Syntax::List(lhs_list), Syntax::List(rhs_list)) =
        (lhs_arena.get(lhs_id), rhs_arena.get(rhs_id))
        && lhs_list.kind == rhs_list.kind
    {
        let lhs_grand = lhs_list.children.clone();
        let rhs_grand = rhs_list.children.clone();
        out.entered.push((lhs_id, rhs_id));
        pair_children(lhs_arena, rhs_arena, &lhs_grand, &rhs_grand, facts, out);
        true
    } else {
        false
    }
}

fn count_ids(arena: &SyntaxArena) -> IdCounts {
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
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
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
    arena: &SyntaxArena,
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
/// [`LcsPairSink`] reconstructs the in-order pairs from them. No slider
/// heuristic runs, since a content id carries no indentation to place a
/// region by.
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
    use crate::structural_diff::lower_rust;
    use std::ops::Range;

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
        let (lhs_arena, lhs_root) = lower_rust(source);
        let (rhs_arena, rhs_root) = lower_rust(source);
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
        let (lhs_arena, lhs_root) = lower_rust(lhs);
        let (rhs_arena, rhs_root) = lower_rust(rhs);
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

    /// A grown binary chain shares its `||` with the old one. Under the budget
    /// the `||` anchors nothing, so the old chain's three children stay in one
    /// section beside the new chain's three.
    #[test]
    fn an_atom_anchors_no_split_in_a_small_run() {
        let lhs = "fn f() { if a.is_some() || b.is_some() { go(); } }";
        let rhs = "fn f() { if a.is_some() || b.is_some() || c { go(); } }";

        assert_eq!(
            section_texts(lhs, rhs),
            [(
                vec!["a.is_some()", "||", "b.is_some()"],
                vec!["a.is_some() || b.is_some()", "||", "c"],
            )],
            "one section holds both chains",
        );
    }

    /// A list anchors at every size, so the tuple an array kept splits its two
    /// edits apart, while the commas beside it anchor nothing.
    #[test]
    fn a_list_anchors_a_split_in_a_small_run() {
        assert_eq!(
            section_texts(
                "fn f() { let a = [0, (1, 1), 2]; }",
                "fn f() { let a = [9999, (1, 1), 8888]; }",
            ),
            [
                (vec!["0", ","], vec!["9999", ","]),
                (vec![",", "2"], vec![",", "8888"])
            ],
            "the kept tuple splits the run, and each comma stays with its edit",
        );
    }

    /// Past the budget an atom anchors again, so the commas of a long array
    /// keep its first and last edits apart. The node counts decide, not the
    /// member counts, so ten members that hold eight tuples keep their commas
    /// too. Ten numbers with the same edits search as one section.
    #[test]
    fn an_atom_anchors_a_split_past_the_budget() {
        let source = |items: &[String]| format!("fn f() {{ let a = [{}]; }}", items.join(", "));
        let ends_edited = |items: Vec<String>| {
            let mut edited = items.clone();
            let last = edited.len() - 1;
            edited[0] = "9999".to_string();
            edited[last] = "8888".to_string();
            (source(&items), source(&edited))
        };
        let numbers = |len: usize| (0..len).map(|i| i.to_string()).collect::<Vec<_>>();
        let tuples = (0..10)
            .map(|i| match i {
                0 | 9 => i.to_string(),
                _ => format!("({i}, {i}, {i})"),
            })
            .collect::<Vec<_>>();

        let (lhs, rhs) = ends_edited(numbers(1501));
        assert_eq!(
            section_texts(&lhs, &rhs),
            [(vec!["0"], vec!["9999"]), (vec!["1500"], vec!["8888"])],
            "the commas of a long array keep its two edits apart",
        );

        let (lhs, rhs) = ends_edited(tuples);
        assert_eq!(
            section_texts(&lhs, &rhs),
            [(vec!["0"], vec!["9999"]), (vec!["9"], vec!["8888"])],
            "19 members holding 75 nodes a side keep their commas",
        );

        let (lhs, rhs) = ends_edited(numbers(10));
        assert_eq!(
            section_texts(&lhs, &rhs),
            [(
                vec![
                    "0", ",", "1", ",", "2", ",", "3", ",", "4", ",", "5", ",", "6", ",", "7", ",",
                    "8", ",", "9"
                ],
                vec![
                    "9999", ",", "1", ",", "2", ",", "3", ",", "4", ",", "5", ",", "6", ",", "7",
                    ",", "8", ",", "8888"
                ],
            )],
            "ten numbers and their commas search as one section",
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

    #[test]
    fn a_large_run_pairs_its_members_by_shared_content() {
        let (lhs, rhs) = method_beside_new_module();
        let (lhs_arena, lhs_root) = lower_rust(&lhs);
        let (rhs_arena, rhs_root) = lower_rust(&rhs);
        let result = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);

        let keep = |arena: &SyntaxArena, source: &str, changes: &ChangeMap| {
            let id = (0..arena.len())
                .map(SyntaxId)
                .find(|id| {
                    arena.get(*id).kind() == "function_item"
                        && source[span(arena, *id)].starts_with("fn keep")
                })
                .expect("keep is a function");
            (count_unchanged(arena, id, changes), count_total(arena, id))
        };
        let (lhs_keep, rhs_keep) = (
            keep(&lhs_arena, &lhs, &result.lhs_changes),
            keep(&rhs_arena, &rhs, &result.rhs_changes),
        );
        assert_eq!(
            (lhs_keep.0, rhs_keep.0),
            (lhs_keep.1, rhs_keep.1),
            "every node of keep is unchanged on both sides",
        );

        let kinds = |arena: &SyntaxArena, ids: &[SyntaxId]| -> Vec<&str> {
            ids.iter().map(|id| arena.get(*id).kind()).collect()
        };
        let sections: Vec<_> = result
            .sections
            .iter()
            .map(|(l, r)| (kinds(&lhs_arena, l), kinds(&rhs_arena, r)))
            .collect();
        assert_eq!(
            sections,
            [
                (
                    vec!["use_declaration"],
                    vec!["use_declaration", "use_declaration"]
                ),
                (vec!["identifier"], vec!["identifier"]),
                (vec![], vec!["mod_item"]),
            ],
            "the two impls pair, so no section holds either",
        );
    }

    #[test]
    fn the_heaviest_rising_chain_drops_a_crossing_pair() {
        assert_eq!(
            heaviest_rising_chain(&[(0, 1, 5), (1, 0, 10), (2, 2, 3)]),
            [(1, 0), (2, 2)],
            "the heavier of two crossing pairs stays, with the pair after both",
        );
    }

    #[test]
    fn boilerplate_held_by_many_members_pairs_nothing() {
        let members = |tail: &str| -> String {
            (0..9)
                .map(|k| format!("fn f{k}() {{ common(1, 2); {tail}{k}(); }}\n"))
                .collect()
        };

        assert_eq!(
            content_pairs(&members("u"), &members("w")),
            Vec::<(usize, usize)>::new(),
            "a call that all nine members hold weighs nothing",
        );
    }

    #[test]
    fn a_shared_subtree_weighs_once_however_deep() {
        let lhs = "fn a() { outer(inner(x, y)); p1(); p2(); p3(); p4(); }\n";
        let rhs = "fn b() { outer(inner(x, y)); }\nfn c() { p1(); p2(); p3(); p4(); }\n";

        assert_eq!(
            content_pairs(lhs, rhs),
            [(0, 1)],
            "four calls of 3 leaves outweigh one statement of 10, nested calls and all",
        );
    }

    #[test]
    fn a_lone_member_against_several_skips_the_pairing() {
        let (lhs, rhs) = method_beside_new_module();
        let (lhs_arena, lhs_root) = lower_rust(&lhs);
        let (rhs_arena, rhs_root) = lower_rust(&rhs);
        let lhs_run = list_children(&lhs_arena, lhs_root);
        let rhs_run = list_children(&rhs_arena, rhs_root);
        let facts = ArenaFacts::default();

        assert_eq!(
            [&lhs_run[1..], &lhs_run[..]].map(|lhs_members| pairs_by_content(
                &lhs_arena,
                &rhs_arena,
                lhs_members,
                &rhs_run,
                &facts
            )),
            [false, true],
            "the impl alone against four members searches whole, and two members against four pair",
        );
    }

    #[test]
    fn a_use_never_pairs_with_an_impl() {
        assert_eq!(
            content_pairs("use a::b::c;\n", "impl X { fn f() { a::b::c(); } }\n"),
            Vec::<(usize, usize)>::new(),
            "the shared path weighs, but the two members are lists of different kinds",
        );
    }

    /// The pairs that [`pair_by_shared_content`] finds between the top-level
    /// items of `lhs` and `rhs`.
    fn content_pairs(lhs: &str, rhs: &str) -> Vec<(usize, usize)> {
        let (lhs_arena, lhs_root) = lower_rust(lhs);
        let (rhs_arena, rhs_root) = lower_rust(rhs);
        let lhs_run = list_children(&lhs_arena, lhs_root);
        let rhs_run = list_children(&rhs_arena, rhs_root);
        pair_by_shared_content(
            &lhs_arena,
            &rhs_arena,
            &lhs_run,
            &rhs_run,
            &ArenaFacts::default(),
        )
    }

    /// An `impl` with an unchanged 300-statement method and a changed one,
    /// against the same beside changed `use` lines and a new 300-statement
    /// module, as `(lhs, rhs)`.
    fn method_beside_new_module() -> (String, String) {
        let lets = |name: &str| -> String {
            (0..300)
                .map(|i| format!("let {name}{i} = {i};\n"))
                .collect()
        };
        let lhs = format!(
            "use crate::{{A, B}};\nimpl W {{ fn keep() {{ {} }} fn change() {{ old(); }} }}\n",
            lets("v"),
        );
        let rhs = format!(
            "use util::T;\nuse crate::{{S, A, B}};\n\
             impl W {{ fn keep() {{ {} }} fn change() {{ new(); }} }}\n\
             mod tests {{ fn t() {{ {} }} }}\n",
            lets("v"),
            lets("t"),
        );
        (lhs, rhs)
    }

    /// The sections that [`mark_unchanged`] leaves for `lhs` against `rhs`, as
    /// the texts of their members.
    fn section_texts<'a>(lhs: &'a str, rhs: &'a str) -> Vec<(Vec<&'a str>, Vec<&'a str>)> {
        let (lhs_arena, lhs_root) = lower_rust(lhs);
        let (rhs_arena, rhs_root) = lower_rust(rhs);
        let result = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);

        let texts = |arena: &SyntaxArena, source: &'a str, ids: &[SyntaxId]| -> Vec<&'a str> {
            ids.iter().map(|id| &source[span(arena, *id)]).collect()
        };
        result
            .sections
            .iter()
            .map(|(l, r)| (texts(&lhs_arena, lhs, l), texts(&rhs_arena, rhs, r)))
            .collect()
    }

    /// The byte span from the first atom under `id` to the last.
    fn span(arena: &SyntaxArena, id: SyntaxId) -> Range<usize> {
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
        let (lhs_arena, lhs_root) = lower_rust(lhs);
        let (rhs_arena, rhs_root) = lower_rust(rhs);
        let result = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);
        assert_eq!(result.lhs_changes.get(lhs_root), ChangeKind::Pending);
        assert_eq!(result.rhs_changes.get(rhs_root), ChangeKind::Pending);
    }

    #[test]
    fn lcs_pairs_in_order() {
        // Trivial sanity check on the LCS routine: input [A, B, C] and
        // [A, X, B] should produce [(0,0), (1,2)].
        let mut arena = SyntaxArena::default();
        use crate::structural_diff::{
            arena::{Atom, AtomClass, Syntax},
            ContentId,
        };
        let mk = |arena: &mut SyntaxArena, name: &str| {
            let id = ContentId::for_atom("ident", name);
            arena.alloc(Syntax::Atom(Atom {
                kind: "ident",
                class: AtomClass::Code,
                byte_range: 0..0,
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
            arena::{Atom, AtomClass, Syntax},
            ContentId,
        };
        let mut arena = SyntaxArena::default();
        let mk = |arena: &mut SyntaxArena, name: &str| {
            arena.alloc(Syntax::Atom(Atom {
                kind: "ident",
                class: AtomClass::Code,
                byte_range: 0..0,
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
