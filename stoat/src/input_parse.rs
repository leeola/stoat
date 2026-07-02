//! Parse Helix/vim-style input strings into crossterm [`KeyEvent`] sequences.
//!
//! Bare characters become single-key events and a literal space (or `<Space>`)
//! becomes the space key. Angle-bracket tokens are matched case-insensitively:
//! named keys (`<Esc>`, `<Enter>` / `<CR>`, `<Tab>`, `<BS>` / `<Backspace>`,
//! the arrows, `<Home>`/`<End>`/`<PageUp>`/`<PageDown>`/`<Delete>`/`<Insert>`,
//! `<F1>`..`<F12>`), modifier prefixes that set ctrl/shift/alt on a single
//! trailing key (`<C-w>`, `<C-S-w>`, `<A-Left>`), and `<lt>` for a literal `<`.
//!
//! Events are emitted in the same canonical form the live input path produces
//! after [`crate::keymap_state::normalize_shift_event`], so a driver that feeds
//! them through that path reproduces real typing. A shifted letter is an
//! uppercase `Char` with no SHIFT modifier, and `<S-Tab>` is `BackTab`.

use crate::keymap_state::normalize_shift_event;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use snafu::{Location, Snafu};

/// Failure parsing an input string in [`parse_input_sequence`].
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum InputParseError {
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

/// Parse a Helix/vim-style input string into the crossterm [`KeyEvent`]
/// sequence a self-driver feeds through the input path.
///
/// Errors on a `<...>` token that names no known key or a `<` with no closing
/// `>`.
pub fn parse_input_sequence(input: &str) -> Result<Vec<KeyEvent>, InputParseError> {
    let mut events = Vec::new();
    let mut chars = input.char_indices();

    while let Some((offset, ch)) = chars.next() {
        let event = match ch {
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
            _ => canonical(KeyCode::Char(ch), KeyModifiers::NONE),
        };
        events.push(event);
    }

    Ok(events)
}

fn parse_angle_token(token: &str) -> Result<KeyEvent, InputParseError> {
    let (modifiers, rest, modified) = strip_modifiers(token);

    let code = match rest.to_ascii_lowercase().as_str() {
        "space" => KeyCode::Char(' '),
        "esc" => KeyCode::Esc,
        "enter" | "cr" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "bs" | "backspace" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "lt" if !modified => return Ok(canonical(KeyCode::Char('<'), KeyModifiers::NONE)),
        other => {
            match function_key(other).or_else(|| modified.then(|| single_char(rest)).flatten()) {
                Some(code) => code,
                None => {
                    return UnknownTokenSnafu {
                        token: token.to_string(),
                    }
                    .fail()
                },
            }
        },
    };

    Ok(canonical(code, modifiers))
}

/// Fold `code`/`modifiers` into the canonical form the live input path
/// produces. A terminal reports Shift+Tab as the distinct `BackTab` keycode;
/// [`normalize_shift_event`] then folds a shifted letter to its uppercase
/// `Char` and drops the redundant SHIFT.
fn canonical(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    let code = if modifiers.contains(KeyModifiers::SHIFT) && code == KeyCode::Tab {
        KeyCode::BackTab
    } else {
        code
    };
    normalize_shift_event(KeyEvent::new(code, modifiers))
}

fn function_key(rest: &str) -> Option<KeyCode> {
    let number = rest.strip_prefix('f')?;
    let n: u8 = number.parse().ok()?;
    (1..=12).contains(&n).then_some(KeyCode::F(n))
}

fn strip_modifiers(token: &str) -> (KeyModifiers, &str, bool) {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = token;
    let mut modified = false;

    loop {
        if let Some(stripped) = strip_prefix_ci(rest, "c-") {
            modifiers |= KeyModifiers::CONTROL;
            modified = true;
            rest = stripped;
        } else if let Some(stripped) = strip_prefix_ci(rest, "s-") {
            modifiers |= KeyModifiers::SHIFT;
            modified = true;
            rest = stripped;
        } else if let Some(stripped) = strip_prefix_ci(rest, "a-") {
            modifiers |= KeyModifiers::ALT;
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

fn single_char(token: &str) -> Option<KeyCode> {
    let mut chars = token.chars();
    let ch = chars.next()?;
    chars.next().is_none().then_some(KeyCode::Char(ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Vec<KeyEvent> {
        parse_input_sequence(input).expect("parse")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn modified(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn bare_chars_and_spaces() {
        assert_eq!(
            parse("if :G"),
            vec![
                key(KeyCode::Char('i')),
                key(KeyCode::Char('f')),
                key(KeyCode::Char(' ')),
                key(KeyCode::Char(':')),
                key(KeyCode::Char('G')),
            ],
        );
    }

    #[test]
    fn named_tokens_are_case_insensitive() {
        assert_eq!(
            parse("<Esc><enter><CR><Tab><BS><Backspace><Up><Down><Left><Right><Space>"),
            vec![
                key(KeyCode::Esc),
                key(KeyCode::Enter),
                key(KeyCode::Enter),
                key(KeyCode::Tab),
                key(KeyCode::Backspace),
                key(KeyCode::Backspace),
                key(KeyCode::Up),
                key(KeyCode::Down),
                key(KeyCode::Left),
                key(KeyCode::Right),
                key(KeyCode::Char(' ')),
            ],
        );
    }

    #[test]
    fn extended_named_keys_and_function_keys() {
        assert_eq!(
            parse("<Home><End><PageUp><PageDown><Delete><Insert><F1><F12>"),
            vec![
                key(KeyCode::Home),
                key(KeyCode::End),
                key(KeyCode::PageUp),
                key(KeyCode::PageDown),
                key(KeyCode::Delete),
                key(KeyCode::Insert),
                key(KeyCode::F(1)),
                key(KeyCode::F(12)),
            ],
        );
    }

    #[test]
    fn modifier_tokens_fold_to_canonical_form() {
        assert_eq!(
            parse("<C-w><C-S-w><s-w><c-S-W>"),
            vec![
                modified(KeyCode::Char('w'), KeyModifiers::CONTROL),
                modified(KeyCode::Char('W'), KeyModifiers::CONTROL),
                key(KeyCode::Char('W')),
                modified(KeyCode::Char('W'), KeyModifiers::CONTROL),
            ],
        );
    }

    #[test]
    fn modifier_prefix_attaches_to_named_keys() {
        assert_eq!(
            parse("<S-Tab><C-Enter><C-Backspace><C-Up><A-Left>"),
            vec![
                key(KeyCode::BackTab),
                modified(KeyCode::Enter, KeyModifiers::CONTROL),
                modified(KeyCode::Backspace, KeyModifiers::CONTROL),
                modified(KeyCode::Up, KeyModifiers::CONTROL),
                modified(KeyCode::Left, KeyModifiers::ALT),
            ],
        );
    }

    #[test]
    fn lt_yields_literal_angle_bracket() {
        assert_eq!(
            parse("<lt>a"),
            vec![key(KeyCode::Char('<')), key(KeyCode::Char('a'))],
        );
    }

    #[test]
    fn combined_sequence() {
        assert_eq!(
            parse(":wq<Enter>"),
            vec![
                key(KeyCode::Char(':')),
                key(KeyCode::Char('w')),
                key(KeyCode::Char('q')),
                key(KeyCode::Enter),
            ],
        );
    }

    #[test]
    fn empty_input_yields_empty() {
        assert_eq!(parse(""), Vec::<KeyEvent>::new());
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
}
