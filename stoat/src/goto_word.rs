//! Helix-style two-character interactive label jump (`g w`). Visible
//! word starts in the focused editor are tagged with one- or
//! two-character labels; the user types a label to jump the cursor.
//!
//! The pure logic in this module - viewport word detection and label
//! assignment - is independent of [`crate::Stoat`]; the action wiring
//! and rendering live in [`crate::action_handlers`] and
//! [`crate::render`].

use std::collections::BTreeMap;
use stoat_text::{char_is_word, Rope};

/// Label alphabet, lowercase only. 26 letters yields 676 two-char
/// labels which is more than enough for any practical viewport.
pub(crate) const ALPHABET: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z',
];

/// Find the byte ranges of words on rows in `[first_row, last_row]`
/// (inclusive). A word is a run of two or more word characters
/// (alphanumerics or underscore), matching Helix's filter. The ranges
/// are in document order, capped at `max` entries.
///
/// Each range runs from the word's first character to the byte past its
/// last, so a jump lands over the whole word rather than at its start.
pub(crate) fn find_word_starts(
    rope: &Rope,
    first_row: u32,
    last_row: u32,
    max: usize,
) -> Vec<(usize, usize)> {
    if max == 0 {
        return Vec::new();
    }
    let total_rows = rope.max_point().row;
    let last_row = last_row.min(total_rows);
    if first_row > last_row {
        return Vec::new();
    }

    let mut targets = Vec::new();
    for row in first_row..=last_row {
        let line_text: String = rope.chunks_in_line(row).collect();
        let row_offset = rope.point_to_offset(stoat_text::Point::new(row, 0));
        scan_line_word_starts(&line_text, row_offset, max, &mut targets);
        if targets.len() >= max {
            break;
        }
    }
    targets.truncate(max);
    targets
}

fn scan_line_word_starts(line: &str, row_offset: usize, max: usize, out: &mut Vec<(usize, usize)>) {
    let mut start: Option<usize> = None;
    for (byte_idx, ch) in line.char_indices() {
        match (char_is_word(ch), start) {
            (true, None) => start = Some(byte_idx),
            (true, Some(_)) => {},
            // A run of one character is not a word here, matching the filter
            // the labels are sized against.
            (false, Some(from)) => {
                start = None;
                if line[from..byte_idx].chars().count() >= 2 {
                    out.push((row_offset + from, row_offset + byte_idx));
                    if out.len() >= max {
                        return;
                    }
                }
            },
            (false, None) => {},
        }
    }
    // A word ending at the line's end has no non-word character to close it.
    if let Some(from) = start
        && line[from..].chars().count() >= 2
    {
        out.push((row_offset + from, row_offset + line.len()));
    }
}

/// Assign labels to `targets`. When `targets.len() <= alphabet.len()`,
/// each target gets a single-character label. Otherwise every target
/// gets a two-character label drawn from the alphabet. Labels are
/// allocated in target order so the first visible word always lands
/// on `aa` (or `a` in the single-char regime), giving a stable mapping
/// regardless of which key the user has rebound.
///
/// Returns a [`BTreeMap`] (rather than a [`HashMap`]) so iteration
/// order is deterministic for snapshot tests and label rendering.
///
/// A label maps to the whole word's range. Rendering keys off the start,
/// which is where the label is drawn.
pub(crate) fn assign_labels(
    targets: &[(usize, usize)],
    alphabet: &[char],
) -> BTreeMap<String, (usize, usize)> {
    let mut map = BTreeMap::new();
    if alphabet.is_empty() || targets.is_empty() {
        return map;
    }
    let alpha_len = alphabet.len();
    let single = targets.len() <= alpha_len;
    let max = if single {
        alpha_len
    } else {
        alpha_len * alpha_len
    };
    let count = targets.len().min(max);
    for (i, &offset) in targets.iter().take(count).enumerate() {
        let label = if single {
            alphabet[i].to_string()
        } else {
            let first = alphabet[i / alpha_len];
            let second = alphabet[i % alpha_len];
            let mut s = String::with_capacity(2);
            s.push(first);
            s.push(second);
            s
        };
        map.insert(label, offset);
    }
    map
}

/// Result of feeding one character into an in-progress jump.
pub(crate) enum JumpStep {
    /// Label fully typed. The jump lands over the word this range covers.
    Jump((usize, usize)),
    /// Input prefix still has multiple matching labels - keep waiting.
    Continue,
    /// No labels match the new prefix - cancel the in-progress jump.
    Cancel,
}

/// Step the in-progress jump: append `ch` to `input` and look up
/// `input` in the label map. Caller is responsible for clearing /
/// updating its own state based on the returned [`JumpStep`].
pub(crate) fn step_jump(
    labels: &BTreeMap<String, (usize, usize)>,
    input: &str,
    ch: char,
) -> JumpStep {
    let mut next = String::with_capacity(input.len() + 1);
    next.push_str(input);
    next.push(ch);
    if let Some(&range) = labels.get(&next) {
        return JumpStep::Jump(range);
    }
    let any_prefix_match = labels.keys().any(|k| k.starts_with(&next));
    if any_prefix_match {
        JumpStep::Continue
    } else {
        JumpStep::Cancel
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(text: &str) -> Rope {
        Rope::from(text)
    }

    #[test]
    fn assign_labels_one_char_when_under_alphabet_size() {
        let targets = vec![(0, 2), (5, 7), (10, 12)];
        let labels = assign_labels(&targets, ALPHABET);
        let collected: Vec<(&String, &(usize, usize))> = labels.iter().collect();
        assert_eq!(collected.len(), 3);
        assert_eq!(*collected[0].0, "a");
        assert_eq!(*collected[1].0, "b");
        assert_eq!(*collected[2].0, "c");
        assert_eq!(*collected[0].1, (0, 2));
        assert_eq!(*collected[1].1, (5, 7));
        assert_eq!(*collected[2].1, (10, 12));
    }

    #[test]
    fn assign_labels_two_char_when_over_alphabet_size() {
        let targets: Vec<(usize, usize)> = (0..30).map(|i| (i * 4, i * 4 + 2)).collect();
        let labels = assign_labels(&targets, ALPHABET);
        assert_eq!(labels.len(), 30);
        // First target -> "aa", 26th -> "ba", 27th -> "bb".
        assert_eq!(labels.get("aa"), Some(&(0, 2)));
        assert_eq!(labels.get("az"), Some(&(25 * 4, 25 * 4 + 2)));
        assert_eq!(labels.get("ba"), Some(&(26 * 4, 26 * 4 + 2)));
        assert_eq!(labels.get("bd"), Some(&(29 * 4, 29 * 4 + 2)));
        for label in labels.keys() {
            assert_eq!(label.len(), 2, "expected two-char label, got {label:?}");
        }
    }

    #[test]
    fn assign_labels_caps_at_alphabet_squared() {
        let targets: Vec<(usize, usize)> = (0..1000).map(|i| (i, i + 2)).collect();
        let labels = assign_labels(&targets, ALPHABET);
        assert_eq!(labels.len(), ALPHABET.len() * ALPHABET.len());
    }

    #[test]
    fn assign_labels_unique_strings_no_collisions() {
        let targets: Vec<(usize, usize)> = (0..200).map(|i| (i, i + 2)).collect();
        let labels = assign_labels(&targets, ALPHABET);
        let unique_offsets: std::collections::BTreeSet<(usize, usize)> =
            labels.values().copied().collect();
        assert_eq!(unique_offsets.len(), labels.len());
    }

    #[test]
    fn find_word_starts_skips_one_char_words() {
        let r = rope("a abc d efgh\n");
        let starts = find_word_starts(&r, 0, 0, 100);
        // "a" is one char (skipped), "abc" starts at offset 2,
        // "d" is one char (skipped), "efgh" starts at offset 8.
        assert_eq!(starts, vec![(2, 5), (8, 12)]);
    }

    #[test]
    fn find_word_starts_within_visible_rows_only() {
        let r = rope("alpha\nbeta\ngamma\n");
        let starts = find_word_starts(&r, 1, 1, 100);
        // Only row 1 ("beta") is in scope.
        assert_eq!(starts.len(), 1);
        let beta_offset = "alpha\n".len();
        assert_eq!(starts[0], (beta_offset, beta_offset + 4));
    }

    #[test]
    fn find_word_starts_caps_at_max() {
        let r = rope("aa bb cc dd ee ff gg\n");
        let starts = find_word_starts(&r, 0, 0, 3);
        assert_eq!(starts.len(), 3);
        assert_eq!(starts, vec![(0, 2), (3, 5), (6, 8)]);
    }

    #[test]
    fn find_word_starts_handles_punctuation_boundaries() {
        let r = rope("foo.bar baz\n");
        let starts = find_word_starts(&r, 0, 0, 100);
        // foo, bar, baz are each separate runs; all 3+ chars qualify.
        assert_eq!(starts, vec![(0, 3), (4, 7), (8, 11)]);
    }

    #[test]
    fn step_jump_returns_jump_on_exact_match() {
        let mut labels = BTreeMap::new();
        labels.insert("a".to_string(), (42, 45));
        labels.insert("b".to_string(), (7, 10));
        match step_jump(&labels, "", 'a') {
            JumpStep::Jump(range) => assert_eq!(range, (42, 45)),
            _ => panic!("expected Jump"),
        }
    }

    #[test]
    fn step_jump_returns_continue_on_partial_match() {
        let mut labels = BTreeMap::new();
        labels.insert("aa".to_string(), (1, 3));
        labels.insert("ab".to_string(), (2, 4));
        labels.insert("ba".to_string(), (3, 5));
        assert!(matches!(step_jump(&labels, "", 'a'), JumpStep::Continue));
    }

    #[test]
    fn step_jump_returns_cancel_on_no_match() {
        let mut labels = BTreeMap::new();
        labels.insert("aa".to_string(), (1, 3));
        labels.insert("bb".to_string(), (2, 4));
        assert!(matches!(step_jump(&labels, "", 'z'), JumpStep::Cancel));
    }

    #[test]
    fn step_jump_two_char_path() {
        let mut labels = BTreeMap::new();
        labels.insert("aa".to_string(), (10, 12));
        labels.insert("ab".to_string(), (20, 22));
        // First char "a" -> Continue.
        assert!(matches!(step_jump(&labels, "", 'a'), JumpStep::Continue));
        // Then "ab" -> Jump.
        match step_jump(&labels, "a", 'b') {
            JumpStep::Jump(range) => assert_eq!(range, (20, 22)),
            _ => panic!("expected Jump"),
        }
    }

    #[test]
    fn g_w_arms_pending_labels_and_typing_jumps_cursor() {
        use std::path::PathBuf;
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/goto-word-test");
        h.fake_fs()
            .insert_files([(root.join("buf.rs"), b"alpha beta gamma\n".as_ref())]);
        h.stoat.active_workspace_mut().git_root = root.clone();
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("buf.rs"),
            },
        );

        h.type_keys("g w");
        let labels = h
            .stoat
            .pending_goto_word
            .as_ref()
            .expect("labels should be armed")
            .clone();
        // Three words, all two characters or more, each labelled over its whole
        // span rather than at its start alone.
        assert_eq!(labels.len(), 3);
        assert_eq!(labels.get("a"), Some(&(0, 5)));
        assert_eq!(labels.get("b"), Some(&(6, 10)));
        assert_eq!(labels.get("c"), Some(&(11, 16)));

        h.type_keys("c");
        assert!(h.stoat.pending_goto_word.is_none());
        assert_eq!(
            crate::test_harness::editor::selection_spans(&mut h.stoat),
            vec![(11, 16, false)],
            "the one selection covers gamma, running forward",
        );
    }

    /// The jump crosses the viewport in one press, so the place it left is on
    /// the jumplist and one step back returns there.
    #[test]
    fn g_w_pushes_the_origin_onto_the_jumplist() {
        use std::path::PathBuf;
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/goto-word-jumplist");
        h.fake_fs()
            .insert_files([(root.join("buf.rs"), b"alpha beta gamma\n".as_ref())]);
        h.stoat.active_workspace_mut().git_root = root.clone();
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("buf.rs"),
            },
        );
        h.type_keys("l l");
        let origin = crate::test_harness::editor::selection_spans(&mut h.stoat);

        h.type_keys("g w");
        h.type_keys("c");
        assert_eq!(
            crate::test_harness::editor::selection_spans(&mut h.stoat),
            vec![(11, 16, false)],
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(
            crate::test_harness::editor::selection_spans(&mut h.stoat),
            origin,
            "one step back returns to where the jump started",
        );
    }

    /// The key that drops the labels reaches nothing else, so Escape does not
    /// also run what normal mode binds it to.
    #[test]
    fn escape_drops_the_labels_and_nothing_else() {
        use std::path::PathBuf;
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/goto-word-cancel");
        h.fake_fs()
            .insert_files([(root.join("buf.rs"), b"alpha beta gamma\n".as_ref())]);
        h.stoat.active_workspace_mut().git_root = root.clone();
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenFile {
                path: root.join("buf.rs"),
            },
        );
        h.stoat.key_hints_visible = true;

        h.type_keys("g w");
        assert!(h.stoat.pending_goto_word.is_some(), "labels armed");

        h.type_keys("escape");
        assert!(h.stoat.pending_goto_word.is_none(), "labels dropped");
        assert!(
            h.stoat.key_hints_visible,
            "normal mode binds Escape to dismissing the hints, which this press never reached"
        );
    }
}
