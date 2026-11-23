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
/// rather than mutating any input, so the caller owns the input swap. It reads
/// no clock, so a caller that keeps ages passes each line's time to
/// [`Self::push_at`].
#[derive(Debug, Default)]
pub(crate) struct InputHistory {
    entries: Vec<String>,
    /// The time each line of [`Self::entries`] last ran at, in unix seconds,
    /// at the same index, or `None` for a line with no known time.
    ///
    /// It holds one time per line, so a line and its time leave the history
    /// together.
    times: Vec<Option<i64>>,
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
    /// Restore a history from persisted entries, oldest-first, with no known
    /// times and no active recall walk.
    pub(crate) fn from_entries(entries: Vec<String>) -> Self {
        let times = vec![None; entries.len()];
        Self::from_timed(entries, times)
    }

    /// Restore a history from persisted entries, oldest-first, and the time
    /// each one last ran at, with no active recall walk.
    ///
    /// The times line up with the entries by index. A list shorter than the
    /// entries pads with `None`, as a state file that predates the times holds
    /// none. Times past the last entry drop.
    pub(crate) fn from_timed(entries: Vec<String>, mut times: Vec<Option<i64>>) -> Self {
        times.resize(entries.len(), None);
        Self {
            entries,
            times,
            cursor: None,
            pending: None,
            last_recalled: None,
        }
    }

    /// The retained entries, oldest-first, for persistence.
    pub(crate) fn entries(&self) -> &[String] {
        &self.entries
    }

    /// The time each retained entry last ran at, in unix seconds, at the index
    /// of its entry in [`Self::entries`].
    pub(crate) fn times(&self) -> &[Option<i64>] {
        &self.times
    }

    /// Record an executed `entry` as the newest, with no time, as
    /// [`Self::push_at`] records it.
    pub(crate) fn push(&mut self, entry: String) {
        self.push_at(entry, None);
    }

    /// Record an executed `entry` as the newest, run at `at` in unix seconds.
    ///
    /// Empty lines are dropped. A line equal to the current newest is not
    /// stored a second time, so re-running the same command does not stack
    /// duplicates. The newest line takes `at` instead, because its age names
    /// its last run. Retention is capped at [`MAX_ENTRIES`] by evicting the
    /// oldest line with its time.
    pub(crate) fn push_at(&mut self, entry: String, at: Option<i64>) {
        if entry.is_empty() {
            return;
        }
        if self.entries.last() == Some(&entry) {
            if let Some(newest) = self.times.last_mut() {
                *newest = at;
            }
            return;
        }

        self.entries.push(entry);
        self.times.push(at);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
            self.times.remove(0);
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

    /// Whether a recall walk is active, which the palette shows as History.
    pub(crate) fn is_walking(&self) -> bool {
        self.cursor.is_some()
    }

    /// The needle of the active walk, the text typed before the walk started,
    /// or `""` with no walk.
    pub(crate) fn needle(&self) -> &str {
        self.pending
            .as_deref()
            .filter(|_| self.is_walking())
            .unwrap_or("")
    }

    /// The index into [`Self::entries`] of the line that the walk recalls, or
    /// `None` with no walk.
    pub(crate) fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// The indices into [`Self::entries`] of the lines that hold the walk's
    /// needle, newest first, in the order that the walk visits them.
    ///
    /// With no walk the needle is empty, so every line matches.
    pub(crate) fn matches(&self) -> Vec<usize> {
        let needle = self.needle();
        (0..self.entries.len())
            .rev()
            .filter(|&index| self.entries[index].contains(needle))
            .collect()
    }

    /// End an active walk and answer the text typed before it started, or
    /// `None` with no walk.
    pub(crate) fn leave(&mut self) -> Option<String> {
        self.cursor?;
        let typed = self.pending.take().unwrap_or_default();
        self.reset();
        Some(typed)
    }

    /// Move an active walk to the entry at `index` and answer its line, which
    /// the walk then holds as the recalled line.
    ///
    /// With no walk, or with an `index` past the last entry, it answers `None`
    /// and the walk stays as it is.
    pub(crate) fn recall(&mut self, index: usize) -> Option<String> {
        self.cursor?;
        let line = self.entries.get(index)?.clone();
        self.cursor = Some(index);
        self.last_recalled = Some(line.clone());
        Some(line)
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

    /// A pushed line keeps its time, and a repeat of the newest line stores no
    /// second line but gives the newest its new time.
    #[test]
    fn a_pushed_line_keeps_its_time_and_a_repeat_takes_the_new_time() {
        let mut h = InputHistory::default();
        h.push_at("a".to_string(), Some(10));
        h.push_at("a".to_string(), Some(20));
        assert_eq!(
            (h.entries().to_vec(), h.times().to_vec()),
            (vec!["a".to_string()], vec![Some(20)]),
        );
    }

    /// The cap evicts the oldest time with the oldest line, so each kept line
    /// keeps its own time.
    #[test]
    fn the_cap_drops_the_oldest_time_with_its_line() {
        let mut h = InputHistory::default();
        for i in 0..150 {
            h.push_at(format!("cmd{i}"), Some(i));
        }
        assert_eq!(h.times(), (50..150).map(Some).collect::<Vec<_>>());
    }

    /// A walk reports its needle, the lines that hold it newest first, and the
    /// index of the recalled line. With no walk every line matches.
    #[test]
    fn a_walk_reports_its_matches_newest_first_and_its_place() {
        let mut h = history(&["cd a", "ls", "cd b"]);
        let state = |h: &InputHistory| {
            (
                h.is_walking(),
                h.needle().to_owned(),
                h.cursor(),
                h.matches(),
            )
        };
        let idle = state(&h);
        h.prev("cd");
        let newest = state(&h);
        h.prev("cd b");
        assert_eq!(
            [idle, newest, state(&h)],
            [
                (false, String::new(), None, vec![2, 1, 0]),
                (true, "cd".to_owned(), Some(2), vec![2, 0]),
                (true, "cd".to_owned(), Some(0), vec![2, 0]),
            ],
        );
    }

    /// Leaving a walk ends it and answers the text typed before it started. A
    /// second leave finds no walk.
    #[test]
    fn leaving_a_walk_returns_the_typed_text() {
        let mut h = history(&["cd a", "cd b"]);
        h.prev("cd");
        let left = h.leave();
        assert_eq!(
            (left, h.is_walking(), h.leave()),
            (Some("cd".to_owned()), false, None)
        );
    }

    /// `recall` moves an active walk to an entry and holds its line as the
    /// recalled line, so an untouched input keeps the walk. It answers `None`
    /// with no walk and past the last entry.
    #[test]
    fn recall_moves_a_walk_to_an_entry() {
        let mut h = history(&["a", "b", "c"]);
        let idle = h.recall(0);
        h.prev("");
        let recalled = h.recall(0);
        h.reset_if_edited("a");
        let past = h.recall(3);
        assert_eq!(
            (idle, recalled, past, h.cursor(), h.is_walking()),
            (None, Some("a".to_owned()), None, Some(0), true),
        );
    }

    /// Restored times shorter than the lines pad with `None`, and times past
    /// the last line drop, so the history holds one time per line.
    #[test]
    fn lines_without_times_pad_with_none() {
        let lines = |count: usize| (0..count).map(|i| format!("line{i}")).collect();
        let padded = InputHistory::from_timed(lines(2), vec![Some(1)]);
        let cut = InputHistory::from_timed(lines(1), vec![Some(1), Some(2)]);
        assert_eq!(
            (padded.times(), cut.times()),
            (&[Some(1), None][..], &[Some(1)][..]),
        );
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
