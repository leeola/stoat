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
pub mod minimap;
pub mod panel;
pub mod polyline;
pub mod popover;
pub mod scale;
pub mod scroll_region;
pub mod status_bar;
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
/// A scene may also be *dead*, which is how a host that is not stoatty is
/// described to the widgets. [`Self::live`] reports it so each widget picks its
/// cell form, and a dead scene swallows anything pushed into it, so a fork
/// nobody remembered to update emits nothing rather than half a frame.
///
/// Per frame: [`Self::clear`], let widgets append via [`Self::buffer`], then
/// [`Self::flush_to`].
pub struct ApcScene {
    current: Vec<u8>,
    previous: Vec<u8>,
    live: bool,
    /// Where [`Self::buffer`] sends writes while dead. Cleared on every handout,
    /// so it never holds more than the one append its borrow allows.
    discard: Vec<u8>,
}

impl ApcScene {
    /// A live scene, which is what a caller that never mentions a host wants.
    /// Deadness is set explicitly via [`Self::set_live`].
    pub fn new() -> ApcScene {
        ApcScene {
            current: Vec::new(),
            previous: Vec::new(),
            live: true,
            discard: Vec::new(),
        }
    }

    /// Whether the host can render APC components, which is what widgets branch
    /// on to choose their rich form over their cell form.
    pub fn live(&self) -> bool {
        self.live
    }

    /// Declare whether the host can render APC components.
    ///
    /// Expected once per frame rather than at construction, since whether a
    /// stoatty is listening is only learned after the session has already
    /// painted.
    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    /// Empty the scene buffer so widgets can build the next frame from scratch.
    pub fn clear(&mut self) {
        self.current.clear();
    }

    /// The buffer widgets append their APC frames into via the protocol's
    /// `encode_*_into` encoders.
    ///
    /// While dead this hands back a scratch buffer instead, so the append lands
    /// nowhere.
    pub fn buffer(&mut self) -> &mut Vec<u8> {
        match self.live {
            true => &mut self.current,
            false => {
                self.discard.clear();
                &mut self.discard
            },
        }
    }

    /// The built scene bytes, for a reader that decodes the frame without
    /// appending to it.
    pub fn bytes(&self) -> &[u8] {
        &self.current
    }

    /// Append the surface's per-line row heights as a `line_layout` frame.
    ///
    /// Most lines are one row; a height above one is an integer-cell inline
    /// expansion that pushes later lines down. The full layout is re-sent on each
    /// change, so this rides alongside the widgets in the same frame.
    pub fn set_line_layout(&mut self, heights: &[u16]) {
        command::encode_line_layout_into(self.buffer(), heights);
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

    /// Widgets branch on liveness themselves, but one that forgets to must not
    /// be able to put half a frame on the wire.
    #[test]
    fn a_dead_scene_swallows_what_is_pushed_into_it() {
        let mut scene = ApcScene::new();
        scene.set_live(false);

        command::encode_border_into(scene.buffer(), &border());
        scene.set_line_layout(&[1, 2, 3]);

        assert!(!scene.live(), "and reports itself dead to the widgets");
        assert!(scene.bytes().is_empty(), "nothing reached the scene");
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");
        assert!(out.is_empty(), "so a flush has nothing to write");
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
