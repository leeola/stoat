//! A stoatty demo that draws an image: a gradient transmitted as Kitty
//! graphics, then held on screen.
//!
//! The bytes are the Kitty protocol rather than stoatty's own, so this renders
//! in any terminal that speaks it and prints nothing in one that does not. Run
//! as the PTY shell by the `image` example, it proves the transmit to decode to
//! place to draw path end to end.

use base64::Engine;
use std::{
    io::{self, Write},
    thread,
};
use stoatty_protocol::kitty::{self, Action, ControlData, Format};

/// Side of the gradient, in pixels. Large enough that the chunking is real
/// rather than a single frame pretending to be chunked.
const SIDE: u32 = 96;

fn main() {
    let mut stdout = io::stdout();

    stdout
        .write_all(b"\x1b[H\x1b[1;36mA Kitty graphics image:\x1b[0m\r\n\r\n")
        .expect("write to stdout");

    let mut frames = Vec::new();
    kitty::encode_chunked_into(
        &mut frames,
        &ControlData {
            action: Action::TransmitAndDisplay,
            format: Format::Rgba,
            width: SIDE,
            height: SIDE,
            id: 1,
            ..ControlData::default()
        },
        &base64::engine::general_purpose::STANDARD
            .encode(gradient())
            .into_bytes(),
    );
    stdout.write_all(&frames).expect("write to stdout");
    stdout.flush().expect("flush stdout");

    // Hold so the shell does not exit and close the window. The window owns this
    // process's lifetime and kills it on close.
    loop {
        thread::park();
    }
}

/// A red-to-green gradient across x, blue rising with y, fully opaque.
///
/// Chosen so every channel varies: a bug that dropped one, or that read the
/// rows in the wrong order, shows as a flat or mirrored image rather than
/// something that merely looks a bit off.
fn gradient() -> Vec<u8> {
    let mut pixels = Vec::with_capacity((SIDE * SIDE * 4) as usize);
    for y in 0..SIDE {
        for x in 0..SIDE {
            let across = (x * 255 / (SIDE - 1)) as u8;
            let down = (y * 255 / (SIDE - 1)) as u8;
            pixels.extend_from_slice(&[255 - across, across, down, 255]);
        }
    }
    pixels
}
