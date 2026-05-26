//! Parse Helix/vim-style input strings into [`Keystroke`] sequences.
//!
//! Bare characters become single-character keystrokes and a literal
//! space (or `<Space>`) becomes the `space` key. Angle-bracket tokens
//! are matched case-insensitively: named keys (`<Esc>`, `<Enter>` /
//! `<CR>`, `<Tab>`, `<BS>` / `<Backspace>`, the four arrows), modified
//! keys whose modifier prefixes set `control` / `shift` on a single
//! trailing key (`<C-w>`, `<C-S-w>`), and `<lt>` for a literal `<`.

// The `stoat gui --inputs` driver is this parser's consumer; absent
// that call site the lib build has no non-test users of these items.
#![allow(dead_code)]

use gpui::{Keystroke, Modifiers};
use snafu::{Location, Snafu};

/// Failure parsing an input string in [`parse_input_sequence`].
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub(crate) enum InputParseError {
    #[snafu(display("unterminated '<' token at byte {offset}"))]
    UnterminatedToken {
        offset: usize,
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("unknown key token '<{token}>'"))]
    UnknownToken {
        token: String,
        #[snafu(implicit)]
        location: Location,
    },
}

/// Parse a Helix/vim-style input string into the [`Keystroke`]
/// sequence the input state machine's `feed` consumes.
///
/// Errors on a `<...>` token that names no known key or a `<` with no
/// closing `>`.
pub(crate) fn parse_input_sequence(input: &str) -> Result<Vec<Keystroke>, InputParseError> {
    let mut keystrokes = Vec::new();
    let mut chars = input.char_indices();

    while let Some((offset, ch)) = chars.next() {
        let keystroke = match ch {
            '<' => {
                let mut token = String::new();
                let mut closed = false;
                for (_, c) in chars.by_ref() {
                    if c == '>' {
                        closed = true;
                        break;
                    }
                    token.push(c);
                }
                if !closed {
                    return UnterminatedTokenSnafu { offset }.fail();
                }
                parse_angle_token(&token)?
            },
            ' ' => plain_keystroke("space"),
            _ => plain_keystroke(ch),
        };
        keystrokes.push(keystroke);
    }

    Ok(keystrokes)
}

fn parse_angle_token(token: &str) -> Result<Keystroke, InputParseError> {
    let (modifiers, rest, modified) = strip_modifiers(token);

    let named = match rest.to_ascii_lowercase().as_str() {
        "space" => Some("space"),
        "esc" => Some("escape"),
        "enter" | "cr" => Some("enter"),
        "tab" => Some("tab"),
        "bs" | "backspace" => Some("backspace"),
        "up" => Some("up"),
        "down" => Some("down"),
        "left" => Some("left"),
        "right" => Some("right"),
        "home" => Some("home"),
        "end" => Some("end"),
        "pageup" | "pgup" => Some("pageup"),
        "pagedown" | "pgdn" => Some("pagedown"),
        "delete" | "del" => Some("delete"),
        "insert" | "ins" => Some("insert"),
        "f1" => Some("f1"),
        "f2" => Some("f2"),
        "f3" => Some("f3"),
        "f4" => Some("f4"),
        "f5" => Some("f5"),
        "f6" => Some("f6"),
        "f7" => Some("f7"),
        "f8" => Some("f8"),
        "f9" => Some("f9"),
        "f10" => Some("f10"),
        "f11" => Some("f11"),
        "f12" => Some("f12"),
        "lt" if !modified => return Ok(plain_keystroke('<')),
        _ => None,
    };

    if let Some(key) = named {
        return Ok(Keystroke {
            modifiers,
            key: key.to_string(),
            key_char: None,
        });
    }

    if modified {
        if let Some(key) = single_char(rest) {
            return Ok(Keystroke {
                modifiers,
                key: key.to_string(),
                key_char: None,
            });
        }
    }

    UnknownTokenSnafu {
        token: token.to_string(),
    }
    .fail()
}

fn strip_modifiers(token: &str) -> (Modifiers, &str, bool) {
    let mut modifiers = Modifiers::default();
    let mut rest = token;
    let mut modified = false;

    loop {
        if let Some(stripped) = strip_prefix_ci(rest, "c-") {
            modifiers.control = true;
            modified = true;
            rest = stripped;
        } else if let Some(stripped) = strip_prefix_ci(rest, "s-") {
            modifiers.shift = true;
            modified = true;
            rest = stripped;
        } else if let Some(stripped) = strip_prefix_ci(rest, "a-") {
            modifiers.alt = true;
            modified = true;
            rest = stripped;
        } else {
            return (modifiers, rest, modified);
        }
    }
}

fn strip_prefix_ci<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    let head = input.get(..prefix.len())?;
    let rest = &input[prefix.len()..];
    head.eq_ignore_ascii_case(prefix).then_some(rest)
}

fn single_char(token: &str) -> Option<char> {
    let mut chars = token.chars();
    let ch = chars.next()?;
    chars.next().is_none().then_some(ch)
}

fn plain_keystroke(key: impl Into<String>) -> Keystroke {
    Keystroke {
        modifiers: Modifiers::default(),
        key: key.into(),
        key_char: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Vec<Keystroke> {
        parse_input_sequence(input).expect("parse")
    }

    fn modified(key: &str, control: bool, shift: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                control,
                shift,
                ..Default::default()
            },
            key: key.to_string(),
            key_char: None,
        }
    }

    #[test]
    fn bare_chars_and_spaces() {
        assert_eq!(
            parse("if :G"),
            vec![
                plain_keystroke("i"),
                plain_keystroke("f"),
                plain_keystroke("space"),
                plain_keystroke(":"),
                plain_keystroke("G"),
            ],
        );
    }

    #[test]
    fn named_tokens_are_case_insensitive() {
        assert_eq!(
            parse("<Esc><enter><CR><Tab><BS><Backspace><Up><Down><Left><Right><Space>"),
            vec![
                plain_keystroke("escape"),
                plain_keystroke("enter"),
                plain_keystroke("enter"),
                plain_keystroke("tab"),
                plain_keystroke("backspace"),
                plain_keystroke("backspace"),
                plain_keystroke("up"),
                plain_keystroke("down"),
                plain_keystroke("left"),
                plain_keystroke("right"),
                plain_keystroke("space"),
            ],
        );
    }

    #[test]
    fn modifier_tokens_set_control_and_shift() {
        assert_eq!(
            parse("<C-w><C-S-w><s-w><c-S-W>"),
            vec![
                modified("w", true, false),
                modified("w", true, true),
                modified("w", false, true),
                modified("W", true, true),
            ],
        );
    }

    #[test]
    fn lt_yields_literal_angle_bracket() {
        assert_eq!(
            parse("<lt>a"),
            vec![plain_keystroke("<"), plain_keystroke("a")],
        );
    }

    #[test]
    fn combined_sequence() {
        assert_eq!(
            parse(":wq<Enter>"),
            vec![
                plain_keystroke(":"),
                plain_keystroke("w"),
                plain_keystroke("q"),
                plain_keystroke("enter"),
            ],
        );
    }

    #[test]
    fn empty_input_yields_empty() {
        assert_eq!(parse(""), Vec::<Keystroke>::new());
    }

    #[test]
    fn unknown_token_errors() {
        assert!(matches!(
            parse_input_sequence("<nope>"),
            Err(InputParseError::UnknownToken { .. })
        ));
    }

    #[test]
    fn unterminated_token_reports_offset() {
        assert!(matches!(
            parse_input_sequence("ab<Esc"),
            Err(InputParseError::UnterminatedToken { offset: 2, .. })
        ));
    }

    #[test]
    fn modifier_without_key_errors() {
        assert!(matches!(
            parse_input_sequence("<C->"),
            Err(InputParseError::UnknownToken { .. })
        ));
    }

    #[test]
    fn multi_char_after_modifier_errors() {
        assert!(matches!(
            parse_input_sequence("<C-ab>"),
            Err(InputParseError::UnknownToken { .. })
        ));
    }

    fn modified_named(key: &str, control: bool, shift: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                control,
                shift,
                ..Default::default()
            },
            key: key.to_string(),
            key_char: None,
        }
    }

    #[test]
    fn modifier_prefix_attaches_to_named_keys() {
        assert_eq!(
            parse("<S-Tab><C-Enter><C-Backspace><C-Up>"),
            vec![
                modified_named("tab", false, true),
                modified_named("enter", true, false),
                modified_named("backspace", true, false),
                modified_named("up", true, false),
            ],
        );
    }

    fn alt_named(key: &str, control: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                alt: true,
                control,
                ..Default::default()
            },
            key: key.to_string(),
            key_char: None,
        }
    }

    #[test]
    fn alt_prefix_attaches_alt_modifier() {
        assert_eq!(
            parse("<A-Backspace><A-Left><C-A-w>"),
            vec![
                alt_named("backspace", false),
                alt_named("left", false),
                alt_named("w", true),
            ],
        );
    }

    #[test]
    fn extended_named_keys_round_trip_to_gpui_names() {
        let cases = [
            ("<Home>", "home"),
            ("<End>", "end"),
            ("<PageUp>", "pageup"),
            ("<PgUp>", "pageup"),
            ("<PageDown>", "pagedown"),
            ("<PgDn>", "pagedown"),
            ("<Delete>", "delete"),
            ("<Del>", "delete"),
            ("<Insert>", "insert"),
            ("<Ins>", "insert"),
            ("<F1>", "f1"),
            ("<F2>", "f2"),
            ("<F3>", "f3"),
            ("<F4>", "f4"),
            ("<F5>", "f5"),
            ("<F6>", "f6"),
            ("<F7>", "f7"),
            ("<F8>", "f8"),
            ("<F9>", "f9"),
            ("<F10>", "f10"),
            ("<F11>", "f11"),
            ("<F12>", "f12"),
        ];
        for (token, expected) in cases {
            let parsed = parse(token);
            assert_eq!(
                parsed,
                vec![plain_keystroke(expected)],
                "{token} should parse to key {expected:?}",
            );
        }
    }
}
