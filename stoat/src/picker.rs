use crate::{fuzzy, paths};
use std::path::{Path, PathBuf};

/// Query-driven fuzzy result list over a fixed `base` set of paths, decoupled
/// from any input widget.
///
/// The file finder and the palette's inline pickers drive the same list from a
/// query string. The owner sets `base`, calls [`PickList::refilter`] with the
/// query, and reads `filtered`/`match_indices`/`selected` to render.
#[derive(Default)]
pub(crate) struct PickList {
    /// Candidate paths the query filters over.
    pub(crate) base: Vec<PathBuf>,
    /// Indices into `base`, after filtering, in display order.
    pub(crate) filtered: Vec<usize>,
    /// Per-row matched character offsets into the row's display string,
    /// parallel to `filtered`. A row is empty when no pattern is active. The
    /// offsets are sorted and deduplicated so the renderer can `contains`-test
    /// without further work.
    pub(crate) match_indices: Vec<Vec<u32>>,
    pub(crate) selected: usize,
}

impl PickList {
    /// Absolute path of the currently selected filtered row, if any.
    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.filtered
            .get(self.selected)
            .and_then(|i| self.base.get(*i))
            .map(|p| p.as_path())
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.filtered.len() - 1) as i32;
        let next = (self.selected as i32 + delta).clamp(0, max);
        self.selected = next as usize;
    }

    /// Re-run the matcher over `base` for `query` via
    /// [`crate::fuzzy::match_and_rank`], ordering matches by score descending,
    /// ties alphabetical. Empty or whitespace-only input lists every candidate
    /// alphabetically.
    ///
    /// `match_indices` is rebuilt in parallel to `filtered`. Each element is the
    /// sorted, deduplicated set of matched character offsets in that row's
    /// display string, or empty when no pattern is active.
    pub(crate) fn refilter(&mut self, query: &str, git_root: &Path) {
        self.filtered.clear();
        self.match_indices.clear();

        let items = self
            .base
            .iter()
            .enumerate()
            .map(|(idx, path)| (idx, paths::display_relative(path, git_root)));
        let Some(mut matches) = fuzzy::match_and_rank(query, items) else {
            let mut rows: Vec<(usize, String)> = self
                .base
                .iter()
                .enumerate()
                .map(|(idx, path)| (idx, paths::display_relative(path, git_root)))
                .collect();
            rows.sort_by(|a, b| a.1.cmp(&b.1));
            for (idx, _) in &rows {
                self.filtered.push(*idx);
                self.match_indices.push(Vec::new());
            }
            self.clamp_selected();
            return;
        };

        matches.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.haystack.cmp(&b.haystack))
        });
        for m in matches {
            self.filtered.push(m.item);
            self.match_indices.push(m.matched_indices);
        }
        self.clamp_selected();
    }

    fn clamp_selected(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Display strings of the filtered rows after running `query` over `base`.
    fn names(query: &str, base: Vec<PathBuf>, git_root: &Path) -> Vec<String> {
        let mut list = PickList {
            base,
            ..PickList::default()
        };
        list.refilter(query, git_root);
        list.filtered
            .iter()
            .map(|i| paths::display_relative(&list.base[*i], git_root))
            .collect()
    }

    #[test]
    fn empty_input_lists_all_base_paths_sorted() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs"), p("/r/a.rs"), p("/r/sub/c.rs")];
        assert_eq!(names("", base, &git_root), vec!["a.rs", "b.rs", "sub/c.rs"]);
    }

    #[test]
    fn prefix_ranks_before_substring_before_fuzzy() {
        let git_root = p("/r");
        let base = vec![
            p("/r/file.rs"),      // prefix
            p("/r/sub/file.rs"),  // substring
            p("/r/fee/nile.rs"),  // fuzzy (f..i..l..e)
            p("/r/unrelated.rs"), // filtered out
        ];
        assert_eq!(
            names("file", base, &git_root),
            vec!["file.rs", "sub/file.rs", "fee/nile.rs"]
        );
    }

    #[test]
    fn case_insensitive_filter() {
        let git_root = p("/r");
        let base = vec![p("/r/Foo.rs"), p("/r/bar.rs")];
        assert_eq!(names("foo", base, &git_root), vec!["Foo.rs"]);
    }

    #[test]
    fn trailing_space_does_not_eliminate_matches() {
        let git_root = p("/r");
        let base = vec![p("/r/foo.rs"), p("/r/bar.rs")];
        assert_eq!(names(".rs ", base, &git_root), vec!["bar.rs", "foo.rs"]);
    }

    #[test]
    fn multi_token_query_matches_in_either_order() {
        let git_root = p("/r");
        let base = vec![p("/r/src/foo.rs"), p("/r/src/bar.rs")];
        let forward = names(".rs foo", base.clone(), &git_root);
        let reverse = names("foo .rs", base, &git_root);
        assert_eq!(forward, vec!["src/foo.rs"]);
        assert_eq!(forward, reverse);
    }

    #[test]
    fn whitespace_only_query_lists_all_paths() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs"), p("/r/a.rs")];
        assert_eq!(names("   ", base, &git_root), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn exact_basename_match_outranks_longer_prefix_match() {
        let git_root = p("/r");
        let base = vec![p("/r/food_handler.rs"), p("/r/foo.rs")];
        assert_eq!(
            names("foo", base, &git_root),
            vec!["foo.rs", "food_handler.rs"]
        );
    }

    #[test]
    fn filters_against_a_subset_base() {
        let git_root = p("/r");
        let base = vec![p("/r/b.rs")];
        assert_eq!(names("", base, &git_root), vec!["b.rs"]);
    }

    #[test]
    fn empty_base_lists_nothing() {
        let git_root = p("/r");
        assert!(names("", vec![], &git_root).is_empty());
    }

    #[test]
    fn lists_every_base_path_on_empty_query() {
        let git_root = p("/r");
        let base = vec![p("/r/a.rs"), p("/r/c.rs")];
        assert_eq!(names("", base, &git_root), vec!["a.rs", "c.rs"]);
    }

    #[test]
    fn refilter_clamps_selected_when_results_shrink() {
        let git_root = p("/r");
        let mut list = PickList {
            base: vec![p("/r/a.rs"), p("/r/b.rs"), p("/r/c.rs")],
            selected: 2,
            ..PickList::default()
        };
        list.refilter("b", &git_root);
        assert_eq!(list.filtered.len(), 1);
        assert_eq!(list.selected, 0);
    }
}
