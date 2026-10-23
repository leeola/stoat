//! Ratatui widgets that emit stoatty APC component frames.
//!
//! Each widget renders both a graceful-degradation cell form into a ratatui
//! buffer and its rich APC frame into an [`ApcScene`], the shared emission buffer
//! a frame's widgets append into and then flush to the terminal.
//!
//! A widget that carries both also exposes them separately, as
//! `draw_components` and `draw_fallback`. An app that composites its own rich
//! chrome calls one of the two rather than the combined render, which
//! otherwise leaves the cell form showing beneath the components.
//!
//! [`ApcSession`] wraps a scene in the terminal work a program needs around it.
//! It finds out which terminal answers, takes raw mode or mouse reporting when
//! the program asks, and gives both back however the program ends.
//!
//! `docs/stoatty-protocol.md` in the repository is the prose reference for the
//! wire format these widgets emit, for a program that reaches past the kit and
//! encodes frames itself.

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
    command::{self, HelloCommand, PanelCommand},
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
pub mod pool;
pub mod popover;
pub mod scale;
pub mod scroll_region;
pub mod sketch;
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
/// The scene has two lanes, because the protocol has two kinds of command. A
/// *decoration* (a border, a panel, a bar) is re-declared every frame and is
/// what the leading reset clears, so removing one from the scene is all it takes
/// to remove it from the screen. A *persistent* command instead updates state
/// the terminal keeps, and a reset in front of it is either wasteful or
/// destructive. A re-declared `scroll_region` whose region the reset just
/// cleared seeds a full-height glide rather than easing from where it was.
/// [`Self::buffer`] is the decoration lane and [`Self::dynamic_buffer`] the
/// persistent one.
///
/// A scene may also be *dead*, which is how a host that is not stoatty is
/// described. No widget reads that itself. [`Self::live`] is for the caller,
/// which picks between a widget's `draw_components` and its `draw_fallback`,
/// since only the caller knows whether it composites the chrome or leaves that
/// to the widget's own render.
///
/// A dead scene swallows anything pushed into it regardless, so a caller that
/// gets the choice wrong emits nothing rather than half a frame.
///
/// The decoration side divides further into numbered lanes, which
/// [`Self::set_lane`] opens and [`Self::end_lane`] closes. A terminal at
/// protocol 12 or later reads them, and with [`Self::set_lane_protocol`] on, a
/// flush writes only the lanes that changed, each inside its own lane markers.
/// With it off, the lanes read end to end and flush as one undivided lane,
/// which is what an older terminal understands.
///
/// Per frame: [`Self::clear`], let widgets append via [`Self::buffer`] or
/// [`Self::dynamic_buffer`], then [`Self::flush_to`].
pub struct ApcScene {
    /// The numbered decoration lanes, sorted by id. Lane 0 is always first, and
    /// it holds every decoration appended outside a lane.
    lanes: Vec<Lane>,
    /// The index in [`Self::lanes`] of the lane [`Self::buffer`] appends to.
    active: usize,
    /// Whether the host reads lane markers, as [`Self::set_lane_protocol`] set.
    lanes_enabled: bool,
    /// Whether the next flush resets the terminal and sends the whole scene.
    ///
    /// Set by a switch of the lane protocol, since a flush in the other mode
    /// left the terminal's lanes out of step with the baselines. A flush
    /// without markers puts every lane's bytes in the terminal's lane 0, so a
    /// lane sent on its own after it draws twice.
    resync: bool,
    dynamic_current: Vec<u8>,
    dynamic_previous: Vec<u8>,
    live: bool,
    /// Where the two buffer handouts send writes while dead. Cleared on every
    /// handout, so it never holds more than the one append its borrow allows.
    discard: Vec<u8>,
}

impl ApcScene {
    /// A live scene, which is what a caller that never mentions a host wants.
    /// Deadness is set explicitly via [`Self::set_live`].
    pub fn new() -> ApcScene {
        ApcScene {
            lanes: vec![Lane::new(0)],
            active: 0,
            lanes_enabled: false,
            resync: false,
            dynamic_current: Vec::new(),
            dynamic_previous: Vec::new(),
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

    /// Declare whether the host reads lane markers, which a stoatty at protocol
    /// 12 or later does.
    ///
    /// Expected once per frame like [`Self::set_live`]. A switch makes the next
    /// flush reset the terminal and send the whole scene.
    pub fn set_lane_protocol(&mut self, enabled: bool) {
        if self.lanes_enabled != enabled {
            self.lanes_enabled = enabled;
            self.resync = true;
        }
    }

    /// Empty every lane so widgets build the next frame from scratch, and send
    /// appends to lane 0 again.
    pub fn clear(&mut self) {
        for lane in &mut self.lanes {
            lane.current.clear();
        }
        self.active = 0;
        self.dynamic_current.clear();
    }

    /// Send the decorations that follow to lane `id`, until [`Self::end_lane`]
    /// or the next [`Self::clear`].
    ///
    /// A lane is the unit a flush sends on its own, so a part of the frame that
    /// changes apart from the rest, such as one pane's status row, gets a lane.
    /// A change there then leaves the rest of the scene off the wire.
    pub fn set_lane(&mut self, id: u32) {
        self.active = match self.lanes.binary_search_by_key(&id, |lane| lane.id) {
            Ok(index) => index,
            Err(index) => {
                self.lanes.insert(index, Lane::new(id));
                index
            },
        };
    }

    /// Send the decorations that follow to lane 0 again.
    pub fn end_lane(&mut self) {
        self.active = 0;
    }

    /// Forget what the last flush wrote, so the next [`Self::flush_to`] sends
    /// the whole scene even when it matches.
    ///
    /// The baselines describe what the terminal holds, which stops being true
    /// once another program drives the same terminal: its reset dropped this
    /// scene, and a matching lane would write nothing to put it back. Call this
    /// when the terminal comes back, then build the next frame as usual.
    pub fn forget_flushed(&mut self) {
        for lane in &mut self.lanes {
            lane.previous.clear();
        }
        self.dynamic_previous.clear();
    }

    /// The buffer widgets append their decoration frames into via the protocol's
    /// `encode_*_into` encoders.
    ///
    /// It is the lane [`Self::set_lane`] opened, or lane 0 outside one. A widget
    /// that stops appending here leaves the screen on the next flush, which
    /// clears what the lane held. Every widget in the kit but
    /// [`scroll_region::ScrollRegion`] emits into it.
    ///
    /// While dead this hands back a scratch buffer instead, so the append lands
    /// nowhere.
    pub fn buffer(&mut self) -> &mut Vec<u8> {
        match self.live {
            true => &mut self.lanes[self.active].current,
            false => {
                self.discard.clear();
                &mut self.discard
            },
        }
    }

    /// The buffer widgets append persistent commands into, the ones no reset
    /// precedes.
    ///
    /// A command belongs here when it updates terminal state that outlives the
    /// frame rather than re-declaring a decoration. `scroll_region` is the
    /// standing case, since the terminal eases it by the change between
    /// declarations, and a reset in front of it restarts that ease from
    /// nothing. The pool commands driving a smooth scroll are the same shape.
    ///
    /// Appending here does not mean the terminal never drops the command. A
    /// reset the decoration lane emits still clears whatever it clears, which is
    /// why [`Self::flush_to`] re-sends this lane whole behind one.
    ///
    /// While dead this hands back a scratch buffer instead, so the append lands
    /// nowhere.
    pub fn dynamic_buffer(&mut self) -> &mut Vec<u8> {
        match self.live {
            true => &mut self.dynamic_current,
            false => {
                self.discard.clear();
                &mut self.discard
            },
        }
    }

    /// The open lane under construction, for a reader that inspects the frame
    /// without appending to it.
    ///
    /// Valid between [`Self::clear`] and [`Self::flush_to`], where it grows as
    /// the frame's widgets append. That window is what lets a caller record a
    /// length before some part of the frame paints and slice out what that part
    /// emitted afterwards. The length marks the lane that was open when it was
    /// read, so the slice holds only while that lane stays open.
    ///
    /// After a flush the value is stale, and stale in one of two ways: a lane
    /// that was written holds the frame before the one just flushed, because the
    /// flush swaps it with the baseline rather than copying, and a lane that was
    /// unchanged holds the frame it just compared. Read it before the flush, not
    /// after.
    pub fn bytes(&self) -> &[u8] {
        &self.lanes[self.active].current
    }

    /// The bytes lane `id` holds under construction, whichever lane is open.
    ///
    /// Empty for a lane nothing has opened. A caller that records what one part
    /// of the frame wrote to several lanes marks each lane by its length here.
    pub fn lane_bytes(&self, id: u32) -> &[u8] {
        match self.lanes.binary_search_by_key(&id, |lane| lane.id) {
            Ok(index) => &self.lanes[index].current,
            Err(_) => &[],
        }
    }

    /// Every lane under construction, read end to end in id order.
    ///
    /// That is the order the terminal composes the lanes in, so this is the
    /// whole frame as a reader that composites it has to see it. Allocates,
    /// unlike [`Self::bytes`].
    pub fn all_bytes(&self) -> Vec<u8> {
        self.lanes
            .iter()
            .flat_map(|lane| lane.current.iter().copied())
            .collect()
    }

    /// Every panel the frame under construction declares, lane by lane.
    ///
    /// A pane pool's composite leaves each panel's rect to the live grid, so a
    /// program that keeps its cells out of a pool's rect still has to write
    /// the cells under these. Valid where [`Self::bytes`] is.
    pub fn panels(&self) -> impl Iterator<Item = PanelCommand> {
        self.lanes
            .iter()
            .flat_map(|lane| command::decode_panels(&lane.current))
    }

    /// Append the surface's per-line row heights as a `line_layout` frame.
    ///
    /// Most lines are one row; a height above one is an integer-cell inline
    /// expansion that pushes later lines down. The full layout is re-sent on each
    /// change, so this rides alongside the widgets in the same frame.
    pub fn set_line_layout(&mut self, heights: &[u16]) {
        command::encode_line_layout_into(self.buffer(), heights);
    }

    /// Flush the built scene to `out`, lane by lane, writing only what differs
    /// from the last flush.
    ///
    /// Without the lane protocol, the numbered lanes read end to end as one
    /// decoration lane. A change anywhere in it writes a leading `Gstoatty;reset`
    /// so the terminal drops the prior scene, then the new bytes. An unchanged
    /// one writes nothing, since the terminal-side components from the previous
    /// flush still stand.
    ///
    /// With the lane protocol, each numbered lane that changed goes out on its
    /// own inside its lane markers. A lane that lost every decoration still
    /// writes its bare markers, which clear it in the terminal. Lane 0 has no
    /// markers. It changes behind a reset as above, and since a reset clears
    /// every lane, every other lane with content then goes out again behind it.
    ///
    /// The dynamic lane follows, never behind a reset of its own. It writes when
    /// it changed, and also whenever a reset just went out, because that reset
    /// clears the persistent commands it holds too and an unchanged lane never
    /// puts them back on its own. Writing it last is what puts it after the
    /// reset rather than under it.
    ///
    /// Each lane it writes becomes the baseline for the next comparison.
    ///
    /// A dead scene writes nothing and records nothing. The lanes are empty
    /// while dead, so a byte comparison alone reads that as "everything was
    /// removed" and sends a bare reset at a host that never asked for one. The
    /// baselines are left standing, so a scene that goes live again still
    /// describes what the terminal was last told and re-sends only what moved
    /// since.
    pub fn flush_to(&mut self, out: &mut impl Write) -> io::Result<()> {
        if !self.live {
            return Ok(());
        }

        let reset = match self.lanes_enabled {
            true => self.flush_lanes(out)?,
            false => self.flush_undivided(out)?,
        };
        self.resync = false;

        if reset || self.dynamic_current != self.dynamic_previous {
            out.write_all(&self.dynamic_current)?;
            std::mem::swap(&mut self.dynamic_current, &mut self.dynamic_previous);
        }

        Ok(())
    }

    /// Write every lane behind one reset when the scene changed anywhere, and
    /// return whether it did.
    fn flush_undivided(&mut self, out: &mut impl Write) -> io::Result<bool> {
        let changed = self.resync || concatenations_differ(&self.lanes);
        if changed {
            out.write_all(&command::encode_reset())?;
            for lane in &mut self.lanes {
                out.write_all(&lane.current)?;
                std::mem::swap(&mut lane.current, &mut lane.previous);
            }
        }

        Ok(changed)
    }

    /// Write each changed lane inside its markers, and return whether lane 0
    /// wrote a reset.
    ///
    /// Behind a reset every lane's baseline moves to its current bytes, the
    /// ones not sent included. The terminal then holds exactly those bytes,
    /// since the reset emptied every lane that sends nothing.
    fn flush_lanes(&mut self, out: &mut impl Write) -> io::Result<bool> {
        let base = &mut self.lanes[0];
        let reset = self.resync || base.current != base.previous;
        if reset {
            out.write_all(&command::encode_reset())?;
            out.write_all(&base.current)?;
            std::mem::swap(&mut base.current, &mut base.previous);
        }

        for lane in &mut self.lanes[1..] {
            let send = match reset {
                true => !lane.current.is_empty(),
                false => lane.current != lane.previous,
            };
            if send {
                out.write_all(&command::encode_lane_begin(lane.id))?;
                out.write_all(&lane.current)?;
                out.write_all(&command::encode_lane_end())?;
            }
            if send || reset {
                std::mem::swap(&mut lane.current, &mut lane.previous);
            }
        }

        Ok(reset)
    }
}

impl Default for ApcScene {
    fn default() -> ApcScene {
        ApcScene::new()
    }
}

/// One numbered decoration lane, with the bytes under construction and the
/// bytes the terminal holds for the lane as of the last flush.
struct Lane {
    id: u32,
    current: Vec<u8>,
    previous: Vec<u8>,
}

impl Lane {
    fn new(id: u32) -> Lane {
        Lane {
            id,
            current: Vec::new(),
            previous: Vec::new(),
        }
    }
}

/// Whether the lanes' current bytes, read end to end, differ from their
/// baselines read the same way.
///
/// Lanes that each equal their baseline settle it without a walk, which is
/// every frame that changed nothing.
fn concatenations_differ(lanes: &[Lane]) -> bool {
    if lanes.iter().all(|lane| lane.current == lane.previous) {
        return false;
    }

    let current = lanes.iter().flat_map(|lane| &lane.current);
    let previous = lanes.iter().flat_map(|lane| &lane.previous);
    !current.eq(previous)
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
        self, encode_border, encode_lane_begin, encode_lane_end, encode_line_layout, encode_reset,
        encode_scroll_region, BorderCommand, BorderStyle, LineLayoutCommand, PanelCommand,
        PanelShadow, ScrollRegionCommand,
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

    fn region() -> ScrollRegionCommand {
        ScrollRegionCommand {
            top: 1,
            left: 0,
            width: 20,
            height: 10,
            offset: 4,
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

    /// The caller branches on liveness, and one that gets it wrong must not be
    /// able to put half a frame on the wire. Both lanes swallow alike.
    #[test]
    fn a_dead_scene_swallows_what_is_pushed_into_it() {
        let mut scene = ApcScene::new();
        scene.set_live(false);

        command::encode_border_into(scene.buffer(), &border());
        scene.set_line_layout(&[1, 2, 3]);
        command::encode_scroll_region_into(scene.dynamic_buffer(), &region());

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
        command::encode_scroll_region_into(scene.dynamic_buffer(), &region());
        scene.flush_to(&mut Vec::new()).expect("vec write");

        scene.clear();
        command::encode_border_into(scene.buffer(), &border());
        command::encode_scroll_region_into(scene.dynamic_buffer(), &region());
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");

        assert!(out.is_empty(), "an unchanged scene emits nothing");
    }

    /// The baseline is a claim about what the terminal holds. Another program
    /// driving the same terminal breaks that claim, and an unchanged scene
    /// would otherwise put nothing back on the screen.
    #[test]
    fn forget_flushed_re_sends_an_unchanged_scene() {
        fn build(scene: &mut ApcScene) {
            scene.clear();
            command::encode_border_into(scene.buffer(), &border());
            command::encode_scroll_region_into(scene.dynamic_buffer(), &region());
        }

        let mut scene = ApcScene::new();
        build(&mut scene);
        scene.flush_to(&mut Vec::new()).expect("vec write");
        build(&mut scene);
        scene.flush_to(&mut Vec::new()).expect("vec write");

        scene.forget_flushed();
        build(&mut scene);
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");

        let mut expected = encode_reset();
        expected.extend(encode_border(&border()));
        expected.extend(encode_scroll_region(&region()));
        assert_eq!(out, expected, "both lanes go out again");
    }

    /// The whole point of the second lane. A reset in front of a `scroll_region`
    /// drops the region the terminal eases, so a lane-only change must reach the
    /// wire bare.
    #[test]
    fn a_dynamic_change_flushes_without_a_reset() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());
        command::encode_scroll_region_into(scene.dynamic_buffer(), &region());
        scene.flush_to(&mut Vec::new()).expect("vec write");

        scene.clear();
        command::encode_border_into(scene.buffer(), &border());
        let scrolled = ScrollRegionCommand {
            offset: 12,
            ..region()
        };
        command::encode_scroll_region_into(scene.dynamic_buffer(), &scrolled);
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");

        assert_eq!(out, encode_scroll_region(&scrolled));
    }

    /// A reset clears the region too, and the dynamic lane holds the same bytes
    /// as last frame, so nothing but this re-send puts the region back.
    #[test]
    fn a_decoration_change_re_sends_the_dynamic_lane() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());
        command::encode_scroll_region_into(scene.dynamic_buffer(), &region());
        scene.flush_to(&mut Vec::new()).expect("vec write");

        scene.clear();
        let moved = BorderCommand { top: 7, ..border() };
        command::encode_border_into(scene.buffer(), &moved);
        command::encode_scroll_region_into(scene.dynamic_buffer(), &region());
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");

        let mut expected = encode_reset();
        expected.extend(encode_border(&moved));
        expected.extend(encode_scroll_region(&region()));
        assert_eq!(out, expected, "and the region lands after the reset");
    }

    /// Append `border` to lane `id`, then return to lane 0.
    fn in_lane(scene: &mut ApcScene, id: u32, border: &BorderCommand) {
        scene.set_lane(id);
        command::encode_border_into(scene.buffer(), border);
        scene.end_lane();
    }

    /// A frame with `base` in lane 0, `first` in lane 1, `second` in lane 2,
    /// and the scroll region in the dynamic lane.
    fn build_lanes(
        scene: &mut ApcScene,
        base: &BorderCommand,
        first: &BorderCommand,
        second: Option<&BorderCommand>,
    ) {
        scene.clear();
        command::encode_border_into(scene.buffer(), base);
        in_lane(scene, 1, first);
        if let Some(second) = second {
            in_lane(scene, 2, second);
        }
        command::encode_scroll_region_into(scene.dynamic_buffer(), &region());
    }

    fn flushed(scene: &mut ApcScene) -> Vec<u8> {
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");
        out
    }

    #[test]
    fn a_changed_lane_flushes_alone_inside_its_markers() {
        let mut scene = ApcScene::new();
        scene.set_lane_protocol(true);
        build_lanes(&mut scene, &border(), &border(), Some(&border()));
        flushed(&mut scene);

        let moved = BorderCommand { top: 7, ..border() };
        scene.set_lane_protocol(true);
        build_lanes(&mut scene, &border(), &border(), Some(&moved));

        assert_eq!(
            flushed(&mut scene),
            [
                encode_lane_begin(2),
                encode_border(&moved),
                encode_lane_end()
            ]
            .concat(),
            "no reset, no other lane, and no dynamic re-send"
        );
    }

    #[test]
    fn an_emptied_lane_flushes_its_bare_markers() {
        let mut scene = ApcScene::new();
        scene.set_lane_protocol(true);
        build_lanes(&mut scene, &border(), &border(), Some(&border()));
        flushed(&mut scene);

        build_lanes(&mut scene, &border(), &border(), None);

        assert_eq!(
            flushed(&mut scene),
            [encode_lane_begin(2), encode_lane_end()].concat(),
            "the markers alone clear the lane the frame stopped declaring"
        );
    }

    /// A reset empties every lane in the terminal, so the lanes it did not
    /// touch have to go out again, and so does the dynamic lane.
    #[test]
    fn a_lane_zero_change_resets_and_re_sends_every_lane_with_content() {
        let mut scene = ApcScene::new();
        scene.set_lane_protocol(true);
        build_lanes(&mut scene, &border(), &border(), Some(&border()));
        flushed(&mut scene);

        let moved = BorderCommand { top: 7, ..border() };
        build_lanes(&mut scene, &moved, &border(), None);

        assert_eq!(
            flushed(&mut scene),
            [
                encode_reset(),
                encode_border(&moved),
                encode_lane_begin(1),
                encode_border(&border()),
                encode_lane_end(),
                encode_scroll_region(&region()),
            ]
            .concat(),
            "lane 1 goes out again, and the emptied lane 2 needs nothing behind the reset"
        );

        build_lanes(&mut scene, &moved, &border(), Some(&border()));
        assert_eq!(
            flushed(&mut scene),
            [
                encode_lane_begin(2),
                encode_border(&border()),
                encode_lane_end()
            ]
            .concat(),
            "the reset left lane 2 empty, so declaring it again sends it"
        );
    }

    #[test]
    fn without_the_lane_protocol_every_lane_flushes_behind_one_reset() {
        let moved = BorderCommand { top: 7, ..border() };
        let frame = |scene: &mut ApcScene, second: &BorderCommand| {
            scene.clear();
            in_lane(scene, 2, second);
            in_lane(scene, 1, &moved);
            command::encode_border_into(scene.buffer(), &border());
        };
        let mut scene = ApcScene::new();

        frame(&mut scene, &border());
        assert_eq!(
            flushed(&mut scene),
            [
                encode_reset(),
                encode_border(&border()),
                encode_border(&moved),
                encode_border(&border()),
            ]
            .concat(),
            "the lanes read end to end in id order, with no markers"
        );

        frame(&mut scene, &border());
        assert_eq!(flushed(&mut scene), Vec::<u8>::new(), "an unchanged scene");

        frame(&mut scene, &moved);
        assert_eq!(
            flushed(&mut scene),
            [
                encode_reset(),
                encode_border(&border()),
                encode_border(&moved),
                encode_border(&moved),
            ]
            .concat(),
            "a change in one lane sends them all"
        );

        scene.clear();
        command::encode_border_into(scene.buffer(), &border());
        command::encode_border_into(scene.buffer(), &moved);
        in_lane(&mut scene, 2, &moved);
        assert_eq!(
            flushed(&mut scene),
            Vec::<u8>::new(),
            "bytes that moved between lanes but read the same end to end"
        );
    }

    #[test]
    fn end_lane_and_clear_send_appends_back_to_lane_zero() {
        let moved = BorderCommand { top: 7, ..border() };
        let mut scene = ApcScene::new();
        scene.set_lane_protocol(true);
        in_lane(&mut scene, 1, &border());
        command::encode_border_into(scene.buffer(), &moved);
        assert_eq!(
            flushed(&mut scene),
            [
                encode_reset(),
                encode_border(&moved),
                encode_lane_begin(1),
                encode_border(&border()),
                encode_lane_end(),
            ]
            .concat(),
            "the append after end_lane lands in lane 0"
        );

        scene.set_lane(1);
        scene.clear();
        command::encode_border_into(scene.buffer(), &moved);
        assert_eq!(
            flushed(&mut scene),
            [encode_lane_begin(1), encode_lane_end()].concat(),
            "the append after clear lands in lane 0, which holds the same bytes"
        );
    }

    /// A flush without markers left lane 1's bytes in the terminal's lane 0,
    /// so lane 1 sent alone afterwards draws twice.
    #[test]
    fn switching_the_lane_protocol_resets_and_sends_the_whole_scene() {
        let mut scene = ApcScene::new();
        build_lanes(&mut scene, &border(), &border(), None);
        flushed(&mut scene);

        scene.set_lane_protocol(true);
        build_lanes(&mut scene, &border(), &border(), None);

        assert_eq!(
            flushed(&mut scene),
            [
                encode_reset(),
                encode_border(&border()),
                encode_lane_begin(1),
                encode_border(&border()),
                encode_lane_end(),
                encode_scroll_region(&region()),
            ]
            .concat(),
            "the unchanged scene goes out whole, lanes and all"
        );

        scene.set_lane_protocol(false);
        build_lanes(&mut scene, &border(), &border(), None);
        assert_eq!(
            flushed(&mut scene),
            [
                encode_reset(),
                encode_border(&border()),
                encode_border(&border()),
                encode_scroll_region(&region()),
            ]
            .concat(),
            "switching back sends it whole once more, without markers"
        );
    }

    /// The verdict arrives after the session has already painted, so a scene
    /// flushes live and then learns the host is foreign. Its lanes are empty by
    /// then, which a byte comparison alone reads as a scene to tear down.
    #[test]
    fn a_scene_that_goes_dead_after_a_flush_writes_nothing() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());
        command::encode_scroll_region_into(scene.dynamic_buffer(), &region());
        scene.flush_to(&mut Vec::new()).expect("vec write");

        scene.set_live(false);
        scene.clear();
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");

        assert!(out.is_empty(), "no reset reaches the foreign host");
    }

    /// What stoat's pane cache reads: a length taken before a pane paints, then
    /// a slice of what that pane appended.
    #[test]
    fn bytes_tracks_the_frame_under_construction() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());

        let mark = scene.bytes().len();
        let second = BorderCommand { top: 9, ..border() };
        command::encode_border_into(scene.buffer(), &second);

        assert_eq!(&scene.bytes()[mark..], encode_border(&second).as_slice());
    }

    #[test]
    fn lane_bytes_reads_a_lane_that_is_not_open() {
        let mut scene = ApcScene::new();
        in_lane(&mut scene, 2, &border());

        assert_eq!(
            (scene.lane_bytes(2), scene.lane_bytes(5)),
            (encode_border(&border()).as_slice(), &[][..]),
            "lane 2 holds its border, and a lane nothing opened holds nothing"
        );
    }

    #[test]
    fn all_bytes_reads_every_lane_in_id_order() {
        let mut scene = ApcScene::new();
        let moved = BorderCommand { top: 7, ..border() };
        in_lane(&mut scene, 2, &moved);
        command::encode_border_into(scene.buffer(), &border());

        assert_eq!(
            scene.all_bytes(),
            [encode_border(&border()), encode_border(&moved)].concat(),
            "lane 0 comes first, though lane 2 was written first"
        );
    }

    #[test]
    fn panels_reads_the_panels_of_every_lane() {
        let panel = |top| PanelCommand {
            top,
            left: 0,
            width: 6,
            height: 3,
            style: BorderStyle::Rounded,
            border: [1, 2, 3],
            corner_radius: 6,
            fill: None,
            shadow: PanelShadow::None_,
            inset_x: 0,
            above_pools: false,
            anchor: None,
        };
        let mut scene = ApcScene::new();
        scene.set_lane(2);
        command::encode_panel_into(scene.buffer(), &panel(5));
        scene.end_lane();
        command::encode_border_into(scene.buffer(), &border());
        command::encode_panel_into(scene.buffer(), &panel(1));

        assert_eq!(scene.panels().collect::<Vec<_>>(), [panel(1), panel(5)]);
    }

    /// The same mark and slice inside a lane, which is how a caller captures
    /// what one part of the frame wrote to the lane it opened.
    #[test]
    fn bytes_reads_the_open_lane() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());

        scene.set_lane(3);
        let mark = scene.bytes().len();
        let second = BorderCommand { top: 9, ..border() };
        command::encode_border_into(scene.buffer(), &second);

        assert_eq!(&scene.bytes()[mark..], encode_border(&second).as_slice());
    }

    /// The flush swaps its lane with the baseline rather than copying it, so
    /// what a reader finds afterwards is the frame before the flushed one.
    #[test]
    fn bytes_goes_stale_at_the_flush() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());
        scene.flush_to(&mut Vec::new()).expect("vec write");

        scene.clear();
        let second = BorderCommand { top: 9, ..border() };
        command::encode_border_into(scene.buffer(), &second);
        scene.flush_to(&mut Vec::new()).expect("vec write");

        assert_eq!(scene.bytes(), encode_border(&border()), "the prior frame");
        scene.clear();
        assert!(scene.bytes().is_empty(), "until the next clear");
    }

    #[test]
    fn clear_empties_both_lanes() {
        let mut scene = ApcScene::new();
        command::encode_border_into(scene.buffer(), &border());
        command::encode_scroll_region_into(scene.dynamic_buffer(), &region());

        scene.clear();
        let mut out = Vec::new();
        scene.flush_to(&mut out).expect("vec write");

        assert!(scene.bytes().is_empty(), "the decoration lane is empty");
        assert!(out.is_empty(), "and so is the dynamic one");
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
