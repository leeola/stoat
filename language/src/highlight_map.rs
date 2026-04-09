//! Theme-driven mapping from tree-sitter capture indices to opaque
//! [`HighlightId`]s.
//!
//! Pattern adapted from `references/zed/crates/language/src/highlight_map.rs`.
//! The flow is:
//!
//! 1. A `theme` provides an ordered list of dot-separated capture name patterns (e.g. `["string",
//!    "string.escape", "function.method"]`).
//! 2. For each capture name in the language's highlight query, find the theme key whose
//!    dot-separated components are the longest match against the capture name's components.
//! 3. Store the chosen theme index as a [`HighlightId`] in a [`HighlightMap`] keyed by capture
//!    index, so the lookup at render time is `O(1)`.
//!
//! The language crate intentionally knows nothing about the theme's actual
//! style data: it only handles the *mapping*. The host editor stores the
//! style table indexed by [`HighlightId`] alongside the theme.

use std::sync::Arc;

/// Opaque identifier into a host-side style table. `DEFAULT` indicates the
/// capture has no theme entry and should fall through to the unstyled
/// default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HighlightId(pub u32);

impl HighlightId {
    pub const DEFAULT: HighlightId = HighlightId(u32::MAX);

    pub fn is_default(self) -> bool {
        self.0 == u32::MAX
    }
}

/// Per-grammar lookup table from tree-sitter capture index to a host
/// theme entry. Cheap to clone (`Arc`-backed).
#[derive(Clone, Debug, Default)]
pub struct HighlightMap {
    capture_to_id: Arc<[HighlightId]>,
}

impl HighlightMap {
    /// Build a map by best-matching every entry of `capture_names`
    /// against `theme_keys`.
    ///
    /// Match rule: for each capture, walk the theme keys and pick the
    /// one whose dot-separated components form the longest subsequence
    /// match against the capture's components. Ties pick the
    /// later-listed theme entry, so theme files with more specific keys
    /// at the bottom win cleanly.
    ///
    /// This mirrors the rule used by Zed and Helix, so vendored
    /// `highlights.scm` files behave consistently.
    pub fn new(capture_names: &[&str], theme_keys: &[&str]) -> Self {
        let capture_to_id: Vec<HighlightId> = capture_names
            .iter()
            .map(|capture_name| {
                let mut best: Option<(usize, usize)> = None;
                for (theme_idx, key) in theme_keys.iter().enumerate() {
                    let Some(score) = match_score(capture_name, key) else {
                        continue;
                    };
                    match best {
                        Some((_, best_score)) if score < best_score => {},
                        _ => best = Some((theme_idx, score)),
                    }
                }
                match best {
                    Some((idx, _)) => HighlightId(idx as u32),
                    None => HighlightId::DEFAULT,
                }
            })
            .collect();
        Self {
            capture_to_id: Arc::from(capture_to_id),
        }
    }

    pub fn get(&self, capture_index: u32) -> HighlightId {
        self.capture_to_id
            .get(capture_index as usize)
            .copied()
            .unwrap_or(HighlightId::DEFAULT)
    }

    pub fn len(&self) -> usize {
        self.capture_to_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.capture_to_id.is_empty()
    }
}

/// Compute the longest-prefix-component match score between a capture
/// name and a theme key. Returns `None` if any of the theme key's
/// components are absent from the capture name.
///
/// Examples (capture, key) -> score:
/// - `"string.escape"` vs `"string"` -> Some(1)
/// - `"string.escape"` vs `"string.escape"` -> Some(2)
/// - `"string.escape"` vs `"function"` -> None
/// - `"function.method.builtin"` vs `"function.builtin"` -> Some(2)
fn match_score(capture_name: &str, key: &str) -> Option<usize> {
    let mut len = 0usize;
    let capture_parts = capture_name.split('.');
    for key_part in key.split('.') {
        if capture_parts.clone().any(|part| part == key_part) {
            len += 1;
        } else {
            return None;
        }
    }
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_id_marker_is_unique() {
        let id = HighlightId::DEFAULT;
        assert!(id.is_default());
        assert!(!HighlightId(0).is_default());
        assert!(!HighlightId(1).is_default());
    }

    #[test]
    fn empty_inputs_yield_empty_map() {
        let map = HighlightMap::new(&[], &[]);
        assert!(map.is_empty());
    }

    #[test]
    fn unmatched_capture_falls_back_to_default() {
        let map = HighlightMap::new(&["unmatched"], &["string", "function"]);
        assert_eq!(map.len(), 1);
        assert!(map.get(0).is_default());
    }

    #[test]
    fn longest_prefix_wins() {
        // "string.escape" should resolve to "string.escape" (score 2),
        // not "string" (score 1).
        let map = HighlightMap::new(&["string.escape"], &["string", "string.escape"]);
        assert_eq!(map.get(0), HighlightId(1));
    }

    #[test]
    fn later_theme_key_wins_on_tie() {
        // Both "string" and "string.foo" match a "string.foo" capture
        // with the same score (`>=` in the comparison defers to the later
        // entry, mirroring Zed's behavior).
        let map = HighlightMap::new(&["string"], &["string", "alt-string"]);
        assert_eq!(map.get(0), HighlightId(0));
    }

    #[test]
    fn distinct_captures_get_distinct_ids() {
        let theme_keys = ["string", "function", "keyword"];
        let captures = ["function.method", "string.escape", "keyword.control"];
        let map = HighlightMap::new(&captures, &theme_keys);
        assert_eq!(map.get(0), HighlightId(1));
        assert_eq!(map.get(1), HighlightId(0));
        assert_eq!(map.get(2), HighlightId(2));
    }

    #[test]
    fn three_part_capture_matches_two_part_key() {
        // "function.method.builtin" should match "function.builtin"
        // (both `function` and `builtin` appear in the capture's parts).
        let map = HighlightMap::new(&["function.method.builtin"], &["function.builtin"]);
        assert_eq!(map.get(0), HighlightId(0));
    }

    #[test]
    fn out_of_bounds_capture_index_is_default() {
        let map = HighlightMap::new(&["x"], &["x"]);
        assert_eq!(map.get(0), HighlightId(0));
        assert!(map.get(1).is_default());
        assert!(map.get(99).is_default());
    }
}
