//! A plain text-editing stoatty demo: the cursor walks each line as code is
//! typed, fixes a typo by backspacing and retyping, and the buffer scrolls once
//! the lines fill the screen.
//!
//! Pure VT -- text, a homing CUP, and `\r\n` newlines, with no `Gstoatty` frames
//! -- so it exercises only the renderer's cursor easing (the cursor glides toward
//! each new position, most visibly on the carriage return back to column 0) and
//! whole-grid eased scroll (lines past the bottom glide up into place). The
//! emitter paces itself with short delays so the easing is legible. Run as the
//! PTY shell by the `edit` example.

use std::{
    io::{self, Write},
    thread,
    time::Duration,
};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so the scene looks the same under any
/// theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Delay between typed characters, so the cursor walks the line at a natural
/// pace and the renderer's ease trails it smoothly.
const CHAR_DELAY: Duration = Duration::from_millis(28);

/// Delay between deleted characters while backspacing a typo.
const BACKSPACE_DELAY: Duration = Duration::from_millis(45);

/// Pause after a line's newline, long enough for the cursor's glide back to
/// column 0 to read before the next line starts.
const LINE_PAUSE: Duration = Duration::from_millis(180);

/// Pause after typing a wrong token, before backspacing it, so the typo registers.
const TYPO_PAUSE: Duration = Duration::from_millis(280);

/// Pause after backspacing a typo, before typing the correction.
const FIX_PAUSE: Duration = Duration::from_millis(130);

/// A piece of a line: literal text to type, or a typo to type then correct.
enum Seg {
    Text(&'static str),
    Fix {
        wrong: &'static str,
        right: &'static str,
    },
}

/// The program typed line by line, looped forever. Two lines carry a [`Seg::Fix`]
/// so a typo is typed, backspaced, and retyped mid-line.
const PROGRAM: &[&[Seg]] = &[
    &[Seg::Text("fn main() {")],
    &[Seg::Text("    let mut total = 0;")],
    &[
        Seg::Text("    for line in "),
        Seg::Fix {
            wrong: "imput",
            right: "input",
        },
        Seg::Text(".lines() {"),
    ],
    &[Seg::Text("        let n: i32 = line")],
    &[Seg::Text("            .trim()")],
    &[Seg::Text("            .parse()")],
    &[Seg::Text("            .unwrap_or(0);")],
    &[Seg::Text("        total += n;")],
    &[Seg::Text("    }")],
    &[
        Seg::Text("    "),
        Seg::Fix {
            wrong: "printn",
            right: "println",
        },
        Seg::Text("!(\"sum = {total}\");"),
    ],
    &[Seg::Text("}")],
    &[Seg::Text("")],
];

fn main() {
    let mut out = io::stdout();
    set_palette(&mut out);
    out.write_all(b"\x1b[2J\x1b[H").expect("clear and home");
    out.flush().expect("flush the reset");

    loop {
        for line in PROGRAM {
            for segment in *line {
                type_segment(&mut out, segment);
            }
            newline(&mut out);
        }
    }
}

/// Set the scene's foreground and background so the typed text and the cleared
/// screen share the editor colors.
fn set_palette(out: &mut impl Write) {
    let sgr = format!(
        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
        EDITOR_FG[0], EDITOR_FG[1], EDITOR_FG[2], EDITOR_BG[0], EDITOR_BG[1], EDITOR_BG[2],
    );
    out.write_all(sgr.as_bytes()).expect("set the palette");
}

/// Type one segment: literal text straight through, or a typo typed then
/// backspaced and replaced with the correction.
fn type_segment(out: &mut impl Write, segment: &Seg) {
    match segment {
        Seg::Text(text) => type_text(out, text),
        Seg::Fix { wrong, right } => {
            type_text(out, wrong);
            sleep(TYPO_PAUSE);
            backspace(out, wrong.chars().count());
            sleep(FIX_PAUSE);
            type_text(out, right);
        },
    }
}

/// Type `text` one character at a time, flushing and pausing between each so the
/// cursor walks the line.
fn type_text(out: &mut impl Write, text: &str) {
    let mut buf = [0u8; 4];
    for ch in text.chars() {
        out.write_all(ch.encode_utf8(&mut buf).as_bytes())
            .expect("write a character");
        out.flush().expect("flush a character");
        sleep(CHAR_DELAY);
    }
}

/// Delete the last `count` characters with destructive backspaces: move left,
/// overwrite with a space, move left again.
fn backspace(out: &mut impl Write, count: usize) {
    for _ in 0..count {
        out.write_all(b"\x08 \x08").expect("backspace a character");
        out.flush().expect("flush a backspace");
        sleep(BACKSPACE_DELAY);
    }
}

/// End the line: a carriage return eases the cursor back to column 0 and the line
/// feed advances it, scrolling the screen once the lines fill it.
fn newline(out: &mut impl Write) {
    out.write_all(b"\r\n").expect("write a newline");
    out.flush().expect("flush a newline");
    sleep(LINE_PAUSE);
}

fn sleep(duration: Duration) {
    thread::sleep(duration);
}
