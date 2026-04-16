//! Unchanged-region preprocessing for the structural diff.
//!
//! Difftastic's diff is Dijkstra over an edit graph; the search space is
//! enormous on real files, so before running the search a preprocessing
//! pass tags every node that is *trivially* unchanged. The preprocessing
//! cost is `O(n)` (plus an `O(n*m)` LCS over top-level [`ContentId`]s);
//! the savings are substantial because the Dijkstra walk only sees the
//! novel regions.
//!
//! Three steps, in order:
//!
//! 1. **Shrink at endpoints** (cheap): walk both sides from the front, pairing children with
//!    matching `ContentId`. Stop on the first mismatch. Repeat from the back. The matched ranges
//!    are tagged [`ChangeKind::Unchanged`] recursively.
//!
//! 2. **LCS over top-level children**: for the remaining middle of each list, run an LCS over child
//!    `ContentId`s. Each match in the LCS recursively marks its subtree unchanged.
//!
//! 3. **Recursive deep-mark**: when both sides agree the same subtree is unchanged, walk into it
//!    and tag every descendant the same way. The Dijkstra search can then skip these subtrees
//!    entirely.
//!
//! After this pass, every node carries a [`ChangeKind`] in a side
//! [`ChangeMap`]; the diff search only enumerates `Pending` nodes.
//!
//! Reference: `references/difftastic/src/diff/unchanged.rs`. The
//! algorithm is the same; we use a vendored LCS instead of `wu_diff`
//! to avoid an external crate.

use super::arena::{Syntax, SyntaxArena, SyntaxId};

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

/// Result of [`mark_unchanged`]. Holds one [`ChangeMap`] per side; the
/// caller threads them through to the structural-diff search.
#[derive(Clone, Debug, Default)]
pub struct PreprocessResult {
    pub lhs_changes: ChangeMap,
    pub rhs_changes: ChangeMap,
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
    );

    result
}

/// Pair two child lists via shrink-then-LCS, marking matched subtrees
/// recursively. Unmatched [`Syntax::List`] pairs at the same position
/// (kind matches, content differs) recurse into their grandchildren so
/// nested unchanged regions are also tagged.
fn pair_children(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs: &[SyntaxId],
    rhs: &[SyntaxId],
    lhs_changes: &mut ChangeMap,
    rhs_changes: &mut ChangeMap,
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

    // Middle ranges to consider via LCS.
    let lhs_mid = &lhs[prefix..lhs.len() - suffix];
    let rhs_mid = &rhs[prefix..rhs.len() - suffix];

    // Phase 2: LCS over content_ids. Matched pairs are byte-for-byte
    // equal, so we mark them and their descendants Unchanged.
    let mut lhs_matched = vec![false; lhs_mid.len()];
    let mut rhs_matched = vec![false; rhs_mid.len()];
    if !lhs_mid.is_empty() && !rhs_mid.is_empty() {
        let lcs = lcs_pairs(lhs_arena, rhs_arena, lhs_mid, rhs_mid);
        for (lhs_idx, rhs_idx) in lcs {
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
            lhs_matched[lhs_idx] = true;
            rhs_matched[rhs_idx] = true;
        }
    }

    // Phase 3: same-kind List pairs that the LCS could not match
    // (because their content_ids differ) get a recursive descent. This
    // catches nested unchanged regions inside otherwise-novel
    // containers, e.g. an unchanged statement inside a modified
    // function body.
    let lhs_unmatched: Vec<usize> = (0..lhs_mid.len()).filter(|i| !lhs_matched[*i]).collect();
    let rhs_unmatched: Vec<usize> = (0..rhs_mid.len()).filter(|i| !rhs_matched[*i]).collect();
    let pair_count = lhs_unmatched.len().min(rhs_unmatched.len());
    for k in 0..pair_count {
        let lhs_id = lhs_mid[lhs_unmatched[k]];
        let rhs_id = rhs_mid[rhs_unmatched[k]];
        // Only recurse when both sides are lists of the same kind. Mixing
        // an Atom with a List, or two lists of different kinds, is left
        // for the structural-diff search to resolve.
        if let (Syntax::List(lhs_list), Syntax::List(rhs_list)) =
            (lhs_arena.get(lhs_id), rhs_arena.get(rhs_id))
        {
            if lhs_list.kind == rhs_list.kind {
                let lhs_grand = lhs_list.children.clone();
                let rhs_grand = rhs_list.children.clone();
                pair_children(
                    lhs_arena,
                    rhs_arena,
                    &lhs_grand,
                    &rhs_grand,
                    lhs_changes,
                    rhs_changes,
                );
            }
        }
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

/// O(n*m) LCS over [`super::ContentId`]s, returning matching index
/// pairs `(lhs_idx, rhs_idx)` in source order. The algorithm is the
/// classic DP table walk; we accept the quadratic memory cost because
/// the input is bounded by the children of one node, which is small in
/// practice.
fn lcs_pairs(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs: &[SyntaxId],
    rhs: &[SyntaxId],
) -> Vec<(usize, usize)> {
    let l = lhs.len();
    let r = rhs.len();
    let mut table = vec![vec![0u32; r + 1]; l + 1];
    for i in 0..l {
        for j in 0..r {
            if lhs_arena.get(lhs[i]).content_id() == rhs_arena.get(rhs[j]).content_id() {
                table[i + 1][j + 1] = table[i][j] + 1;
            } else {
                table[i + 1][j + 1] = table[i + 1][j].max(table[i][j + 1]);
            }
        }
    }

    let mut pairs = Vec::new();
    let mut i = l;
    let mut j = r;
    while i > 0 && j > 0 {
        if lhs_arena.get(lhs[i - 1]).content_id() == rhs_arena.get(rhs[j - 1]).content_id() {
            pairs.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if table[i][j - 1] >= table[i - 1][j] {
            j -= 1;
        } else {
            i -= 1;
        }
    }
    pairs.reverse();
    pairs
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
}
