//! Stoatty's APC sub-protocol: the `Gstoatty` frame grammar and the
//! typed command surface emitting programs link against to drive
//! renderer features.
//!
//! Kept dependency-light -- no GPU, windowing, or terminal-state deps --
//! so a program needs only this crate to emit stoatty bytes.
//!
//! `docs/stoatty-protocol.md` in the repository is the prose reference. It
//! covers the frame grammar, the command table with its
//! persistent-versus-decoration lanes, and the detection recipes, written for a
//! reader coming from outside this codebase.
//!
//! # What degrades, and what does not
//!
//! A frame degrades to an ignorable escape sequence in any other terminal,
//! since an APC string is consumed and never drawn. That covers every
//! command in [`command`].
//!
//! Streamed content does not degrade. A popover's text, a text run's
//! characters, and a page fill's cells travel outside the frame wrapper as
//! ordinary bytes, so a terminal that never opened the capture prints them
//! over whatever is on screen. Settle which terminal answers with
//! [`detect`] before you emit any of it.
//!
//! # Evolving a command
//!
//! A command grows only by appending. A new field goes at the end of its
//! fixed head, or arrives as an argument after the ones already defined.
//! Never reorder, resize, or repurpose what is already there.
//!
//! The rule exists because the two sides of a session are versioned
//! separately. A program may be newer than the terminal it is talking to,
//! over ssh or simply because one of them was updated first. Every decoder
//! reads the prefix it knows and ignores what follows, so a frame carrying
//! a field an older terminal has never heard of still delivers everything
//! that terminal does understand. An unknown enum code degrades to a member
//! it knows rather than dropping the command around it.
//!
//! Anything a decoder cannot express as an append needs a new sub-command
//! instead, which an older terminal ignores whole rather than misreading.

/// The revision of this protocol each side announces in the handshake.
///
/// A peer reports what it can render, so an emitter can hold back a command the
/// other end predates rather than sending bytes it will print raw. Bumped
/// whenever a command is added, an existing one grows a field, or the terminal
/// answers something it did not answer before.
///
/// Version 2 draws Kitty graphics images. That is a capability rather than a
/// command of this protocol, but an emitter still has to know: a client that
/// streams an image to a terminal that will not draw it has sent a great many
/// bytes for nothing.
///
/// Version 3 strokes the `sketch_ellipse`, `sketch_rect`, and `sketch_line`
/// marks, and reads the `follow` and anchor fields a text run appends. An
/// emitter talking to an older terminal falls back to whatever presentation it
/// had before the marks existed, since a terminal that predates them ignores
/// those frames whole.
///
/// Version 4 reads `minimap_runs`, whose runs carry the glyph weight under them
/// beside the palette class. An emitter talking to an older terminal sends
/// `minimap_lines` instead, whose runs are the flat blocks that terminal draws.
///
/// Version 5 reads `fill_decorations`, which replaces a buffered page's text
/// runs, bars, and polylines while its cells stay. An emitter talking to an
/// older terminal sends a whole `fill` instead. That terminal ignores a
/// `fill_decorations` marker and applies the runs after it to the live grid.
///
/// Version 6 strokes `sketch_path`, a hand-drawn curve through a list of
/// points. A terminal that predates it ignores those frames whole.
///
/// Version 7 strokes `sketch_elbow`, a hand-drawn polyline whose corners round.
/// A terminal that predates it ignores those frames whole.
///
/// Version 8 reads `pool_cursor_release`, which takes the cursor anchor off a
/// pool. A terminal that predates it ignores those frames whole, and keeps the
/// anchor until the pool is dropped.
///
/// Version 9 reads the kind byte a `pool_region` appends for a terminal pool,
/// and answers such a pool with a `pool_sized` line on the window socket. A
/// terminal that predates it reads the region's head alone, so an emitter
/// declares terminal pools only to a terminal at version 9 or later.
///
/// Version 10 paints `fill_rows`, which repaints some rows of a buffered page
/// and keeps the rest, and keeps a terminal pool's pages across a viewport
/// resize, so a patch lands on the page the program last filled. A terminal
/// that predates it paints no patch, and one that does not read the frame at
/// all lays the patch's rows on its live grid. An emitter therefore sends a
/// patch only to a terminal at version 10 or later.
///
/// Version 11 reads the dim byte a terminal pool's `pool_region` appends, and
/// fades the pool's composite toward the clear color by it. A re-declare that
/// changes only the dim keeps the pool's pages. A terminal that predates it
/// ignores the byte, so an emitter bakes the fade into the pages it sends to a
/// terminal before version 11.
///
/// Version 12 reads `lane_begin` and `lane_end`, which split the decorations
/// into numbered lanes. A re-declared lane replaces only its own decorations,
/// so an emitter re-sends the lane that changed rather than a reset and the
/// whole scene. A terminal that predates it ignores both markers, so an
/// emitter opens lanes only for a terminal at version 12 or later.
///
/// Version 13 reads a `minimap` palette of up to 256 entries, one per class a
/// run's `u8` names. A terminal that predates it drops a declaration past 64
/// entries whole, so an emitter declares at most 64 entries to it and names no
/// class past them.
///
/// Version 14 reads a fixed 19-byte `pool_region` head for every kind, which
/// ends in the kind byte, the dim, and a resident byte. A resident pool's pages
/// are the region's content at rest, so the terminal keeps the pool's
/// composite when a glide settles. A terminal that predates it reads the kind
/// and the dim it knows and ignores the resident byte. This version drops a
/// shorter head.
///
/// Version 15 reads border style code 4, which draws no line, in a `border` and
/// in a `panel`. A panel in it with no fill and no shadow draws nothing and
/// still covers its rect, so pane pool composites and the chrome declared before
/// it leave the rect to the live grid. A terminal that predates it reads the
/// code as a light line.
///
/// Version 16 reports a gesture pressed in a terminal pool as a `pool_mouse`
/// line on the window socket, which names the pool's own cell under the
/// pointer, and writes no SGR report for it. A terminal that predates it
/// reports the gesture in the grid's cells alone.
///
/// Zero is reserved for a peer whose handshake carries no version at all, which
/// is every build from before the field existed.
pub const PROTOCOL_VERSION: u32 = 16;

pub mod command;
pub mod detect;
pub mod frame;
pub mod iterm;
pub mod kitty;
pub mod window_ipc;
