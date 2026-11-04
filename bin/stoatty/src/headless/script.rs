//! Parse a key script into the steps a headless session plays.
//!
//! The grammar is the one `stoat --inputs` and the agent socket's `keys`
//! request read, so a script moves between them and a headless run unchanged.
//! A bare character types itself, and a literal space or `<Space>` is the space
//! key. Angle-bracket tokens match without case. They name keys (`<Esc>`,
//! `<Enter>` or `<CR>`, `<Tab>`, `<BS>` or `<Backspace>`, the arrows, `<Home>`,
//! `<End>`, `<PageUp>`, `<PageDown>`, `<Delete>`, `<Insert>`, and `<F1>` to
//! `<F12>`), set the `C-`, `S-`, and `A-` modifiers on the one key that follows
//! (`<C-w>`, `<C-S-w>`, `<A-Left>`), or type a literal `<` (`<lt>`).
//!
//! Three directives hold or capture rather than type. `<Wait:N>` pauses for N
//! whole milliseconds. `<Settle>` waits until the child draws nothing new for
//! 250 ms, and `<Settle:N>` for N ms. `<Shot:name>` marks where the caller
//! takes a shot called `name`. A modifier prefix applies to none of them, so
//! `<C-Wait:5>` is an unknown key.

use crate::input::{KeyEventKind, KeyPress};
use snafu::{Location, Snafu};
use std::time::Duration;
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey, SmolStr};

/// The hold a `<Settle>` with no count waits for.
const DEFAULT_SETTLE: Duration = Duration::from_millis(250);

/// One element of a parsed key script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Key {
        key: Key,
        mods: ModifiersState,
    },
    Wait(Duration),
    /// A wait until the child draws nothing new for this long.
    Settle(Duration),
    /// A shot for the caller to take, by name.
    Shot(String),
}

/// Failure parsing a key script in [`parse`].
#[derive(Debug, Snafu)]
pub enum ScriptError {
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

    #[snafu(display("invalid count in '<{token}>': expected whole milliseconds"))]
    InvalidNumber {
        token: String,
        #[snafu(implicit)]
        location: Location,
    },

    #[snafu(display("invalid shot name in '<{token}>': expected letters, digits, '_', or '-'"))]
    InvalidName {
        token: String,
        #[snafu(implicit)]
        location: Location,
    },
}

/// Parse `script` into the steps a headless session plays in order.
///
/// Errors on a `<...>` token that names no key or directive, a count that is
/// not whole milliseconds, a shot name that is empty or holds a character
/// outside `[A-Za-z0-9_-]`, or a `<` with no closing `>`.
pub fn parse(script: &str) -> Result<Vec<Step>, ScriptError> {
    let mut steps = Vec::new();
    let mut chars = script.char_indices();

    while let Some((offset, ch)) = chars.next() {
        let step = match ch {
            '<' => {
                let end = chars
                    .by_ref()
                    .find_map(|(index, c)| (c == '>').then_some(index));
                let Some(end) = end else {
                    return UnterminatedTokenSnafu { offset }.fail();
                };
                parse_token(&script[offset + 1..end])?
            },
            ' ' => Step::Key {
                key: Key::Named(NamedKey::Space),
                mods: ModifiersState::empty(),
            },
            _ => Step::Key {
                key: character(ch),
                mods: ModifiersState::empty(),
            },
        };
        steps.push(step);
    }

    Ok(steps)
}

/// The press the encoder reads for `key` with `mods` held.
///
/// It carries the text a real press of the key types. Under Ctrl or Alt it
/// carries none, since the encoder works from the key for those.
pub(crate) fn press(key: &Key, mods: ModifiersState) -> KeyPress<'_> {
    KeyPress {
        logical: key,
        unmodified: key,
        text: match mods.control_key() || mods.alt_key() {
            true => None,
            false => key.to_text(),
        },
        location: KeyLocation::Standard,
        kind: KeyEventKind::Press,
    }
}

/// The step the text between a token's brackets names.
fn parse_token(token: &str) -> Result<Step, ScriptError> {
    if let Some(count) = strip_prefix_ci(token, "wait:") {
        return Ok(Step::Wait(millis(token, count)?));
    }
    if token.eq_ignore_ascii_case("settle") {
        return Ok(Step::Settle(DEFAULT_SETTLE));
    }
    if let Some(count) = strip_prefix_ci(token, "settle:") {
        return Ok(Step::Settle(millis(token, count)?));
    }
    if let Some(name) = strip_prefix_ci(token, "shot:") {
        return shot(token, name);
    }
    key_token(token)
}

fn millis(token: &str, count: &str) -> Result<Duration, ScriptError> {
    match count.parse::<u32>() {
        Ok(millis) => Ok(Duration::from_millis(u64::from(millis))),
        Err(_) => InvalidNumberSnafu { token }.fail(),
    }
}

fn shot(token: &str, name: &str) -> Result<Step, ScriptError> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !valid {
        return InvalidNameSnafu { token }.fail();
    }
    Ok(Step::Shot(name.to_owned()))
}

/// The key a `<...>` token names, with the modifiers its prefixes set.
fn key_token(token: &str) -> Result<Step, ScriptError> {
    let (mods, rest, modified) = strip_modifiers(token);

    let named = match rest.to_ascii_lowercase().as_str() {
        "space" => NamedKey::Space,
        "esc" => NamedKey::Escape,
        "enter" | "cr" => NamedKey::Enter,
        "tab" => NamedKey::Tab,
        "bs" | "backspace" => NamedKey::Backspace,
        "up" => NamedKey::ArrowUp,
        "down" => NamedKey::ArrowDown,
        "left" => NamedKey::ArrowLeft,
        "right" => NamedKey::ArrowRight,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" | "pgup" => NamedKey::PageUp,
        "pagedown" | "pgdn" => NamedKey::PageDown,
        "delete" | "del" => NamedKey::Delete,
        "insert" | "ins" => NamedKey::Insert,
        "lt" if !modified => {
            return Ok(Step::Key {
                key: character('<'),
                mods,
            })
        },
        other => match (
            function_key(other),
            modified.then(|| single_char(rest)).flatten(),
        ) {
            (Some(named), _) => named,
            (None, Some(ch)) => {
                return Ok(Step::Key {
                    key: modified_character(ch, mods),
                    mods,
                })
            },
            (None, None) => return UnknownTokenSnafu { token }.fail(),
        },
    };

    Ok(Step::Key {
        key: Key::Named(named),
        mods,
    })
}

/// The key a bare `ch` names.
fn character(ch: char) -> Key {
    Key::Character(SmolStr::from(&*ch.encode_utf8(&mut [0; 4])))
}

/// The key `ch` names under `mods`.
///
/// Shift types the uppercase of a character, as a real Shift does, so `<S-a>`
/// types `A` here as it does for `stoat --inputs`. The SHIFT bit stays set, as
/// a real press reports it.
fn modified_character(ch: char, mods: ModifiersState) -> Key {
    match mods.shift_key() {
        true => Key::Character(ch.to_uppercase().collect()),
        false => character(ch),
    }
}

fn function_key(name: &str) -> Option<NamedKey> {
    const KEYS: [NamedKey; 12] = [
        NamedKey::F1,
        NamedKey::F2,
        NamedKey::F3,
        NamedKey::F4,
        NamedKey::F5,
        NamedKey::F6,
        NamedKey::F7,
        NamedKey::F8,
        NamedKey::F9,
        NamedKey::F10,
        NamedKey::F11,
        NamedKey::F12,
    ];

    let number: usize = name.strip_prefix('f')?.parse().ok()?;
    KEYS.get(number.checked_sub(1)?).cloned()
}

/// The modifiers the `C-`, `S-`, and `A-` prefixes of `token` set, the rest of
/// the token after them, and whether any prefix was there.
fn strip_modifiers(token: &str) -> (ModifiersState, &str, bool) {
    let mut mods = ModifiersState::empty();
    let mut rest = token;
    let mut modified = false;

    loop {
        let (bit, stripped) = if let Some(stripped) = strip_prefix_ci(rest, "c-") {
            (ModifiersState::CONTROL, stripped)
        } else if let Some(stripped) = strip_prefix_ci(rest, "s-") {
            (ModifiersState::SHIFT, stripped)
        } else if let Some(stripped) = strip_prefix_ci(rest, "a-") {
            (ModifiersState::ALT, stripped)
        } else {
            return (mods, rest, modified);
        };
        mods |= bit;
        rest = stripped;
        modified = true;
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

#[cfg(test)]
mod tests {
    use super::{parse, press, ScriptError, Step};
    use std::time::Duration;
    use winit::keyboard::{Key, ModifiersState, NamedKey};

    fn keys(script: &str) -> Vec<(Key, ModifiersState)> {
        parse(script)
            .expect("parse")
            .into_iter()
            .map(|step| match step {
                Step::Key { key, mods } => (key, mods),
                other => panic!("unexpected step {other:?}"),
            })
            .collect()
    }

    fn named(key: NamedKey, mods: ModifiersState) -> (Key, ModifiersState) {
        (Key::Named(key), mods)
    }

    fn text(text: &str, mods: ModifiersState) -> (Key, ModifiersState) {
        (Key::Character(text.into()), mods)
    }

    #[test]
    fn bare_characters_space_and_named_keys_parse() {
        let plain = ModifiersState::empty();
        assert_eq!(
            keys(
                "i :G<Space><Esc><enter><CR><Tab><BS><Backspace><Up><Down><Left><Right>\
                 <Home><End><PgUp><PageDown><Del><Insert><F1><f12><lt>"
            ),
            [
                text("i", plain),
                named(NamedKey::Space, plain),
                text(":", plain),
                text("G", plain),
                named(NamedKey::Space, plain),
                named(NamedKey::Escape, plain),
                named(NamedKey::Enter, plain),
                named(NamedKey::Enter, plain),
                named(NamedKey::Tab, plain),
                named(NamedKey::Backspace, plain),
                named(NamedKey::Backspace, plain),
                named(NamedKey::ArrowUp, plain),
                named(NamedKey::ArrowDown, plain),
                named(NamedKey::ArrowLeft, plain),
                named(NamedKey::ArrowRight, plain),
                named(NamedKey::Home, plain),
                named(NamedKey::End, plain),
                named(NamedKey::PageUp, plain),
                named(NamedKey::PageDown, plain),
                named(NamedKey::Delete, plain),
                named(NamedKey::Insert, plain),
                named(NamedKey::F1, plain),
                named(NamedKey::F12, plain),
                text("<", plain),
            ],
        );
    }

    #[test]
    fn a_modifier_prefix_sets_the_state_on_one_key() {
        assert_eq!(
            keys("<C-w><c-S-w><A-Left><S-Tab>x"),
            [
                text("w", ModifiersState::CONTROL),
                text("W", ModifiersState::CONTROL | ModifiersState::SHIFT),
                named(NamedKey::ArrowLeft, ModifiersState::ALT),
                named(NamedKey::Tab, ModifiersState::SHIFT),
                text("x", ModifiersState::empty()),
            ],
            "a shifted character types its uppercase, and the next key is plain",
        );
    }

    #[test]
    fn wait_settle_and_shot_directives_parse() {
        assert_eq!(
            parse("<Wait:40><Settle><settle:900><Shot:final_1-a>").expect("parse"),
            [
                Step::Wait(Duration::from_millis(40)),
                Step::Settle(Duration::from_millis(250)),
                Step::Settle(Duration::from_millis(900)),
                Step::Shot("final_1-a".to_owned()),
            ],
        );
    }

    #[test]
    fn an_unknown_or_unterminated_token_fails() {
        let failure = |script| match parse(script) {
            Err(ScriptError::UnterminatedToken { offset, .. }) => {
                format!("unterminated at {offset}")
            },
            Err(ScriptError::UnknownToken { token, .. }) => format!("unknown {token}"),
            Err(ScriptError::InvalidNumber { token, .. }) => format!("number {token}"),
            Err(ScriptError::InvalidName { token, .. }) => format!("name {token}"),
            Ok(steps) => format!("parsed {steps:?}"),
        };
        assert_eq!(
            [
                "ab<Esc",
                "<Bogus>",
                "<w>",
                "<C-Wait:5>",
                "<Wait:x>",
                "<Settle:-1>",
                "<Shot:a b>",
                "<Shot:>",
            ]
            .map(failure),
            [
                "unterminated at 2",
                "unknown Bogus",
                "unknown w",
                "unknown C-Wait:5",
                "number Wait:x",
                "number Settle:-1",
                "name Shot:a b",
                "name Shot:",
            ],
        );
    }

    #[test]
    fn a_press_carries_text_only_without_control_or_alt() {
        let a = Key::Character("A".into());
        let enter = Key::Named(NamedKey::Enter);
        assert_eq!(
            [
                press(&a, ModifiersState::empty()).text,
                press(&a, ModifiersState::SHIFT).text,
                press(&a, ModifiersState::CONTROL).text,
                press(&a, ModifiersState::ALT).text,
                press(&enter, ModifiersState::empty()).text,
            ],
            [Some("A"), Some("A"), None, None, Some("\r")],
        );
    }
}
