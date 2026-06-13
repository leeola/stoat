//! Helix-style two-character interactive label jump (`g w`). Visible
//! word starts in the focused editor are tagged with one- or
//! two-character labels; the user types a label to jump the cursor.
//!
//! The pure logic in this module - viewport word detection and label
//! assignment - is independent of [`crate::Stoat`]; the action wiring
//! and rendering live in [`crate::action_handlers`] and
//! [`crate::render`].

use std::collections::BTreeMap;
use stoat_text::Rope;

/// Label alphabet, lowercase only. 26 letters yields 676 two-char
/// labels which is more than enough for any practical viewport.
pub const ALPHABET: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z',
];

/// Find byte offsets of word starts on rows in `[first_row, last_row]`
/// (inclusive). A "word start" is the first char of a run of two or
/// more word characters (alphanumerics or underscore), matching
/// Helix's filter. The returned offsets are in document order, capped
/// at `max` entries.
pub fn find_word_starts(rope: &Rope, first_row: u32, last_row: u32, max: usize) -> Vec<usize> {
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

fn scan_line_word_starts(line: &str, row_offset: usize, max: usize, out: &mut Vec<usize>) {
    let mut chars = line.char_indices().peekable();
    let mut prev_was_word = false;
    while let Some((byte_idx, ch)) = chars.next() {
        let is_word = is_word_char(ch);
        if is_word && !prev_was_word {
            let next_is_word = chars.peek().is_some_and(|&(_, c)| is_word_char(c));
            if next_is_word {
                out.push(row_offset + byte_idx);
                if out.len() >= max {
                    return;
                }
            }
        }
        prev_was_word = is_word;
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
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
pub fn assign_labels(targets: &[usize], alphabet: &[char]) -> BTreeMap<String, usize> {
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
pub enum JumpStep {
    /// Label fully typed; the cursor should jump to the byte offset.
    Jump(usize),
    /// Input prefix still has multiple matching labels - keep waiting.
    Continue,
    /// No labels match the new prefix - cancel the in-progress jump.
    Cancel,
}

/// Step the in-progress jump: append `ch` to `input` and look up
/// `input` in the label map. Caller is responsible for clearing /
/// updating its own state based on the returned [`JumpStep`].
pub fn step_jump(labels: &BTreeMap<String, usize>, input: &str, ch: char) -> JumpStep {
    let mut next = String::with_capacity(input.len() + 1);
    next.push_str(input);
    next.push(ch);
    if let Some(&offset) = labels.get(&next) {
        return JumpStep::Jump(offset);
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
        let targets = vec![0, 5, 10];
        let labels = assign_labels(&targets, ALPHABET);
        let collected: Vec<(&String, &usize)> = labels.iter().collect();
        assert_eq!(collected.len(), 3);
        assert_eq!(*collected[0].0, "a");
        assert_eq!(*collected[1].0, "b");
        assert_eq!(*collected[2].0, "c");
        assert_eq!(*collected[0].1, 0);
        assert_eq!(*collected[1].1, 5);
        assert_eq!(*collected[2].1, 10);
    }

    #[test]
    fn assign_labels_two_char_when_over_alphabet_size() {
        let targets: Vec<usize> = (0..30).map(|i| i * 4).collect();
        let labels = assign_labels(&targets, ALPHABET);
        assert_eq!(labels.len(), 30);
        // First target -> "aa", 26th -> "ba", 27th -> "bb".
        assert_eq!(labels.get("aa"), Some(&0));
        assert_eq!(labels.get("az"), Some(&(25 * 4)));
        assert_eq!(labels.get("ba"), Some(&(26 * 4)));
        assert_eq!(labels.get("bd"), Some(&(29 * 4)));
        for label in labels.keys() {
            assert_eq!(label.len(), 2, "expected two-char label, got {label:?}");
        }
    }

    #[test]
    fn assign_labels_caps_at_alphabet_squared() {
        let targets: Vec<usize> = (0..1000).collect();
        let labels = assign_labels(&targets, ALPHABET);
        assert_eq!(labels.len(), ALPHABET.len() * ALPHABET.len());
    }

    #[test]
    fn assign_labels_unique_strings_no_collisions() {
        let targets: Vec<usize> = (0..200).collect();
        let labels = assign_labels(&targets, ALPHABET);
        let unique_offsets: std::collections::BTreeSet<usize> = labels.values().copied().collect();
        assert_eq!(unique_offsets.len(), labels.len());
    }

    #[test]
    fn find_word_starts_skips_one_char_words() {
        let r = rope("a abc d efgh\n");
        let starts = find_word_starts(&r, 0, 0, 100);
        // "a" is one char (skipped), "abc" starts at offset 2,
        // "d" is one char (skipped), "efgh" starts at offset 8.
        assert_eq!(starts, vec![2, 8]);
    }

    #[test]
    fn find_word_starts_within_visible_rows_only() {
        let r = rope("alpha\nbeta\ngamma\n");
        let starts = find_word_starts(&r, 1, 1, 100);
        // Only row 1 ("beta") is in scope.
        assert_eq!(starts.len(), 1);
        let beta_offset = "alpha\n".len();
        assert_eq!(starts[0], beta_offset);
    }

    #[test]
    fn find_word_starts_caps_at_max() {
        let r = rope("aa bb cc dd ee ff gg\n");
        let starts = find_word_starts(&r, 0, 0, 3);
        assert_eq!(starts.len(), 3);
        assert_eq!(starts, vec![0, 3, 6]);
    }

    #[test]
    fn find_word_starts_handles_punctuation_boundaries() {
        let r = rope("foo.bar baz\n");
        let starts = find_word_starts(&r, 0, 0, 100);
        // foo, bar, baz are each separate runs; all 3+ chars qualify.
        assert_eq!(starts, vec![0, 4, 8]);
    }

    #[test]
    fn step_jump_returns_jump_on_exact_match() {
        let mut labels = BTreeMap::new();
        labels.insert("a".to_string(), 42);
        labels.insert("b".to_string(), 7);
        match step_jump(&labels, "", 'a') {
            JumpStep::Jump(off) => assert_eq!(off, 42),
            _ => panic!("expected Jump"),
        }
    }

    #[test]
    fn step_jump_returns_continue_on_partial_match() {
        let mut labels = BTreeMap::new();
        labels.insert("aa".to_string(), 1);
        labels.insert("ab".to_string(), 2);
        labels.insert("ba".to_string(), 3);
        assert!(matches!(step_jump(&labels, "", 'a'), JumpStep::Continue));
    }

    #[test]
    fn step_jump_returns_cancel_on_no_match() {
        let mut labels = BTreeMap::new();
        labels.insert("aa".to_string(), 1);
        labels.insert("bb".to_string(), 2);
        assert!(matches!(step_jump(&labels, "", 'z'), JumpStep::Cancel));
    }

    #[test]
    fn step_jump_two_char_path() {
        let mut labels = BTreeMap::new();
        labels.insert("aa".to_string(), 10);
        labels.insert("ab".to_string(), 20);
        // First char "a" -> Continue.
        assert!(matches!(step_jump(&labels, "", 'a'), JumpStep::Continue));
        // Then "ab" -> Jump.
        match step_jump(&labels, "a", 'b') {
            JumpStep::Jump(off) => assert_eq!(off, 20),
            _ => panic!("expected Jump"),
        }
    }
}
