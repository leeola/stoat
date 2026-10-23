pub(crate) mod badges;
pub(crate) mod chrome;
pub(crate) mod code_action;
pub(crate) mod code_search;
pub(crate) mod command_palette;
pub(crate) mod commit_message;
pub(crate) mod commit_picker;
pub(crate) mod commits;
pub(crate) mod completion;
pub(crate) mod conflict;
pub(crate) mod conflict_view;
pub(crate) mod cursor_popup;
pub(crate) mod diagnostics_picker;
pub(crate) mod diff_changes;
pub(crate) mod dock;
pub(crate) mod editor;
pub(crate) mod file_finder;
pub(crate) mod help;
pub(crate) mod hints;
pub(crate) mod hover;
pub(crate) mod jumplist_picker;
pub(crate) mod layout;
pub(crate) mod location_picker;
pub(crate) mod paint;
pub(crate) mod pane;
pub(crate) mod pane_cache;
pub(crate) mod picker;
pub(crate) mod popout;
pub(crate) mod quit_all_confirm;
pub(crate) mod rebase;
pub(crate) mod rename_input;
pub(crate) mod review;
pub(crate) mod reword;
pub(crate) mod run_pane;
pub(crate) mod sanitize;
pub(crate) mod screen;
pub(crate) mod signature_help;
pub(crate) mod symbol_finder;
pub(crate) mod symbol_picker;
pub(crate) mod table;
pub(crate) mod term_pane;
pub(crate) mod text;
pub(crate) mod undercurl;
pub(crate) mod walkthrough;
pub(crate) mod walkthrough_picker;
pub(crate) mod workspace_picker;

use self::{review::DiffDials, undercurl::UndercurlBatch};
use crate::{
    action_handlers::search::{self, StatusPrompt},
    app::{self, modal_split_percent, modal_zoom_steps, ModalKind, Stoat},
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    commit_list::{CommitListId, CommitListState},
    diff_changes::{DiffChangesId, DiffChangesState},
    editor_state::{EditorId, EditorState},
    keymap_state::{
        active_modal, binding_display_desc, cursor_token, focus_flags, focused_pane_pinned,
        pane_predicate, ActiveModal, AppFlags, Flags, FocusFlags, StoatKeymapState,
    },
    lsp::{progress::LspProgressMap, registry::LspRegistry},
    minimap::{emit, MinimapContent},
    pane::{DockVisibility, FocusTarget, PaneId, View},
    rebase::RebasePause,
    run::{RunId, RunState},
    term_session::{TermId, TermSession},
    toggle::{self, Toggle, ToggleStates},
    workspace::{
        self,
        diff::{self, HunkPosition},
        Workspace, WorkspaceId,
    },
};
use crossterm::{
    queue,
    style::{Attribute, Color as CrosstermColor, Colored, SetAttribute},
};
use ratatui::{
    backend::IntoCrossterm,
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Color, Modifier},
    widgets::StatefulWidget,
};
use slotmap::SlotMap;
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    io,
    ops::Range,
    path::{Path, PathBuf},
};
use stoat_config::{LineNumbers, MinimapMode, WrapMode};
use stoat_widgets::{minimap::Minimap, popover::Popover, ApcScene};
use unicode_width::UnicodeWidthStr;

/// Full-cell text scale under stoatty, in 256ths of a cell, for grid-size modal
/// titles.
pub(crate) const TEXT_SCALE_FULL: u16 = 256;
/// Popup body text scale under stoatty, 0.85x a cell, for hint rows and hover
/// bodies.
pub(crate) const TEXT_SCALE_POPUP: u16 = 218;
/// Compact chrome text scale under stoatty, 0.625x a cell, for line numbers and
/// the status bar.
///
/// Under `cfg(test)` it resolves to a full cell (256) instead: the test
/// compositor draws one glyph per cell, so running compact text at cell scale
/// keeps composited geometry exact against the fallback layout the harness scans
/// and snapshots.
pub(crate) const TEXT_SCALE_COMPACT: u16 = if cfg!(test) { 256 } else { 160 };

/// Strip id for the single-mode minimap, reserved above every pane index (which
/// are small and dense from zero). stoatty keys minimap strips in a namespace
/// separate from scroll pools, so `u32::MAX` never collides.
pub(crate) const SINGLE_MINIMAP_STRIP_ID: u32 = u32::MAX;

/// The scene lane for everything a frame draws after its split panes, such as
/// the dividers, the docks, and every overlay from a hover card to a modal.
///
/// stoatty composes lanes in ascending id order, and a panel occludes only what
/// was declared before it. An overlay therefore has to sit in a lane above
/// every pane's to cover them, and the highest id stays above any pane index.
pub(crate) const OVERLAY_LANE: u32 = u32::MAX;

pub(crate) struct PaneCtx<'a> {
    pub(crate) editors: &'a mut SlotMap<EditorId, EditorState>,
    pub(crate) buffers: &'a BufferRegistry,
    pub(crate) runs: &'a SlotMap<RunId, RunState>,
    pub(crate) terms: &'a SlotMap<TermId, TermSession>,
    /// The terms stoatty draws as pools at the terminal font size, see
    /// [`crate::workspace::Workspace::term_pool_grids`].
    pub(crate) term_pool_grids: &'a HashMap<TermId, (u16, u16)>,
    /// Every open commits list, which a [`View::Commits`] pane paints from.
    pub(crate) commit_lists: &'a mut SlotMap<CommitListId, CommitListState>,
    /// Every open changes view, which a [`View::DiffChanges`] pane paints from.
    pub(crate) diff_changes_views: &'a mut SlotMap<DiffChangesId, DiffChangesState>,
}

/// The lookup and colors a pane needs to declare its minimap strip.
///
/// Carried on [`FrameCtx`] so a pane resolves its strip's content-store id (via
/// `(workspace, buffer)`) and the palette and thumb color stoatty paints it in,
/// without the render path reaching back into [`Stoat`].
#[derive(Clone, Copy)]
pub(crate) struct MinimapChrome<'a> {
    /// Active workspace, the first half of the [`Stoat::minimap_content`] key.
    pub(crate) workspace: WorkspaceId,
    /// Content stores this session declared, read for each strip's `content_id`.
    pub(crate) content: &'a HashMap<(WorkspaceId, BufferId), MinimapContent>,
    /// Syntax-scope palette a plain editor's strip declares and its run
    /// summaries index.
    ///
    /// This and the three palettes below are cut to
    /// [`ClassTable::legacy_palette`](crate::minimap::ClassTable::legacy_palette)'s
    /// length for a terminal before protocol 13.
    pub(crate) palette: &'a [[u8; 3]],
    /// [`Self::palette`] pre-blended toward the background at the inactive dim,
    /// for an unfocused pane's strip.
    ///
    /// Every unfocused pane blends the same palette the same way, so the frame
    /// does it once rather than each pane doing it again. Aliases
    /// [`Self::palette`] when no dim applies.
    pub(crate) dimmed_palette: &'a [[u8; 3]],
    /// The palette a diff view's strip declares, which follows the diff dials.
    pub(crate) diff_palette: &'a [[u8; 3]],
    /// [`Self::diff_palette`] at the inactive dim, for an unfocused diff view's
    /// strip. Aliases [`Self::diff_palette`] when no dim applies.
    pub(crate) dimmed_diff_palette: &'a [[u8; 3]],
    /// The content store of the active workspace's changes view strip, which
    /// every changes view pane's strip and the single band over a focused
    /// changes view show. `None` while the workspace holds no changes view.
    pub(crate) diff_changes_content: Option<u32>,
    /// Viewport-thumb fill color, rgba.
    pub(crate) thumb: [u8; 4],
}

/// One language server as the status bar shows it.
pub(crate) struct StatusServer {
    /// The abbreviation the badge paints, from [`pane::lsp_short_name`].
    pub(crate) short: String,
    /// The server's own name. Kept because the busy lookup is keyed by it while
    /// the bar paints the abbreviation, so refreshing the flag needs both.
    name: String,
    /// Whether the server has work-done progress in flight, which drives the
    /// badge's spinner glyph.
    pub(crate) busy: bool,
}

/// The status bar's server list for the focused buffer, held across frames.
///
/// Deriving it costs a string for the language, a vector of server names, and
/// three allocations per server for the abbreviation, none of which moves unless
/// the focus or the registry's server set does. Only [`StatusServer::busy`]
/// changes per frame, and that is a map lookup.
#[derive(Default)]
pub(crate) struct LspServerList {
    /// Focused buffer the names were derived for, `None` when the focused pane
    /// holds no editor.
    buffer: Option<BufferId>,
    /// [`LspRegistry::generation`] the names were derived at.
    generation: u64,
    servers: Vec<StatusServer>,
}

impl LspServerList {
    /// Bring the list up to date for `buffer` and hand back what the bar paints.
    ///
    /// Rebuilds the names only when the focus or the server set has moved since
    /// the last call, then refreshes every busy flag, which is the only part a
    /// steady frame can change.
    fn refresh(
        &mut self,
        buffer: Option<BufferId>,
        buffers: &BufferRegistry,
        registry: &LspRegistry,
        progress: &LspProgressMap,
    ) -> &[StatusServer] {
        let generation = registry.generation();
        if self.buffer != buffer || self.generation != generation {
            self.buffer = buffer;
            self.generation = generation;
            self.servers.clear();

            let language =
                buffer.and_then(|buffer| crate::lsp::session::lsp_language_name(buffers, buffer));
            if let Some(language) = language {
                self.servers
                    .extend(
                        registry
                            .names_for_language(&language)
                            .into_iter()
                            .map(|name| StatusServer {
                                short: pane::lsp_short_name(&name),
                                name,
                                busy: false,
                            }),
                    );
            }
        }

        for server in &mut self.servers {
            server.busy = progress.server_busy(&server.name);
        }
        &self.servers
    }
}

/// Ambient workspace and frame state shared across render functions. Bundled
/// so individual render functions stay under the `clippy::too_many_arguments`
/// threshold; every field is a cheap borrow or `Copy`.
#[derive(Clone, Copy)]
pub(crate) struct FrameCtx<'a> {
    pub(crate) workspace_name: &'a str,
    pub(crate) workspace_root: &'a Path,
    pub(crate) mode: &'a str,
    /// The app screen in the foreground, from [`crate::keymap_state::view_predicate`].
    /// Drives the status-bar screen label. `None` or `Some("file")` for a plain
    /// editor with no screen over it.
    pub(crate) screen: Option<&'static str>,
    pub(crate) theme: &'a crate::theme::Theme,
    /// The commits screen's dragged list share, as a percentage of the pane
    /// body. `None` until the reader drags that screen's separator, which
    /// leaves the width to the renderer's own formula.
    pub(crate) commits_split: Option<u16>,
    /// The diff view's styling dials, resolved for the commit previews. Those
    /// previews paint through `:diff-files`'s own painter, so they answer the
    /// same session dials rather than carrying a second set.
    pub(crate) preview_dials: commits::PreviewDials,
    /// Whether the repository tally is owed or on the pool, so a changes view
    /// with no excerpt shows that its list builds rather than that nothing
    /// changed.
    pub(crate) diff_changes_building: bool,
    /// The gutter, severity, and diff-mark colors already resolved from
    /// [`Self::theme`], so each pane paints from them rather than re-walking
    /// the scope table for every one.
    pub(crate) chrome: &'a editor::ResolvedChrome,
    /// Mid-typing count prefix waiting on a motion (e.g. `4` between
    /// keypresses on the way to `4j`). The status bar shows it so the
    /// user knows a partial count is in flight; cleared after every
    /// action dispatch.
    pub(crate) pending_count: Option<u32>,
    /// Register an in-progress macro recording captures into, `None` when no
    /// recording runs.
    ///
    /// Recording is modal state that takes every keypress until it is toggled
    /// off, so the status bar shows it. Without the sign, a user who looks away
    /// has nothing to tell them their next edits go into a macro.
    pub(crate) recording_register: Option<char>,
    /// The buffers a command waits on, each with the name of what waits, which
    /// the status line of every pane that shows one names.
    ///
    /// A quit over such a buffer returns to the shell and ends the command, so
    /// the reader has to see the tie before the quit, not after it.
    pub(crate) held_buffers: &'a [(BufferId, String)],
    /// Whether the detailed LSP status popout is open (pinned) above the focused
    /// pane's status bar. Drives whether the multi-row status card paints.
    pub(crate) lsp_status_open: bool,
    /// Every in-flight LSP work-done entry across servers, freshest first, painted
    /// as rows of the detailed status card when it is open.
    pub(crate) lsp_progress_entries: &'a [&'a crate::lsp::progress::LspProgressEntry],
    /// Braille spinner glyph index for the [`lsp_progress`](Self::lsp_progress)
    /// popout, advanced by the frame tick so the spinner animates.
    pub(crate) spinner_phase: u8,
    /// The focused buffer's running language servers, painted as compact badges
    /// at the focused pane's bar right edge. Empty for an unfocused pane or a
    /// buffer with no named server.
    pub(crate) lsp_servers: &'a [StatusServer],
    /// True while a background diff warm runs, driving the focused pane's
    /// transient ` <spinner> diff ` status-bar segment. False when idle.
    pub(crate) diff_warm_busy: bool,
    /// Hunks staged and unstaged across the repo, which the focused pane's bar
    /// leads its staged segment with. `None` before the first diff lands and
    /// outside a repo, which drops the segment back to the focused file's own
    /// hunk counts.
    pub(crate) repo_change_counts: Option<(usize, usize)>,
    /// Where the focused diff view stands among the repo's changed files and
    /// hunks, which the focused pane's bar names in its staged segment. `None`
    /// when the focused pane shows no diff view, which is what keeps the
    /// position off a pane with no hunks to walk.
    ///
    /// The bar prints `-` for a cursor above the focused file's first hunk, the
    /// one place the walk has nothing behind it.
    pub(crate) hunk_position: Option<HunkPosition>,
    /// What the bar names the workspace's diff base, when that is not the
    /// working tree. It replaces [`Self::repo_change_counts`], which describes
    /// a working tree the pane is then not showing.
    pub(crate) diff_base: Option<&'a str>,
    /// The names of the diff's left and right sides, which the diff view's top
    /// bar reads.
    pub(crate) diff_sides: (&'a str, &'a str),
    /// The sides a paired file's diff view names, keyed by the measured path.
    ///
    /// A pane reads its file's entry ahead of [`Self::diff_sides`], since a
    /// pair holds for its own file alone and the workspace base names every
    /// other pane.
    pub(crate) pair_sides: &'a HashMap<PathBuf, (String, String)>,
    /// Label of the explicit LSP request still in flight, so the status bar
    /// shows a "lsp: {label}..." segment until the response lands. `None` when no
    /// such request is pending. See [`crate::lsp::lsp_pending_label`].
    pub(crate) lsp_pending: Option<&'static str>,
    /// Freshest `window/showMessage` text. `MessageType::ERROR` paints as a
    /// wrapped popout card above the status bar. Other levels paint in the bar's
    /// right side.
    pub(crate) lsp_message: Option<(lsp_types::MessageType, &'a str)>,
    /// The transient status message ([`Stoat::pending_message`]), already
    /// checked against its TTL deadline. Painted in the focused pane's status
    /// bar just left of the diagnostics badge, styled as an error.
    pub(crate) status_message: Option<&'a str>,
    /// Active labels for an in-progress `GotoWord` jump, keyed by label
    /// string with byte-offset values. Painted by the focused editor's
    /// render path; non-focused panes ignore this field.
    pub(crate) goto_word_labels: Option<&'a std::collections::BTreeMap<String, (usize, usize)>>,
    /// [`Self::mode`]'s status-line label and colour, resolved once for the
    /// frame by [`pane::mode_segment`].
    ///
    /// Resolving it builds a `ui.statusline.{mode}` scope string to probe the
    /// theme with, which the panes would otherwise repeat for a mode that is
    /// the same for all of them. The label is a per-mode override from
    /// `Settings` when one exists, so a user-defined mode shows something more
    /// meaningful than `---`.
    pub(crate) mode_label: (&'a str, Color),
    /// Workspace-wide LSP diagnostic store. The status bar reads the
    /// focused buffer's path and paints a compact severity badge when
    /// any diagnostics are present.
    pub(crate) diagnostics: &'a crate::diagnostics::DiagnosticSet,
    /// The in-buffer search query every editor pane lights visible matches for,
    /// in the `ui.search.match` style, so users see all hits at once.
    ///
    /// An open `/` or `?` prompt supplies the query, so the highlights track
    /// what the user types rather than the last search. An emptied prompt shows
    /// no matches. With no prompt open the query is the most recently submitted
    /// one, which the highlights return to as the prompt closes.
    pub(crate) search_query: Option<&'a str>,
    /// Whether [`Self::search_query`] compiles under smart case, resolved from
    /// `search.smart_case` (default on). It travels with the query because the
    /// paint compiles its own regex, and a highlight disagreeing with the jump
    /// lights up a different set of matches than `n` walks.
    pub(crate) search_smart_case: bool,
    /// The open search or regex prompt, painted over the focused pane's
    /// status-bar left segments so the user reads the pattern as they type it.
    /// `None` whenever neither input is open.
    pub(crate) status_prompt: Option<&'a StatusPrompt>,
    /// The editor the terminal draws the primary cursor over this frame, which
    /// is the focused editor while no modal input holds focus. A pane whose
    /// editor is any other paints its own primary block.
    pub(crate) terminal_cursor_editor: Option<EditorId>,
    /// How document editor panes number the gutter, resolved from
    /// `editor.line_numbers` (default [`LineNumbers::Relative`]).
    /// [`LineNumbers::Off`] keeps the diagnostic-only gutter column.
    pub(crate) line_numbers: LineNumbers,
    /// How document editor panes soft-wrap long lines, resolved from
    /// `editor.wrap` (default [`WrapMode::EditorWidth`]). Applied only to pane
    /// editors. Non-pane inputs and pickers never wrap.
    pub(crate) wrap_mode: WrapMode,
    /// The wrap column [`WrapMode::Bounded`] caps against, resolved from
    /// `editor.wrap_column` (default 80, at least 1). Ignored by the other wrap
    /// modes.
    pub(crate) wrap_column: u32,
    /// Fraction an unfocused pane's colors blend toward the theme background,
    /// resolved from `ui.inactive_dim` (default 0.25, clamped to `0.0..=1.0`).
    /// `0.0` disables dimming. Applied by [`crate::render::pane::render_pane`]
    /// to unfocused panes only.
    pub(crate) inactive_dim: f32,
    /// The diff view's paint dials, resolved from the session per
    /// [`DiffDials::from_stoat`]. Read only by diff-view panes.
    pub(crate) diff_dials: DiffDials,
    /// Whether the terminal can draw an image, which is a stoatty new enough to
    /// have the pass for it. An image pane says what is missing when it cannot.
    pub(crate) images_capable: bool,
    /// Whether editor and changes view panes reserve their own per-pane
    /// right-edge minimap strip. `true` only in [`MinimapMode::PerPane`] under
    /// stoatty. Single mode gates this off and declares one shared strip over
    /// the reserved band instead.
    pub(crate) minimap_enabled: bool,
    /// The lookup and colors a pane needs to declare its minimap strip, `Some`
    /// only when the strip is active (under stoatty with the minimap enabled).
    pub(crate) minimap_chrome: Option<MinimapChrome<'a>>,
    /// The reserved single-minimap band, `Some` only when [`MinimapMode::Single`]
    /// stamped one this frame. It stops one row above the bottom, so a pane whose
    /// status bar sits on that freed row flush against the band reclaims its
    /// width and runs edge to edge.
    pub(crate) minimap_band: Option<Rect>,
    /// The span of the window's bottom row under a bottom-right badge's box,
    /// from [`badges::bottom_right_cover`]. A status bar on that row keeps its
    /// segments left of it, since the box paints over the bar.
    pub(crate) badge_cover: Option<Rect>,
    /// Terminal cell the mouse last rested over, or `None` when it has not
    /// moved over a pane. The focused editor resolves the diagnostic under it
    /// to raise a hover popover.
    pub(crate) hover_cell: Option<(u16, u16)>,
    /// The user home directory, for `~`-abbreviating run-pane prompt cwds.
    /// Resolved through [`crate::host::EnvHost`] so tests control it instead of
    /// the paint reading the real environment. `None` when `$HOME` is unset.
    pub(crate) home: Option<&'a Path>,
    /// Latency readout for the status bar, or `None` before any frame has
    /// been painted. Present only under the `perf` feature.
    #[cfg(feature = "perf")]
    pub(crate) perf: Option<PerfSegment>,
}

/// A screen buffer whose every cell holds `blank`, blanked off the run loop
/// ahead of the paint that reuses it.
pub(crate) struct BlankedBuffer {
    pub(crate) buffer: Buffer,
    pub(crate) blank: Cell,
}

/// A status-bar latency readout holding the most recent paint time and the
/// p95 input-to-publish latency, both in microseconds.
#[cfg(feature = "perf")]
#[derive(Clone, Copy)]
pub(crate) struct PerfSegment {
    pub(crate) last_paint_us: u64,
    pub(crate) p95_input_us: u64,
}

#[cfg(feature = "perf")]
impl PerfSegment {
    /// Read the headline metrics for the status bar, or `None` until at least
    /// one frame has been painted (so a fresh session shows no readout).
    pub(crate) fn capture(perf: &crate::perf::PerfStats) -> Option<PerfSegment> {
        let paint = perf.paint_stats()?;
        Some(PerfSegment {
            last_paint_us: paint.last / 1_000,
            p95_input_us: perf.input_to_publish_stats().map_or(0, |s| s.p95 / 1_000),
        })
    }
}

/// The colors, attributes, and cursor position that the bytes of a page stream
/// leave on the terminal.
///
/// A cell writes its attribute and color changes only where they differ from
/// this state. It writes a cursor move only where it does not follow the last
/// cell. [`CrosstermBackend`](ratatui::backend::CrosstermBackend) keeps the
/// same state as it draws, so a cell goes out as the bytes that backend writes
/// for it.
///
/// The pen formats the parameters of its sequences itself. crossterm's
/// commands format each number through `core::fmt` and each attribute code
/// through a new `String`, and on a full page that costs more than the rest of
/// the encode. The bytes are the ones those commands write.
///
/// See also:
/// - [`serialize_rows`], the page stream this state follows.
struct PagePen {
    fg: Color,
    bg: Color,
    underline_color: Color,
    modifier: Modifier,
    last: Option<(u16, u16)>,
}

impl PagePen {
    fn new() -> Self {
        PagePen {
            fg: Color::Reset,
            bg: Color::Reset,
            underline_color: Color::Reset,
            modifier: Modifier::empty(),
            last: None,
        }
    }

    /// Write `cell` at column `x` of row `y`, after the state changes it needs.
    fn paint(&mut self, out: &mut Vec<u8>, x: u16, y: u16, cell: &Cell) -> io::Result<()> {
        self.move_to(out, x, y);

        if cell.modifier != self.modifier {
            self.set_modifier(out, cell.modifier)?;
        }
        self.set_colors(out, cell.fg, cell.bg);
        if cell.underline_color != self.underline_color {
            push_color_sgr(
                out,
                &[Colored::UnderlineColor(
                    cell.underline_color.into_crossterm(),
                )],
            );
            self.underline_color = cell.underline_color;
        }

        out.extend_from_slice(cell.symbol().as_bytes());
        Ok(())
    }

    /// Clear from column `x` of row `y` to the end of the line under the colors
    /// of `blank`.
    fn erase(&mut self, out: &mut Vec<u8>, x: u16, y: u16, blank: &Cell) -> io::Result<()> {
        self.move_to(out, x, y);
        // The erase leaves the cursor at `x`, not past a painted cell.
        self.last = None;

        // Terminals do not agree on which attributes an erase applies, so the
        // erase runs with none. SGR 0 also resets the colors, to the state of a
        // new pen.
        if !self.modifier.is_empty() {
            set_attribute(out, Attribute::Reset)?;
            *self = PagePen::new();
        }
        self.set_colors(out, blank.fg, blank.bg);

        out.extend_from_slice(b"\x1b[K");
        Ok(())
    }

    /// Reset the colors and attributes, so the stream ends in the state a new
    /// pen starts from.
    fn finish(self, out: &mut Vec<u8>) -> io::Result<()> {
        let reset = CrosstermColor::Reset;
        for colored in [
            Colored::ForegroundColor(reset),
            Colored::BackgroundColor(reset),
            Colored::UnderlineColor(reset),
        ] {
            push_color_sgr(out, &[colored]);
        }
        set_attribute(out, Attribute::Reset)
    }

    fn move_to(&mut self, out: &mut Vec<u8>, x: u16, y: u16) {
        if self.last.map(|(last_x, last_y)| (last_x + 1, last_y)) != Some((x, y)) {
            out.extend_from_slice(b"\x1b[");
            push_u16(out, y + 1);
            out.push(b';');
            push_u16(out, x + 1);
            out.push(b'H');
        }
        self.last = Some((x, y));
    }

    fn set_colors(&mut self, out: &mut Vec<u8>, fg: Color, bg: Color) {
        if (fg, bg) != (self.fg, self.bg) {
            push_color_sgr(
                out,
                &[
                    Colored::ForegroundColor(fg.into_crossterm()),
                    Colored::BackgroundColor(bg.into_crossterm()),
                ],
            );
            self.fg = fg;
            self.bg = bg;
        }
    }

    /// Change the attributes to `to`.
    ///
    /// The commands and their order copy the private attribute diff of
    /// [`CrosstermBackend`](ratatui::backend::CrosstermBackend), so a cell's
    /// bytes stay the ones that backend writes.
    fn set_modifier(&mut self, out: &mut Vec<u8>, to: Modifier) -> io::Result<()> {
        let removed = self.modifier - to;
        let added = to - self.modifier;
        self.modifier = to;

        if removed.contains(Modifier::REVERSED) {
            set_attribute(out, Attribute::NoReverse)?;
        }

        // SGR 22 clears bold and dim together, so the one of them that stays
        // goes back on.
        let intensity = Modifier::BOLD | Modifier::DIM;
        let reset_intensity = removed.intersects(intensity);
        if reset_intensity {
            set_attribute(out, Attribute::NormalIntensity)?;
            if to.contains(Modifier::DIM) {
                set_attribute(out, Attribute::Dim)?;
            }
            if to.contains(Modifier::BOLD) {
                set_attribute(out, Attribute::Bold)?;
            }
        }

        for (modifier, off) in [
            (Modifier::ITALIC, Attribute::NoItalic),
            (Modifier::UNDERLINED, Attribute::NoUnderline),
            (Modifier::CROSSED_OUT, Attribute::NotCrossedOut),
            (Modifier::HIDDEN, Attribute::NoHidden),
        ] {
            if removed.contains(modifier) {
                set_attribute(out, off)?;
            }
        }
        if removed.intersects(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK) {
            set_attribute(out, Attribute::NoBlink)?;
        }

        for (modifier, on) in [
            (Modifier::REVERSED, Attribute::Reverse),
            (Modifier::BOLD, Attribute::Bold),
            (Modifier::ITALIC, Attribute::Italic),
            (Modifier::UNDERLINED, Attribute::Underlined),
            (Modifier::DIM, Attribute::Dim),
            (Modifier::CROSSED_OUT, Attribute::CrossedOut),
            (Modifier::HIDDEN, Attribute::Hidden),
            (Modifier::SLOW_BLINK, Attribute::SlowBlink),
            (Modifier::RAPID_BLINK, Attribute::RapidBlink),
        ] {
            let restored = reset_intensity && intensity.contains(modifier);
            if added.contains(modifier) && !restored {
                set_attribute(out, on)?;
            }
        }
        Ok(())
    }
}

/// Append one SGR sequence that sets `colors`, as crossterm's color commands
/// write it.
fn push_color_sgr(out: &mut Vec<u8>, colors: &[Colored]) {
    out.extend_from_slice(b"\x1b[");
    for (i, &colored) in colors.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        push_colored(out, colored);
    }
    out.push(b'm');
}

/// Append the SGR parameters that crossterm writes for `colored`.
///
/// A named color takes its parameters from crossterm's own table. Under
/// `NO_COLOR`, crossterm writes no color parameters at all, and neither does
/// this, so a page keeps matching the live grid.
fn push_colored(out: &mut Vec<u8>, colored: Colored) {
    if Colored::ansi_color_disabled_memoized() {
        return;
    }

    let (layer, color) = match colored {
        Colored::ForegroundColor(color) => (38, color),
        Colored::BackgroundColor(color) => (48, color),
        Colored::UnderlineColor(color) => (58, color),
    };
    match color {
        CrosstermColor::Reset => push_u16(out, layer + 1),
        CrosstermColor::Rgb { r, g, b } => {
            push_u16(out, layer);
            out.extend_from_slice(b";2;");
            push_u16(out, r.into());
            out.push(b';');
            push_u16(out, g.into());
            out.push(b';');
            push_u16(out, b.into());
        },
        CrosstermColor::AnsiValue(index) => {
            push_u16(out, layer);
            out.extend_from_slice(b";5;");
            push_u16(out, index.into());
        },
        _ => out.extend_from_slice(colored.to_string().as_bytes()),
    }
}

/// Append the SGR sequence that crossterm writes for `attribute`.
///
/// An attribute the pen never writes goes through crossterm.
fn set_attribute(out: &mut Vec<u8>, attribute: Attribute) -> io::Result<()> {
    let code = match attribute {
        Attribute::Reset => 0,
        Attribute::Bold => 1,
        Attribute::Dim => 2,
        Attribute::Italic => 3,
        Attribute::Underlined => 4,
        Attribute::SlowBlink => 5,
        Attribute::RapidBlink => 6,
        Attribute::Reverse => 7,
        Attribute::Hidden => 8,
        Attribute::CrossedOut => 9,
        Attribute::NormalIntensity => 22,
        Attribute::NoItalic => 23,
        Attribute::NoUnderline => 24,
        Attribute::NoBlink => 25,
        Attribute::NoReverse => 27,
        Attribute::NoHidden => 28,
        Attribute::NotCrossedOut => 29,
        other => return queue!(out, SetAttribute(other)),
    };
    out.extend_from_slice(b"\x1b[");
    push_u16(out, code);
    out.push(b'm');
    Ok(())
}

/// Append the decimal digits of `n`.
fn push_u16(out: &mut Vec<u8>, mut n: u16) {
    let mut digits = [0; 5];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[start..]);
}

/// Format a [`PerfSegment`] for the status bar, padded like the other
/// segments so neighbors stay separated.
#[cfg(feature = "perf")]
pub(crate) fn perf_label(seg: PerfSegment) -> String {
    format!(
        " paint {}us in-p95 {}us ",
        seg.last_paint_us, seg.p95_input_us
    )
}

pub(crate) const PRIMARY_MODES: &[&str] = &["normal", "insert", "select"];

/// Reserve the bottom row for the pane status bar so the which-key hint box,
/// bottom-anchored over the full-window right edge, never paints over it.
pub(crate) fn hints_overlay_area(size: Rect) -> Rect {
    Rect {
        x: size.x,
        y: size.y,
        width: size.width,
        height: size.height.saturating_sub(1),
    }
}

/// True while a centered modal could paint where the single-minimap strip
/// draws.
///
/// The strip and every modal share the same GPU passes with the strip on top,
/// so a modal cannot paint over it. The strip is undeclared instead on frames
/// where one is open, and the modal lays out on the full window rather than
/// yielding the band.
///
/// Which modals cover the band is [`ActiveModal::hides_minimap`].
fn modal_overlay_open(stoat: &Stoat) -> bool {
    active_modal(stoat).is_some_and(ActiveModal::hides_minimap)
}

/// True while an open modal owns the zoom combo, so a zoom step belongs to it
/// rather than to the panes behind it.
///
/// See also:
/// - [`ActiveModal::owns_zoom`] for which modals claim a step, and why.
pub(crate) fn zoom_context_modal(stoat: &Stoat) -> bool {
    active_modal(stoat).is_some_and(ActiveModal::owns_zoom)
}

/// The open modal a zoom step applies to, or `None` when the open modal has no
/// zoom of its own and when no modal is open at all.
///
/// See also:
/// - [`zoom_context_modal`] to tell a zoomless modal apart from no modal.
pub(crate) fn zoom_target_kind(stoat: &Stoat) -> Option<ModalKind> {
    active_modal(stoat).and_then(ActiveModal::zoom_kind)
}

/// The blank cell a themed surface starts from.
///
/// A cell left at `Color::Reset` is resolved by whatever terminal stoat runs
/// inside, against *that* terminal's theme rather than stoat's, so a surface
/// blanked with [`ratatui::buffer::Cell::EMPTY`] ignores `:theme` entirely and
/// disagrees with the widgets that paint `ui.background` for themselves.
///
/// A channel the theme does not define stays at Reset, since there is nothing
/// truer to put there than the terminal's own default.
pub(crate) fn themed_blank(theme: &crate::theme::Theme) -> Cell {
    let mut cell = Cell::EMPTY;
    cell.fg = themed_fg(theme);
    cell.bg = themed_bg(theme);
    cell
}

/// Blanks every cell of `buffer` to `blank`.
///
/// The run loop gives a published frame's buffer to a blocking-pool task that
/// calls this. The fill of a whole screen then runs while the newest frame is
/// on screen, not between a key and the frame it causes. The returned blank
/// lets the paint that takes the buffer back tell whether the theme still
/// matches.
pub(crate) fn blank_buffer(mut buffer: Buffer, blank: Cell) -> BlankedBuffer {
    buffer.content.fill(blank.clone());
    BlankedBuffer { buffer, blank }
}

/// The foreground the theme gives ordinary text, for a caller that has nothing
/// more specific to fall back to.
pub(crate) fn themed_fg(theme: &crate::theme::Theme) -> Color {
    theme
        .try_get(crate::theme::scope::UI_TEXT)
        .and_then(|s| s.fg)
        .unwrap_or(Color::Reset)
}

/// The background the theme gives the editor surface, for a caller that has
/// nothing more specific to fall back to.
pub(crate) fn themed_bg(theme: &crate::theme::Theme) -> Color {
    theme
        .try_get(crate::theme::scope::UI_BACKGROUND)
        .and_then(|s| s.bg)
        .unwrap_or(Color::Reset)
}

/// Blank `area` to the themed cell, wiping whatever was painted under it.
///
/// This is what a modal lays down before its frame, in place of ratatui's
/// [`Clear`](ratatui::widgets::Clear). `modal_frame` draws borders and never
/// fills, so a modal's blank interior is whatever the clear left, and Reset
/// there means the terminal's own background shows through a themed screen.
pub(crate) fn clear_themed(area: Rect, buf: &mut Buffer, theme: &crate::theme::Theme) {
    let blank = themed_blank(theme);
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)] = blank.clone();
        }
    }
}

/// Serialize every row of `buf` to a self-contained VT byte stream, as
/// [`serialize_rows`] serializes them.
pub(crate) fn serialize_buffer(buf: &Buffer) -> Vec<u8> {
    serialize_rows(buf, buf.area.top()..buf.area.bottom())
}

/// Serialize the cells of `buf` whose row is in `rows` to a self-contained VT
/// byte stream.
///
/// Unlike the live render path, which diffs against the previous frame, this
/// paints all those cells unconditionally, so the bytes fully paint those rows
/// of a pool slot regardless of what the slot held before. Cursor moves are
/// absolute, so the stream is positioned for the page's top-left independent
/// of the live grid cursor.
///
/// The trailing run of identical blanks in each row goes out as one erase to
/// the end of the line under their background. On a code page that run is
/// most of the row, and a cell-by-cell paint costs the wire and the receiving
/// parser one cell for each blank. An erased cell takes the receiver's default
/// foreground, which a blank never shows.
///
/// The erase stops at the right edge of the receiving grid, so `buf` must end
/// at that edge, as a page buffer at the size of its pool region does.
pub(crate) fn serialize_rows(buf: &Buffer, rows: Range<u16>) -> Vec<u8> {
    let mut out = Vec::new();
    // Writes into a Vec<u8> never fail. The Results exist only because the
    // crossterm commands write through io::Write.
    let _ = queue_rows(&mut out, buf, rows);
    out
}

fn queue_rows(out: &mut Vec<u8>, buf: &Buffer, rows: Range<u16>) -> io::Result<()> {
    let mut pen = PagePen::new();
    let mut shown = Vec::new();

    for y in rows.start.max(buf.area.top())..rows.end.min(buf.area.bottom()) {
        shown_cells(buf, y, &mut shown);
        let painted = shown.len() - trailing_blank_run(&shown);

        for &(x, cell) in &shown[..painted] {
            pen.paint(out, x, y, cell)?;
        }
        if let Some(&(x, blank)) = shown.get(painted) {
            pen.erase(out, x, y, blank)?;
        }
    }

    pen.finish(out)
}

/// Replace `shown` with the cells of row `y` that print, each with its column.
///
/// The cells a wide glyph covers are left out, as the live diff leaves them
/// out. The terminal advances its cursor across them when it prints the
/// glyph, so a blank printed for one lands a column too far right and shifts
/// the rest of the row. Because of the gap, the next cell writes a cursor move
/// first.
fn shown_cells<'a>(buf: &'a Buffer, y: u16, shown: &mut Vec<(u16, &'a Cell)>) {
    shown.clear();

    let width = usize::from(buf.area.width);
    let start = usize::from(y - buf.area.y) * width;
    let row = &buf.content[start..start + width];
    let mut hidden = 0;
    for (x, cell) in (buf.area.left()..buf.area.right()).zip(row) {
        if hidden > 0 {
            hidden -= 1;
            continue;
        }
        hidden = cell.symbol().width().saturating_sub(1);
        shown.push((x, cell));
    }
}

/// The number of cells at the end of `shown` that go out as one erase.
///
/// The run is the cells equal to the last cell, when that cell is a space with
/// no modifier. Such a cell shows only its background, so an erase under that
/// background paints what the cells paint.
fn trailing_blank_run(shown: &[(u16, &Cell)]) -> usize {
    let Some(&(_, last)) = shown.last() else {
        return 0;
    };
    if last.symbol() != " " || !last.modifier.is_empty() {
        return 0;
    }
    shown
        .iter()
        .rev()
        .take_while(|(_, cell)| *cell == last)
        .count()
}

/// Paint one full frame of the TUI into `buf`. Called once per [`Stoat::render`]
/// tick after the parse pipeline and commits pump have run.
///
/// Retires an expired [`Stoat::pending_message`] up front, then hands the live
/// one to the panes as a status-bar segment. The panes always keep full height.
pub(crate) fn frame(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    undercurls: &mut UndercurlBatch,
) {
    let full = stoat.size();

    // Settled before the frame borrows the rest of the state it paints from,
    // since every pane reads these and only a theme change moves them.
    stoat.refresh_chrome();
    stoat.refresh_frame_mode();

    if let Some(deadline) = stoat.pending_message_deadline
        && stoat.executor.now() >= deadline
    {
        stoat.pending_message = None;
        stoat.pending_message_deadline = None;
        stoat.pending_message_expiry = None;
    }

    let minimap_mode = stoat.minimap_mode();
    let minimap_enabled = minimap_mode != MinimapMode::Off;
    emit::ensure_minimap_content_ids(stoat);

    let tab_bar_rows = u16::from(stoat.tab_bar_visible());

    // Single mode reserves a strip band at the window's right edge and shrinks
    // the pane layout by the strip width, so the panes never overlap the strip.
    // The band stops one row above the bottom so a status bar on that row runs
    // the full window width, and starts one row down when the tab bar holds the
    // top, since the band derives its own rect instead of going through
    // `layout_size`. The band is stamped on Stoat for the mouse handler, then
    // read back for this paint.
    stoat.single_minimap_rect = (minimap_mode == MinimapMode::Single
        && full.width >= editor::MINIMAP_MIN_PANE_COLS)
        .then(|| Rect {
            x: full.x + full.width - editor::MINIMAP_STRIP_COLS,
            y: full.y + tab_bar_rows,
            width: editor::MINIMAP_STRIP_COLS,
            height: full.height.saturating_sub(1 + tab_bar_rows),
        });
    let single_minimap_rect = stoat.single_minimap_rect;
    let modal_overlay = modal_overlay_open(stoat);
    let size = stoat.layout_size();
    let inactive_dim = stoat
        .settings
        .ui_inactive_dim
        .unwrap_or(0.25)
        .clamp(0.0, 1.0) as f32;

    stoat.refresh_minimap_palettes(minimap_enabled, inactive_dim);

    // Resolved before the field borrows below, because snapshotting the prompt
    // editor's display map takes the whole app mutably. Nothing between here
    // and the status bars opens or closes the prompt.
    let status_prompt = search::prompt_display(stoat);

    // The last whole-app borrows are behind us, so what the frame reads from
    // here on is field by field and can live alongside the workspace below.
    let mode = stoat.frame_mode.as_str();

    // The palettes exist exactly while the minimap is on, per the refresh above.
    let minimap_chrome = stoat.minimap_palettes.as_ref().map(|palettes| {
        let thumb = {
            let bar = stoat.theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED);
            let [r, g, b] = paint::style_rgb(bar.bg).unwrap_or([90, 90, 110]);
            [r, g, b, 96]
        };
        // A terminal before the population palette drops a declaration past 64
        // entries, so it receives the prefix it reads.
        let table = &stoat.minimap_class_table;
        let declared = match stoat.stoatty_protocol >= emit::POPULATION_PALETTE_PROTOCOL {
            true => table.palette().len(),
            false => table.legacy_palette().len(),
        };
        let palette = &table.palette()[..declared];
        let diff_palette = &palettes.diff[..declared];
        MinimapChrome {
            workspace: stoat.active_workspace,
            content: &stoat.minimap_content,
            palette,
            dimmed_palette: palettes
                .dimmed
                .as_deref()
                .map_or(palette, |dimmed| &dimmed[..declared]),
            diff_palette,
            dimmed_diff_palette: palettes
                .diff_dimmed
                .as_deref()
                .map_or(diff_palette, |dimmed| &dimmed[..declared]),
            diff_changes_content: stoat
                .diff_changes_strips
                .get(&stoat.active_workspace)
                .map(|strip| strip.content_id()),
            thumb,
        }
    });

    let lsp_pending = crate::lsp::lsp_pending_label(stoat);
    let diff_warm_busy = stoat.diff_warm_busy();

    // Resolved here because the modal chain below cannot resolve it. active_modal
    // takes the whole &Stoat, and ws holds a &mut into it from the next line to
    // the end of the function. Nothing between here and the chain opens or closes
    // a modal, so the value still holds there.
    let painted_modal = active_modal(stoat);
    // Resolved here for the same reason. It reads the whole &Stoat, where the
    // search query it travels with is read below through a disjoint field.
    let search_smart_case = search::smart_case(stoat);
    // Resolved here for the same reason. Both constructors read the whole
    // &Stoat, and the dials are separate fields on it.
    let diff_dials = DiffDials::from_stoat(stoat);
    let preview_dials = commits::PreviewDials::from_stoat(stoat);
    // Resolved here for the same reason. It reads the whole &Stoat to find the
    // input that holds focus.
    let terminal_cursor_editor = stoat.focused_editor_ids().map(|(id, _)| id);

    let ws = &mut stoat.workspaces[stoat.active_workspace];

    ws.layout(size);

    // Synced ahead of the panes, because the bottom status rows lay their
    // segments out around the badges' boxes before the badges paint.
    badges::sync_agent_badge(&mut ws.badges, ws.agent.as_ref());
    badges::sync_trail_badge(&mut ws.badges, ws.trail.as_ref());
    badges::sync_walkthrough_badge(&mut ws.badges, ws.walkthrough.as_ref());
    let pinned = focused_pane_pinned(ws) || app::is_pinned_mode(mode);
    badges::sync_pinned_badge(&mut ws.badges, pinned);
    let badge_cover = badges::bottom_right_cover(&ws.badges, &stoat.badges, size);

    let screen = crate::keymap_state::view_predicate(ws);

    let overlay_pane = overlay_pane(ws);

    let workspace_name = workspace::display_name_of(&ws.name, &ws.git_root);

    // Reading which buffer is focused costs nothing, so the server list settles
    // from that rather than re-deriving its names every frame.
    let focused_buffer = {
        let focused = ws.panes.pane(ws.panes.focus());
        match &focused.view {
            View::Editor(editor_id) => ws.editors.get(*editor_id).map(|editor| editor.buffer_id),
            _ => None,
        }
    };
    let lsp_servers = stoat.lsp_server_list.refresh(
        focused_buffer,
        &ws.buffers,
        &stoat.lsp_registry,
        &stoat.lsp_progress,
    );

    // The status card is the only reader, so a closed one collects nothing.
    // Otherwise every frame sorts the in-flight entries for a card that is not
    // painting. Both read one value so the gate cannot drift off the card's own
    // condition.
    let lsp_status_open = stoat.lsp_status_pinned || stoat.lsp_badge_hovered;
    let lsp_progress_entries = match lsp_status_open {
        true => stoat.lsp_progress.entries_by_freshness(),
        false => Vec::new(),
    };

    // Resolved before the frame so the bars carry the text rather than a
    // borrow of the workspace, which the pane loop mutates as it paints.
    let diff_base_lead = ws.diff_base().and_then(pane::diff_base_lead);
    let diff_sides = diff::diff_sides(ws.diff_base());
    let pair_sides: HashMap<PathBuf, (String, String)> = ws
        .pairs()
        .map(|(path, pair)| (path.to_path_buf(), diff::pair_names(&pair.base_path, path)))
        .collect();
    let held_buffers = ws.held_buffer_labels();

    let frame = FrameCtx {
        workspace_name,
        workspace_root: &ws.git_root,
        mode,
        screen,
        theme: &stoat.theme,
        commits_split: stoat.commits_split,
        preview_dials,
        diff_changes_building: ws.tally_pending(),
        chrome: &stoat
            .chrome
            .as_ref()
            .expect("refresh_chrome ran at the top of the frame")
            .1,
        pending_count: stoat.pending_count,
        recording_register: stoat
            .macro_recording
            .as_ref()
            .map(|rec| rec.register.name()),
        held_buffers: &held_buffers,
        lsp_status_open,
        lsp_progress_entries: &lsp_progress_entries,
        spinner_phase: app::spinner_phase(stoat.spinner_clock),
        lsp_servers,
        diff_warm_busy,
        repo_change_counts: ws.repo_change_counts(),
        hunk_position: diff::repo_hunk_position(ws),
        diff_base: diff_base_lead.as_deref(),
        diff_sides: (&diff_sides.0, &diff_sides.1),
        pair_sides: &pair_sides,
        lsp_pending,
        lsp_message: stoat
            .lsp_message
            .as_ref()
            .map(|(typ, message)| (*typ, message.as_str())),
        status_message: stoat.pending_message.as_deref(),
        goto_word_labels: stoat.pending_goto_word.as_ref(),
        mode_label: pane::mode_segment(mode, &stoat.theme, &stoat.settings.mode_badges),
        diagnostics: &stoat.diagnostics,
        // Only a search prompt types a query. The regex prompt's pattern picks
        // selections inside the current ones, so the highlights stay on the
        // stored search, which is also what `n` walks.
        search_query: match status_prompt
            .as_ref()
            .filter(|_| stoat.search_input.is_some())
        {
            Some(prompt) => Some(prompt.text.as_str()),
            None => stoat.last_search.as_ref().map(|s| s.query.as_str()),
        },
        search_smart_case,
        status_prompt: status_prompt.as_ref(),
        terminal_cursor_editor,
        line_numbers: stoat
            .settings
            .editor_line_numbers
            .unwrap_or(LineNumbers::Relative),
        wrap_mode: stoat.settings.editor_wrap.unwrap_or(WrapMode::EditorWidth),
        wrap_column: stoat.settings.editor_wrap_column.unwrap_or(80).max(1),
        inactive_dim,
        diff_dials,
        images_capable: stoat.stoatty && stoat.stoatty_protocol >= 2,
        minimap_enabled: minimap_enabled && minimap_mode == MinimapMode::PerPane,
        minimap_chrome,
        minimap_band: single_minimap_rect,
        badge_cover,
        hover_cell: stoat.hover_cell,
        home: stoat.home.as_deref(),
        #[cfg(feature = "perf")]
        perf: PerfSegment::capture(&stoat.perf),
    };

    if tab_bar_rows == 1 {
        let bar = Rect {
            x: full.x,
            y: full.y,
            width: full.width,
            height: 1,
        };
        pane::render_tab_bar(ws, bar, frame, buf, scene, &mut stoat.tab_bar_spans);
    } else {
        stoat.tab_bar_spans.clear();
    }

    let paint_generation = stoat.paint_generation;
    let workspace = stoat.active_workspace;
    let split_focused = ws.panes.focus();
    let mut lsp_badge_rect: Option<Rect> = None;
    for (id, pane) in ws.panes.split_panes() {
        let is_focused = matches!(ws.focus, FocusTarget::SplitPane) && id == split_focused;
        if Some(id) == overlay_pane {
            continue;
        }
        if pane.area.width == 0 || pane.area.height == 0 {
            continue;
        }

        let status_row = pane::status_row(
            pane,
            is_focused,
            frame,
            &mut ws.editors,
            &ws.buffers,
            &ws.terms,
        );
        if status_row.badge.is_some() {
            lsp_badge_rect = status_row.badge;
        }

        // Only an editor pane and a changes view pane are replayable. A run,
        // terminal or label pane reads state this never audited.
        let key = pane_cache::pane_cache_key(
            pane,
            is_focused,
            &mut ws.editors,
            &ws.buffers,
            &mut ws.diff_changes_views,
            frame,
            paint_generation,
            scene.live(),
        );
        if let Some(key) = key
            && let Some(entry) = stoat.pane_cache.get_mut(&(workspace, id))
            && entry.key == Some(key)
        {
            entry.replay(buf, scene, undercurls);
            pane::render_pane_status(
                pane,
                is_focused,
                frame,
                &mut ws.editors,
                &status_row,
                buf,
                scene,
            );
            scene.end_lane();
            continue;
        }

        let lane = content_lane(pane.index);
        let mark = pane_cache::PaintMark::take(scene, lane, undercurls);
        scene.set_lane(lane);
        pane::render_pane(
            pane,
            is_focused,
            PaneCtx {
                editors: &mut ws.editors,
                buffers: &ws.buffers,
                runs: &ws.runs,
                terms: &ws.terms,
                term_pool_grids: &ws.term_pool_grids,
                commit_lists: &mut ws.commit_lists,
                diff_changes_views: &mut ws.diff_changes_views,
            },
            frame,
            buf,
            scene,
            undercurls,
            &status_row,
        );
        scene.end_lane();
        stoat.pane_paints += 1;

        if let Some(key) = key {
            let (content, _) = pane::pane_areas(pane.area, frame.minimap_band);
            let captured = stoat
                .pane_cache
                .entry((workspace, id))
                .or_default()
                .capture_into(key, content, buf, scene, undercurls, mark);
            if captured {
                stoat.pane_captures += 1;
            }
        }
    }
    scene.set_lane(OVERLAY_LANE);
    stoat.lsp_badge_rect = lsp_badge_rect;
    if lsp_badge_rect.is_none() {
        stoat.lsp_badge_hovered = false;
    }

    // A pane id never returns, so an entry a close left behind would hold one
    // pane's cells for the process. Every close path meets this sweep, which is
    // why none of them removes an entry itself. The live set is a handful of
    // ids, so the walk costs less than a cell blend.
    let live: Vec<PaneId> = ws
        .pane_trees()
        .flat_map(|tree| tree.split_pane_ids())
        .collect();
    stoat
        .pane_cache
        .retain(|(ws_id, pane_id), _| *ws_id == workspace && live.contains(pane_id));

    // Single mode declares one strip over the reserved right-edge band for the
    // focused split pane's buffer or changes view. The scene re-stamps every
    // paint, so a focus switch to another buffer redeclares it. Any other view
    // leaves the band empty.
    if !modal_overlay
        && let (Some(band), Some(chrome)) = (single_minimap_rect, frame.minimap_chrome)
        && let Some((content_id, palette)) = single_band_content(ws, chrome)
    {
        let [tr, tg, tb, _] = chrome.thumb;
        Minimap {
            strip_id: SINGLE_MINIMAP_STRIP_ID,
            content_id,
            lines_per_cell: pane::MINIMAP_LINES_PER_CELL,
            max_columns: pane::MINIMAP_MAX_COLUMNS,
            bg: [0, 0, 0, 0],
            thumb: chrome.thumb,
            thumb_border: [tr, tg, tb],
            palette,
        }
        .render(band, buf, scene);
    }

    // Record each undercurl span's cells now, after the editor panes painted but
    // before the overlay stack, so the re-stamp can drop cells a later layer
    // repaints and never draw over an overlay covering a diagnostic.
    undercurl::snapshot_cells(buf, undercurls.spans_mut());

    pane::render_pane_dividers(&ws.panes.dividers(), &stoat.theme, buf, &mut *scene);

    if let Some(pane_id) = overlay_pane {
        let pane = ws.panes.pane(pane_id);
        let is_focused = matches!(ws.focus, FocusTarget::SplitPane) && ws.panes.focus() == pane_id;
        if screen == Some("rebase") {
            if let Some(state) = ws.rebase.as_mut() {
                rebase::render_rebase(pane, is_focused, state, frame, buf, &mut *scene);
            }
        } else if screen == Some("reword") {
            let reword_ctx = ws
                .rebase_active
                .as_ref()
                .and_then(|a| a.pause.as_ref())
                .and_then(|p| match p {
                    RebasePause::Reword {
                        cherry_picked_commit,
                        original_message,
                        input,
                    } => Some((
                        cherry_picked_commit.clone(),
                        original_message.clone(),
                        input.editor_id,
                    )),
                    _ => None,
                });
            if let Some((sha, orig, editor_id)) = reword_ctx
                && let Some(editor) = ws.editors.get_mut(editor_id)
            {
                reword::render_reword(
                    pane,
                    is_focused,
                    editor,
                    &sha,
                    &orig,
                    frame,
                    buf,
                    &mut *scene,
                );
            }
        } else if screen == Some("commit_message")
            && let Some(commit) = ws.commit_message.as_ref()
            && let Some(editor) = ws.editors.get_mut(commit.input.editor_id)
        {
            commit_message::render_commit_message(
                pane,
                is_focused,
                editor,
                &commit.staged,
                frame,
                buf,
                &mut *scene,
            );
        } else if screen == Some("rebase_conflict")
            && let Some(active) = ws.rebase_active.as_ref()
        {
            conflict::render_conflict(pane, is_focused, active, frame, buf, &mut *scene);
        }
    }

    for (dock_id, dock) in &ws.docks {
        if matches!(dock.visibility, DockVisibility::Hidden) {
            continue;
        }
        let is_focused = matches!(ws.focus, FocusTarget::Dock(id) if id == dock_id);
        if matches!(dock.visibility, DockVisibility::Minimized) {
            dock::render_dock_minimized(dock, is_focused, &stoat.theme, buf, &mut *scene);
        } else {
            dock::render_dock_open(
                dock,
                is_focused,
                PaneCtx {
                    editors: &mut ws.editors,
                    buffers: &ws.buffers,
                    runs: &ws.runs,
                    terms: &ws.terms,
                    term_pool_grids: &ws.term_pool_grids,
                    commit_lists: &mut ws.commit_lists,
                    diff_changes_views: &mut ws.diff_changes_views,
                },
                frame,
                buf,
                &mut *scene,
            );
        }
    }
    // Before the hover, so the card the slide points at is already placed when
    // the connector to it is emitted.
    walkthrough::render_slide(stoat, buf, &mut *scene);
    hover::render_hover(stoat, buf, &mut *scene);
    signature_help::render_signature_help(stoat, buf, &mut *scene);
    completion::render_completion(stoat, buf, &mut *scene);
    code_action::render_code_action(stoat, buf, &mut *scene);
    rename_input::render_rename_input(stoat, buf, &mut *scene);
    symbol_picker::render_symbol_picker(stoat, buf, &mut *scene);
    // The modals below paint editors of their own, and this is the same set the
    // pane path already rides down FrameCtx. Bound here because the workspace
    // borrow on the next line rules out rebuilding a FrameCtx.
    let chrome = &stoat
        .chrome
        .as_ref()
        .expect("refresh_chrome ran at the top of the frame")
        .1;
    let mode = stoat.frame_mode.as_str();
    let toggles = ToggleStates::read(stoat);
    let app = AppFlags::from_stoat(stoat);
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    badges::render_badges(
        &ws.badges,
        &stoat.badges,
        size,
        stoat.render_tick,
        &stoat.theme,
        buf,
        &mut *scene,
    );
    match painted_modal {
        Some(ActiveModal::Run) => {
            if let Some(run_id) = stoat.modal_run
                && let Some(run_state) = ws.runs.get(run_id)
            {
                run_pane::render_modal_run(run_state, &stoat.theme, full, buf, &mut *scene);
            }
        },
        Some(ActiveModal::QuitConfirm) => {
            if let Some(modal) = &stoat.quit_all_confirm {
                quit_all_confirm::render_quit_all_confirm(
                    modal,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "quit_confirm",
                    None,
                    Some("quit"),
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::WorkspacePicker) => {
            if let Some(picker) = &mut stoat.workspace_picker {
                workspace_picker::render_workspace_picker(
                    picker,
                    ws,
                    &stoat.theme,
                    chrome,
                    full,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "workspace_picker",
                    None,
                    Some("picker"),
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::Jumplist) => {
            if let Some(picker) = &mut stoat.jumplist_picker {
                let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::JumplistPicker);
                let split = modal_split_percent(&stoat.modal_split, ModalKind::JumplistPicker);
                jumplist_picker::render_jumplist_picker(
                    picker,
                    ws,
                    &stoat.theme,
                    chrome,
                    full,
                    zoom,
                    split,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "jumplist",
                    None,
                    None,
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::WalkthroughPicker) => {
            if let Some(picker) = &mut stoat.walkthrough_picker {
                let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::WalkthroughPicker);
                let split = modal_split_percent(&stoat.modal_split, ModalKind::WalkthroughPicker);
                walkthrough_picker::render_walkthrough_picker(
                    picker,
                    ws,
                    &stoat.theme,
                    chrome,
                    full,
                    zoom,
                    split,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "walkthrough_picker",
                    None,
                    None,
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::Diagnostics) => {
            if let Some(picker) = &mut stoat.diagnostics_picker {
                let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::DiagnosticsPicker);
                let split = modal_split_percent(&stoat.modal_split, ModalKind::DiagnosticsPicker);
                let git_root = ws.git_root.clone();
                diagnostics_picker::render_diagnostics_picker(
                    picker,
                    ws,
                    &git_root,
                    &stoat.theme,
                    chrome,
                    full,
                    zoom,
                    split,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "diagnostics",
                    None,
                    None,
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::CommitPicker) => {
            if let Some(picker) = &mut stoat.commit_picker {
                let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::CommitPicker);
                let split = modal_split_percent(&stoat.modal_split, ModalKind::CommitPicker);
                commit_picker::render_commit_picker(
                    picker,
                    ws,
                    &stoat.theme,
                    chrome,
                    full,
                    zoom,
                    split,
                    preview_dials,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "commit_picker",
                    None,
                    None,
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::Location) => {
            if let Some(picker) = &mut stoat.location_picker {
                let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::LocationPicker);
                let split = modal_split_percent(&stoat.modal_split, ModalKind::LocationPicker);
                let git_root = ws.git_root.clone();
                location_picker::render_location_picker(
                    picker,
                    ws,
                    &git_root,
                    &stoat.theme,
                    chrome,
                    full,
                    zoom,
                    split,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "location",
                    None,
                    Some("locations"),
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::FileFinder) => {
            if let Some(finder) = &mut stoat.file_finder {
                let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::FileFinder);
                let split = modal_split_percent(&stoat.modal_split, ModalKind::FileFinder);
                file_finder::render_file_finder(
                    finder,
                    ws,
                    stoat.home.as_deref(),
                    &stoat.theme,
                    chrome,
                    full,
                    zoom,
                    split,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "finder",
                    Some(finder.scope().context_name()),
                    None,
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::SymbolFinder) => {
            if let Some(finder) = &mut stoat.symbol_finder {
                symbol_finder::render_symbol_finder(
                    finder,
                    ws,
                    &stoat.theme,
                    stoat.theme_epoch,
                    chrome,
                    &stoat.language_registry,
                    full,
                    modal_zoom_steps(&stoat.modal_zoom, ModalKind::SymbolFinder),
                    modal_split_percent(&stoat.modal_split, ModalKind::SymbolFinder),
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "symbols",
                    None,
                    None,
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::CodeSearch) => {
            if let Some(finder) = &mut stoat.code_search {
                let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::CodeSearch);
                let split = modal_split_percent(&stoat.modal_split, ModalKind::CodeSearch);
                code_search::render_code_search(
                    finder,
                    ws,
                    &stoat.theme,
                    chrome,
                    full,
                    zoom,
                    split,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "code_search",
                    None,
                    None,
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::Palette) => {
            if let Some(palette) = &mut stoat.command_palette {
                let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::Palette);
                command_palette::render_command_palette(
                    palette,
                    ws,
                    stoat.home.as_deref(),
                    &stoat.theme,
                    chrome,
                    full,
                    zoom,
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "palette",
                    None,
                    None,
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::Help) => {
            if let Some(help) = &stoat.help {
                help::render_help(
                    help,
                    mode,
                    ws,
                    &stoat.theme,
                    chrome,
                    &stoat.settings.mode_badges,
                    full,
                    modal_zoom_steps(&stoat.modal_zoom, ModalKind::Help),
                    buf,
                    &mut *scene,
                );
                cached_modal_hints(
                    &mut stoat.hints_cache,
                    &stoat.keymap,
                    mode,
                    "help",
                    None,
                    None,
                    toggles,
                    &stoat.theme,
                    full,
                    buf,
                    &mut *scene,
                );
            }
        },
        // The transient text inputs paint no centered box. Search paints in the
        // focused pane's status bar, Rename in the popup section further down,
        // and SplitSelection, FilterSelections and ShellInput do not paint yet.
        // All of them leave the key-hints branch below to run exactly as
        // no-modal frames do.
        Some(
            ActiveModal::Rename
            | ActiveModal::Search
            | ActiveModal::SplitSelection
            | ActiveModal::FilterSelections
            | ActiveModal::ShellInput,
        )
        | None => {},
    }

    // A pinned chord repeats until Escape, so the box it auto-shows would sit
    // over the text for the whole run rather than for the one press an unpinned
    // chord takes. The toggle read below still forces it on.
    let pin_hidden = stoat.settings.ui_pin_hides_hints.unwrap_or(true)
        && (focused_pane_pinned(ws) || app::is_pinned_mode(mode));

    if !painted_modal.is_some_and(ActiveModal::paints_own_box)
        && mode != "space_pane_display"
        && ((!PRIMARY_MODES.contains(&mode) && !pin_hidden)
            || screen == Some("review")
            || screen == Some("conflict")
            || stoat.key_hints_visible)
    {
        // The space_pane_display chord paints its own digit badges below, and
        // the auto-shown hint box would only stack an eleven-row overlay over
        // them. The guard above keeps it suppressed even when the `?` key-hints
        // toggle is on.
        let flags = Flags::from_workspace(ws).with_app(app);
        let pane = pane_predicate(ws);
        let token = cursor_token(ws);
        let focus = focus_flags(ws, &stoat.diagnostics, &stoat.lsp_registry);
        let key = hints_cache_key(mode, screen, pane, &flags, token, &focus, None, None);

        if stoat.hints_cache.as_ref().map(|c| c.key) != Some(key) {
            // The conflict screen rides on normal mode, so scope to its own
            // `view == conflict` bindings. A terminal or agent pane at rest
            // sends every other key to its child, so it scopes to the bindings
            // that name its pane kind. A primary mode narrows to the chord
            // entries and the context-bound keys, because the whole editor
            // keymap is what the help screen is for. A chord sub-mode owns its
            // whole mode, so take them all.
            let state = StoatKeymapState::with_flags(mode, flags)
                .with_view(screen)
                .with_pane(pane)
                .with_token(token)
                .with_focus_flags(focus);
            let raw = if screen == Some("conflict") {
                stoat.keymap.scoped_bindings(&state, "view", "conflict")
            } else if let Some(kind @ ("terminal" | "agent")) = pane.filter(|_| mode == "normal") {
                stoat.keymap.scoped_bindings(&state, "pane", kind)
            } else {
                let keys = if PRIMARY_MODES.contains(&mode) {
                    stoat.keymap.context_bindings(&state, PRIMARY_MODES)
                } else {
                    stoat.keymap.active_keys(&state)
                };
                // Wheel gestures are left out. This overlay affords pressing
                // one of the keys it lists, which a notch is not. Width is also
                // the scarce resource here, because a large chord already fills
                // two columns and a third overflows any ordinary terminal. The
                // help screen reads the unfiltered set, so nothing is hidden
                // from a reader looking for it.
                keys.into_iter()
                    .filter(|(key, _)| key.wheel.is_none())
                    .map(|(key, actions)| (key.display_label(), actions))
                    .collect()
            };
            let bindings: Vec<(&str, String, Option<Toggle>)> = raw
                .iter()
                .map(|(key, actions)| {
                    (
                        key.as_str(),
                        binding_display_desc(actions),
                        toggle::binding_toggle(actions),
                    )
                })
                .collect();
            stoat.hints_cache = Some(hints::HintsCache::new(
                key,
                hints::group_by_action(&bindings),
            ));
        }

        let conflict_footer = if screen == Some("conflict") {
            let conflict_state = match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => ws.editors.get(id).and_then(|e| e.conflict_view.as_ref()),
                _ => None,
            };
            conflict_state.map(|state| {
                let total = state.doc.chunks.len();
                let resolved = state.doc.chunks.iter().filter(|c| c.auto.is_some()).count();
                let text = format!(
                    "file {}/{} {} · {} conflicts · {} resolved",
                    state.file_index + 1,
                    state.file_count,
                    state.rel_path,
                    total,
                    resolved,
                );
                hints::HintsFooter {
                    text,
                    style: stoat.theme.get(crate::theme::scope::UI_TEXT),
                }
            })
        } else {
            None
        };
        let footer = conflict_footer.as_ref();

        let hint_label = match screen {
            Some("conflict") => "conflict",
            _ => mode,
        };
        let cache = stoat.hints_cache.as_mut().expect("cache populated above");
        hints::render_hints_grouped(
            hint_label,
            cache,
            footer,
            toggles,
            &stoat.theme,
            full,
            buf,
            &mut *scene,
        );
    }

    if mode == "space_pane_display" {
        render_pane_id_badges(&stoat.theme, ws, buf, scene);
    }
    scene.end_lane();
}

/// The content store and palette the single band declares for the focused
/// split pane of `ws`, or `None` when that pane shows neither a buffer nor an
/// changes view.
///
/// An editor's strip shows its buffer, in the diff palette while the diff view
/// is on. A changes view's strip shows the workspace's changes view document,
/// always in the diff palette.
fn single_band_content<'a>(
    ws: &Workspace,
    chrome: MinimapChrome<'a>,
) -> Option<(u32, &'a [[u8; 3]])> {
    match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(editor_id) => {
            let editor = ws.editors.get(editor_id)?;
            let content = chrome.content.get(&(chrome.workspace, editor.buffer_id))?;
            let palette = match editor.diff_view {
                true => chrome.diff_palette,
                false => chrome.palette,
            };
            Some((content.content_id(), palette))
        },
        View::DiffChanges(_) => Some((chrome.diff_changes_content?, chrome.diff_palette)),
        _ => None,
    }
}

/// The scene lane of a pane's content, which holds its gutter, its minimap
/// strip, and the cards it pops out.
///
/// Each pane takes two lanes, so a cursor move that redraws only the status
/// row sends only that row. Lane 0 stays below every pane for the tab bar, and
/// [`OVERLAY_LANE`] above them.
pub(crate) fn content_lane(pane_index: u32) -> u32 {
    1 + 2 * pane_index
}

/// The scene lane of a pane's status row, which changes with every cursor move
/// while the content beside it stands.
pub(crate) fn status_lane(pane_index: u32) -> u32 {
    2 + 2 * pane_index
}

/// The pane that a rebase, reword, conflict, or commit message screen paints
/// over.
///
/// The screen covers the focused pane alone, so every other pane keeps
/// painting beside it. `None` while no such screen is up.
pub(crate) fn overlay_pane(ws: &Workspace) -> Option<PaneId> {
    match crate::keymap_state::view_predicate(ws) {
        Some("rebase" | "reword" | "rebase_conflict" | "commit_message") => Some(ws.panes.focus()),
        _ => None,
    }
}

/// Hash the keymap-state inputs that decide which bindings are active into a
/// cache key. An unchanged key means the same hints list, so the binding walk
/// and regrouping can be skipped.
#[allow(clippy::too_many_arguments)]
fn hints_cache_key(
    mode: &str,
    screen: Option<&str>,
    pane: Option<&str>,
    flags: &Flags,
    token: Option<Option<crate::lsp::LspSymbolKind>>,
    focus: &FocusFlags,
    modal: Option<&str>,
    scope: Option<&str>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    mode.hash(&mut hasher);
    screen.hash(&mut hasher);
    pane.hash(&mut hasher);
    flags.hash(&mut hasher);
    token.hash(&mut hasher);
    focus.hash(&mut hasher);
    modal.hash(&mut hasher);
    scope.hash(&mut hasher);
    hasher.finish()
}

/// Paint a modal's hint box, reusing `cache` when its mode, modal, and scope
/// are unchanged so the scoped keymap walk and regrouping run only on a key
/// miss.
///
/// `modal` names the keymap scope to walk, which is the modal's name in
/// config. It also titles the box unless `title` overrides it, for the modals
/// whose config name is not what a reader wants to see over the hints.
///
/// `scope` is the file finder's scope, so the box shows the bindings that
/// hold in it, such as Tab in the buffer picker. Every other modal passes
/// `None`.
#[allow(clippy::too_many_arguments)]
fn cached_modal_hints(
    cache: &mut Option<hints::HintsCache>,
    keymap: &crate::keymap::Keymap,
    mode: &str,
    modal: &'static str,
    scope: Option<&str>,
    title: Option<&str>,
    toggles: ToggleStates,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let key = hints_cache_key(
        mode,
        None,
        None,
        &Flags::default(),
        None,
        &FocusFlags::default(),
        Some(modal),
        scope,
    );
    if cache.as_ref().map(|c| c.key) != Some(key) {
        let state = StoatKeymapState::with_flags(mode, Flags::default())
            .with_modal(modal)
            .with_scope(scope);
        let raw = keymap.scoped_bindings(&state, "modal", modal);
        let bindings: Vec<(&str, String, Option<Toggle>)> = raw
            .iter()
            .map(|(key, actions)| {
                (
                    key.as_str(),
                    binding_display_desc(actions),
                    toggle::binding_toggle(actions),
                )
            })
            .collect();
        *cache = Some(hints::HintsCache::new(
            key,
            hints::group_by_action(&bindings),
        ));
    }
    let cache = cache.as_mut().expect("cache populated above");
    hints::render_hints_grouped(
        title.unwrap_or(modal),
        cache,
        None,
        toggles,
        theme,
        area,
        buf,
        scene,
    );
}

/// Paint a large digit badge centered on each split pane while the
/// `space_pane_display` chord is active, so a pane can be focused by its number.
///
/// Panes are numbered 1-9 then 0 for the tenth in `split_panes` layout order,
/// and panes past the tenth get no badge. The focused pane's badge is inverted
/// (fill and mark swapped) so the current pane reads highlighted. Each badge is
/// a bold [`Popover`], so a plain terminal draws the cell-fallback box and
/// stoatty floats the scaled bold glyph.
fn render_pane_id_badges(
    theme: &crate::theme::Theme,
    ws: &Workspace,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let accent = paint::style_rgb(theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED).bg)
        .unwrap_or([90, 90, 110]);
    let background =
        paint::style_rgb(theme.get(crate::theme::scope::UI_BACKGROUND).bg).unwrap_or([40, 44, 52]);
    let focused = ws.panes.focus();

    for (i, (pane_id, pane)) in ws.panes.split_panes().enumerate().take(10) {
        let Some(digit) = char::from_digit((i as u32 + 1) % 10, 10) else {
            continue;
        };
        if pane.area.width == 0 || pane.area.height == 0 {
            continue;
        }
        let scale = pane
            .area
            .width
            .saturating_sub(2)
            .min(pane.area.height.saturating_sub(2))
            .clamp(1, 4);
        let side = scale + 2;
        let rect = Rect::new(
            pane.area.x + pane.area.width.saturating_sub(side) / 2,
            pane.area.y + pane.area.height.saturating_sub(side) / 2,
            side,
            side,
        );

        let (fill, mark) = if pane_id == focused {
            (accent, background)
        } else {
            (background, accent)
        };
        let digit = digit.to_string();
        Popover {
            fill,
            border: mark,
            content_fg: mark,
            scale: scale as u8,
            offset: [0, 0],
            bold: true,
            content: &digit,
        }
        .render(rect, buf, scene);
    }
}

#[cfg(all(test, feature = "perf"))]
mod perf_tests {
    use super::{perf_label, PerfSegment};
    use crate::perf::PerfStats;
    use std::time::Duration;

    #[test]
    fn capture_is_none_until_a_frame_is_painted() {
        assert!(PerfSegment::capture(&PerfStats::default()).is_none());
    }

    #[test]
    fn capture_reads_paint_and_input_percentiles_in_micros() {
        let mut perf = PerfStats::default();
        perf.record_paint(Duration::from_micros(123));
        perf.record_input_to_publish(Duration::from_micros(456));
        let seg = PerfSegment::capture(&perf).expect("data recorded");
        assert_eq!(seg.last_paint_us, 123);
        assert_eq!(seg.p95_input_us, 456);
    }

    #[test]
    fn perf_label_formats_both_values() {
        let seg = PerfSegment {
            last_paint_us: 12,
            p95_input_us: 34,
        };
        assert_eq!(perf_label(seg), " paint 12us in-p95 34us ");
    }
}

#[cfg(test)]
mod themed_blank_tests {
    use super::{clear_themed, themed_blank};
    use crate::{app::Stoat, theme::Theme};
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};

    /// A theme with nothing to say about a channel leaves it at Reset, since the
    /// terminal's own default is the only answer left. That is the whole reason
    /// the blank is built from the theme rather than asserted to be opaque.
    #[test]
    fn a_channel_the_theme_omits_stays_reset() {
        let blank = themed_blank(&Theme::empty());

        assert_eq!(
            (blank.fg, blank.bg),
            (Color::Reset, Color::Reset),
            "an empty theme has nothing truer than the terminal default",
        );
    }

    /// A modal blanks its area before drawing a frame that fills nothing, so
    /// what the blank carries is what shows through the modal's empty rows.
    #[test]
    fn a_themed_clear_blanks_to_the_theme() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        buf[(1u16, 1u16)].set_char('x').set_fg(Color::Green);

        let theme = {
            let src = r##"theme t { ui.background.bg = "#102030"; ui.text.fg = "#405060"; }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        clear_themed(Rect::new(1, 1, 2, 1), &mut buf, &theme);

        assert_eq!(
            (buf[(1u16, 1u16)].symbol(), buf[(1u16, 1u16)].fg),
            (" ", Color::Rgb(0x40, 0x50, 0x60)),
            "the cleared cell lost its content and took the theme's text color",
        );
        assert_eq!(
            buf[(1u16, 1u16)].bg,
            Color::Rgb(0x10, 0x20, 0x30),
            "and the theme's background",
        );
        assert_eq!(
            buf[(0u16, 1u16)].bg,
            Color::Reset,
            "a cell outside the cleared rect is left alone",
        );
    }

    /// A cell that reaches the terminal at Reset is resolved against the
    /// hosting terminal's theme rather than stoat's, so the ambient screen
    /// stops tracking `:theme`. The modal case is checked separately because a
    /// modal's blank rows come from its own clear, not from the frame fill.
    #[test]
    fn a_painted_frame_leaves_no_reset_cell() {
        let mut h = Stoat::test();

        let buf = h.render_composited();
        assert!(
            buf.content
                .iter()
                .all(|cell| cell.bg != Color::Reset && cell.fg != Color::Reset),
            "every cell of a plain frame carries the theme's colors",
        );

        h.stoat.key_hints_visible = true;
        let buf = h.render_composited();
        assert!(
            buf.content
                .iter()
                .all(|cell| cell.bg != Color::Reset && cell.fg != Color::Reset),
            "and every cell of a frame with a box open over it",
        );
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::{hints_cache_key, Flags, FocusFlags};
    use crate::{app::Stoat, keymap_state::modal_predicate, location_picker::LocationEntry};

    /// The `modal` a frame painted, read back off the hints cache the painting arm
    /// populated.
    fn painted_modal_hints(h: &crate::test_harness::TestHarness) -> Option<u64> {
        h.stoat.hints_cache.as_ref().map(|c| c.key)
    }

    fn modal_hints_key(mode: &str, modal: &str, scope: Option<&str>) -> u64 {
        hints_cache_key(
            mode,
            None,
            None,
            &Flags::default(),
            None,
            &FocusFlags::default(),
            Some(modal),
            scope,
        )
    }

    /// The frame used to walk the modal fields in its own order, so a second modal
    /// opened over a first could be the one painted while keys still routed to the
    /// other. Both now derive from the same authority.
    #[test]
    fn the_painted_modal_is_the_one_keys_route_to() {
        let mut h = Stoat::test();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.render_composited();
        let mode = h.stoat.focused_mode().to_string();
        assert_eq!(
            painted_modal_hints(&h),
            Some(modal_hints_key(&mode, "finder", Some("all"))),
            "the finder alone paints the finder"
        );

        h.stoat.location_picker = Some(crate::action_handlers::lsp::open_location_picker(
            &mut h.stoat,
            vec![LocationEntry {
                path: std::path::PathBuf::from("/repo/a.rs"),
                offset: 0,
                line: 1,
                column: 1,
                text: "candidate".to_owned(),
                block: None,
            }],
        ));
        h.render_composited();

        assert_eq!(
            modal_predicate(&h.stoat),
            Some("location"),
            "the location picker outranks the finder for keys"
        );
        let mode = h.stoat.focused_mode().to_string();
        assert_eq!(
            painted_modal_hints(&h),
            Some(modal_hints_key(&mode, "location", None)),
            "and so it is what the frame paints"
        );
    }
}

#[cfg(test)]
mod lsp_filter_tests {
    use crate::{lsp::LspSymbolKind, test_harness::TestHarness};
    use std::sync::Arc;
    use stoat_action::registry;

    /// Render one frame and flatten the painted cells into searchable text.
    fn box_text(h: &mut TestHarness) -> String {
        let buf = h.render_composited();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Open "Foo bar" and return its buffer id and focused-editor workspace.
    fn open_foo_bar(h: &mut TestHarness) -> crate::buffer::BufferId {
        let root = std::path::PathBuf::from("/lsp");
        let path = root.join("a.rs");
        h.fake_fs().insert_file(&path, b"Foo bar");
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
        h.settle();
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            crate::pane::View::Editor(id) => ws.editors[id].buffer_id,
            _ => panic!("focused pane is not an editor"),
        }
    }

    /// The text a hints row shows for the registered action `name`.
    fn short_desc(name: &str) -> &'static str {
        registry::lookup(name)
            .unwrap_or_else(|| panic!("{name} is registered"))
            .def
            .short_desc()
    }

    #[test]
    fn space_lsp_box_filters_rows_by_cursor_symbol_kind() {
        let mut h = TestHarness::with_size(150, 50);
        let buffer_id = open_foo_bar(&mut h);

        {
            let ws = h.stoat.active_workspace_mut();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let snapshot = buffer.read().unwrap().snapshot.clone();
            let start = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Right)[0];
            let end = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Left)[0];
            let kinds = Arc::from(vec![
                (start(0usize)..end(3usize), LspSymbolKind::Trait),
                (start(4usize)..end(7usize), LspSymbolKind::Function),
            ]);
            ws.buffers.store_lsp_symbol_kinds(buffer_id, kinds);
        }

        let goto_rows_list = |text: &str| {
            text.contains(short_desc("GotoTypeDefinition"))
                && text.contains(short_desc("GotoImplementation"))
        };

        // The cursor starts on "Foo", the trait.
        h.type_keys("space l");
        let over_trait = box_text(&mut h);
        assert!(
            over_trait.contains("implementor of the trait"),
            "the implementors row shows over a trait"
        );
        assert!(
            !over_trait.contains("caller of the symbol"),
            "the caller row is hidden over a trait"
        );
        assert!(
            goto_rows_list(&over_trait),
            "the Helix goto rows list over every token kind"
        );

        // Move onto "bar", the function.
        h.type_keys("escape");
        h.type_keys("l l l l");
        h.type_keys("space l");
        let over_function = box_text(&mut h);
        assert!(
            over_function.contains("caller of the symbol"),
            "the caller row shows over a function"
        );
        assert!(
            !over_function.contains("implementor of the trait"),
            "the implementors row is hidden over a function"
        );
        assert!(
            goto_rows_list(&over_function),
            "the Helix goto rows list over every token kind"
        );
    }

    /// The normal-mode box, forced open, lists the chords that lead on from
    /// here, not the whole editor keymap that the help screen holds.
    #[test]
    fn the_forced_normal_box_lists_chords_not_the_editor_keys() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);
        h.stoat.key_hints_visible = true;

        let text = box_text(&mut h);
        let move_down = registry::lookup("MoveDown")
            .expect("MoveDown is registered")
            .def
            .short_desc();
        assert_eq!(
            (
                text.contains("space mode"),
                text.contains("goto mode"),
                text.contains(move_down),
            ),
            (true, true, false),
            "the box lists the chord entries and leaves out the editor keys:\n{text}",
        );
    }

    #[test]
    fn the_forced_normal_box_lists_the_editor_pane_keys() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);
        h.stoat.key_hints_visible = true;

        let text = box_text(&mut h);
        assert_eq!(
            (
                text.contains(short_desc("ToggleComments")),
                text.contains(short_desc("RunInterrupt")),
            ),
            (true, false),
            "the box lists the editor pane's Ctrl-c and not the run pane's:\n{text}",
        );
    }

    #[test]
    fn the_forced_insert_box_over_a_run_pane_lists_the_run_keys() {
        let mut h = TestHarness::with_size(150, 50);
        h.open_run();
        h.stoat.key_hints_visible = true;

        let text = box_text(&mut h);
        let listed = [
            "RunSubmit",
            "RunHistoryPrev",
            "RunHistoryNext",
            "RunInterrupt",
        ]
        .map(|name| text.contains(short_desc(name)));
        assert_eq!(
            listed, [true; 4],
            "the box lists every run pane key:\n{text}"
        );
    }

    /// A run pane and a label pane in normal mode give the box the same mode,
    /// view, token, and focus flags. Only the pane kind tells their lists apart.
    #[test]
    fn the_box_follows_focus_to_a_pane_of_another_kind() {
        let mut h = TestHarness::with_size(150, 50);
        h.open_run();
        h.type_keys("escape");
        h.stoat.key_hints_visible = true;
        let over_run = box_text(&mut h);

        let ws = h.stoat.active_workspace_mut();
        let focus = ws.panes.focus();
        ws.panes.pane_mut(focus).view = crate::pane::View::Label("label".into());
        let over_label = box_text(&mut h);

        let interrupt = short_desc("RunInterrupt");
        assert_eq!(
            (over_run.contains(interrupt), over_label.contains(interrupt)),
            (true, false),
            "the run pane's Ctrl-c leaves the box with the run pane:\n{over_label}",
        );
    }

    /// The select, goto, view, bracket, and match chords act on an editor, and
    /// so do the space menus' position save, clipboard, buffer close, and symbol
    /// hops. A run pane lists none of them.
    #[test]
    fn the_editor_keys_list_only_over_an_editor() {
        let mut h = TestHarness::with_size(150, 50);
        h.stoat.key_hints_visible = true;
        let chords = [
            "select mode",
            "goto mode",
            "z mode",
            "bracket_next mode",
            "bracket_prev mode",
            "match mode",
        ];
        let menu_rows = [
            ("space", short_desc("SaveSelection")),
            ("space", "space_clipboard mode"),
            ("space b", short_desc("CloseBuffer")),
            ("space l", short_desc("GotoReferences")),
            ("space l", short_desc("GotoImplementors")),
            ("space l", short_desc("GotoCaller")),
            ("space l", short_desc("GotoCallee")),
        ];
        let listed = |h: &mut TestHarness| {
            let normal = box_text(h);
            let menus = menu_rows.map(|(keys, row)| chord_rows(h, keys).iter().any(|r| r == row));
            (chords.map(|row| normal.contains(row)), menus)
        };
        let over_editor = listed(&mut h);

        h.open_run();
        h.type_keys("escape");
        assert_eq!(
            [over_editor, listed(&mut h)],
            [([true; 6], [true; 7]), ([false; 6], [false; 7])],
            "a scratch editor, then a run pane",
        );
    }

    /// The buffer close refuses unsaved edits, so `space b` lists it only while
    /// the focused buffer is clean.
    #[test]
    fn the_buffer_box_lists_the_close_only_for_a_clean_buffer() {
        let mut h = TestHarness::with_size(150, 50);
        let close = short_desc("CloseBuffer");
        let lists_close = |rows: Vec<String>| rows.iter().any(|row| row == close);
        let clean = lists_close(chord_rows(&mut h, "space b"));

        h.type_keys("i x escape");
        assert_eq!(
            (clean, lists_close(chord_rows(&mut h, "space b"))),
            (true, false),
            "a clean scratch buffer, then one with an unsaved edit",
        );
    }

    #[test]
    fn the_git_chord_box_lists_the_pair_diff_only_with_a_pair() {
        let mut h = TestHarness::with_size(150, 50);
        h.stoat.set_focused_mode("space_git".into());
        let alone = box_text(&mut h);
        h.stoat.set_focused_mode("normal".into());

        h.open_side_by_side(("/pair/a.txt", "a\n"), ("/pair/b.txt", "b\n"));
        h.stoat.set_focused_mode("space_git".into());
        let paired = box_text(&mut h);

        let row = short_desc("DiffPair");
        assert_eq!(
            (alone.contains(row), paired.contains(row)),
            (false, true),
            "the pair diff row follows the layout:\n{paired}",
        );
    }

    /// The text of each row the box of the chord `keys` lists.
    fn chord_rows(h: &mut TestHarness, keys: &str) -> Vec<String> {
        h.type_keys(keys);
        h.render_composited();
        let rows = h
            .stoat
            .hints_cache
            .as_ref()
            .unwrap_or_else(|| panic!("{keys} laid out its box"))
            .rows
            .iter()
            .map(|row| row.action.clone())
            .collect();
        h.type_keys("escape");
        rows
    }

    #[test]
    fn the_git_chord_box_lists_git_rows_only_inside_a_repository() {
        let mut h = TestHarness::with_size(150, 50);
        let scratch = chord_rows(&mut h, "space G");

        h.stage_review_scenario("/repo", &[("a.rs", "a\n", "b\n")]);
        h.type_keys("escape");
        let staged = chord_rows(&mut h, "space G");

        let listed = |rows: &[String], names: &[&str]| -> Vec<bool> {
            names
                .iter()
                .map(|name| rows.iter().any(|row| row == short_desc(name)))
                .collect()
        };
        assert_eq!(
            (
                listed(&scratch, &["ReloadAll", "LiveReload", "ChangeWalkWrap"]),
                listed(&scratch, &["GitLs", "ToggleStageHunk", "GotoNextChange"]),
                listed(&staged, &["GitLs"]),
            ),
            (vec![true; 3], vec![false; 3], vec![true]),
            "a scratch root lists only the ungated rows, and a repository brings GitLs back",
        );
    }

    #[test]
    fn the_pane_chord_box_lists_layout_keys_only_with_a_layout() {
        let mut h = TestHarness::with_size(150, 50);
        let lists = |rows: &[String], text: &str| rows.iter().any(|row| row == text);
        let ungated = |rows: &[String]| {
            [
                "SplitRight",
                "SplitDown",
                "OpenFileFinderHSplit",
                "OpenFileFinderVSplit",
            ]
            .map(short_desc)
            .into_iter()
            .chain(["space_pane_nav_new mode", "space_pane_display mode"])
            .all(|text| lists(rows, text))
        };
        let gated = |rows: &[String]| {
            [
                "ClosePane",
                "CloseOtherPanes",
                "TogglePaneWiden",
                "FocusNext",
                "DetachPane",
                "ReattachPane",
            ]
            .map(|name| lists(rows, short_desc(name)))
        };
        let one_pane = chord_rows(&mut h, "space a");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let split = chord_rows(&mut h, "space a");

        h.stoat.window_ipc_connected = true;
        let linked = chord_rows(&mut h, "space a");

        // A third pane leaves two split panes beside the detached one, so only
        // the detach gates the layout rows.
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DetachPane);
        let detached = chord_rows(&mut h, "space a");

        assert_eq!(
            [one_pane, split, linked, detached].map(|rows| (ungated(&rows), gated(&rows))),
            [
                (true, [false, false, false, false, false, false]),
                (true, [true, true, true, true, false, false]),
                (true, [true, true, true, true, true, false]),
                (true, [false, false, false, false, false, true]),
            ],
            "close, close others, widen, focus, detach, and reattach rows on one pane, \
             a split, a split with the window link, and a detached pane",
        );
    }

    /// The workspace picker lists every published diagnostic, so its row lists
    /// only once one exists, whether or not the focused buffer holds it.
    #[test]
    fn the_space_lsp_box_lists_the_workspace_picker_only_with_a_diagnostic() {
        let mut h = TestHarness::with_size(150, 50);
        let picker = short_desc("OpenWorkspaceDiagnosticsPicker");
        let lists_picker = |rows: Vec<String>| rows.iter().any(|row| row == picker);
        let clean = lists_picker(chord_rows(&mut h, "space l"));

        h.seed_diagnostics("/lsp/a.rs", vec![lsp_types::Diagnostic::default()]);
        assert_eq!(
            (clean, lists_picker(chord_rows(&mut h, "space l"))),
            (false, true),
        );
    }

    /// The implementation jump asks a language server about the token under
    /// the cursor. Its row lists only in a served buffer, and once the buffer
    /// has a token index, only over a token.
    #[test]
    fn the_goto_box_lists_the_implementation_jump_only_where_a_server_answers() {
        let mut h = TestHarness::with_size(150, 50);
        let jump = short_desc("GotoImplementation");
        let lists_jump = |rows: Vec<String>| rows.iter().any(|row| row == jump);
        let scratch = lists_jump(chord_rows(&mut h, "g"));

        let buffer_id = open_foo_bar(&mut h);
        let served = lists_jump(chord_rows(&mut h, "g"));

        h.stoat
            .active_workspace_mut()
            .buffers
            .store_lsp_symbol_kinds(buffer_id, Arc::from(Vec::new()));
        assert_eq!(
            [scratch, served, lists_jump(chord_rows(&mut h, "g"))],
            [false, true, false],
            "a scratch buffer, a served one, then one whose index puts no token \
             under the cursor",
        );
    }

    /// The recall key reopens the last picker, so its row lists only once a
    /// picker has opened.
    #[test]
    fn the_space_box_lists_the_recall_only_after_a_picker_opened() {
        let mut h = TestHarness::with_size(150, 50);
        let recall = short_desc("OpenLastPicker");
        let lists_recall = |rows: Vec<String>| rows.iter().any(|row| row == recall);
        let fresh = lists_recall(chord_rows(&mut h, "space"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.type_keys("escape");
        assert_eq!(
            (fresh, lists_recall(chord_rows(&mut h, "space"))),
            (false, true),
        );
    }

    /// Closing the only workspace is refused, so its row lists only once a
    /// second workspace gives the close somewhere to land.
    #[test]
    fn the_workspace_chord_box_lists_close_only_with_another_workspace() {
        let mut h = TestHarness::with_size(150, 50);
        let close = short_desc("CloseWorkspace");
        let lists_close = |rows: Vec<String>| rows.iter().any(|row| row == close);
        let alone = lists_close(chord_rows(&mut h, "space w"));

        h.type_action("NewWorkspace()");
        assert_eq!(
            (alone, lists_close(chord_rows(&mut h, "space w"))),
            (false, true),
        );
    }

    #[test]
    fn space_lsp_box_shows_all_rows_without_an_index() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);

        h.type_keys("space l");
        let text = box_text(&mut h);
        assert!(
            text.contains("implementor of the trait") && text.contains("caller of the symbol"),
            "a buffer with no symbol-kind index shows every row"
        );
    }

    #[test]
    fn space_lsp_box_gates_the_lsp_and_diagnostic_rows() {
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

        // The whole text, since the workspace picker's row starts the same way.
        let picker_row = "open the diagnostics picker for the focused buffer";
        let steps = |text: &str| {
            [
                text.contains("jump to the next diagnostic"),
                text.contains("jump to the previous diagnostic"),
            ]
        };

        let mut h = TestHarness::with_size(150, 50);

        // The initial scratch buffer has no language and no path, so no server
        // serves it and it carries no diagnostics.
        h.type_keys("space l");
        let scratch = box_text(&mut h);
        assert!(
            scratch.contains("caller of the symbol"),
            "token rows fail open without an index"
        );
        assert!(
            !scratch.contains("format document via LSP"),
            "the format row is hidden without a language server"
        );
        assert_eq!(
            steps(&scratch),
            [false, false],
            "the diagnostic steps hide without a diagnostic"
        );
        assert!(
            !scratch.contains(picker_row),
            "the picker row is hidden without a diagnostic"
        );

        // The sole fake client serves the rust buffer, so lsp rows appear.
        h.type_keys("escape");
        open_foo_bar(&mut h);
        h.type_keys("space l");
        let served = box_text(&mut h);
        assert!(
            served.contains("format document via LSP"),
            "the format row shows once a server serves the buffer"
        );
        assert_eq!(
            steps(&served),
            [false, false],
            "the diagnostic steps hide in a served buffer without a diagnostic"
        );
        assert!(
            !served.contains(picker_row),
            "the picker row stays hidden until a diagnostic exists"
        );

        h.type_keys("escape");
        h.seed_diagnostics(
            std::path::PathBuf::from("/lsp/a.rs"),
            vec![Diagnostic {
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                severity: Some(DiagnosticSeverity::ERROR),
                code: None,
                code_description: None,
                source: None,
                message: String::new(),
                related_information: None,
                tags: None,
                data: None,
            }],
        );
        h.type_keys("space l");
        let with_diag = box_text(&mut h);
        assert!(
            with_diag.contains(picker_row),
            "the picker row shows once a diagnostic is seeded"
        );
        assert_eq!(
            steps(&with_diag),
            [true, true],
            "the diagnostic steps list once a diagnostic is seeded"
        );
    }
}

#[cfg(test)]
mod serialize_tests {
    use super::{serialize_buffer, serialize_rows};
    use crate::term_screen::{StyledCell, TermScreen};
    use ratatui::{
        backend::{Backend, CrosstermBackend},
        buffer::{Buffer, Cell},
        layout::Rect,
        style::{Color, Modifier, Style},
    };
    use unicode_width::UnicodeWidthStr;

    const BLANK_FG: Color = Color::Rgb(200, 200, 200);
    const BLANK_BG: Color = Color::Rgb(10, 20, 30);

    /// The cell a wide glyph covers prints nothing, so the cells after the
    /// glyph keep the columns the buffer gave them.
    #[test]
    fn cells_after_a_wide_glyph_keep_their_columns() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 2));
        buf.set_string(0, 0, "a", Style::default());
        buf.set_string(0, 1, "\u{4e2d}bcde", Style::default());

        let mut screen = TermScreen::new(2, 6);
        screen.feed(&serialize_buffer(&buf));

        assert_eq!(screen.text(), "a\n\u{4e2d}bcde");
    }

    #[test]
    fn a_page_with_trailing_blanks_projects_as_the_cell_by_cell_page_does() {
        let mut buf = themed_page(3, 12);
        buf.set_string(0, 0, "let x", Style::new().fg(Color::Rgb(220, 220, 100)));
        buf.set_string(
            5,
            0,
            "!",
            Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        );
        buf.set_string(0, 1, "\u{4e2d}ab", Style::new().fg(Color::Indexed(5)));
        buf.set_style(
            Rect::new(4, 1, 8, 1),
            Style::new().bg(Color::Rgb(60, 60, 60)),
        );

        assert_eq!(
            composite(&serialize_buffer(&buf), &buf),
            composite(&backend_bytes(&buf), &buf),
        );
    }

    #[test]
    fn a_blank_row_is_one_move_and_one_erase() {
        let mut buf = themed_page(2, 8);
        buf.set_string(0, 0, "ab", Style::new());

        assert_eq!(
            String::from_utf8_lossy(&serialize_rows(&buf, 1..2)),
            "\x1b[2;1H\x1b[38;2;200;200;200;48;2;10;20;30m\x1b[K\x1b[39m\x1b[49m\x1b[59m\x1b[0m",
        );
    }

    #[test]
    fn an_erase_after_an_attribute_resets_it_first() {
        let mut buf = themed_page(1, 4);
        buf.set_string(0, 0, "x", Style::new().add_modifier(Modifier::BOLD));

        assert_eq!(
            String::from_utf8_lossy(&serialize_buffer(&buf)),
            "\x1b[1;1H\x1b[1m\x1b[38;2;200;200;200;48;2;10;20;30mx\x1b[0m\
             \x1b[38;2;200;200;200;48;2;10;20;30m\x1b[K\x1b[39m\x1b[49m\x1b[59m\x1b[0m",
        );
    }

    #[test]
    fn a_zero_width_page_is_only_the_closing_reset() {
        assert_eq!(
            String::from_utf8_lossy(&serialize_buffer(&themed_page(2, 0))),
            "\x1b[39m\x1b[49m\x1b[59m\x1b[0m",
        );
    }

    #[test]
    fn a_page_with_no_trailing_blank_serializes_as_the_backend_does() {
        let mut buf = themed_page(3, 6);
        let modifiers = [
            Modifier::BOLD
                | Modifier::ITALIC
                | Modifier::UNDERLINED
                | Modifier::REVERSED
                | Modifier::CROSSED_OUT
                | Modifier::HIDDEN
                | Modifier::SLOW_BLINK,
            Modifier::DIM,
            Modifier::BOLD | Modifier::RAPID_BLINK,
            Modifier::empty(),
            Modifier::BOLD,
            Modifier::BOLD | Modifier::DIM,
        ];
        for (x, modifier) in (0..).zip(modifiers) {
            buf.set_string(x, 0, "m", Style::new().add_modifier(modifier));
        }
        buf.set_string(
            0,
            1,
            "\u{4e2d}",
            Style::new()
                .fg(Color::Red)
                .bg(Color::Reset)
                .underline_color(Color::Indexed(9))
                .add_modifier(Modifier::UNDERLINED),
        );
        buf.set_string(
            2,
            1,
            "q",
            Style::new().fg(Color::Reset).bg(Color::Indexed(17)),
        );
        buf.set_string(5, 1, " ", Style::new().add_modifier(Modifier::REVERSED));
        buf.set_string(3, 2, "end", Style::new().fg(Color::Gray));

        assert_eq!(
            String::from_utf8_lossy(&serialize_buffer(&buf)),
            String::from_utf8_lossy(&backend_bytes(&buf)),
        );
    }

    /// Every escape sequence the pen writes for itself matches the backend's.
    ///
    /// The page sits far from the origin, and two wide glyphs make the cursor
    /// jump past column 10000, so the moves carry four- and five-digit
    /// coordinates. The colors cover RGB, indexed, reset, and named values on
    /// all three layers, and the first row turns every attribute on and off.
    #[test]
    fn a_page_of_every_written_sequence_serializes_as_the_backend_does() {
        let (left, top) = (9_996, 65_530);
        let mut buf = Buffer::filled(Rect::new(left, top, 14, 3), Cell::EMPTY);
        for y in top..top + 3 {
            buf.set_string(left, y, "abcdefghijklmn", Style::new());
        }

        let modifiers = [
            Modifier::all(),
            Modifier::empty(),
            Modifier::BOLD | Modifier::DIM,
            Modifier::DIM,
            Modifier::BOLD,
            Modifier::empty(),
            Modifier::SLOW_BLINK,
            Modifier::RAPID_BLINK,
            Modifier::REVERSED,
            Modifier::HIDDEN,
            Modifier::CROSSED_OUT,
            Modifier::ITALIC | Modifier::UNDERLINED,
        ];
        for (x, modifier) in (left..).zip(modifiers) {
            buf[(x, top)].modifier = modifier;
        }

        let colors = [
            (Color::Rgb(0, 7, 255), Color::Indexed(200), Color::Reset),
            (
                Color::Indexed(5),
                Color::Rgb(10, 100, 250),
                Color::Rgb(1, 22, 133),
            ),
            (Color::Reset, Color::Reset, Color::Indexed(255)),
            (Color::Rgb(1, 22, 133), Color::Reset, Color::Reset),
            (Color::LightBlue, Color::Rgb(4, 5, 6), Color::LightRed),
            (Color::Reset, Color::Indexed(0), Color::Reset),
        ];
        for (x, (fg, bg, underline)) in (left..).zip(colors) {
            let cell = &mut buf[(x, top + 1)];
            (cell.fg, cell.bg, cell.underline_color) = (fg, bg, underline);
        }

        buf.set_string(left + 2, top + 2, "\u{4e2d}", Style::new());
        buf.set_string(left + 6, top + 2, "\u{4e2d}", Style::new());

        assert_eq!(
            String::from_utf8_lossy(&serialize_buffer(&buf)),
            String::from_utf8_lossy(&backend_bytes(&buf)),
        );
    }

    /// A page of `rows` by `cols` blanks in the theme's colors.
    fn themed_page(rows: u16, cols: u16) -> Buffer {
        let mut blank = Cell::EMPTY;
        blank.fg = BLANK_FG;
        blank.bg = BLANK_BG;
        Buffer::filled(Rect::new(0, 0, cols, rows), blank)
    }

    /// The cells of the grid of `buf` after `bytes` paint it, as a page
    /// composite reads them.
    ///
    /// A space with no modifier shows no foreground, so its foreground is left
    /// out.
    fn composite(bytes: &[u8], buf: &Buffer) -> Vec<StyledCell> {
        let mut screen = TermScreen::new(buf.area.height, buf.area.width);
        screen.feed(bytes);
        (0..usize::from(buf.area.height))
            .flat_map(|row| screen.row(row))
            .map(|cell| {
                if cell.ch == ' ' && cell.modifiers.is_empty() {
                    StyledCell { fg: None, ..cell }
                } else {
                    cell
                }
            })
            .collect()
    }

    /// The page bytes [`CrosstermBackend`] draws for `buf`, one cell at a time
    /// with no erase.
    ///
    /// A shown cell must go out as these bytes do, and a whole page must paint
    /// what these bytes paint.
    fn backend_bytes(buf: &Buffer) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut backend = CrosstermBackend::new(&mut bytes);
            let mut hidden = 0;
            let cells = buf.content.iter().enumerate().filter_map(|(i, cell)| {
                let (x, y) = buf.pos_of(i);
                if x == buf.area.x {
                    hidden = 0;
                }
                if hidden > 0 {
                    hidden -= 1;
                    return None;
                }
                hidden = cell.symbol().width().saturating_sub(1);
                Some((x, y, cell))
            });
            backend
                .draw(cells)
                .expect("a draw into a Vec<u8> never fails");
        }
        bytes
    }
}
