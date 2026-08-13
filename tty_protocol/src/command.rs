//! The typed command surface: a parsed [`Frame`] dispatched by its namespaced
//! sub-command.
//!
//! [`decode`] is the terminal-facing entry point. It returns `None` for any
//! frame the terminal should ignore, whether malformed or carrying a
//! sub-command this build does not recognize, so an unsupported feature
//! degrades to nothing rather than erroring.
//!
//! Each command family owns a submodule holding its payload types, both halves
//! of its wire format, and its tests, so adding a command edits one file rather
//! than four distant regions of a shared one. Only what spans the families stays
//! here. That is the [`Command`] enum, the decode entry points, [`encode_into`],
//! and the sub-command dispatch.
//!
//! Every family name re-exports through here. A caller imports from
//! `command::` as it always has, so where a command lives is this module's
//! business rather than its callers'.

use crate::frame::{self, FrameScratch};
use bar::decode_bar;
use border::decode_border;
use icon::decode_icon;
use line_layout::decode_line_layout;
use minimap::{decode_minimap, decode_minimap_drop, decode_minimap_lines, decode_minimap_view};
use panel::decode_panel;
use polyline::decode_polyline;
use pool::{
    decode_fill, decode_pool_cursor, decode_pool_drop, decode_pool_region, decode_reposition,
    decode_scroll,
};
use popover::decode_popover;
use scale::decode_scale;
use scroll_region::decode_scroll_region;
use terminal_control::{decode_font_step, decode_hello, decode_zoom_capture};
use text_run::decode_text_run;
use window::{decode_window_close, decode_window_focus, decode_window_open};

pub mod bar;
pub mod border;
pub mod icon;
pub mod line_layout;
pub mod minimap;
pub mod panel;
pub mod polyline;
pub mod pool;
pub mod popover;
pub mod scale;
pub mod scroll_region;
pub mod terminal_control;
pub mod text_run;
pub mod window;

pub use bar::{encode_bar, encode_bar_into, BarCommand};
pub use border::{encode_border, encode_border_into, BorderCommand, BorderStyle};
pub use icon::{encode_icon, encode_icon_into, IconCommand, IconKind};
pub use line_layout::{encode_line_layout, encode_line_layout_into, LineLayoutCommand};
pub use minimap::{
    encode_minimap, encode_minimap_drop, encode_minimap_drop_into, encode_minimap_into,
    encode_minimap_lines, encode_minimap_lines_into, encode_minimap_view, encode_minimap_view_into,
    LineSummary, MinimapCommand, MinimapDropCommand, MinimapLinesCommand, MinimapRun,
    MinimapViewCommand,
};
pub use panel::{encode_panel, encode_panel_into, PanelCommand, PanelShadow};
pub use polyline::{encode_polyline, encode_polyline_into, PolylineCommand};
pub use pool::{
    encode_fill, encode_fill_end, encode_fill_end_into, encode_fill_into, encode_fill_scope,
    encode_pool_cursor, encode_pool_cursor_into, encode_pool_drop, encode_pool_drop_into,
    encode_pool_region, encode_pool_region_into, encode_reposition, encode_reposition_into,
    encode_scroll, encode_scroll_into, fill_batch_key, FillCommand, PoolCursorCommand,
    PoolDropCommand, PoolRegionCommand, RepositionCommand, ScrollCommand, NON_PANE_POOL_BASE,
};
pub use popover::{
    encode_popover, encode_popover_end, encode_popover_end_into, encode_popover_into,
    encode_popover_scope, PopoverCommand,
};
pub use scale::{encode_scale, encode_scale_into, ScaleCommand};
pub use scroll_region::{encode_scroll_region, encode_scroll_region_into, ScrollRegionCommand};
pub use terminal_control::{
    decode_ident_reply, encode_config_reload, encode_config_reload_into, encode_font_step,
    encode_font_step_into, encode_hello, encode_hello_into, encode_ident_reply,
    encode_ident_reply_into, encode_reset, encode_reset_into, encode_zoom_capture,
    encode_zoom_capture_into, HelloCommand, IdentReply,
};
pub use text_run::{
    encode_text_run, encode_text_run_end, encode_text_run_end_into, encode_text_run_into,
    encode_text_run_scope, TextRunCommand,
};
pub use window::{
    encode_window_close, encode_window_close_into, encode_window_focus, encode_window_focus_into,
    encode_window_open, encode_window_open_into, WindowCloseCommand, WindowFocusCommand,
    WindowOpenCommand,
};

/// A decoded stoatty command.
///
/// The enum is intentionally exhaustive: adding a variant forces every matcher,
/// including the terminal's apply seam, to handle it rather than silently
/// dropping the new command.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    Border(BorderCommand),
    Panel(PanelCommand),
    Scale(ScaleCommand),
    /// Open a popover whose content text streams as the bytes that follow,
    /// until [`Command::PopoverEnd`] commits it. The fixed head (region, colors,
    /// scale, offset) rides on this marker; the content is captured off-frame so
    /// it is not bounded by the frame-size cap.
    Popover(PopoverCommand),
    /// Close the popover opened by [`Command::Popover`], committing the streamed
    /// content into its [`PopoverCommand::content`]. Carries no payload.
    PopoverEnd,
    ScrollRegion(ScrollRegionCommand),
    PoolRegion(PoolRegionCommand),
    Icon(IconCommand),
    /// Open a text run whose text streams as the bytes that follow, until
    /// [`Command::TextRunEnd`] commits it. The fixed head (position, scale,
    /// colors) rides on this marker; the text is captured off-frame so it is not
    /// bounded by the frame-size cap.
    TextRun(TextRunCommand),
    /// Close the text run opened by [`Command::TextRun`], committing the streamed
    /// text into its [`TextRunCommand::text`]. Carries no payload.
    TextRunEnd,
    Bar(BarCommand),
    Polyline(PolylineCommand),
    LineLayout(LineLayoutCommand),
    /// Open the page-fill redirect onto a recycled pool slot.
    ///
    /// The streamed bytes that follow paint the page named by
    /// [`FillCommand::index`] instead of the live grid, until [`Command::FillEnd`]
    /// (or the next `fill`/`reset`) commits the slot and restores the live grid.
    Fill(FillCommand),
    /// Close the page-fill redirect opened by [`Command::Fill`].
    ///
    /// Commits the page painted since the open marker onto its pool slot and
    /// restores the live grid as the write target. Carries no payload.
    FillEnd,
    /// Set the smooth-scroll target to an app-declared document offset.
    ///
    /// The renderer eases the live scroll offset toward [`ScrollCommand`]'s
    /// page-plus-fraction position over subsequent frames, so the program
    /// reports where it wants the viewport and the terminal owns the animation.
    Scroll(ScrollCommand),
    /// Anchor the primary cursor to the content while a pool glides.
    ///
    /// Carries the cursor's document row and grid column for pool
    /// [`PoolCursorCommand::pool`], so the terminal draws the cursor riding the
    /// eased scroll offset instead of easing it toward its last VT cell. Sent
    /// once per glide tick alongside the pool's [`Command::Scroll`] frame.
    PoolCursor(PoolCursorCommand),
    /// Jump the smooth-scroll target to a document page across an unbuffered gap.
    ///
    /// Re-anchors the live offset to a local neighbour of [`RepositionCommand`]'s
    /// page and lands softly on it, for a jump too far to ease across the pool
    /// window. Pair with the `fill`s that buffer the destination neighbourhood.
    Reposition(RepositionCommand),
    /// Retire a smooth-scroll pool, freeing the pages it buffered.
    ///
    /// Sent when the surface backing pool [`PoolDropCommand::pool`] goes away (a
    /// closed pane, a dismissed modal), so the terminal frees its region and page
    /// buffer rather than holding them for a pool that will never scroll again. A
    /// later [`Command::PoolRegion`] with the same id starts a fresh pool.
    PoolDrop(PoolDropCommand),
    /// Declare a minimap strip, a right-edge region rendering a whole buffer as
    /// colored per-line run blocks with a viewport thumb.
    ///
    /// A per-frame decoration cleared by [`Command::Reset`], like a border or a
    /// bar. The line run summaries it renders live in a persistent content store
    /// keyed by [`MinimapCommand::content_id`] and updated out of band by
    /// [`Command::MinimapLines`], so a redeclared strip keeps its content.
    Minimap(MinimapCommand),
    /// Splice per-line run summaries into a minimap content store.
    ///
    /// Persistent, unlike the [`Command::Minimap`] declaration: it survives
    /// [`Command::Reset`] and is retired only by [`Command::MinimapDrop`], so the
    /// program streams incremental line updates without resending the whole file.
    MinimapLines(MinimapLinesCommand),
    /// Update a minimap strip's viewport thumb position.
    ///
    /// Persistent state carrying only where the thumb sits, so a scroll moves the
    /// thumb with a small frame rather than re-declaring the strip.
    MinimapView(MinimapViewCommand),
    /// Retire a minimap content store, freeing the line summaries it held.
    ///
    /// Sent when the buffer backing [`MinimapDropCommand::content_id`] closes, so
    /// the terminal need not hold summaries for a minimap that will never render
    /// again.
    MinimapDrop(MinimapDropCommand),
    /// Open an aux OS window as a second render target for window-bound pools.
    WindowOpen(WindowOpenCommand),
    /// Close an aux OS window and free its render target.
    WindowClose(WindowCloseCommand),
    /// Raise and OS-focus an aux window.
    WindowFocus(WindowFocusCommand),
    /// Clear all accumulated stoatty decoration state, so the program can redraw
    /// its scene from scratch. Carries no payload.
    Reset,
    /// The terminal's own config file changed on disk, so it should re-read and
    /// re-apply it. Carries no payload: the terminal reads the file itself, and
    /// the program that sends this is only reporting that it wrote it.
    ConfigReload,
    /// The program claims the platform zoom combo for its session, or releases
    /// it.
    ///
    /// While claimed the terminal forwards each press upstream instead of
    /// stepping its own font size, so the program can make the combo mean
    /// whatever its current context calls for. A program that never sends this
    /// leaves the terminal's native font zoom exactly as it was, which is what
    /// keeps a plain shell child working and what a crashed program degrades
    /// back to.
    ZoomCapture {
        on: bool,
    },
    /// Step the terminal's font size by `delta`, positive to grow.
    ///
    /// The counterpart to claiming the combo. A program that took the combo
    /// away from font zoom uses this to offer font zoom back through its own
    /// commands.
    FontStep {
        delta: i32,
    },
    /// A handshake the program sends to identify itself to the terminal, so the
    /// terminal's log records which process drives it. The terminal replies with
    /// its own [`IdentReply`].
    Hello(HelloCommand),
}

/// Decode a stoatty APC frame into a typed [`Command`], or `None` to ignore it.
///
/// `None` covers both a malformed frame and a well-formed one whose
/// sub-command is unknown to this build. Ignoring rather than erroring is what
/// lets the same byte stream degrade to nothing in another terminal.
pub fn decode(bytes: &[u8]) -> Option<Command> {
    let mut scratch = FrameScratch::default();
    decode_with(bytes, &mut scratch)
}

/// Decode a stoatty command, reusing `scratch`'s argument buffers.
///
/// Behaves like [`decode`] but takes a caller-owned [`FrameScratch`] so a hot
/// decode loop reuses the per-argument buffers across frames instead of
/// allocating fresh ones each call.
pub fn decode_with(bytes: &[u8], scratch: &mut FrameScratch) -> Option<Command> {
    let (sub, args) = frame::decode_into(bytes, scratch)?;
    dispatch(sub, args)
}

/// Decode every stoatty frame in an emitted byte stream, in emission order.
///
/// This is what an emitter asserts its own output with. [`decode`] reads one
/// frame, and a program that emits a scene produces a run of them mixed with
/// content that rides outside the wrapper, so reading a scene back needs a walk
/// rather than a parse.
///
/// [`Command::TextRun`] and [`Command::Popover`] stream their text between the
/// open frame and the matching `_end`, so [`decode`] alone yields them empty.
/// This stitches those bytes back onto the command from the gap that follows
/// the open frame. The gap ends at the next frame introducer, not at the first
/// `ESC`, so a `fill` batch works too: a page's VT carries its own escape
/// sequences, and the walk steps over them to the `fill_end` rather than
/// stopping mid-page.
///
/// Anything between frames that no capture claims is skipped, as is a frame
/// this build does not recognize and one that is malformed. A trailing frame
/// with no terminator ends the walk, since nothing later in the stream belongs
/// to a frame that never closed.
pub fn decode_stream(bytes: &[u8]) -> Vec<Command> {
    let mut out = Vec::new();
    let mut scratch = FrameScratch::default();
    let mut rest = bytes;

    while let Some(span) = frame::apc_span(rest) {
        let decoded = decode_with(&rest[span.clone()], &mut scratch);
        rest = &rest[span.end..];

        match decoded {
            Some(Command::TextRun(mut command)) => {
                command.text = take_streamed(&mut rest);
                out.push(Command::TextRun(command));
            },
            Some(Command::Popover(mut command)) => {
                command.content = take_streamed(&mut rest);
                out.push(Command::Popover(command));
            },
            Some(command) => out.push(command),
            None => {},
        }
    }

    out
}

/// Split the streamed content off the front of `rest`, leaving it at the next
/// frame.
///
/// Lossy because a capture is plain text by contract, so bytes that are not
/// valid UTF-8 mean the emitter broke that contract. Reading them as
/// replacement characters still shows the caller what arrived. Dropping the
/// whole command on a decode failure shows nothing at all.
fn take_streamed(rest: &mut &[u8]) -> String {
    let end = frame::apc_span(rest).map_or(rest.len(), |span| span.start);
    let content = String::from_utf8_lossy(&rest[..end]).into_owned();
    *rest = &rest[end..];
    content
}

/// Append the full `Gstoatty` frame for any [`Command`] to `out` without
/// allocating, dispatching on the variant.
///
/// The encode-side mirror of [`decode`]: an emitter assembling a scene appends
/// each command into one reused buffer.
pub fn encode_into(out: &mut Vec<u8>, command: &Command) {
    match command {
        Command::Border(c) => encode_border_into(out, c),
        Command::Panel(c) => encode_panel_into(out, c),
        Command::Scale(c) => encode_scale_into(out, c),
        Command::Popover(c) => encode_popover_into(out, c),
        Command::PopoverEnd => encode_popover_end_into(out),
        Command::ScrollRegion(c) => encode_scroll_region_into(out, c),
        Command::PoolRegion(c) => encode_pool_region_into(out, c),
        Command::Icon(c) => encode_icon_into(out, c),
        Command::TextRun(c) => encode_text_run_into(out, c),
        Command::TextRunEnd => encode_text_run_end_into(out),
        Command::Bar(c) => encode_bar_into(out, c),
        Command::Polyline(c) => encode_polyline_into(out, c),
        Command::LineLayout(c) => encode_line_layout_into(out, &c.heights),
        Command::Fill(c) => encode_fill_into(out, c.pool, c.index),
        Command::FillEnd => encode_fill_end_into(out),
        Command::Scroll(c) => encode_scroll_into(out, c),
        Command::PoolCursor(c) => encode_pool_cursor_into(out, c),
        Command::Reposition(c) => encode_reposition_into(out, c.pool, c.page),
        Command::PoolDrop(c) => encode_pool_drop_into(out, c.pool),
        Command::Minimap(c) => encode_minimap_into(out, c),
        Command::MinimapLines(c) => encode_minimap_lines_into(out, c),
        Command::MinimapView(c) => encode_minimap_view_into(out, c),
        Command::MinimapDrop(c) => encode_minimap_drop_into(out, c),
        Command::WindowOpen(c) => encode_window_open_into(out, c),
        Command::WindowClose(c) => encode_window_close_into(out, c),
        Command::WindowFocus(c) => encode_window_focus_into(out, c),
        Command::Reset => encode_reset_into(out),
        Command::ConfigReload => encode_config_reload_into(out),
        Command::ZoomCapture { on } => encode_zoom_capture_into(out, *on),
        Command::FontStep { delta } => encode_font_step_into(out, *delta),
        Command::Hello(c) => encode_hello_into(out, c),
    }
}

/// Map a sub-command name and its decoded arguments to a [`Command`].
///
/// An unknown sub-command, or a known one whose payload does not parse, yields
/// `None` so the frame is ignored.
fn dispatch(sub: &str, args: &[Vec<u8>]) -> Option<Command> {
    match sub {
        "border" => decode_border(args).map(Command::Border),
        "panel" => decode_panel(args).map(Command::Panel),
        "scale" => decode_scale(args).map(Command::Scale),
        "popover" => decode_popover(args).map(Command::Popover),
        "popover_end" => Some(Command::PopoverEnd),
        "scroll_region" => decode_scroll_region(args).map(Command::ScrollRegion),
        "pool_region" => decode_pool_region(args).map(Command::PoolRegion),
        "icon" => decode_icon(args).map(Command::Icon),
        "text_run" => decode_text_run(args).map(Command::TextRun),
        "text_run_end" => Some(Command::TextRunEnd),
        "bar" => decode_bar(args).map(Command::Bar),
        "polyline" => decode_polyline(args).map(Command::Polyline),
        "line_layout" => decode_line_layout(args).map(Command::LineLayout),
        "fill" => decode_fill(args).map(Command::Fill),
        "fill_end" => Some(Command::FillEnd),
        "scroll" => decode_scroll(args).map(Command::Scroll),
        "pool_cursor" => decode_pool_cursor(args).map(Command::PoolCursor),
        "reposition" => decode_reposition(args).map(Command::Reposition),
        "pool_drop" => decode_pool_drop(args).map(Command::PoolDrop),
        "minimap" => decode_minimap(args).map(Command::Minimap),
        "minimap_lines" => decode_minimap_lines(args).map(Command::MinimapLines),
        "minimap_view" => decode_minimap_view(args).map(Command::MinimapView),
        "minimap_drop" => decode_minimap_drop(args).map(Command::MinimapDrop),
        "window_open" => decode_window_open(args).map(Command::WindowOpen),
        "window_close" => decode_window_close(args).map(Command::WindowClose),
        "window_focus" => decode_window_focus(args).map(Command::WindowFocus),
        "reset" => Some(Command::Reset),
        "config_reload" => Some(Command::ConfigReload),
        "zoom_capture" => decode_zoom_capture(args),
        "font_step" => decode_font_step(args),
        "hello" => decode_hello(args).map(Command::Hello),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{border::decode_style, icon::decode_icon_kind, *};

    /// The frame an encoder emits today with bytes appended to its head
    /// argument, the shape a later version's added field arrives in.
    fn with_grown_head(encoded: &[u8]) -> Vec<u8> {
        let mut frame = frame::decode(encoded).expect("the encoder emits a valid frame");
        frame
            .args
            .first_mut()
            .expect("a head argument")
            .extend_from_slice(&[0xab, 0xcd]);
        frame::encode(&frame)
    }

    /// The frame with one argument more than it carries today, the shape a later
    /// version arrives in when it appends a whole field rather than widening the
    /// head.
    fn with_extra_arg(encoded: &[u8]) -> Vec<u8> {
        let mut frame = frame::decode(encoded).expect("the encoder emits a valid frame");
        frame.args.push(b"later".to_vec());
        frame::encode(&frame)
    }

    /// Just the opening frame of a streamed command's encoding, which is the
    /// only part a whole-frame decode can read.
    fn open_marker(encoded: &[u8]) -> Vec<u8> {
        let end = frame::first_frame_end(encoded).expect("the encoding opens with a frame");
        encoded[..end].to_vec()
    }

    /// One command per fixed-head family, enough to encode and grow.
    fn fixed_head_samples() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            (
                "border",
                encode_border(&BorderCommand {
                    top: 1,
                    left: 2,
                    width: 3,
                    height: 4,
                    style: BorderStyle::Heavy,
                    color: [5, 6, 7],
                }),
            ),
            (
                "scale",
                encode_scale(&ScaleCommand {
                    top: 1,
                    left: 2,
                    scale: 3,
                }),
            ),
            (
                "scroll_region",
                encode_scroll_region(&ScrollRegionCommand {
                    top: 1,
                    left: 2,
                    width: 3,
                    height: 4,
                    offset: 5,
                }),
            ),
            (
                "pool_region",
                encode_pool_region(&PoolRegionCommand {
                    pool: 1,
                    top: 2,
                    left: 3,
                    width: 4,
                    height: 5,
                    window: 6,
                }),
            ),
            (
                "window_close",
                encode_window_close(&WindowCloseCommand { window: 1 }),
            ),
            (
                "window_focus",
                encode_window_focus(&WindowFocusCommand { window: 1 }),
            ),
            (
                "bar",
                encode_bar(&BarCommand {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                    color: [5, 6, 7],
                }),
            ),
            ("fill", encode_fill(&FillCommand { pool: 1, index: 2 })),
            (
                "scroll",
                encode_scroll(&ScrollCommand {
                    pool: 1,
                    page: 2,
                    fraction: 3,
                }),
            ),
            (
                "pool_cursor",
                encode_pool_cursor(&PoolCursorCommand {
                    pool: 1,
                    row: 2,
                    col: 3,
                }),
            ),
            (
                "reposition",
                encode_reposition(&RepositionCommand { pool: 1, page: 2 }),
            ),
            ("pool_drop", encode_pool_drop(&PoolDropCommand { pool: 1 })),
            (
                "minimap_view",
                encode_minimap_view(&MinimapViewCommand {
                    strip_id: 1,
                    top_256: 2,
                    visible_lines: 3,
                }),
            ),
            (
                "minimap_drop",
                encode_minimap_drop(&MinimapDropCommand { content_id: 1 }),
            ),
            // A popover's encoding is an open marker, its streamed content, and
            // a close marker, so only the opening frame is a decodable one.
            (
                "popover",
                open_marker(&encode_popover(&PopoverCommand {
                    top: 1,
                    left: 2,
                    width: 3,
                    height: 4,
                    fill: [5, 6, 7],
                    border: [8, 9, 10],
                    content_fg: [11, 12, 13],
                    scale: 1,
                    offset: [0, 0],
                    bold: false,
                    content: String::new(),
                })),
            ),
            (
                "minimap",
                encode_minimap(&MinimapCommand {
                    top: 1,
                    left: 2,
                    width: 3,
                    height: 4,
                    strip_id: 5,
                    content_id: 6,
                    lines_per_cell: 7,
                    max_columns: 8,
                    bg: [9, 10, 11, 12],
                    thumb: [13, 14, 15, 16],
                    thumb_border: [17, 18, 19],
                    palette: vec![[20, 21, 22]],
                }),
            ),
        ]
    }

    /// A field appended to a command's head must not make an older terminal drop
    /// the whole frame. Tolerance is only useful if it ships before the field,
    /// so this pins it now rather than when the first one grows.
    #[test]
    fn fixed_head_decoders_tolerate_a_later_versions_appended_field() {
        for (name, encoded) in fixed_head_samples() {
            let plain = decode(&encoded);
            assert!(plain.is_some(), "{name} decodes as emitted today");
            assert_eq!(
                decode(&with_grown_head(&encoded)),
                plain,
                "{name} reads the same command with bytes appended to its head"
            );
        }
    }

    /// The same guarantee for the commands that count arguments rather than
    /// measure a head, where a later version appends a whole argument.
    #[test]
    fn arg_counted_decoders_tolerate_a_later_versions_extra_argument() {
        let samples = [
            ("zoom_capture", encode_zoom_capture(true)),
            ("font_step", encode_font_step(-2)),
            (
                "window_open",
                encode_window_open(&WindowOpenCommand {
                    window: 1,
                    cols: 2,
                    rows: 3,
                    title: "t".to_owned(),
                }),
            ),
            (
                "hello",
                encode_hello(&HelloCommand {
                    pid: 1,
                    log_id: "l".to_owned(),
                    hostname: "h".to_owned(),
                    version: "v".to_owned(),
                    protocol: crate::PROTOCOL_VERSION,
                }),
            ),
        ];

        for (name, encoded) in samples {
            let plain = decode(&encoded);
            assert!(plain.is_some(), "{name} decodes as emitted today");
            assert_eq!(
                decode(&with_extra_arg(&encoded)),
                plain,
                "{name} reads the same command with an argument appended"
            );
        }
    }

    /// A code added to either enum later must degrade to a member an older
    /// terminal knows rather than taking the whole command down with it.
    #[test]
    fn unknown_enum_codes_fall_back_instead_of_dropping_the_command() {
        assert_eq!(decode_style(9), BorderStyle::Light);
        assert_eq!(decode_icon_kind(9), IconKind::Info);

        let mut border = encode_border(&BorderCommand {
            top: 1,
            left: 2,
            width: 3,
            height: 4,
            style: BorderStyle::Heavy,
            color: [5, 6, 7],
        });
        // The style byte sits at offset 8 of the head, so rewrite it through the
        // frame rather than guessing where base64 put it.
        let mut frame = frame::decode(&border).expect("a valid frame");
        frame.args[0][8] = 9;
        border = frame::encode(&frame);

        assert!(
            matches!(decode(&border), Some(Command::Border(command)) if command.style == BorderStyle::Light),
            "an unknown style still draws a border"
        );
    }

    fn stream_border(top: u16) -> BorderCommand {
        BorderCommand {
            top,
            left: 2,
            width: 3,
            height: 4,
            style: BorderStyle::Light,
            color: [1, 2, 3],
        }
    }

    #[test]
    fn decode_stream_reads_frames_in_emission_order() {
        let mut bytes = encode_border(&stream_border(1));
        bytes.extend(encode_reset());
        bytes.extend(encode_border(&stream_border(9)));

        assert_eq!(
            decode_stream(&bytes),
            vec![
                Command::Border(stream_border(1)),
                Command::Reset,
                Command::Border(stream_border(9)),
            ]
        );
    }

    /// The reason the walk exists. A popover's content rides between its open
    /// and close markers rather than inside a frame, so decoding the open frame
    /// alone hands back an empty string.
    #[test]
    fn decode_stream_stitches_popover_content() {
        let command = PopoverCommand {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            fill: [10, 20, 30],
            border: [40, 50, 60],
            content_fg: [70, 80, 90],
            scale: 2,
            offset: [4, -2],
            bold: false,
            content: "two\nlines".to_owned(),
        };

        assert_eq!(
            decode_stream(&encode_popover(&command)),
            vec![Command::Popover(command), Command::PopoverEnd]
        );
    }

    #[test]
    fn decode_stream_stitches_text_run_text() {
        let command = TextRunCommand {
            col: -3,
            row: 8,
            scale: 160,
            color: [1, 2, 3],
            bg: Some([4, 5, 6]),
            text: "src/main.rs".to_owned(),
        };

        assert_eq!(
            decode_stream(&encode_text_run(&command)),
            vec![Command::TextRun(command), Command::TextRunEnd]
        );
    }

    #[test]
    fn decode_stream_reads_a_bel_terminated_frame() {
        let mut bytes = encode_border(&stream_border(1));
        bytes.truncate(bytes.len() - 2);
        bytes.push(0x07);
        bytes.extend(encode_reset());

        assert_eq!(
            decode_stream(&bytes),
            vec![Command::Border(stream_border(1)), Command::Reset]
        );
    }

    /// A page's own escape sequences are not frames. Content cut at the first
    /// ESC rather than at the next introducer stops the walk inside the page and
    /// loses every command after it.
    #[test]
    fn decode_stream_steps_over_a_filled_page() {
        let fill = FillCommand { pool: 2, index: 7 };
        let mut bytes = encode_fill(&fill);
        bytes.extend_from_slice(b"\x1b[H\x1b[38;2;1;2;3mpage text\x1b[0m");
        bytes.extend(encode_fill_end());
        bytes.extend(encode_reset());

        assert_eq!(
            decode_stream(&bytes),
            vec![Command::Fill(fill), Command::FillEnd, Command::Reset]
        );
    }

    #[test]
    fn decode_stream_skips_what_it_cannot_read() {
        let mut bytes = encode_border(&stream_border(1));
        bytes.extend_from_slice(b"\x1b_Gkitty;border\x1b\\");
        bytes.extend_from_slice(b"\x1b_Gstoatty;not_a_command\x1b\\");
        bytes.extend(encode_reset());

        assert_eq!(
            decode_stream(&bytes),
            vec![Command::Border(stream_border(1)), Command::Reset],
            "a foreign or unknown frame drops out without ending the walk"
        );
    }

    /// A batch cut mid-frame carries no complete command past the cut. A guess
    /// at one reports a command the emitter never finished.
    #[test]
    fn decode_stream_stops_at_an_unterminated_frame() {
        let mut bytes = encode_border(&stream_border(1));
        let truncated = encode_reset();
        bytes.extend_from_slice(&truncated[..truncated.len() - 2]);

        assert_eq!(
            decode_stream(&bytes),
            vec![Command::Border(stream_border(1))]
        );
    }

    #[test]
    fn a_scope_that_writes_nothing_still_closes() {
        let mut fill = Vec::new();
        encode_fill_scope(&mut fill, 1, 0, |_| {});
        assert_eq!(
            decode_stream(&fill),
            vec![
                Command::Fill(FillCommand { pool: 1, index: 0 }),
                Command::FillEnd
            ]
        );

        let mut run = Vec::new();
        encode_text_run_scope(
            &mut run,
            &TextRunCommand {
                col: 0,
                row: 0,
                scale: 16,
                color: [1, 2, 3],
                bg: None,
                text: (),
            },
            |_| {},
        );
        assert_eq!(
            decode_stream(&run),
            vec![
                Command::TextRun(TextRunCommand {
                    col: 0,
                    row: 0,
                    scale: 16,
                    color: [1, 2, 3],
                    bg: None,
                    text: String::new(),
                }),
                Command::TextRunEnd
            ]
        );
    }

    /// The borrowed-string form is the scope with a one-line closure, so the two
    /// must stay byte-identical or one of them emits a different frame.
    #[test]
    fn the_scope_and_borrowed_forms_emit_the_same_bytes() {
        let run = TextRunCommand {
            col: -3,
            row: 8,
            scale: 160,
            color: [1, 2, 3],
            bg: Some([4, 5, 6]),
            text: "src/main.rs".to_owned(),
        };
        let mut scoped = Vec::new();
        encode_text_run_scope(&mut scoped, &run, |out| {
            out.extend_from_slice(run.text.as_bytes())
        });
        assert_eq!(scoped, encode_text_run(&run), "text_run");

        let popover = PopoverCommand {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            fill: [10, 20, 30],
            border: [40, 50, 60],
            content_fg: [70, 80, 90],
            scale: 2,
            offset: [4, -2],
            bold: true,
            content: "hover text".to_owned(),
        };
        let mut scoped = Vec::new();
        encode_popover_scope(&mut scoped, &popover, |out| {
            out.extend_from_slice(popover.content.as_bytes())
        });
        assert_eq!(scoped, encode_popover(&popover), "popover");
    }

    fn popover_holding<S>(content: S) -> PopoverCommand<S> {
        PopoverCommand {
            top: 1,
            left: 2,
            width: 4,
            height: 3,
            fill: [10, 20, 30],
            border: [40, 50, 60],
            content_fg: [70, 80, 90],
            scale: 2,
            offset: [4, -2],
            bold: true,
            content,
        }
    }

    fn run_holding<S>(text: S) -> TextRunCommand<S> {
        TextRunCommand {
            col: -3,
            row: 8,
            scale: 160,
            color: [1, 2, 3],
            bg: Some([4, 5, 6]),
            text,
        }
    }

    /// What the container generic is for. An emitter re-declaring a popover
    /// every frame hands a slice rather than building a `String`, so the two
    /// containers have to reach the wire as the same command.
    #[test]
    fn a_borrowed_container_encodes_like_an_owned_one() {
        assert_eq!(
            encode_popover(&popover_holding("hover text")),
            encode_popover(&popover_holding("hover text".to_owned())),
            "popover",
        );
        assert_eq!(
            encode_text_run(&run_holding("src/main.rs")),
            encode_text_run(&run_holding("src/main.rs".to_owned())),
            "text_run",
        );
    }

    #[test]
    fn ignores_unknown_subcommand() {
        assert!(decode(b"Gstoatty;nope").is_none());
    }

    #[test]
    fn ignores_malformed_frame() {
        assert!(decode(b"garbage").is_none());
    }

    #[test]
    fn encode_into_round_trips_every_variant() {
        let commands = [
            Command::Border(BorderCommand {
                top: 1,
                left: 2,
                width: 3,
                height: 4,
                style: BorderStyle::Double,
                color: [9, 8, 7],
            }),
            Command::Scale(ScaleCommand {
                top: 5,
                left: 6,
                scale: 3,
            }),
            // Popover is a multi-frame open/content/close construct, so it does
            // not round-trip through a single-frame `decode`; its head and
            // streamed content are covered by the tty_term popover tests. Its
            // close marker is single-frame and covered here.
            Command::PopoverEnd,
            Command::ScrollRegion(ScrollRegionCommand {
                top: 2,
                left: 3,
                width: 8,
                height: 9,
                offset: 12,
            }),
            Command::Icon(IconCommand {
                top: 4,
                left: 5,
                kind: IconKind::Warning,
                color: [1, 2, 3],
                size: 2,
                offset: [0, 0],
            }),
            // TextRun is a multi-frame open/text/close construct, so it does not
            // round-trip through a single-frame `decode`; its head and streamed
            // text are covered by the tty_term text_run tests. Its close marker is
            // single-frame and covered here.
            Command::TextRunEnd,
            Command::Bar(BarCommand {
                x: -4,
                y: 8,
                width: 6,
                height: 16,
                color: [200, 100, 50],
            }),
            Command::LineLayout(LineLayoutCommand {
                heights: vec![1, 2, 3, 1],
            }),
            Command::Fill(FillCommand { pool: 1, index: 7 }),
            Command::FillEnd,
            Command::Scroll(ScrollCommand {
                pool: 2,
                page: 12,
                fraction: 30_000,
            }),
            Command::PoolCursor(PoolCursorCommand {
                pool: 2,
                row: 12,
                col: 30_000,
            }),
            Command::Reposition(RepositionCommand {
                pool: 3,
                page: 1_000,
            }),
            Command::PoolDrop(PoolDropCommand { pool: 4 }),
            Command::Minimap(MinimapCommand {
                top: 1,
                left: 72,
                width: 8,
                height: 40,
                strip_id: 5,
                content_id: 9,
                lines_per_cell: 8,
                max_columns: 120,
                bg: [10, 20, 30, 0],
                thumb: [200, 200, 200, 48],
                thumb_border: [255, 255, 255],
                palette: vec![[0, 0, 0], [1, 2, 3], [4, 5, 6]],
            }),
            Command::MinimapLines(MinimapLinesCommand {
                content_id: 9,
                start: 2,
                removed: 1,
                lines: vec![
                    vec![MinimapRun {
                        start_col: 0,
                        len: 4,
                        class: 2,
                    }]
                    .into(),
                    Vec::new().into(),
                ],
            }),
            Command::MinimapView(MinimapViewCommand {
                strip_id: 5,
                top_256: 1_280,
                visible_lines: 40,
            }),
            Command::MinimapDrop(MinimapDropCommand { content_id: 9 }),
            Command::Reset,
        ];

        for command in commands {
            let mut out = Vec::new();
            encode_into(&mut out, &command);
            assert_eq!(decode(&out), Some(command));
        }
    }

    #[test]
    fn encode_into_appends_each_frame() {
        let border = BorderCommand {
            top: 0,
            left: 0,
            width: 2,
            height: 2,
            style: BorderStyle::Light,
            color: [1, 1, 1],
        };
        let bar = BarCommand {
            x: 1,
            y: 1,
            width: 4,
            height: 8,
            color: [2, 2, 2],
        };

        let mut out = Vec::new();
        encode_into(&mut out, &Command::Border(border));
        encode_into(&mut out, &Command::Bar(bar));

        let mut expected = encode_border(&border);
        expected.extend(encode_bar(&bar));
        assert_eq!(out, expected);
    }
}
