//! Bracket and quote pairing for insert mode.
//!
//! A reader typing an opening bracket almost always wants its closer, and
//! almost always wants to type the closer anyway once they reach it. Pairing
//! serves both. Typing `(` writes `()` and leaves the cursor inside, and typing
//! `)` against the one already there steps over it rather than doubling it. The
//! text a reader types therefore reads back the same whether pairing is on or
//! off, which is why balanced typing is unaffected by it.
//!
//! The decisions live here, over a [`Rope`] and a byte offset. Which edit to
//! write and where the cursor lands belong to the editor instead, since that is
//! where a cursor is defined.

use crate::Rope;

/// The brackets and quotes paired when a language names no table of its own.
pub const DEFAULT_PAIRS: &[(char, char)] = &[
    ('(', ')'),
    ('{', '}'),
    ('[', ']'),
    ('\'', '\''),
    ('"', '"'),
    ('`', '`'),
];

/// One opening character and the closing character that answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    pub open: char,
    pub close: char,
}

/// A language's pair table, searched by either half of a pair.
///
/// A scan, not a map. Six entries are cheaper to walk than to hash, and this is
/// read on every typed bracket, quote, and space.
#[derive(Debug, Clone, Copy)]
pub struct AutoPairs(&'static [(char, char)]);

/// What typing one character at a cursor does.
///
/// The absence of an action is itself an answer. The character is then typed as
/// written, which is what a reader gets for every character that opens nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairAction {
    /// Write both halves and leave the cursor between them.
    Close(Pair),
    /// Write nothing and move the cursor `width` bytes on, over the closer
    /// already there.
    Skip { width: usize },
}

impl Pair {
    /// True for a pair whose halves are the same character, as every quote is.
    ///
    /// A quote and its closer are the same character, so the two halves share
    /// every rule that otherwise separates them.
    pub fn same(&self) -> bool {
        self.open == self.close
    }

    /// True when a pair typed at `cursor` gets its closing half.
    ///
    /// A bracket before a word traps the word inside it, so the closer is
    /// withheld there. A quote is withheld after a word too, which is what
    /// leaves the apostrophe in `don't` alone.
    pub fn should_close(&self, rope: &Rope, cursor: usize) -> bool {
        let mut close = char_at(rope, cursor).is_none_or(|ch| !ch.is_alphanumeric());

        if self.same() {
            close &= prev_char(rope, cursor).is_none_or(|ch| !ch.is_alphanumeric());
        }

        close
    }
}

impl AutoPairs {
    pub fn new(pairs: &'static [(char, char)]) -> Self {
        Self(pairs)
    }

    /// The pair `ch` belongs to, as either half of it.
    pub fn get(&self, ch: char) -> Option<Pair> {
        self.0
            .iter()
            .find(|(open, close)| *open == ch || *close == ch)
            .map(|&(open, close)| Pair { open, close })
    }
}

impl Default for AutoPairs {
    fn default() -> Self {
        Self(DEFAULT_PAIRS)
    }
}

/// What typing `ch` at `cursor` does, or `None` to type it as written.
pub fn hook_insert(rope: &Rope, cursor: usize, ch: char, pairs: AutoPairs) -> Option<PairAction> {
    if let Some(pair) = pairs.get(ch) {
        if pair.same() {
            return insert_same(rope, cursor, pair);
        }
        if pair.open == ch {
            return insert_open(rope, cursor, pair);
        }
        return insert_close(rope, cursor, pair);
    }

    if ch.is_whitespace() {
        return insert_whitespace(rope, cursor, ch, pairs);
    }

    None
}

/// The span one backspace at `cursor` removes when it sits between a pair, or
/// `None` when the caller's own rule applies.
///
/// Two cases answer here. The cursor between an open and its close removes
/// both. The cursor between two spaces that a pair in turn surrounds removes
/// both spaces, undoing what typing a space inside a pair wrote.
pub fn hook_delete(rope: &Rope, cursor: usize, pairs: AutoPairs) -> Option<(usize, usize)> {
    let cur = char_at(rope, cursor)?;
    let prev = prev_char(rope, cursor)?;

    if rope.len() >= 4 && prev.is_whitespace() && cur.is_whitespace() {
        let before = rope.prev_grapheme_boundary(rope.prev_grapheme_boundary(cursor));
        let second_next = char_at(rope, rope.next_grapheme_boundary(cursor));

        if let Some(second_prev) = char_at(rope, before)
            && let Some(pair) = pairs.get(second_prev)
            && pair.open == second_prev
            && Some(pair.close) == second_next
        {
            return Some(surrounding_span(rope, cursor));
        }
    }

    enclosing_pair(rope, cursor, pairs)?;
    Some(surrounding_span(rope, cursor))
}

/// The pair whose two halves sit either side of `cursor` with nothing between
/// them.
///
/// What "inside a pair" means to everything that acts on it. Backspace removes
/// both halves from here, and a line break opens a line between them.
pub fn enclosing_pair(rope: &Rope, cursor: usize, pairs: AutoPairs) -> Option<Pair> {
    let cur = char_at(rope, cursor)?;
    let prev = prev_char(rope, cursor)?;
    let pair = pairs.get(cur)?;

    (pair.open == prev && pair.close == cur).then_some(pair)
}

fn insert_open(rope: &Rope, cursor: usize, pair: Pair) -> Option<PairAction> {
    // Nothing after the cursor closes unconditionally, since a document's end
    // holds no word for the closer to swallow.
    if char_at(rope, cursor).is_some() && !pair.should_close(rope, cursor) {
        return None;
    }

    Some(PairAction::Close(pair))
}

fn insert_close(rope: &Rope, cursor: usize, pair: Pair) -> Option<PairAction> {
    if char_at(rope, cursor) != Some(pair.close) {
        return None;
    }

    Some(skip_over(rope, cursor))
}

fn insert_same(rope: &Rope, cursor: usize, pair: Pair) -> Option<PairAction> {
    if char_at(rope, cursor) == Some(pair.open) {
        return Some(skip_over(rope, cursor));
    }
    if !pair.should_close(rope, cursor) {
        return None;
    }

    Some(PairAction::Close(pair))
}

/// A space typed inside a freshly written pair gets a second one, so the cursor
/// keeps its room as `( | )` rather than pushing the closer along.
fn insert_whitespace(rope: &Rope, cursor: usize, ch: char, pairs: AutoPairs) -> Option<PairAction> {
    let cur = char_at(rope, cursor)?;
    let pair = pairs.get(cur)?;
    if pair.close != cur || pair.open != prev_char(rope, cursor)? {
        return None;
    }

    insert_same(
        rope,
        cursor,
        Pair {
            open: ch,
            close: ch,
        },
    )
}

fn skip_over(rope: &Rope, cursor: usize) -> PairAction {
    PairAction::Skip {
        width: rope.next_grapheme_boundary(cursor) - cursor,
    }
}

fn surrounding_span(rope: &Rope, cursor: usize) -> (usize, usize) {
    (
        rope.prev_grapheme_boundary(cursor),
        rope.next_grapheme_boundary(cursor),
    )
}

fn char_at(rope: &Rope, offset: usize) -> Option<char> {
    rope.chars_at(offset).next()
}

fn prev_char(rope: &Rope, offset: usize) -> Option<char> {
    rope.reversed_chars_at(offset).next()
}

#[cfg(test)]
mod tests {
    use super::{hook_delete, hook_insert, AutoPairs, Pair, PairAction};
    use crate::Rope;

    fn insert(text: &str, cursor: usize, ch: char) -> Option<PairAction> {
        hook_insert(&Rope::from(text), cursor, ch, AutoPairs::default())
    }

    fn close(open: char, close: char) -> Option<PairAction> {
        Some(PairAction::Close(Pair { open, close }))
    }

    fn delete(text: &str, cursor: usize) -> Option<(usize, usize)> {
        hook_delete(&Rope::from(text), cursor, AutoPairs::default())
    }

    #[test]
    fn an_opener_closes_at_the_end_of_a_document() {
        assert_eq!(insert("", 0, '('), close('(', ')'), "empty document");
        assert_eq!(insert("fn x", 4, '('), close('(', ')'), "after a word");
    }

    #[test]
    fn an_opener_before_a_word_is_left_alone() {
        assert_eq!(insert("word", 0, '('), None, "before a letter");
        assert_eq!(insert("7", 0, '['), None, "before a digit");
        assert_eq!(insert(" x", 0, '{'), close('{', '}'), "before a space");
        assert_eq!(insert(")", 0, '('), close('(', ')'), "before a closer");
    }

    #[test]
    fn a_closer_steps_over_the_one_already_there() {
        assert_eq!(insert("()", 1, ')'), Some(PairAction::Skip { width: 1 }));
        assert_eq!(insert("(x)", 2, ')'), Some(PairAction::Skip { width: 1 }));
        assert_eq!(insert("(", 1, ')'), None, "nothing to step over");
        assert_eq!(insert("(]", 1, ')'), None, "a different closer");
    }

    #[test]
    fn a_quote_needs_a_non_word_on_both_sides() {
        assert_eq!(insert("don t", 3, '\''), None, "between two letters");
        assert_eq!(insert("say ", 4, '"'), close('"', '"'), "after a space");
        assert_eq!(insert("say", 3, '"'), None, "after a letter");
        assert_eq!(insert("", 0, '`'), close('`', '`'), "empty document");
    }

    #[test]
    fn a_quote_steps_over_its_own_close() {
        assert_eq!(insert("\"\"", 1, '"'), Some(PairAction::Skip { width: 1 }));
    }

    #[test]
    fn a_space_inside_a_pair_is_doubled() {
        assert_eq!(insert("()", 1, ' '), close(' ', ' '));
        assert_eq!(insert("( )", 2, ' '), None, "not directly inside");
        assert_eq!(insert("ab", 1, ' '), None, "not inside a pair at all");
    }

    #[test]
    fn backspace_between_a_pair_takes_both_sides() {
        assert_eq!(delete("()", 1), Some((0, 2)));
        assert_eq!(delete("a{}b", 2), Some((1, 3)));
        assert_eq!(delete("\"\"", 1), Some((0, 2)));
    }

    #[test]
    fn backspace_elsewhere_declines() {
        assert_eq!(delete("(x)", 2), None, "not adjacent");
        assert_eq!(delete("(]", 1), None, "mismatched halves");
        assert_eq!(delete(")(", 1), None, "halves reversed");
        assert_eq!(delete("()", 0), None, "nothing before the cursor");
    }

    #[test]
    fn backspace_between_spaces_a_pair_surrounds_takes_both() {
        assert_eq!(delete("(  )", 2), Some((1, 3)));
        assert_eq!(delete("a  b", 2), None, "no pair around the spaces");
        assert_eq!(delete("(  ", 2), None, "no closer");
    }

    #[test]
    fn a_multibyte_neighbor_steps_by_its_width() {
        assert_eq!(
            insert("()\u{e9}", 1, ')'),
            Some(PairAction::Skip { width: 1 }),
            "the closer's own width is what the cursor moves"
        );
        // The two-byte char ahead of the pair puts the cursor between the
        // halves at byte 3, and the span it removes is in bytes too.
        assert_eq!(delete("\u{e9}()", 3), Some((2, 4)));
        assert_eq!(delete("\u{e9}()", 2), None, "before the pair, not inside");
    }
}
