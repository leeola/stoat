// The command-palette consumers -- the Workspace field, the palette Up/Down
// handlers, and per-workspace persistence -- land in follow-up items, so until
// then this module is exercised only by its own tests. Remove this allow once
// those wire it in.
#![allow(dead_code)]

/// Fish-style recall history for a single-line input, such as the command
/// palette.
///
/// Executed lines are remembered oldest-first (the newest is [`Vec::last`]).
/// Recall walks them newest-to-oldest under a substring needle. The text
/// already typed at the first [`Self::prev`] press is captured as `pending` and
/// used to filter matches, so typing `view` then pressing Up recalls
/// `cd ~/work/viewership`. The needle stays fixed across a walk even as the
/// recalled text changes, and walking back past the newest match restores the
/// originally-typed text.
///
/// The type is pure. [`Self::prev`] and [`Self::next`] return the line to show
/// rather than mutating any input, so the caller owns the input swap.
#[derive(Debug, Default)]
pub(crate) struct InputHistory {
    entries: Vec<String>,
    /// Index into [`Self::entries`] of the entry currently recalled, or `None`
    /// when no recall walk is active.
    cursor: Option<usize>,
    /// The text typed before the walk began. It is both the substring needle
    /// and the value restored when the walk steps past the newest match.
    pending: Option<String>,
    /// The last line [`Self::prev`] or [`Self::next`] returned, so
    /// [`Self::reset_if_edited`] can tell an edit from an untouched recall.
    last_recalled: Option<String>,
}

/// Newest-first cap on retained entries, evicting the oldest past this.
const MAX_ENTRIES: usize = 100;

impl InputHistory {
    /// Restore a history from persisted entries, oldest-first, with no active
    /// recall walk.
    pub(crate) fn from_entries(entries: Vec<String>) -> Self {
        Self {
            entries,
            cursor: None,
            pending: None,
            last_recalled: None,
        }
    }

    /// The retained entries, oldest-first, for persistence.
    pub(crate) fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Record an executed `entry` as the newest.
    ///
    /// Empty lines and a line equal to the current newest are dropped, so
    /// re-running the same command does not stack duplicates. Retention is
    /// capped at [`MAX_ENTRIES`] by evicting the oldest.
    pub(crate) fn push(&mut self, entry: String) {
        if entry.is_empty() || self.entries.last() == Some(&entry) {
            return;
        }
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
    }

    /// Walk one entry older, returning the line to show or `None` to leave the
    /// input unchanged.
    ///
    /// The first press (no active walk) captures `current` as the needle. Later
    /// presses keep that needle even though `current` now holds a recalled
    /// line. Returns `None` at the oldest match, so the walk saturates rather
    /// than wrapping.
    pub(crate) fn prev(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        if self.cursor.is_none() {
            self.pending = Some(current.to_string());
        }
        let start = match self.cursor {
            Some(0) => return None,
            Some(cursor) => cursor - 1,
            None => self.entries.len() - 1,
        };

        let needle = self.pending.as_deref().unwrap_or("");
        let found = (0..=start)
            .rev()
            .find(|&i| self.entries[i].contains(needle))?;

        self.cursor = Some(found);
        self.last_recalled = Some(self.entries[found].clone());
        Some(self.entries[found].clone())
    }

    /// Walk one entry newer, returning the line to show or `None` to leave the
    /// input unchanged.
    ///
    /// A no-op without an active walk. Stepping past the newest match ends the
    /// walk and returns the originally-typed text. `current` is accepted for
    /// symmetry with [`Self::prev`]. The walk itself is driven by the cursor.
    pub(crate) fn next(&mut self, current: &str) -> Option<String> {
        let _ = current;
        let cursor = self.cursor?;

        let needle = self.pending.as_deref().unwrap_or("");
        let found = ((cursor + 1)..self.entries.len()).find(|&i| self.entries[i].contains(needle));

        match found {
            Some(i) => {
                self.cursor = Some(i);
                self.last_recalled = Some(self.entries[i].clone());
                Some(self.entries[i].clone())
            },
            None => {
                self.cursor = None;
                let pending = self.pending.clone().unwrap_or_default();
                self.last_recalled = Some(pending.clone());
                Some(pending)
            },
        }
    }

    /// End any active recall walk, dropping the captured needle.
    pub(crate) fn reset(&mut self) {
        self.cursor = None;
        self.pending = None;
        self.last_recalled = None;
    }

    /// End the walk if the input was edited during one, so the next
    /// [`Self::prev`] captures a fresh needle.
    ///
    /// A no-op when no walk is active or `current` still matches the recalled
    /// line.
    pub(crate) fn reset_if_edited(&mut self, current: &str) {
        if self.cursor.is_some() && self.last_recalled.as_deref() != Some(current) {
            self.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::InputHistory;

    fn history(entries: &[&str]) -> InputHistory {
        InputHistory::from_entries(entries.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn prev_recalls_the_newest_match_skipping_non_matches() {
        let mut h = history(&["ls", "cd ~/work/viewership", "cat x"]);
        assert_eq!(h.prev("view").as_deref(), Some("cd ~/work/viewership"));
    }

    #[test]
    fn needle_stays_captured_across_repeated_prev() {
        let mut h = history(&["cd a", "ls", "cd b"]);
        assert_eq!(h.prev("cd").as_deref(), Some("cd b"));
        // The recalled text is now "cd b", but the needle stays "cd", so the
        // next Up reaches the older "cd a" rather than re-capturing "cd b".
        assert_eq!(h.prev("cd b").as_deref(), Some("cd a"));
        assert_eq!(
            h.prev("cd a").as_deref(),
            None,
            "saturates at the oldest match"
        );
    }

    #[test]
    fn next_past_the_newest_restores_pending_then_prev_recaptures() {
        let mut h = history(&["ls -la", "cd b"]);
        assert_eq!(h.prev("cd").as_deref(), Some("cd b"));
        assert_eq!(
            h.next("cd b").as_deref(),
            Some("cd"),
            "restores the typed needle"
        );
        assert_eq!(
            h.next("cd").as_deref(),
            None,
            "no active walk after restore"
        );
        assert_eq!(
            h.prev("ls").as_deref(),
            Some("ls -la"),
            "a fresh needle is captured"
        );
    }

    #[test]
    fn empty_needle_walks_every_entry_newest_first() {
        let mut h = history(&["a", "b", "c"]);
        assert_eq!(h.prev("").as_deref(), Some("c"));
        assert_eq!(h.prev("").as_deref(), Some("b"));
        assert_eq!(h.prev("").as_deref(), Some("a"));
        assert_eq!(h.prev("").as_deref(), None);
    }

    #[test]
    fn push_skips_empty_dedups_the_head_and_caps_at_100() {
        let mut h = InputHistory::default();
        h.push("a".to_string());
        h.push("a".to_string());
        h.push(String::new());
        h.push("b".to_string());
        assert_eq!(h.entries(), ["a", "b"]);

        let mut h = InputHistory::default();
        for i in 0..150 {
            h.push(format!("cmd{i}"));
        }
        assert_eq!(h.entries().len(), 100);
        assert_eq!(h.entries().first().map(String::as_str), Some("cmd50"));
        assert_eq!(h.entries().last().map(String::as_str), Some("cmd149"));
    }

    #[test]
    fn reset_if_edited_only_resets_on_divergence() {
        let mut h = history(&["cd a", "cd b"]);
        assert_eq!(h.prev("cd").as_deref(), Some("cd b"));

        h.reset_if_edited("cd b");
        assert_eq!(
            h.prev("cd b").as_deref(),
            Some("cd a"),
            "an untouched recall keeps walking under the same needle"
        );

        h.reset_if_edited("typed over");
        assert_eq!(
            h.prev("cd").as_deref(),
            Some("cd b"),
            "an edit ends the walk, so the next Up restarts from the newest match"
        );
    }
}
