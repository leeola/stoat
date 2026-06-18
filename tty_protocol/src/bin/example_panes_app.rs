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

/// Rust source shown in the `main.rs` pane.
const MAIN_RS: &[&str] = &[
    "use std::io::Write;",
    "",
    "fn main() {",
    "    let grid = Grid::new(80, 24);",
    "    let frame = render(&grid);",
    "",
    "    for row in frame.rows() {",
    "        println!(\"{row}\");",
    "    }",
    "}",
    "",
    "fn render(grid: &Grid) -> Frame {",
    "    Frame::from_grid(grid)",
    "}",
];

/// Rust source shown in the `lib.rs` pane.
const LIB_RS: &[&str] = &[
    "pub struct Grid {",
    "    rows: usize,",
    "    cols: usize,",
    "}",
    "",
    "impl Grid {",
    "    pub fn new(rows: usize, cols: usize) -> Self {",
    "        Grid { rows, cols }",
    "    }",
    "",
    "    pub fn area(&self) -> usize {",
    "        self.rows * self.cols",
    "    }",
    "}",
];

/// Manifest shown in the `Cargo.toml` pane.
const CARGO_TOML: &[&str] = &[
    "[package]",
    "name = \"stoatty\"",
    "version = \"0.1.0\"",
    "edition = \"2024\"",
    "",
    "[dependencies]",
    "wgpu = \"29\"",
    "winit = \"0.30\"",
    "cosmic-text = \"0.12\"",
    "",
    "[profile.release]",
    "lto = true",
];

/// File tree shown in the `explorer` pane.
const EXPLORER: &[&str] = &[
    "src/",
    "  main.rs",
    "  lib.rs",
    "  gpu.rs",
    "  render/",
    "    text.rs",
    "    bar.rs",
    "  config.rs",
    "tests/",
    "  headless.rs",
    "Cargo.toml",
    "stoatty.toml",
    "README.md",
];

/// Run output shown in the `terminal` pane.
const TERMINAL: &[&str] = &[
    "$ cargo run --example panes",
    "   Compiling stoatty v0.1.0",
    "    Finished dev in 2.31s",
    "     Running `examples/panes`",
];

/// Test output shown in the `output` pane.
const OUTPUT: &[&str] = &[
    "$ cargo test",
    "   Compiling stoatty",
    "    Finished test in 3.04s",
    "     Running unittests",
    "",
    "running 24 tests",
    "........................",
    "test result: ok. 24 passed",
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

/// Frame `pane` with a rounded border, brighter when focused, write its label
/// just inside the top-left corner, and fill the interior with the label's body.
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

    draw_body(out, pane);
}

/// Write the pane's body text below its label, each line clipped to the frame's
/// interior so nothing spills across the border.
///
/// The interior is the `height - 3` rows below the label and the `width - 4`
/// columns inside the left and right borders, both starting one column past the
/// border to match the label. A narrow pane shows a horizontally-clipped view,
/// the same as a real editor that does not wrap.
fn draw_body(out: &mut Vec<u8>, pane: &Pane) {
    let rows = pane.height.saturating_sub(3) as usize;
    let cols = pane.width.saturating_sub(4) as usize;

    for (row, line) in pane_body(pane.label).iter().take(rows).enumerate() {
        let clipped: String = line.chars().take(cols).collect();
        cup(out, pane.top + 2 + row as u16, pane.left + 2);
        out.extend_from_slice(clipped.as_bytes());
    }
}

/// The body lines for a pane's label, or an empty slice for an unknown label.
fn pane_body(label: &str) -> &'static [&'static str] {
    match label {
        "main.rs" => MAIN_RS,
        "lib.rs" => LIB_RS,
        "Cargo.toml" => CARGO_TOML,
        "explorer" => EXPLORER,
        "terminal" => TERMINAL,
        "output" => OUTPUT,
        _ => &[],
    }
}

/// Emit a Cursor Position escape to the 0-based grid (`row`, `col`).
fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}
