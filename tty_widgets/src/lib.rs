//! Ratatui widgets that emit stoatty APC component frames.
//!
//! Each widget renders both a graceful-degradation cell form into a ratatui
//! buffer and its rich APC frame into an [`ApcScene`], the shared emission buffer
//! a frame's widgets append into and then flush to the terminal.
//!
//! [`ApcSession`] wraps a scene in the terminal work a program needs around it.
//! It finds out which terminal answers, takes raw mode or mouse reporting when
//! the program asks, and gives both back however the program ends.

use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::{
    io::{self, Write},
    panic,
    sync::Once,
};
use stoatty_protocol::{
    command::{self, HelloCommand},
    detect,
};

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

/// How a session settles whether the host renders APC components.
///
/// The two answers are not interchangeable. Prefer [`Detect::Handshake`] and
/// reach for [`Detect::Env`] only when the round trip is impossible.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Detect {
    /// Ask the terminal and believe what it answers.
    ///
    /// Costs one round trip and sole ownership of stdin while it runs, and is
    /// right even where the environment lies.
    #[default]
    Handshake,
    /// Believe [`detect::env_says_stoatty`] instead of asking.
    ///
    /// For a program that never owns stdin, such as one that reads a pipe or
    /// runs under a line editor it does not control. A different terminal nested
    /// inside a stoatty session inherits the variable and reads as a stoatty
    /// here, so a session that picks this sends streamed content to a host that
    /// prints it.
    Env,
}

/// What a program wants from the terminal for the life of a session.
///
/// Every field is off by default, which suits a program that only paints and
/// never reads input.
pub struct SessionOptions {
    /// How the program names itself in the terminal's log. Only the log reads
    /// it, so a program with nothing to say leaves the strings empty.
    pub hello: HelloCommand,
    /// How the session settles which terminal answers.
    pub detect: Detect,
    /// Hold raw mode for the session, for a program that reads keys itself.
    pub raw_mode: bool,
    /// Hold mouse reporting for the session.
    pub mouse_capture: bool,
    /// Hide the cursor for the session, for a scene that draws its own or none.
    pub hide_cursor: bool,
}

impl Default for SessionOptions {
    fn default() -> SessionOptions {
        SessionOptions {
            hello: HelloCommand {
                pid: std::process::id(),
                log_id: String::new(),
                hostname: String::new(),
                version: String::new(),
                protocol: stoatty_protocol::PROTOCOL_VERSION,
            },
            detect: Detect::default(),
            raw_mode: false,
            mouse_capture: false,
            hide_cursor: false,
        }
    }
}

/// A program's whole conversation with the terminal: who answered, what was
/// borrowed to talk to it, and the scene emitted into it.
///
/// A program that emits APC has three problems that have nothing to do with the
/// picture it draws. It has to find out whether the host understands the
/// protocol at all, because streamed popover, text-run, and page-fill content
/// rides outside the frame wrapper and prints as characters anywhere else. It
/// has to borrow terminal state to read input. And it has to give that state
/// back on every way out, including the ones it did not plan for. Hand-rolling
/// the three is how a terminal ends up stuck in raw mode with the cursor gone.
///
/// The session answers the first at construction and hands the verdict to the
/// widgets through [`Self::scene`], so a dead host silently gets cell forms
/// only. It answers the second and third together. Whatever [`SessionOptions`]
/// asks for is taken here and released in [`Drop`], and a panic hook releases it
/// too, so a panic deep inside a widget still lands the user back in a usable
/// terminal.
///
/// One session per program. Two at once each restore the terminal on their own
/// schedule.
///
/// See also:
/// - [`ApcScene`] for the emission buffer this hands out.
/// - [`detect`] for the detection this runs, and what each answer is worth.
pub struct ApcSession {
    scene: ApcScene,
    typed: Vec<u8>,
    raw_mode: bool,
    mouse_capture: bool,
    cursor_hidden: bool,
}

impl ApcSession {
    /// Settle who the terminal is, take what `options` asks for, and arm the
    /// restore paths.
    ///
    /// Under [`Detect::Handshake`] this reads raw stdin until the terminal
    /// answers, so call it before anything else reads input. Raw mode is taken
    /// for the probe whether or not `options` wants it, because a terminal in
    /// canonical mode holds the reply back until a newline and echoes it onto
    /// the screen. Cooked mode is restored right after when it was not asked
    /// for.
    ///
    /// The probe consumes whatever arrived on stdin while it ran, including what
    /// someone typed at launch. [`Self::typed_at_open`] hands those bytes back,
    /// so a program that reads input replays them instead of losing them.
    pub fn new(options: SessionOptions) -> ApcSession {
        install_panic_hook();

        let probing = options.detect == Detect::Handshake;
        if probing || options.raw_mode {
            let _ = enable_raw_mode();
        }

        let (live, typed) = match options.detect {
            Detect::Env => (detect::env_says_stoatty(), Vec::new()),
            Detect::Handshake => {
                let (reply, typed) = detect::handshake(&options.hello, detect::HANDSHAKE_FALLBACK);
                (reply.is_some(), typed)
            },
        };

        if probing && !options.raw_mode {
            let _ = disable_raw_mode();
        }
        if options.mouse_capture {
            let _ = execute!(io::stdout(), EnableMouseCapture);
        }
        if options.hide_cursor {
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[?25l");
            let _ = out.flush();
        }

        let mut scene = ApcScene::new();
        scene.set_live(live);

        ApcSession {
            scene,
            typed,
            raw_mode: options.raw_mode,
            mouse_capture: options.mouse_capture,
            cursor_hidden: options.hide_cursor,
        }
    }

    /// Whether the host renders APC components, which is what a program branches
    /// on before it emits anything the protocol does not wrap.
    ///
    /// Frames need no gate, since an APC string is ignored anywhere else. Content
    /// streamed between an open and an end marker does, and so does any command
    /// a program waits on for an effect.
    pub fn live(&self) -> bool {
        self.scene.live()
    }

    /// The scene this frame's widgets append into, already told whether the host
    /// renders what they emit.
    pub fn scene(&mut self) -> &mut ApcScene {
        &mut self.scene
    }

    /// Whatever arrived on stdin while the handshake owned it.
    ///
    /// Someone typing at launch lands here rather than in the program's own
    /// input. Empty under [`Detect::Env`], which never reads.
    pub fn typed_at_open(&self) -> &[u8] {
        &self.typed
    }

    /// Write the scene to stdout, if it changed since the last flush.
    pub fn flush(&mut self) -> io::Result<()> {
        let mut out = io::stdout();
        self.scene.flush_to(&mut out)?;
        out.flush()
    }
}

impl Drop for ApcSession {
    fn drop(&mut self) {
        {
            let mut out = io::stdout();
            let _ = out.write_all(&restore_bytes(self.live(), self.cursor_hidden));
            let _ = out.flush();
        }

        if self.mouse_capture {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}

/// Returns the bytes that undo what a session put on the screen.
///
/// That is the scene it left standing and the cursor it hid. Split out from
/// [`Drop`] so the sequence is checkable without a terminal to run it against.
fn restore_bytes(live: bool, cursor_hidden: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if live {
        out.extend(command::encode_reset());
    }
    if cursor_hidden {
        out.extend_from_slice(b"\x1b[?25h");
    }
    out
}

/// Put the terminal back before the panic message prints, so the user reads it
/// in a terminal that still echoes and scrolls.
///
/// Every step is a no-op when it was never needed, so this restores
/// unconditionally. The hook is process-global and never sees which session, if
/// any, took what.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let prior = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            {
                let mut out = io::stdout();
                let _ = out.write_all(&restore_bytes(true, true));
                let _ = out.flush();
            }
            let _ = execute!(io::stdout(), DisableMouseCapture);
            let _ = disable_raw_mode();

            prior(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::{restore_bytes, ApcScene};
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

    /// A session gives back exactly what it took. A wider restore clears a scene
    /// the session never put up, or shows a cursor the program hid itself.
    #[test]
    fn a_session_restores_only_what_it_changed() {
        let show_cursor = b"\x1b[?25h".to_vec();
        let reset = encode_reset();

        assert_eq!(restore_bytes(false, false), Vec::new(), "took nothing");
        assert_eq!(restore_bytes(false, true), show_cursor, "cursor only");
        assert_eq!(restore_bytes(true, false), reset, "scene only");
        assert_eq!(
            restore_bytes(true, true),
            [reset, show_cursor].concat(),
            "the scene goes before the cursor comes back",
        );
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
