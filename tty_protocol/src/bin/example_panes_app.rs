//! A pane-layout stoatty demo: a tiling editor that splits, rearranges, and
//! merges panes by redrawing their border frames at new positions.
//!
//! Each discrete step clears the prior decorations with a `Gstoatty;reset` frame,
//! then draws the step's pane frames afresh, so panes jump straight to their new
//! positions with no ghosts of the old layout. The steps are discrete: the
//! renderer does not ease a frame between positions, so the demo pauses on each
//! layout rather than faking a smooth reposition. Run as the PTY shell by the
//! `panes` example.

use std::{
    io::{self, Write},
    thread,
    time::Duration,
};
use stoatty_protocol::command::{self, BorderCommand, BorderStyle};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so the scene looks the same under any
/// theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Border color of the focused (first) pane in each layout.
const FOCUSED: [u8; 3] = [97, 175, 239];

/// Border color of the other panes.
const DIM: [u8; 3] = [78, 86, 102];

/// How long each layout holds before the next discrete step.
const STEP_PAUSE: Duration = Duration::from_millis(1100);

/// One pane: a cell rectangle to frame and a label drawn inside it.
struct Pane {
    top: u16,
    left: u16,
    width: u16,
    height: u16,
    label: &'static str,
}

/// The layouts cycled through, one per discrete step. The first pane of each is
/// the focused one. The sequence splits a single pane down to four, rearranges
/// them into a sidebar/main/panel layout, then merges back to one.
const LAYOUTS: &[&[Pane]] = &[
    &[Pane {
        top: 1,
        left: 2,
        width: 70,
        height: 20,
        label: "main.rs",
    }],
    &[
        Pane {
            top: 1,
            left: 2,
            width: 35,
            height: 20,
            label: "main.rs",
        },
        Pane {
            top: 1,
            left: 37,
            width: 35,
            height: 20,
            label: "Cargo.toml",
        },
    ],
    &[
        Pane {
            top: 1,
            left: 2,
            width: 35,
            height: 20,
            label: "main.rs",
        },
        Pane {
            top: 1,
            left: 37,
            width: 35,
            height: 10,
            label: "Cargo.toml",
        },
        Pane {
            top: 11,
            left: 37,
            width: 35,
            height: 10,
            label: "output",
        },
    ],
    &[
        Pane {
            top: 1,
            left: 2,
            width: 35,
            height: 10,
            label: "main.rs",
        },
        Pane {
            top: 11,
            left: 2,
            width: 35,
            height: 10,
            label: "lib.rs",
        },
        Pane {
            top: 1,
            left: 37,
            width: 35,
            height: 10,
            label: "Cargo.toml",
        },
        Pane {
            top: 11,
            left: 37,
            width: 35,
            height: 10,
            label: "output",
        },
    ],
    &[
        Pane {
            top: 1,
            left: 2,
            width: 20,
            height: 20,
            label: "explorer",
        },
        Pane {
            top: 1,
            left: 23,
            width: 49,
            height: 14,
            label: "main.rs",
        },
        Pane {
            top: 15,
            left: 23,
            width: 49,
            height: 6,
            label: "terminal",
        },
    ],
];

fn main() {
    let mut stdout = io::stdout();
    setup(&mut stdout);

    loop {
        for panes in LAYOUTS {
            let mut frame = Vec::new();
            render_step(&mut frame, panes);
            stdout.write_all(&frame).expect("write a layout");
            stdout.flush().expect("flush a layout");
            thread::sleep(STEP_PAUSE);
        }
    }
}

/// Set the editor palette and hide the cursor, since the demo is about pane
/// frames rather than a text cursor.
fn setup(out: &mut impl Write) {
    let sgr = format!(
        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
        EDITOR_FG[0], EDITOR_FG[1], EDITOR_FG[2], EDITOR_BG[0], EDITOR_BG[1], EDITOR_BG[2],
    );
    out.write_all(sgr.as_bytes()).expect("set the palette");
    out.write_all(b"\x1b[?25l").expect("hide the cursor");
    out.flush().expect("flush the setup");
}

/// Reset the prior decorations and cells, then draw this step's panes from
/// scratch, so the new layout replaces the old with no leftover frames.
fn render_step(out: &mut Vec<u8>, panes: &[Pane]) {
    out.extend_from_slice(&command::encode_reset());
    out.extend_from_slice(b"\x1b[2J\x1b[H");

    for (index, pane) in panes.iter().enumerate() {
        draw_pane(out, pane, index == 0);
    }
}

/// Frame `pane` with a rounded border, brighter when focused, and write its label
/// just inside the top-left corner.
fn draw_pane(out: &mut Vec<u8>, pane: &Pane, focused: bool) {
    out.extend_from_slice(&command::encode_border(&BorderCommand {
        top: pane.top,
        left: pane.left,
        width: pane.width,
        height: pane.height,
        style: BorderStyle::Rounded,
        color: if focused { FOCUSED } else { DIM },
    }));

    cup(out, pane.top + 1, pane.left + 2);
    out.extend_from_slice(pane.label.as_bytes());
}

/// Emit a Cursor Position escape to the 0-based grid (`row`, `col`).
fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}
