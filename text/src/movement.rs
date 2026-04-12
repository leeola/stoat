use crate::Rope;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CharCategory {
    Whitespace,
    Eol,
    Word,
    Punctuation,
    Unknown,
}

pub fn categorize_char(ch: char) -> CharCategory {
    if char_is_line_ending(ch) {
        CharCategory::Eol
    } else if ch.is_whitespace() {
        CharCategory::Whitespace
    } else if char_is_word(ch) {
        CharCategory::Word
    } else if ch.is_ascii_punctuation() {
        CharCategory::Punctuation
    } else {
        CharCategory::Unknown
    }
}

fn char_is_line_ending(ch: char) -> bool {
    matches!(ch, '\n' | '\r')
}

fn char_is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_word_boundary(a: char, b: char) -> bool {
    categorize_char(a) != categorize_char(b)
}

pub fn next_word_start(rope: &Rope, from: usize) -> usize {
    let mut chars = rope.chars_at(from);
    let Some(first_char) = chars.next() else {
        return from;
    };
    let head_start = from + first_char.len_utf8();
    let mut head = head_start;
    let mut prev_ch = first_char;

    loop {
        let Some(ch) = chars.next() else {
            return head;
        };
        let boundary = is_word_boundary(prev_ch, ch);
        let target = boundary && (char_is_line_ending(ch) || !ch.is_whitespace());
        if target && head != head_start {
            return head;
        }
        prev_ch = ch;
        head += ch.len_utf8();
    }
}

pub fn next_word_end(rope: &Rope, from: usize) -> usize {
    let mut chars = rope.chars_at(from);
    let Some(first_char) = chars.next() else {
        return from;
    };
    let head_start = from + first_char.len_utf8();
    let mut head = head_start;
    let mut prev_ch = first_char;

    loop {
        let Some(ch) = chars.next() else {
            return head;
        };
        let boundary = is_word_boundary(prev_ch, ch);
        let target = boundary && (!prev_ch.is_whitespace() || char_is_line_ending(ch));
        if target && head != head_start {
            return head;
        }
        prev_ch = ch;
        head += ch.len_utf8();
    }
}

pub fn prev_word_start(rope: &Rope, from: usize) -> usize {
    if from == 0 {
        return 0;
    }
    let Some(first_char) = rope.chars_at(from).next() else {
        return prev_word_start_from_end(rope, from);
    };
    let head_start = from;
    let mut head = head_start;
    let mut prev_ch = first_char;
    let mut iter = rope.reversed_chars_at(from);

    loop {
        let Some(ch) = iter.next() else {
            return head;
        };
        let boundary = is_word_boundary(prev_ch, ch);
        let target = boundary && (!prev_ch.is_whitespace() || char_is_line_ending(ch));
        if target && head != head_start {
            return head;
        }
        prev_ch = ch;
        head -= ch.len_utf8();
    }
}

fn prev_word_start_from_end(rope: &Rope, from: usize) -> usize {
    let head_start = from;
    let mut head = head_start;
    let mut iter = rope.reversed_chars_at(from);
    let Some(seed) = iter.next() else {
        return head;
    };
    head -= seed.len_utf8();
    let mut prev_ch = seed;

    loop {
        let Some(ch) = iter.next() else {
            return head;
        };
        let boundary = is_word_boundary(prev_ch, ch);
        let target = boundary && (!prev_ch.is_whitespace() || char_is_line_ending(ch));
        if target && head != head_start - seed.len_utf8() {
            return head;
        }
        prev_ch = ch;
        head -= ch.len_utf8();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        let mut r = Rope::new();
        r.push(s);
        r
    }

    #[test]
    fn categorize_covers_main_classes() {
        assert_eq!(categorize_char('a'), CharCategory::Word);
        assert_eq!(categorize_char('1'), CharCategory::Word);
        assert_eq!(categorize_char('_'), CharCategory::Word);
        assert_eq!(categorize_char(' '), CharCategory::Whitespace);
        assert_eq!(categorize_char('\t'), CharCategory::Whitespace);
        assert_eq!(categorize_char('\n'), CharCategory::Eol);
        assert_eq!(categorize_char('.'), CharCategory::Punctuation);
        assert_eq!(categorize_char(','), CharCategory::Punctuation);
    }

    #[test]
    fn next_word_start_basic() {
        let r = rope("hello world");
        assert_eq!(next_word_start(&r, 0), 6);
        assert_eq!(next_word_start(&r, 6), 11);
    }

    #[test]
    fn next_word_start_from_whitespace_jumps_across_word() {
        let r = rope("hello world foo");
        assert_eq!(next_word_start(&r, 5), 12);
    }

    #[test]
    fn next_word_start_three_words() {
        let r = rope("abc def ghi");
        assert_eq!(next_word_start(&r, 0), 4);
        assert_eq!(next_word_start(&r, 4), 8);
        assert_eq!(next_word_start(&r, 8), 11);
    }

    #[test]
    fn next_word_start_at_end_is_noop() {
        let r = rope("hello");
        assert_eq!(next_word_start(&r, 5), 5);
    }

    #[test]
    fn next_word_start_empty_rope() {
        let r = rope("");
        assert_eq!(next_word_start(&r, 0), 0);
    }

    #[test]
    fn next_word_start_underscore_is_word() {
        let r = rope("foo_bar baz");
        assert_eq!(next_word_start(&r, 0), 8);
    }

    #[test]
    fn next_word_start_punctuation_boundary() {
        let r = rope("alphanumeric.and");
        assert_eq!(next_word_start(&r, 0), 12);
        assert_eq!(next_word_start(&r, 12), 16);
    }

    #[test]
    fn next_word_start_punctuation_group_boundary() {
        let r = rope("alphanumeric.!,and");
        assert_eq!(next_word_start(&r, 0), 12);
        assert_eq!(next_word_start(&r, 12), 15);
    }

    #[test]
    fn next_word_start_stops_on_newline() {
        let r = rope("foo\nbar");
        assert_eq!(next_word_start(&r, 0), 3);
        assert_eq!(next_word_start(&r, 3), 7);
    }

    #[test]
    fn next_word_start_bridges_consecutive_newlines() {
        let r = rope("foo\n\nbar");
        assert_eq!(next_word_start(&r, 0), 3);
        assert_eq!(next_word_start(&r, 3), 5);
    }

    #[test]
    fn next_word_start_multibyte() {
        let r = rope("héllo wörld");
        let world_start = "héllo ".len();
        assert_eq!(next_word_start(&r, 0), world_start);
    }

    #[test]
    fn next_word_end_basic() {
        let r = rope("hello world");
        assert_eq!(next_word_end(&r, 0), 5);
    }

    #[test]
    fn next_word_end_from_whitespace() {
        let r = rope("hello world");
        assert_eq!(next_word_end(&r, 5), 11);
    }

    #[test]
    fn next_word_end_on_last_word_char_goes_to_next_word_end() {
        let r = rope("hello world foo");
        assert_eq!(next_word_end(&r, 10), 15);
    }

    #[test]
    fn next_word_end_at_end_is_noop() {
        let r = rope("hello");
        assert_eq!(next_word_end(&r, 5), 5);
    }

    #[test]
    fn next_word_end_empty_rope() {
        let r = rope("");
        assert_eq!(next_word_end(&r, 0), 0);
    }

    #[test]
    fn next_word_end_punctuation() {
        let r = rope("abc..def");
        assert_eq!(next_word_end(&r, 0), 3);
        assert_eq!(next_word_end(&r, 3), 5);
        assert_eq!(next_word_end(&r, 5), 8);
    }

    #[test]
    fn next_word_end_multibyte() {
        let r = rope("héllo wörld");
        let hello_end = "héllo".len();
        assert_eq!(next_word_end(&r, 0), hello_end);
    }

    #[test]
    fn prev_word_start_basic() {
        let r = rope("hello world");
        assert_eq!(prev_word_start(&r, 6), 0);
        assert_eq!(prev_word_start(&r, 11), 6);
    }

    #[test]
    fn prev_word_start_from_whitespace() {
        let r = rope("hello world");
        assert_eq!(prev_word_start(&r, 5), 0);
    }

    #[test]
    fn prev_word_start_from_end() {
        let r = rope("hello world");
        assert_eq!(prev_word_start(&r, 11), 6);
    }

    #[test]
    fn prev_word_start_at_start_is_noop() {
        let r = rope("hello");
        assert_eq!(prev_word_start(&r, 0), 0);
    }

    #[test]
    fn prev_word_start_empty_rope() {
        let r = rope("");
        assert_eq!(prev_word_start(&r, 0), 0);
    }

    #[test]
    fn prev_word_start_punctuation() {
        let r = rope("abc.def");
        assert_eq!(prev_word_start(&r, 7), 4);
        assert_eq!(prev_word_start(&r, 4), 3);
        assert_eq!(prev_word_start(&r, 3), 0);
    }

    #[test]
    fn prev_word_start_multibyte() {
        let r = rope("héllo wörld");
        let world_start = "héllo ".len();
        let rope_len = r.len();
        assert_eq!(prev_word_start(&r, rope_len), world_start);
        assert_eq!(prev_word_start(&r, world_start), 0);
    }

    #[test]
    fn prev_word_start_across_newline() {
        let r = rope("foo\nbar");
        assert_eq!(prev_word_start(&r, 7), 4);
        assert_eq!(prev_word_start(&r, 4), 0);
    }

    #[test]
    fn next_word_start_trailing_whitespace() {
        let r = rope("hello   ");
        assert_eq!(next_word_start(&r, 0), 8);
    }
}
