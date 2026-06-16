//! A minimal stoatty demo program: write one styled line, then hold.
//!
//! Emits pure VT (cursor positioning plus SGR) and no stoatty APC codes, so it
//! renders the same in any terminal. Run as the PTY shell by the `hello`
//! example, it proves the bytes-to-render path end to end.

use std::{
    io::{self, Write},
    thread,
};

fn main() {
    let mut stdout = io::stdout();

    // Cursor home, bold green "Hello, world!", then reset.
    stdout
        .write_all(b"\x1b[H\x1b[1;32mHello, world!\x1b[0m")
        .expect("write to stdout");
    stdout.flush().expect("flush stdout");

    // Hold so the shell does not exit and close the window. The window owns this
    // process's lifetime and kills it on close.
    loop {
        thread::park();
    }
}
