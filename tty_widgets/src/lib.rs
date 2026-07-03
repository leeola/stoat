//! Ratatui widgets that emit stoatty APC component frames.
//!
//! Each widget renders both a graceful-degradation cell form into a ratatui
//! buffer and its rich APC frame into an [`ApcScene`], the shared emission buffer
//! a frame's widgets append into and then flush to the terminal.

use std::io::{self, Write};
use stoatty_protocol::command;

pub mod bar;
pub mod border;
pub(crate) mod cells;
pub mod gutter;
pub mod icon;
pub mod panel;
pub mod popover;
pub mod scale;
pub mod scroll_region;
pub mod text_run;

/// The reused emission buffer a frame's widgets append their APC frames into.
///
/// Holds the scene under construction plus the bytes of the last flushed scene.
/// Because terminal-side components persist until replaced, a scene that did not
/// change since the previous flush needs no bytes on the wire at all; comparing
/// against the previous frame turns static or rarely-changing decoration into
/// zero traffic. Both buffers are reused across frames, so steady-state emission
/// allocates nothing.
///
/// Per frame: [`Self::clear`], let widgets append via [`Self::buffer`], then
/// [`Self::flush_to`].
pub struct ApcScene {
    current: Vec<u8>,
    previous: Vec<u8>,
}

impl ApcScene {
    pub fn new() -> ApcScene {
        ApcScene {
            current: Vec::new(),
            previous: Vec::new(),
        }
    }

    /// Empty the scene buffer so widgets can build the next frame from scratch.
    pub fn clear(&mut self) {
        self.current.clear();
    }

    /// The buffer widgets append their APC frames into via the protocol's
    /// `encode_*_into` encoders.
    pub fn buffer(&mut self) -> &mut Vec<u8> {
        &mut self.current
    }

    /// Append the surface's per-line row heights as a `line_layout` frame.
    ///
    /// Most lines are one row; a height above one is an integer-cell inline
    /// expansion that pushes later lines down. The full layout is re-sent on each
    /// change, so this rides alongside the widgets in the same frame.
    pub fn set_line_layout(&mut self, heights: &[u16]) {
        command::encode_line_layout_into(&mut self.current, heights);
    }

    /// Flush the built scene to `out`, but only when it differs from the last
    /// flush.
    ///
    /// On a change, writes a leading `Gstoatty;reset` so the terminal drops the
    /// prior scene, then the new bytes, and records them as the baseline for the
    /// next comparison. An unchanged scene writes nothing, since the terminal-side
    /// components from the previous flush still stand.
    pub fn flush_to(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.current == self.previous {
            return Ok(());
        }

        out.write_all(&command::encode_reset())?;
        out.write_all(&self.current)?;

        std::mem::swap(&mut self.current, &mut self.previous);
        Ok(())
    }
}

impl Default for ApcScene {
    fn default() -> ApcScene {
        ApcScene::new()
    }
}

#[cfg(test)]
mod tests {
    use super::ApcScene;
    use stoatty_protocol::command::{
        self, encode_border, encode_line_layout, encode_reset, BorderCommand, BorderStyle,
        LineLayoutCommand,
    };

    fn border() -> BorderCommand {
        BorderCommand {
            top: 1,
            left: 2,
            width: 3,
            height: 4,
            style: BorderStyle::Light,
            color: [1, 2, 3],
        }
    }

    #[test]
    fn flush_emits_reset_then_scene_when_changed() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());

        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");

        let mut expected = encode_reset();
        expected.extend(encode_border(&border()));
        assert_eq!(out, expected);
    }

    #[test]
    fn flush_skips_an_unchanged_scene() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());
        scene.flush_to(&mut Vec::new()).expect("vec write");

        scene.clear();
        command::encode_border_into(scene.buffer(), &border());
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");

        assert!(out.is_empty(), "an unchanged scene emits nothing");
    }

    #[test]
    fn set_line_layout_appends_the_heights_frame() {
        let mut scene = ApcScene::new();
        scene.set_line_layout(&[1, 2, 1]);

        let expected = encode_line_layout(&LineLayoutCommand {
            heights: vec![1, 2, 1],
        });
        assert_eq!(scene.buffer().as_slice(), expected.as_slice());
    }

    #[test]
    fn flush_re_emits_after_a_change() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());
        scene.flush_to(&mut Vec::new()).expect("vec write");

        scene.clear();
        let changed = BorderCommand {
            color: [9, 9, 9],
            ..border()
        };
        command::encode_border_into(scene.buffer(), &changed);
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");

        let mut expected = encode_reset();
        expected.extend(encode_border(&changed));
        assert_eq!(out, expected);
    }
}
