pub(crate) mod badges;
pub(crate) mod chrome;
pub(crate) mod code_action;
pub(crate) mod code_search;
pub(crate) mod command_palette;
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
pub(crate) mod git_status;
pub(crate) mod help;
pub(crate) mod hints;
pub(crate) mod hover;
pub(crate) mod jumplist_picker;
pub(crate) mod location_picker;
pub(crate) mod paint;
pub(crate) mod pane;
pub(crate) mod pane_cache;
pub(crate) mod pane_finder;
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
    action_handlers::{
        marks, movement,
        search::{self, StatusPrompt},
        surround, textobject, yank,
    },
    apc_emit,
    app::{
        self, modal_split_percent, modal_zoom_steps, ModalKind, Stoat, STATUS_ROW_PAD, TAB_BAR_PAD,
    },
    auto_reload::ReloadFlash,
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    commit_list::{CommitListId, CommitListState},
    conflict_session::{self, ConflictPosition},
    diff_changes::{DiffChangesId, DiffChangesState},
    editor_state::{EditorId, EditorState},
    keymap::KeymapState,
    keymap_state::{
        active_modal, binding_display_desc, cursor_token, focus_flags, focused_pane_pinned,
        only_enters_primary_mode, pane_predicate, ActiveModal, AppFlags, Flags, FocusFlags,
        StoatKeymapState,
    },
    lsp::{progress::LspProgressMap, registry::LspRegistry},
    minimap::{diff_view::DiffViewStrip, emit, EdgeClass, MinimapContent},
    pane::{Divider, DockVisibility, FocusTarget, Pane, PaneId, View},
    rebase::RebasePause,
    run::{RunId, RunState},
    term_session::{TermId, TermSession},
    toggle::{self, Toggle, ToggleStates},
    workspace::{
        self,
        diff::{self, HunkPosition, RepoStatus},
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
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
use slotmap::SlotMap;
use std::{
    borrow::Cow,
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_config::{LineNumbers, MinimapMode, WrapMode};
use stoat_widgets::{minimap::Minimap, popover::Popover, ApcScene};
use stoatty_protocol::{
    command::PopoverChrome, BACKDROP_PANEL_PROTOCOL, BATCHED_DECORATIONS_PROTOCOL,
};
use unicode_width::UnicodeWidthStr;

/// Strip id for the single-mode minimap, reserved above every pane index (which
/// are small and dense from zero). stoatty keys minimap strips in a namespace
/// separate from scroll pools, so `u32::MAX` never collides.
pub(crate) const SINGLE_MINIMAP_STRIP_ID: u32 = u32::MAX;

/// The bit that sets a Coverage Rail's strip id apart from its strip's.
///
/// Pane indexes stay far under it, so a pane's rail sets the bit. The single
/// band's id already sets it, so its rail clears it, and [`rail_strip_id`]
/// flips the bit rather than setting it.
pub(crate) const RAIL_STRIP_BIT: u32 = 1 << 31;

/// The strip id of the Coverage Rail beside strip `strip_id`.
pub(crate) fn rail_strip_id(strip_id: u32) -> u32 {
    strip_id ^ RAIL_STRIP_BIT
}

/// The scene lane for everything a frame draws after its split panes, such as
/// the dividers, the docks, and every overlay from a hover card to a modal.
///
/// stoatty composes lanes in ascending id order, and a panel occludes only what
/// was declared before it. An overlay therefore has to sit in a lane above
/// every pane's to cover them, and the highest id stays above any pane index.
pub(crate) const OVERLAY_LANE: u32 = u32::MAX;

/// The scene lane of every split pane's status row, which changes with every
/// cursor move while the panes stand.
///
/// Each row sits under its pane and covers none, so the lane only has to stay
/// under [`OVERLAY_LANE`], where a card docked above a row draws.
pub(crate) const STATUS_LANE: u32 = u32::MAX - 1;

/// The opacity of the minimap's viewport thumb, eighteen percent of 255.
///
/// The thumb takes the focus accent, so a light wash marks the viewport and
/// leaves the strip's lines readable under it.
const MINIMAP_THUMB_ALPHA: u8 = 46;

/// The opacity of the minimap thumb's outline, ninety percent of 255.
///
/// The fill alone loses the viewport's ends against a busy strip, so a strong
/// line in the same accent states where the viewport starts and stops.
const MINIMAP_THUMB_OUTLINE_ALPHA: u8 = 230;

/// The chrome of a Pane Picker badge, as the Ermine board draws it.
///
/// The board sets `border-radius: 10px`, `border: 2px solid`, and `box-shadow:
/// 0 10px 24px rgba(0,0,0,0.55)`. A 24 px blur is a gaussian of sigma 12,
/// which fades over 36 px, and 0.55 of 255 is 140.
const BADGE_CHROME: PopoverChrome = PopoverChrome {
    corner_radius: 10,
    border_width: 2,
    shadow_offset: [0, 10],
    shadow_margin: 36,
    shadow_alpha: 140,
};

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
    /// The strip of each diff view editor, which a diff view's strip shows in
    /// place of its buffer's content store.
    pub(crate) diff_view_strips: &'a HashMap<(WorkspaceId, EditorId), DiffViewStrip>,
    /// Syntax-scope palette a plain editor's strip declares and its run
    /// summaries index.
    pub(crate) palette: &'a [[u8; 3]],
    /// [`Self::palette`] pre-blended toward the background at the frame's fade,
    /// for the strip of a pane that fades.
    ///
    /// Every pane that fades blends the same palette the same way, so the frame
    /// does it once rather than each pane doing it again. Aliases
    /// [`Self::palette`] when no dim applies.
    pub(crate) dimmed_palette: &'a [[u8; 3]],
    /// The content store of the active workspace's changes view strip, which
    /// every changes view pane's strip and the single band over a focused
    /// changes view show. `None` while the workspace holds no changes view.
    pub(crate) diff_changes_content: Option<u32>,
    /// The Coverage Rail the changes view strip holds beside
    /// [`Self::diff_changes_content`], `None` while the stack fits the strip.
    pub(crate) diff_changes_rail: Option<u32>,
    /// Viewport-thumb fill color, rgba.
    pub(crate) thumb: [u8; 4],
    /// Viewport-thumb outline color, rgba.
    pub(crate) thumb_border: [u8; 4],
    /// The class whose runs every strip draws as an Item Rule hairline, the
    /// class table's [`EdgeClass::Rule`].
    pub(crate) rule_class: u8,
    /// The ground of a Coverage Rail, rgba, the background of
    /// `ui.minimap.rail` made opaque.
    pub(crate) rail_ground: [u8; 4],
    /// The block a Coverage Rail draws over the part of the file its strip
    /// covers, rgba, the foreground of `ui.minimap.rail` made opaque.
    pub(crate) rail_cover: [u8; 4],
}

impl MinimapChrome<'_> {
    /// The content store that editor `editor_id`'s strip shows and the
    /// Coverage Rail beside it, or `None` before the store exists.
    ///
    /// A diff view's strip shows the editor's display rows, with the rail its
    /// strip holds while the display does not fit. Any other editor's strip
    /// shows its buffer's lines, with the rail its buffer's store holds while
    /// the file does not fit.
    pub(crate) fn editor_strip(
        &self,
        editor_id: EditorId,
        editor: &EditorState,
    ) -> Option<(u32, Option<u32>)> {
        match editor.diff_view {
            true => self
                .diff_view_strips
                .get(&(self.workspace, editor_id))
                .map(|strip| (strip.content_id(), strip.held_rail_id())),
            false => self
                .content
                .get(&(self.workspace, editor.buffer_id))
                .map(|content| (content.content_id(), content.held_rail_id())),
        }
    }
}

/// One language server as the status bar shows it.
pub(crate) struct StatusServer {
    /// The abbreviation the server status popout paints, from
    /// [`pane::lsp_short_name`].
    pub(crate) short: String,
    /// The server's own name, which the status bar paints and the busy lookup
    /// is keyed by.
    pub(crate) name: String,
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
    /// Whether the tab bar takes the frame's top rows. The bar then leads with
    /// the workspace name, so a status row leaves the name out.
    pub(crate) tab_bar_shown: bool,
    /// Whether the chrome rows draw as bands, from [`Stoat::chrome_bands_on`].
    ///
    /// A band's island then reaches into the spare row the band takes.
    pub(crate) chrome_bands: bool,
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
    /// The active workspace's reload flashes, which every editor of a listed
    /// buffer washes over its changed text.
    ///
    /// Empty for a pooled page, since the wash paints on the live grid alone.
    pub(crate) reload_flashes: &'a [ReloadFlash],
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
    /// The repository's head, counts, and rebase state, which the focused
    /// pane's bar leads its staged segment with. `None` before the first diff
    /// lands and outside a repo, which drops the segment back to the focused
    /// file's own hunk counts.
    pub(crate) repo: Option<&'a RepoStatus>,
    /// Where the focused diff view stands among the repo's changed files and
    /// hunks, which the focused pane's bar names in its staged segment. `None`
    /// when the focused pane shows no diff view, which is what keeps the
    /// position off a pane with no hunks to walk.
    ///
    /// The bar prints `-` for a cursor above the focused file's first hunk, the
    /// one place the walk has nothing behind it.
    pub(crate) hunk_position: Option<HunkPosition>,
    /// Where the focused conflict view's cursor stands among the file's chunks,
    /// which the focused pane's bar names on the conflict screen. `None` when
    /// the focused pane shows no conflict view.
    pub(crate) conflict_position: Option<ConflictPosition>,
    /// What the bar names the workspace's diff base, when that is not the
    /// working tree. It replaces [`Self::repo`], which describes
    /// a working tree the pane is then not showing.
    pub(crate) diff_base: Option<&'a str>,
    /// The names of the diff's left and right sides, which a diff view's status
    /// row shows.
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
    /// The status-line mode chip's label and color, resolved once for the
    /// frame by [`pane::mode_segment`].
    ///
    /// The chip shows [`Self::mode`], except while a modal with a
    /// [`Self::modal_label`] is open. It then shows the `prompt` chip, since
    /// keys go to the modal's query and not to the mode under it.
    ///
    /// Resolving it builds a `ui.statusline.{mode}` scope string to probe the
    /// theme with, which each pane otherwise repeats for a mode that is the
    /// same for all of them. The label is a per-mode override from
    /// `Settings` when one exists, so a user-defined mode shows something more
    /// meaningful than `---`.
    pub(crate) mode_label: (&'a str, Color),
    /// The name of the picker modal open over the frame, from
    /// [`ActiveModal::label`], which the focused status row shows in place of
    /// its screen label.
    ///
    /// `None` while no modal is open, and while the open modal has no name.
    pub(crate) modal_label: Option<&'static str>,
    /// Workspace-wide LSP diagnostic store. The status bar reads the
    /// focused buffer's path and paints a compact severity badge when
    /// any diagnostics are present.
    pub(crate) diagnostics: &'a crate::diagnostics::DiagnosticSet,
    /// The open search or regex prompt, painted over the focused pane's
    /// status-bar left segments so the user reads the pattern as they type it.
    /// `None` whenever neither input is open.
    pub(crate) status_prompt: Option<&'a StatusPrompt>,
    /// The editor the terminal draws the primary cursor over this frame, which
    /// is the focused editor while no modal input holds focus. A pane whose
    /// editor is any other paints its own primary block.
    pub(crate) terminal_cursor_editor: Option<EditorId>,
    /// The rects where the terminal composites a resident grid pool over the
    /// live grid, by pool id, from [`apc_emit::composited_stacks`].
    ///
    /// A changes view whose stack paints into one of them leaves the stack's
    /// runs and bars out of the scene, since the pool's pages carry their own
    /// and the composite covers the live ones. Empty for every paint but the
    /// live frame's.
    pub(crate) composited_stacks: &'a [(u32, Rect)],
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
    /// The `editor.scrolloff` padding (default 3), which a diff view keeps as
    /// blank rows past its last row when a jump lands at its end.
    pub(crate) scrolloff: u32,
    /// Fraction an unfocused pane's colors blend toward the theme background,
    /// resolved from `ui.inactive_dim` (default 0.25, clamped to `0.0..=1.0`)
    /// and the `:pane-fade` session override by [`Stoat::inactive_dim`].
    /// `0.0` disables dimming. Applied by [`crate::render::pane::render_pane`]
    /// to unfocused panes only.
    pub(crate) inactive_dim: f32,
    /// Fraction every pane's colors blend toward the theme background while a
    /// modal paints its own box over them, resolved by [`Stoat::modal_dim`].
    /// `0.0` while no such modal is open.
    ///
    /// See also:
    /// - [`Self::pane_dim`] for the fade one pane takes from this and [`Self::inactive_dim`].
    pub(crate) modal_dim: f32,
    /// The diff view's paint dials, resolved from the session per
    /// [`DiffDials::from_stoat`]. Read only by diff-view panes.
    pub(crate) diff_dials: DiffDials,
    /// Whether the terminal can draw an image, which is a stoatty new enough to
    /// have the pass for it. An image pane says what is missing when it cannot.
    pub(crate) images_capable: bool,
    /// Whether the terminal reads `text_runs` and `bars` batches, which a
    /// stoatty at [`BATCHED_DECORATIONS_PROTOCOL`] or later does.
    ///
    /// The frame's own scene takes this before the panes paint. A page paints
    /// into a scene of its own, which takes it from here through
    /// [`ApcScene::set_batch_protocol`].
    pub(crate) batch_decorations: bool,
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
    /// Whether the panes lay out as islands this frame, as
    /// [`Stoat::islands_on`] answers. Each pane then declares its island halo,
    /// and the gaps draw no divider hairline.
    pub(crate) islands: bool,
    /// Whether the terminal reads a backdrop panel, which a stoatty at
    /// [`BACKDROP_PANEL_PROTOCOL`] or later does. An older one takes the flag
    /// as above every pool, so a backdrop it draws hides the pane's own glide.
    pub(crate) backdrop_panels: bool,
    /// The rect the pane tree lays its panes out in. An island whose side meets
    /// it reaches out into the window's chrome margin there.
    pub(crate) pane_root: Rect,
    /// The gaps between sibling panes, as
    /// [`PaneTree::dividers`](crate::pane::PaneTree::dividers) lays them out.
    /// An island whose side meets one reaches out into that gap.
    pub(crate) dividers: &'a [Divider],
    /// The span of the window's bottom row under a bottom-right badge's chip,
    /// from [`badges::bottom_right_cover`]. A status bar on that row keeps its
    /// segments left of it, since the chip paints over the bar.
    pub(crate) badge_cover: Option<Rect>,
    /// Terminal cell the mouse last rested over, or `None` when it has not
    /// moved over a pane. The focused editor resolves the diagnostic under it
    /// to raise a hover popover.
    pub(crate) hover_cell: Option<(u16, u16)>,
    /// The user home directory, for `~`-abbreviating run-pane prompt cwds.
    /// Resolved through [`crate::host::EnvHost`] so tests control it instead of
    /// the paint reading the real environment. `None` when `$HOME` is unset.
    pub(crate) home: Option<&'a Path>,
    /// The working directory of the run the focused pane shows, or `None` when
    /// no run pane holds focus.
    ///
    /// A run pane has no cursor to place, so its status row names this
    /// directory at its right end, and its mode chip reads `RUN` whatever mode
    /// its input is in. See [`pane::focused_run_cwd`].
    pub(crate) run_cwd: Option<&'a Path>,
    /// Latency readout for the status bar, or `None` before any frame has
    /// been painted. Present only under the `perf` feature.
    #[cfg(feature = "perf")]
    pub(crate) perf: Option<PerfSegment>,
}

impl FrameCtx<'_> {
    /// The fade a pane paints with this frame, per [`pane_fade`].
    pub(crate) fn pane_dim(&self, is_focused: bool) -> f32 {
        pane_fade(is_focused, self.inactive_dim, self.modal_dim)
    }
}

/// The fraction a pane's colors blend toward the theme background.
///
/// The fade is the larger of `inactive_dim`, which only an unfocused pane
/// takes, and `modal_dim`, which every pane takes. A modal fade over an
/// unfocused pane so deepens a lighter unfocused fade and keeps a deeper one.
pub(crate) fn pane_fade(is_focused: bool, inactive_dim: f32, modal_dim: f32) -> f32 {
    let unfocused = if is_focused { 0.0 } else { inactive_dim };
    unfocused.max(modal_dim)
}

/// A screen buffer whose every cell holds `blank` or a held capture, blanked
/// off the run loop ahead of the paint that reuses it.
pub(crate) struct BlankedBuffer {
    pub(crate) buffer: Buffer,
    pub(crate) blank: Cell,
    /// The pane captures the blank put back into `buffer`, each in its rect.
    pub(crate) holds: Vec<Hold>,
}

/// A pane's capture that a frame buffer carries, so the paint that takes the
/// buffer back replays that pane with no copy of its cells.
///
/// The frame that replays or captures a pane answers a hold for it, and the
/// hold rides the [`RenderFrame`](crate::ui::RenderFrame) to the UI thread and
/// back with the buffer. The blank job that takes the buffer copies each
/// hold's cells into its rect and blanks every other cell, so a held rect
/// holds the capture whatever painted over it in the buffer's last frame.
#[derive(Clone, Debug)]
pub(crate) struct Hold {
    pub(crate) pane: (WorkspaceId, PaneId),
    pub(crate) rect: Rect,
    /// Which capture of the pane's cache entry the cells are, as the render
    /// tick of the frame that took it.
    pub(crate) capture: u64,
    /// The capture's cells over `rect`, row-major.
    pub(crate) cells: Arc<Vec<Cell>>,
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

/// Reserve the `status_rows` bottom rows for the status rows of the bottom
/// panes, so the which-key hint box, bottom-anchored over the full-window right
/// edge, never paints over them.
///
/// A band's spare row counts, since the band's ground starts inside it.
pub(crate) fn hints_overlay_area(size: Rect, status_rows: u16) -> Rect {
    Rect {
        x: size.x,
        y: size.y,
        width: size.width,
        height: size.height.saturating_sub(status_rows),
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

/// The blank cell of the window ground, which every frame starts from.
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

/// Blanks every cell of `buffer` to `blank`, then copies each hold's capture
/// into its rect.
///
/// The run loop gives a published frame's buffer to a blocking-pool task that
/// calls this. The fill of a whole screen then runs while the newest frame is
/// on screen, not between a key and the frame it causes. The returned blank
/// lets the paint that takes the buffer back tell whether the theme still
/// matches, and the returned holds let it replay each held pane with no copy.
///
/// A hold whose rect leaves `buffer`, or whose cells do not fill the rect,
/// drops out, so a returned hold always names cells the buffer holds.
pub(crate) fn blank_buffer(mut buffer: Buffer, blank: Cell, mut holds: Vec<Hold>) -> BlankedBuffer {
    buffer.content.fill(blank.clone());
    holds.retain(|hold| {
        let (rect, width) = (hold.rect, hold.rect.width as usize);
        if buffer.area.intersection(rect) != rect
            || hold.cells.len() != width * rect.height as usize
        {
            return false;
        }
        for (row, y) in (rect.top()..rect.bottom()).enumerate() {
            let start = buffer.index_of(rect.left(), y);
            buffer.content[start..start + width]
                .clone_from_slice(&hold.cells[row * width..(row + 1) * width]);
        }
        true
    });
    BlankedBuffer {
        buffer,
        blank,
        holds,
    }
}

/// The foreground the theme gives ordinary text, for a caller that has nothing
/// more specific to fall back to.
pub(crate) fn themed_fg(theme: &crate::theme::Theme) -> Color {
    theme
        .try_get(crate::theme::scope::UI_TEXT)
        .and_then(|s| s.fg)
        .unwrap_or(Color::Reset)
}

/// The background the theme gives the window ground, or `None` for a theme
/// that sets no ground apart from the panes.
///
/// The ground is `ui.surface.ground`. A theme that sets only
/// `ui.surface.chrome` grounds the window on the chrome, so one color serves
/// as both its ground and the ink of its chips.
pub(crate) fn ground_surface(theme: &crate::theme::Theme) -> Option<Color> {
    [
        crate::theme::scope::UI_SURFACE_GROUND,
        crate::theme::scope::UI_SURFACE_CHROME,
    ]
    .into_iter()
    .find_map(|scope| theme.try_get(scope).and_then(|s| s.bg))
}

/// The background of the window ground, for a caller that has nothing more
/// specific to fall back to.
///
/// The ground is [`ground_surface`], and it shows in the gaps between panes,
/// since each pane clears its content to [`surface_blank`]. A theme with no
/// ground surface grounds the window on the editor surface, `ui.background`.
pub(crate) fn themed_bg(theme: &crate::theme::Theme) -> Color {
    ground_surface(theme)
        .or_else(|| {
            theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg)
        })
        .unwrap_or(Color::Reset)
}

/// The blank cell a pane's content starts from, which is the themed text color
/// over the editor surface, `ui.background`.
///
/// A channel the theme does not define stays at Reset, as in [`themed_blank`].
///
/// See also:
/// - [`clear_surface`] to blank a pane's content to it.
/// - [`themed_blank`] for the window ground around the panes.
pub(crate) fn surface_blank(theme: &crate::theme::Theme) -> Cell {
    let mut cell = Cell::EMPTY;
    cell.fg = themed_fg(theme);
    cell.bg = theme
        .try_get(crate::theme::scope::UI_BACKGROUND)
        .and_then(|s| s.bg)
        .unwrap_or(Color::Reset);
    cell
}

/// Blank `area` to the window ground, wiping whatever was painted under it.
///
/// This is what a surface lays down before it paints, in place of ratatui's
/// [`Clear`](ratatui::widgets::Clear). A cell left at Reset shows the
/// terminal's own background through a themed screen, so the blank carries the
/// theme's colors.
///
/// See also:
/// - [`clear_surface`] for a pane's content, which blanks to the editor surface.
/// - [`clear_modal`] for a modal box, which blanks to its own background.
pub(crate) fn clear_themed(area: Rect, buf: &mut Buffer, theme: &crate::theme::Theme) {
    fill_cells(area, buf, &themed_blank(theme));
}

/// Blank `area` to the editor surface, wiping whatever was painted under it.
///
/// A pane clears its content to this before its view paints. A view paints only
/// the cells it has content for, and the frame blanks to the window ground.
///
/// See also:
/// - [`clear_themed`] to blank to the window ground.
pub(crate) fn clear_surface(area: Rect, buf: &mut Buffer, theme: &crate::theme::Theme) {
    fill_cells(area, buf, &surface_blank(theme));
}

/// Blank a modal box's `area` to the modal's own blank, wiping whatever was
/// painted under it.
///
/// A modal's empty rows show whatever this leaves, so `style`, the modal's
/// `ui.modal.*` style, decides the surface a theme gives its modal boxes.
pub(crate) fn clear_modal(area: Rect, buf: &mut Buffer, theme: &crate::theme::Theme, style: Style) {
    fill_cells(area, buf, &modal_blank(theme, style));
}

/// The blank cell of a modal box, which is the themed blank on `style`'s
/// background.
///
/// A style that sets no background keeps the themed blank, so a theme that
/// gives its modals no surface of their own draws them on the window ground.
pub(crate) fn modal_blank(theme: &crate::theme::Theme, style: Style) -> Cell {
    let mut cell = themed_blank(theme);
    if let Some(bg) = style.bg {
        cell.bg = bg;
    }
    cell
}

/// The RGB color of a modal box's surface, which is `style`'s background, or the
/// window ground where it sets none, as [`modal_blank`] blanks to.
///
/// A text run over the box backs itself in this color, because a run backed in
/// any other color shows as a band behind its line.
pub(crate) fn modal_fill(theme: &crate::theme::Theme, style: Style) -> Option<[u8; 3]> {
    paint::style_rgb(Some(style.bg.unwrap_or_else(|| themed_bg(theme))))
}

fn fill_cells(area: Rect, buf: &mut Buffer, blank: &Cell) {
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
///
/// `holds` are the pane captures that `buf` carries, each in its rect, with
/// every other cell blank. A pane whose capture still replays leaves its held
/// cells in place, and every other held rect is blanked before a pane paints
/// over it. Answers a hold for each pane the frame replayed or captured.
///
/// Also answers each hold's rect and capture tick, for
/// [`RenderFrame::captures`](crate::ui::RenderFrame::captures), or none of
/// them when anything painted over the panes after they painted.
pub(crate) fn frame(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    undercurls: &mut UndercurlBatch,
    holds: Vec<Hold>,
) -> (Vec<Hold>, Vec<(Rect, u64)>) {
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

    let tab_bar_rows = stoat.tab_bar_rows();
    let status_rows = stoat.status_rows();
    let chrome_bands = stoat.chrome_bands_on();
    let islands = stoat.islands_on();
    let margin = stoat.island_margin();

    // Single mode reserves a strip band at the window's right edge and shrinks
    // the pane layout by the strip width, so the panes never overlap the strip.
    // The band stops above the status rows so a status bar on the last row runs
    // the full window width, and starts under the tab bar's rows when the bar
    // holds the top, since the band derives its own rect instead of going through
    // `layout_size`. The band is stamped on Stoat for the mouse handler, then
    // read back for this paint. It sits inside the island margin, and
    // `layout_size` leaves a gap column of the margin's width left of it, so
    // under islands the window reads margin, panes, gap, band, margin. While
    // no split pane shows an editor or a changes view, the band tracks no pane
    // and is not reserved, so `layout_size` gives its columns to the panes.
    stoat.active_workspace_mut().refresh_band_pane();
    let tracked = stoat.active_workspace().band_pane.is_some();
    stoat.single_minimap_rect = (minimap_mode == MinimapMode::Single
        && full.width >= editor::MINIMAP_MIN_PANE_COLS
        && tracked)
        .then(|| Rect {
            x: full.x + full.width - editor::MINIMAP_STRIP_COLS - margin,
            y: full.y + tab_bar_rows,
            width: editor::MINIMAP_STRIP_COLS,
            height: full.height.saturating_sub(status_rows + tab_bar_rows),
        });
    let single_minimap_rect = stoat.single_minimap_rect;
    let modal_overlay = modal_overlay_open(stoat);
    let size = stoat.layout_size();
    let inactive_dim = stoat.painted_inactive_dim();
    let modal_dim = stoat.modal_dim();

    // A per-pane strip takes its dimmed palette whenever its pane dims, so the
    // palette builds at the deeper of the two fades.
    stoat.refresh_minimap_palettes(minimap_enabled, inactive_dim.max(modal_dim));

    // Resolved before the field borrows below, because snapshotting the prompt
    // editor's display map takes the whole app mutably. Nothing between here
    // and the status bars opens or closes the prompt.
    let status_prompt = search::prompt_display(stoat);

    // The last whole-app borrows are behind us, so what the frame reads from
    // here on is field by field and can live alongside the workspace below.
    let mode = stoat.frame_mode.as_str();

    // The palettes exist exactly while the minimap is on, per the refresh above.
    let minimap_chrome = stoat.minimap_palettes.as_ref().map(|palettes| {
        let [r, g, b] = {
            let focused = stoat.theme.get(crate::theme::scope::UI_BORDER_FOCUSED);
            paint::style_rgb(focused.fg).unwrap_or([90, 90, 110])
        };
        let rail = stoat.theme.get(crate::theme::scope::UI_MINIMAP_RAIL);
        let [gr, gg, gb] = paint::style_rgb(rail.bg).unwrap_or([0, 0, 0]);
        let [cr, cg, cb] = paint::style_rgb(rail.fg).unwrap_or([90, 90, 110]);
        let palette = stoat.minimap_class_table.palette();
        MinimapChrome {
            workspace: stoat.active_workspace,
            content: &stoat.minimap_content,
            diff_view_strips: &stoat.diff_view_strips,
            palette,
            dimmed_palette: palettes.dimmed.as_deref().unwrap_or(palette),
            diff_changes_content: stoat
                .diff_changes_strips
                .get(&stoat.active_workspace)
                .map(|strip| strip.content_id()),
            diff_changes_rail: stoat
                .diff_changes_strips
                .get(&stoat.active_workspace)
                .and_then(|strip| strip.held_rail_id()),
            thumb: [r, g, b, MINIMAP_THUMB_ALPHA],
            thumb_border: [r, g, b, MINIMAP_THUMB_OUTLINE_ALPHA],
            rule_class: stoat.minimap_class_table.edge_class(EdgeClass::Rule),
            rail_ground: [gr, gg, gb, u8::MAX],
            rail_cover: [cr, cg, cb, u8::MAX],
        }
    });

    let lsp_pending = crate::lsp::lsp_pending_label(stoat);
    let diff_warm_busy = stoat.diff_warm_busy();

    // Resolved here because the modal chain below cannot resolve it. active_modal
    // takes the whole &Stoat, and ws holds a &mut into it from the next line to
    // the end of the function. Nothing between here and the chain opens or closes
    // a modal, so the value still holds there.
    let painted_modal = active_modal(stoat);
    let modal_label = painted_modal.and_then(ActiveModal::label);
    // Resolved here for the same reason. Both constructors read the whole
    // &Stoat, and the dials are separate fields on it.
    let diff_dials = DiffDials::from_stoat(stoat);
    let preview_dials = commits::PreviewDials::from_stoat(stoat);
    // Resolved here for the same reason. It reads the whole &Stoat to find the
    // input that holds focus.
    let terminal_cursor_editor = stoat.focused_editor_ids().map(|(id, _)| id);
    // Resolved here for the same reason. It reads the terminal's pools and the
    // protocol off the whole &Stoat.
    let composited_stacks = apc_emit::composited_stacks(stoat);
    // Resolved here for the same reason. It reads the window focus beside the
    // numbering settings off the whole &Stoat.
    let line_numbers = stoat.painted_line_numbers();

    let ws = &mut stoat.workspaces[stoat.active_workspace];

    ws.panes.row_gap = status_rows;
    ws.layout(size);

    // Synced ahead of the panes, because the bottom status rows lay their
    // segments out around the badges' chips before the badges paint.
    badges::sync_agent_badge(&mut ws.badges, ws.agent.as_ref());
    badges::sync_trail_badge(&mut ws.badges, ws.trail.as_ref());
    badges::sync_walkthrough_badge(&mut ws.badges, ws.walkthrough.as_ref());
    let pinned = focused_pane_pinned(ws) || app::is_pinned_mode(mode);
    badges::sync_pinned_badge(&mut ws.badges, pinned);
    // A top-right chip sits in the tab bar row and a bottom-right one on the
    // status row, both of which the pane layout leaves out, so the badges lay
    // out over the layout area stretched to the frame's top and bottom.
    let badge_area = Rect {
        y: full.y,
        height: full.height,
        ..size
    };
    let badge_cover = badges::bottom_right_cover(&ws.badges, &stoat.badges, badge_area);

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
    let repo_status = ws.repo_status();
    // The hints box over the diff view reads the position too, after the pane
    // loop, where no FrameCtx is left to carry it. So does the box over the
    // conflict view.
    let hunk_position = diff::repo_hunk_position(ws);
    let conflict_position = conflict_session::conflict_position(ws);
    let run_cwd = pane::focused_run_cwd(ws.focus, &ws.panes, &ws.runs);
    // The islands read where the gaps run, and the hairlines paint in them when
    // islands are off, so the frame lays them out once for both.
    let dividers = ws.panes.dividers();

    let frame = FrameCtx {
        workspace_name,
        tab_bar_shown: tab_bar_rows > 0,
        chrome_bands,
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
        reload_flashes: &ws.reload_flashes,
        lsp_status_open,
        lsp_progress_entries: &lsp_progress_entries,
        spinner_phase: app::spinner_phase(stoat.spinner_clock),
        lsp_servers,
        diff_warm_busy,
        repo: repo_status.as_ref(),
        hunk_position,
        conflict_position,
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
        mode_label: pane::mode_segment(
            match (modal_label, run_cwd) {
                (Some(_), _) => "prompt",
                (None, Some(_)) => "run",
                (None, None) => mode,
            },
            &stoat.theme,
            &stoat.settings.mode_badges,
        ),
        modal_label,
        diagnostics: &stoat.diagnostics,
        status_prompt: status_prompt.as_ref(),
        terminal_cursor_editor,
        composited_stacks: &composited_stacks,
        line_numbers,
        wrap_mode: stoat.settings.editor_wrap.unwrap_or(WrapMode::EditorWidth),
        wrap_column: stoat.settings.editor_wrap_column.unwrap_or(80).max(1),
        scrolloff: stoat.settings.scrolloff.unwrap_or(3),
        inactive_dim,
        modal_dim,
        diff_dials,
        images_capable: stoat.stoatty && stoat.stoatty_protocol >= 2,
        batch_decorations: stoat.stoatty_protocol >= BATCHED_DECORATIONS_PROTOCOL,
        minimap_enabled: minimap_enabled && minimap_mode == MinimapMode::PerPane,
        minimap_chrome,
        minimap_band: single_minimap_rect,
        islands,
        backdrop_panels: stoat.stoatty && stoat.stoatty_protocol >= BACKDROP_PANEL_PROTOCOL,
        pane_root: ws.panes.area(),
        dividers: &dividers,
        badge_cover,
        hover_cell: stoat.hover_cell,
        home: stoat.home.as_deref(),
        run_cwd,
        #[cfg(feature = "perf")]
        perf: PerfSegment::capture(&stoat.perf),
    };

    let paint_generation = stoat.paint_generation;
    let tick = stoat.render_tick;
    let workspace = stoat.active_workspace;
    let split_focused = ws.panes.focus();

    // A painter writes over a blank buffer and leaves the cells it skips. So
    // before any pane paints, the frame blanks each held rect whose pane is
    // gone, moved, under the overlay screen, or captured again since.
    let mut held = Vec::new();
    for hold in holds {
        let placed = (hold.pane.0 == workspace)
            .then(|| ws.panes.split_panes().find(|(id, _)| *id == hold.pane.1))
            .flatten()
            .is_some_and(|(id, pane)| Some(id) != overlay_pane && pane.area == hold.rect);
        let current = stoat
            .pane_cache
            .get(&hold.pane)
            .is_some_and(|entry| entry.is_held_by(&hold));
        match placed && current {
            true => held.push(hold),
            false => clear_themed(hold.rect, buf, &stoat.theme),
        }
    }
    let mut frame_holds = Vec::new();

    // The bar paints after the stale holds clear. On the frame a second tab
    // opens, the pane it pushes down still holds a rect over the bar's row. A
    // band keeps the bar's cells on its first row and nudges the bar down to
    // center it in 32 px. The bar draws in lane 0, so the nudge ends before
    // the panes paint there.
    if tab_bar_rows > 0 {
        let bar = Rect {
            x: full.x,
            y: full.y,
            width: full.width,
            height: 1,
        };
        if chrome_bands {
            scene.nudge([0, i16::from(TAB_BAR_PAD)]);
        }
        pane::render_tab_bar(ws, bar, frame, buf, scene, &mut stoat.tab_bar_spans);
        if chrome_bands {
            scene.nudge([0, 0]);
        }
    } else {
        stoat.tab_bar_spans.clear();
    }

    for (id, pane) in ws.panes.split_panes() {
        let is_focused = matches!(ws.focus, FocusTarget::SplitPane) && id == split_focused;
        if Some(id) == overlay_pane {
            continue;
        }
        if pane.area.width == 0 || pane.area.height == 0 {
            continue;
        }

        // An editor pane, a changes view pane, and a live terminal pane are
        // replayable. A run or label pane reads state this never audited.
        let key = pane_cache::pane_cache_key(
            pane,
            is_focused,
            &mut ws.editors,
            &ws.buffers,
            &mut ws.diff_changes_views,
            &ws.terms,
            &ws.term_pool_grids,
            frame,
            paint_generation,
            scene.live(),
        );
        let hold = held
            .iter()
            .position(|hold| hold.pane.1 == id)
            .map(|at| held.swap_remove(at));
        // A terminal pane's replay paints again the rows its screen damaged
        // since the capture, and the capture keeps its cells, so a held
        // buffer still puts them back with no copy. When the screen does not
        // name its damage, the pane takes the whole paint and a new capture.
        let terminal = match pane.view {
            View::Agent(term_id) | View::Terminal(term_id) => ws.terms.get(term_id),
            _ => None,
        };
        let damaged = terminal
            .zip(stoat.pane_cache.get(&(workspace, id)))
            .map(|(agent, entry)| agent.term.damaged_rows_since(entry.generation));
        if let Some(key) = key
            && !matches!(damaged, Some(None))
            && let Some(entry) = stoat.pane_cache.get_mut(&(workspace, id))
            && entry.key == Some(key)
        {
            entry.replay(buf, scene, undercurls, hold.is_none());
            if hold.is_some() {
                stoat.pane_replays_held += 1;
            }
            if let (Some(agent), Some(Some(mut rows))) = (terminal, damaged)
                && rows.contains(&true)
            {
                let content = pane.area;
                term_pane::repaint_term_rows(agent, frame, content, is_focused, buf, &mut rows);
            }
            frame_holds.push(entry.hold((workspace, id)));
            if let View::DiffChanges(view) = pane.view
                && let Some(state) = ws.diff_changes_views.get_mut(view)
            {
                diff_changes::replay_cursor(pane, state, &mut ws.editors, frame, scene.live());
            }
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
        );
        scene.end_lane();
        stoat.pane_paints += 1;

        if let Some(key) = key {
            let content = pane.area;
            let generation = terminal.map_or(0, |agent| agent.term.generation());
            let entry = stoat.pane_cache.entry((workspace, id)).or_default();
            if entry.capture_into(key, content, buf, scene, undercurls, mark, tick, generation) {
                stoat.pane_captures += 1;
                frame_holds.push(entry.hold((workspace, id)));
            }
        }
    }
    scene.set_lane(OVERLAY_LANE);

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

    // The band shows a buffer's minimap, which sits on the editor surface as a
    // pane's content does, so the band clears to that surface and not to the
    // window ground. Under islands it stands apart from the panes as an island
    // of its own.
    if let Some(band) = single_minimap_rect {
        clear_surface(band, buf, &stoat.theme);
        if frame.islands && scene.live() {
            pane::render_band_island(band, frame, scene);
        }
    }

    // Single mode declares one strip over the reserved right-edge band for the
    // tracked split pane's buffer or changes view. The scene re-stamps every
    // paint, so a switch of the tracked pane to another buffer redeclares it.
    // With no tracked pane the band stays empty.
    if !modal_overlay
        && let (Some(band), Some(chrome)) = (single_minimap_rect, frame.minimap_chrome)
        && let Some((content_id, palette)) = single_band_content(ws, chrome)
    {
        let strip = Minimap {
            strip_id: SINGLE_MINIMAP_STRIP_ID,
            content_id,
            lines_per_cell: pane::MINIMAP_LINES_PER_CELL,
            max_columns: pane::MINIMAP_MAX_COLUMNS,
            bg: [0, 0, 0, 0],
            thumb: chrome.thumb,
            thumb_border: Some(chrome.thumb_border),
            rule_class: chrome.rule_class,
            thumb_radius: pane::MINIMAP_THUMB_RADIUS,
            palette,
        };
        let rail = single_band_rail(ws, chrome);
        let (band, rail_rect) = pane::split_rail(band, rail.is_some());
        strip.render(band, buf, scene);
        if let (Some(rail_id), Some(rail_rect)) = (rail, rail_rect) {
            pane::rail_minimap(SINGLE_MINIMAP_STRIP_ID, rail_id, chrome)
                .render(rail_rect, buf, scene);
        }
    }

    // Record each undercurl span's cells now, after the editor panes painted but
    // before the overlay stack, so the re-stamp can drop cells a later layer
    // repaints and never draw over an overlay covering a diagnostic.
    undercurl::snapshot_cells(buf, undercurls.spans_mut());

    // An island's halo marks where one pane ends and the next starts, so the gap
    // between islands draws no hairline. A drag in the gap still resizes.
    if !frame.islands {
        pane::render_pane_dividers(&dividers, &stoat.theme, buf, &mut *scene);
    }

    // A paint from here on writes over the panes wherever its box falls, so
    // each one reports whether it wrote, and a frame where one did lists no
    // capture for the UI thread. The band and the dividers above sit outside
    // every pane.
    let mut overlaid = overlay_pane.is_some();
    if let Some(pane_id) = overlay_pane {
        let pane = ws.panes.pane(pane_id);
        let is_focused = matches!(ws.focus, FocusTarget::SplitPane) && ws.panes.focus() == pane_id;
        // The screen paints over its pane in place of the pane's view, so its
        // content clears to the editor surface and takes the pane's island, as
        // `render_pane` does for a view.
        let content = pane.area;
        clear_surface(content, buf, &stoat.theme);
        if frame.islands && scene.live() {
            pane::render_island(content, frame, scene);
        }
        if screen == Some("rebase") {
            if let Some(state) = ws.rebase.as_mut() {
                rebase::render_rebase(pane, state, frame, buf, &mut *scene);
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
                reword::render_reword(pane, is_focused, editor, &sha, &orig, frame, buf);
            }
        } else if screen == Some("rebase_conflict")
            && let Some(active) = ws.rebase_active.as_ref()
        {
            conflict::render_conflict(pane, active, frame, buf, &mut *scene);
        }
    }

    // Every split pane paints its status in the row under it. The rows share
    // one lane, so a cursor move resends them and no pane's content. A band
    // nudges the lane up to center the row in its 24 px, and ends the nudge
    // before the lane does, for a terminal that reads the lanes as one stream.
    scene.set_lane(STATUS_LANE);
    if chrome_bands {
        scene.nudge([0, -i16::from(STATUS_ROW_PAD)]);
    }
    let mut lsp_badge_rect = None;
    for (id, pane) in ws.panes.split_panes() {
        let Some(row) = pane_status_row(pane.area, single_minimap_rect, full, status_rows) else {
            continue;
        };
        let badge = render_split_status(
            pane,
            matches!(ws.focus, FocusTarget::SplitPane) && split_focused == id,
            overlay_pane == Some(id),
            row,
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
        );
        lsp_badge_rect = lsp_badge_rect.or(badge);
    }
    if chrome_bands {
        scene.nudge([0, 0]);
    }
    scene.set_lane(OVERLAY_LANE);
    stoat.lsp_badge_rect = lsp_badge_rect;
    if lsp_badge_rect.is_none() {
        stoat.lsp_badge_hovered = false;
    }

    for (dock_id, dock) in &ws.docks {
        if matches!(dock.visibility, DockVisibility::Hidden) {
            continue;
        }
        let is_focused = matches!(ws.focus, FocusTarget::Dock(id) if id == dock_id);
        if matches!(dock.visibility, DockVisibility::Minimized) {
            overlaid |=
                dock::render_dock_minimized(dock, is_focused, &stoat.theme, buf, &mut *scene);
        } else {
            overlaid |= dock::render_dock_open(
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
    overlaid |= walkthrough::render_slide(stoat, buf, &mut *scene);
    overlaid |= hover::render_hover(stoat, buf, &mut *scene);
    overlaid |= signature_help::render_signature_help(stoat, buf, &mut *scene);
    overlaid |= completion::render_completion(stoat, buf, &mut *scene);
    overlaid |= code_action::render_code_action(stoat, buf, &mut *scene);
    overlaid |= rename_input::render_rename_input(stoat, buf, &mut *scene);
    overlaid |= symbol_picker::render_symbol_picker(stoat, buf, &mut *scene);
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
    // An armed one-char chord waits on its char in a primary mode. The hints
    // box below lists the chars it takes, and a pinned pane shows that box too,
    // since the capture is one press. The read comes before the workspace
    // borrow on the next line.
    let chord = pending_chord_hints(stoat);
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    overlaid |= badges::render_badges(
        &ws.badges,
        &stoat.badges,
        badge_area,
        chrome_bands,
        tab_bar_rows > 0,
        &stoat.theme,
        buf,
        &mut *scene,
    );
    // Each arm that paints a box paints one whenever its modal is open.
    overlaid |= painted_modal.is_some_and(ActiveModal::paints_own_box);
    let on_request = painted_modal.is_some_and(ActiveModal::hints_on_request);
    let modal_hints = stoat.key_hints_visible || !on_request;
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
                    on_request,
                    toggles,
                    &stoat.theme,
                    full,
                    status_rows,
                    chrome_bands,
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
                if modal_hints {
                    cached_modal_hints(
                        &mut stoat.hints_cache,
                        &stoat.keymap,
                        mode,
                        "workspace_picker",
                        None,
                        Some("picker"),
                        on_request,
                        toggles,
                        &stoat.theme,
                        full,
                        status_rows,
                        chrome_bands,
                        buf,
                        &mut *scene,
                    );
                }
            }
        },
        Some(ActiveModal::GitStatus) => {
            if let Some(panel) = &mut stoat.git_status {
                git_status::render_git_status(
                    panel,
                    &stoat.theme,
                    full,
                    tab_bar_rows,
                    status_rows,
                    buf,
                    &mut *scene,
                );
                if modal_hints {
                    cached_modal_hints(
                        &mut stoat.hints_cache,
                        &stoat.keymap,
                        mode,
                        "git_status",
                        None,
                        Some("status"),
                        on_request,
                        toggles,
                        &stoat.theme,
                        full,
                        status_rows,
                        chrome_bands,
                        buf,
                        &mut *scene,
                    );
                }
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
                    on_request,
                    toggles,
                    &stoat.theme,
                    full,
                    status_rows,
                    chrome_bands,
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
                    on_request,
                    toggles,
                    &stoat.theme,
                    full,
                    status_rows,
                    chrome_bands,
                    buf,
                    &mut *scene,
                );
            }
        },
        Some(ActiveModal::PaneFinder) => {
            if let Some(finder) = &mut stoat.pane_finder {
                let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::PaneFinder);
                let split = modal_split_percent(&stoat.modal_split, ModalKind::PaneFinder);
                pane_finder::render_pane_finder(
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
                    "pane_finder",
                    None,
                    None,
                    on_request,
                    toggles,
                    &stoat.theme,
                    full,
                    status_rows,
                    chrome_bands,
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
                    on_request,
                    toggles,
                    &stoat.theme,
                    full,
                    status_rows,
                    chrome_bands,
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
                    on_request,
                    toggles,
                    &stoat.theme,
                    full,
                    status_rows,
                    chrome_bands,
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
                    on_request,
                    toggles,
                    &stoat.theme,
                    full,
                    status_rows,
                    chrome_bands,
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
                if modal_hints {
                    cached_modal_hints(
                        &mut stoat.hints_cache,
                        &stoat.keymap,
                        mode,
                        "finder",
                        Some(finder.scope().context_name()),
                        None,
                        on_request,
                        toggles,
                        &stoat.theme,
                        full,
                        status_rows,
                        chrome_bands,
                        buf,
                        &mut *scene,
                    );
                }
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
                    on_request,
                    toggles,
                    &stoat.theme,
                    full,
                    status_rows,
                    chrome_bands,
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
                    on_request,
                    toggles,
                    &stoat.theme,
                    full,
                    status_rows,
                    chrome_bands,
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
                if modal_hints {
                    cached_modal_hints(
                        &mut stoat.hints_cache,
                        &stoat.keymap,
                        mode,
                        "palette",
                        None,
                        None,
                        on_request,
                        toggles,
                        &stoat.theme,
                        full,
                        status_rows,
                        chrome_bands,
                        buf,
                        &mut *scene,
                    );
                }
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
                    full,
                    modal_zoom_steps(&stoat.modal_zoom, ModalKind::Help),
                    buf,
                    &mut *scene,
                );
                if modal_hints {
                    cached_modal_hints(
                        &mut stoat.hints_cache,
                        &stoat.keymap,
                        mode,
                        "help",
                        None,
                        None,
                        on_request,
                        toggles,
                        &stoat.theme,
                        full,
                        status_rows,
                        chrome_bands,
                        buf,
                        &mut *scene,
                    );
                }
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
    // A screen that keeps its box up shows it until the toggle hides it, and the
    // toggle forces a box on anywhere else.
    let touring = ws.walkthrough.is_some();
    let toggled = match keeps_hints_up(screen, touring) {
        true => !stoat.screen_hints_hidden,
        false => stoat.key_hints_visible,
    };
    // A chord mode that no pin hides, and an armed chord, show the box whatever
    // the toggle holds, so the toggle does not hide that box.
    let auto_shown = (!PRIMARY_MODES.contains(&mode) && !pin_hidden)
        || screen == Some("review")
        || chord.is_some();

    if !painted_modal.is_some_and(ActiveModal::paints_own_box)
        && mode != "space_pane_display"
        && (auto_shown || toggled)
    {
        // The space_pane_display chord paints its own digit badges below, and
        // the auto-shown hint box would only stack an eleven-row overlay over
        // them. The guard above keeps it suppressed even when the `?` key-hints
        // toggle is on.
        let flags = Flags::from_workspace(ws).with_app(app);
        let pane = pane_predicate(ws);
        let token = cursor_token(ws);
        let focus = focus_flags(ws, &stoat.diagnostics, &stoat.lsp_registry);
        let own_box = screen_own_box(screen, mode, touring);
        let key = hints_cache_key(
            mode,
            screen,
            pane,
            &flags,
            token,
            &focus,
            None,
            None,
            chord.as_ref().map(|chord| chord.title),
            pin_hidden,
        );

        let missed = stoat.hints_cache.as_ref().map(|c| c.key) != Some(key);
        if missed {
            let state = StoatKeymapState::with_flags(mode, flags)
                .with_view(screen)
                .with_pane(pane)
                .with_token(token)
                .with_focus_flags(focus);
            // An armed chord takes either footer key as its char, and the toggle
            // hides no box that shows on its own.
            let footer_keys = match (chord.is_some(), auto_shown) {
                (true, _) => hints::FooterKeys::default(),
                (false, true) => hints::FooterKeys {
                    hide: None,
                    ..hints_footer_keys(&stoat.keymap, &state, mode)
                },
                (false, false) => hints_footer_keys(&stoat.keymap, &state, mode),
            };
            let rows = match &chord {
                Some(chord) => hints::chord_rows(chord),
                None => hint_rows(&stoat.keymap, &state, mode, screen, own_box, pane),
            };
            stoat.hints_cache = Some(hints::HintsCache::new(key, rows, footer_keys));
        }

        let conflict_footer = conflict_position
            .filter(|_| screen == Some("conflict"))
            .map(|position| hints::HintsFooter {
                text: format!(
                    "chunk {} / {}",
                    position
                        .chunk_at
                        .map_or_else(|| "-".to_string(), |at| at.to_string()),
                    position.chunk_total,
                ),
                style: stoat.theme.get(crate::theme::scope::UI_TEXT),
            });
        let diff_footer = hunk_position
            .filter(|_| own_box == Some("diff"))
            .map(|position| hints::HintsFooter {
                text: format!(
                    "hunk {}/{}",
                    position
                        .hunk_at
                        .map_or_else(|| "-".to_string(), |at| at.to_string()),
                    position.hunk_total,
                ),
                style: stoat.theme.get(crate::theme::scope::UI_TEXT),
            });
        let footer = conflict_footer.as_ref().or(diff_footer.as_ref());

        let hint_label = match chord {
            Some(chord) => chord.title,
            None => own_box.unwrap_or(mode),
        };
        let cache = stoat.hints_cache.as_mut().expect("cache populated above");
        let subtitle = hints_subtitle(chord.is_some(), own_box.is_some(), cache.rows.len());
        hints::render_hints_grouped(
            hint_label,
            &subtitle,
            cache,
            footer,
            toggles,
            &stoat.theme,
            full,
            status_rows,
            chrome_bands,
            buf,
            &mut *scene,
        );
        overlaid = true;
    }

    if mode == "space_pane_display" {
        render_pane_id_badges(&stoat.theme, ws, buf, scene);
        overlaid = true;
    }
    scene.end_lane();

    let captures = match overlaid {
        true => Vec::new(),
        false => frame_holds
            .iter()
            .map(|hold| (hold.rect, hold.capture))
            .collect(),
    };
    (frame_holds, captures)
}

/// The rows the hints box lists from `state`, its bindings grouped by action.
///
/// A screen with its own box, which `own_box` names, scopes to the bindings
/// that name its view, such as `view == conflict`. The screen rides on the
/// editor's modes, so its mode alone names the editor's keys rather than the
/// screen's. A terminal or agent pane at rest sends every other key to its
/// child, so it scopes to the bindings that name its pane kind. A primary mode
/// narrows to the chord entries and the context-bound keys, because the whole
/// editor keymap is what the help screen is for. A chord sub-mode, the
/// walkthrough's among them, lists the keys its own mode binds, so the root
/// keys every mode answers stay off its box.
///
/// A chord's binding that only returns to a primary mode is its exit, the
/// Escape every chord binds, so it makes no row. In a primary mode such a
/// binding is an entry, as `v` enters select mode, and keeps its row.
fn hint_rows(
    keymap: &crate::keymap::Keymap,
    state: &dyn KeymapState,
    mode: &str,
    screen: Option<&str>,
    own_box: Option<&str>,
    pane: Option<&str>,
) -> Vec<hints::HintRow> {
    let (raw, chord) = if let Some(view) = screen.filter(|_| own_box.is_some()) {
        (keymap.scoped_bindings(state, "view", view), false)
    } else if let Some(kind @ ("terminal" | "agent")) = pane.filter(|_| mode == "normal") {
        (keymap.scoped_bindings(state, "pane", kind), false)
    } else if PRIMARY_MODES.contains(&mode) {
        // Wheel gestures are left out. This overlay affords pressing one of the
        // keys it lists, which a notch is not. Width is also the scarce
        // resource here, because a large chord already fills two columns and a
        // third overflows any ordinary terminal. The help screen reads the
        // unfiltered set, so nothing is hidden from a reader looking for it.
        let keys = keymap
            .context_bindings(state, PRIMARY_MODES)
            .into_iter()
            .filter(|(key, ..)| key.wheel.is_none())
            .map(|(key, actions, label)| (key.display_label(), actions, label))
            .collect();
        (keys, false)
    } else {
        (keymap.scoped_bindings(state, "mode", mode), true)
    };
    let bindings: Vec<(&str, String, Option<Toggle>)> = raw
        .iter()
        .filter(|(_, actions, _)| !(chord && only_enters_primary_mode(actions)))
        .map(|(key, actions, label)| {
            (
                key.as_str(),
                binding_display_desc(actions, *label),
                toggle::binding_toggle(actions),
            )
        })
        .collect();
    hints::group_by_action(&bindings)
}

/// Whether `screen` keeps a hints box up while it is in the foreground.
///
/// The conflict screen, the diff view, and the changes view keep one up. Their
/// box shows with the hints toggle off, and the toggle hides it there instead.
///
/// With `touring` set, a tour is open, and the diff and changes views leave
/// their box to the toggle. The box covers the lower right of the view and
/// hides a stop whose code sits there, and the tour's own slide and box guide
/// the reader.
pub(crate) fn keeps_hints_up(screen: Option<&str>, touring: bool) -> bool {
    match screen {
        Some("conflict") => true,
        Some("diff" | "diff_changes") => !touring,
        _ => false,
    }
}

/// The screen whose own hints box shows over `screen` in `mode`, named as the
/// box's title.
///
/// Only a screen that [`keeps_hints_up`] has a box of its own, so a box that
/// the toggle forces on during a tour reads as the mode's. The conflict
/// screen's box lists the screen's bindings in every mode. The diff and
/// changes views share the `diff` box, which lists the editor's keys, so a
/// chord sub-mode over either view shows the mode's own box in its place,
/// under the mode's title.
fn screen_own_box(screen: Option<&str>, mode: &str, touring: bool) -> Option<&'static str> {
    if !keeps_hints_up(screen, touring) {
        return None;
    }
    match screen {
        Some("conflict") => Some("conflict"),
        Some("diff" | "diff_changes") if PRIMARY_MODES.contains(&mode) => Some("diff"),
        _ => None,
    }
}

/// The subtitle of a hints box that lists `rows` rows.
///
/// An armed chord's box names what it waits on. A screen's own box says that
/// it stays up. Any other box counts its keys.
fn hints_subtitle(chord: bool, screen_box: bool, rows: usize) -> Cow<'static, str> {
    match (chord, screen_box) {
        (true, _) => Cow::Borrowed("waiting on a char"),
        (false, true) => Cow::Borrowed("stays up on this view"),
        (false, false) => Cow::Owned(hints::key_count(rows)),
    }
}

/// The keys the hints box's footer names, the one that hides the box from
/// `state` and the one that opens the full help.
///
/// In a primary `mode` the help key reads from a bare normal-mode state,
/// because that is where a reader goes to press it. A chord mode reads it from
/// its own `state`, because the chord takes its keys first. A chord that binds
/// `?` to an action of its own then names its way out and the help key, such
/// as `Esc ?`.
fn hints_footer_keys(
    keymap: &crate::keymap::Keymap,
    state: &dyn KeymapState,
    mode: &str,
) -> hints::FooterKeys {
    let help = match PRIMARY_MODES.contains(&mode) {
        true => keymap.key_for_action(
            &StoatKeymapState::with_flags("normal", Flags::default()),
            "OpenHelp",
        ),
        false => keymap.key_for_action(state, "OpenHelp"),
    };
    hints::FooterKeys {
        hide: keymap.key_for_action(state, "ToggleKeyHints"),
        help,
    }
}

/// The content store and palette the single band declares for the tracked
/// split pane of `ws`, or `None` with no tracked pane.
///
/// An editor's strip shows what [`MinimapChrome::editor_strip`] answers. A
/// changes view's strip shows the workspace's changes view document. Both take
/// the syntax palette.
///
/// See also:
/// - [`Workspace::refresh_band_pane`] for which pane the band tracks.
fn single_band_content<'a>(
    ws: &Workspace,
    chrome: MinimapChrome<'a>,
) -> Option<(u32, &'a [[u8; 3]])> {
    match *ws.band_view()? {
        View::Editor(editor_id) => {
            let editor = ws.editors.get(editor_id)?;
            let (content_id, _) = chrome.editor_strip(editor_id, editor)?;
            Some((content_id, chrome.palette))
        },
        View::DiffChanges(_) => Some((chrome.diff_changes_content?, chrome.palette)),
        _ => None,
    }
}

/// The content id of the Coverage Rail the single band over `ws` shows in its
/// last cell, `None` while the tracked editor's or changes view's strip fits,
/// and with no tracked pane.
fn single_band_rail(ws: &Workspace, chrome: MinimapChrome<'_>) -> Option<u32> {
    match *ws.band_view()? {
        View::Editor(editor_id) => {
            let editor = ws.editors.get(editor_id)?;
            chrome.editor_strip(editor_id, editor)?.1
        },
        View::DiffChanges(_) => chrome.diff_changes_rail,
        _ => None,
    }
}

/// The status row of the split pane over `area`, the last of the `rows` rows
/// under the pane, over its columns.
///
/// Every pane has `rows` free rows under it, the gap between it and a stacked
/// sibling or the rows that [`Stoat::layout_size`] leaves at the window's
/// bottom, so a status row takes nothing from the panes. A band's two rows put
/// the status row under the spare row. A pane flush against the single
/// minimap `band`, over the band's rows, runs its row under the band too,
/// which only the flush layout allows. `None` for an empty area and for a
/// pane whose status row reaches past the `window`'s last row.
///
/// [`Stoat::layout_size`]: crate::Stoat::layout_size
pub(crate) fn pane_status_row(
    area: Rect,
    band: Option<Rect>,
    window: Rect,
    rows: u16,
) -> Option<Rect> {
    let y = area.bottom() + rows.saturating_sub(1);
    if area.is_empty() || y >= window.bottom() {
        return None;
    }
    let width = match band {
        Some(band) if band.x == area.right() && band.y == area.y && band.height == area.height => {
            area.width + band.width
        },
        _ => area.width,
    };
    Some(Rect {
        y,
        height: 1,
        width,
        ..area
    })
}

/// Paint the status row of the split pane `pane`, `row`, and return the hover
/// rect of the LSP badge the row shows.
///
/// With `screen_over` set, a rebase, reword, or conflict screen covers the
/// pane, and the row names that screen. A commits view's row names the commits
/// screen and its selected commit. Every other view shows its pane status.
#[allow(clippy::too_many_arguments)]
fn render_split_status(
    pane: &Pane,
    is_focused: bool,
    screen_over: bool,
    row: Rect,
    ctx: PaneCtx<'_>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> Option<Rect> {
    let PaneCtx {
        editors,
        buffers,
        terms,
        commit_lists,
        diff_changes_views,
        ..
    } = ctx;
    if screen_over {
        pane::render_overlay_status(row, is_focused, frame.screen, None, frame, buf, scene);
        return None;
    }
    if let View::Commits(id) = pane.view {
        let subject = commit_lists.get(id).and_then(commits::status_subject);
        pane::render_overlay_status(
            row,
            is_focused,
            Some("commits"),
            subject.as_ref(),
            frame,
            buf,
            scene,
        );
        return None;
    }

    let status = pane::status_row(
        pane,
        row,
        is_focused,
        frame,
        editors,
        buffers,
        terms,
        diff_changes_views,
    );
    pane::render_pane_status(pane, frame, editors, &status, buf, scene);
    status.badge
}

/// The scene lane of a pane's content, which holds its gutter, its minimap
/// strip, and the cards it pops out.
///
/// Lane 0 stays below every pane for the tab bar, and [`STATUS_LANE`] and
/// [`OVERLAY_LANE`] above them, so a cursor move that redraws only the status
/// rows sends only those rows.
pub(crate) fn content_lane(pane_index: u32) -> u32 {
    1 + pane_index
}

/// The pane that a rebase, reword, or conflict screen paints over.
///
/// The screen covers the focused pane alone, so every other pane keeps
/// painting beside it. `None` while no such screen is up.
pub(crate) fn overlay_pane(ws: &Workspace) -> Option<PaneId> {
    match crate::keymap_state::view_predicate(ws) {
        Some("rebase" | "reword" | "rebase_conflict") => Some(ws.panes.focus()),
        _ => None,
    }
}

/// Hash the keymap-state inputs that decide which bindings are active into a
/// cache key. An unchanged key means the same hints list, so the binding walk
/// and regrouping can be skipped.
///
/// `chord` is the title of an armed chord whose own table takes the place of
/// the keymap's rows.
///
/// `pin_hidden` is whether a pin hides a chord mode's box. A pinned box shows
/// only for the toggle, so the toggle hides it, and its footer names the hide
/// key that an unpinned chord box leaves out.
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
    chord: Option<&str>,
    pin_hidden: bool,
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
    pin_hidden.hash(&mut hasher);
    chord.hash(&mut hasher);
    hasher.finish()
}

/// The hints box of the one-char chord that waits on the next key, or `None`
/// while no chord is armed.
///
/// The arms follow the order in which `handle_key` consumes the chords, so the
/// box names the chord that the next key reaches.
///
/// [`Stoat::update`] compares the box before and after each event, and asks
/// for a frame when it changed.
pub(crate) fn pending_chord_hints(stoat: &Stoat) -> Option<hints::ChordHints> {
    stoat
        .pending_macro_replay
        .map(|_| hints::ChordHints {
            title: "replay macro",
            rows: yank::REGISTER_HINTS,
        })
        .or_else(|| {
            stoat.pending_find.map(|(kind, _, _)| hints::ChordHints {
                title: kind.hint_title(),
                rows: movement::FIND_HINTS,
            })
        })
        .or_else(|| {
            stoat.pending_mark.map(|request| hints::ChordHints {
                title: request.hint_title(),
                rows: marks::MARK_HINTS,
            })
        })
        .or_else(|| {
            stoat.pending_replace.then_some(hints::ChordHints {
                title: "replace with char",
                rows: movement::REPLACE_HINTS,
            })
        })
        .or_else(|| {
            stoat.pending_surround_add.then_some(hints::ChordHints {
                title: "surround",
                rows: surround::SURROUND_ADD_HINTS,
            })
        })
        .or_else(|| {
            (stoat.pending_register_select || stoat.pending_insert_register).then_some(
                hints::ChordHints {
                    title: "register",
                    rows: yank::REGISTER_HINTS,
                },
            )
        })
        .or_else(|| stoat.pending_surround_replace.hints())
        .or_else(|| {
            stoat.pending_surround_delete.then_some(hints::ChordHints {
                title: "delete pair",
                rows: surround::SURROUND_FROM_HINTS,
            })
        })
        .or_else(|| {
            stoat
                .pending_textobject_select
                .map(|(mode, _)| hints::ChordHints {
                    title: mode.hint_title(),
                    rows: textobject::TEXTOBJECT_HINTS,
                })
        })
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
///
/// `on_request` is whether the modal's box waits for the hints toggle. Only
/// that box names the toggle's key in its footer, because the toggle hides no
/// other. No modal box names a help key, because the modal takes `?` first,
/// as text in its query or as a binding of its own.
#[allow(clippy::too_many_arguments)]
fn cached_modal_hints(
    cache: &mut Option<hints::HintsCache>,
    keymap: &crate::keymap::Keymap,
    mode: &str,
    modal: &'static str,
    scope: Option<&str>,
    title: Option<&str>,
    on_request: bool,
    toggles: ToggleStates,
    theme: &crate::theme::Theme,
    area: Rect,
    status_rows: u16,
    bands: bool,
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
        None,
        false,
    );
    if cache.as_ref().map(|c| c.key) != Some(key) {
        let state = StoatKeymapState::with_flags(mode, Flags::default())
            .with_modal(modal)
            .with_scope(scope);
        let raw = keymap.scoped_bindings(&state, "modal", modal);
        let bindings: Vec<(&str, String, Option<Toggle>)> = raw
            .iter()
            .map(|(key, actions, label)| {
                (
                    key.as_str(),
                    binding_display_desc(actions, *label),
                    toggle::binding_toggle(actions),
                )
            })
            .collect();
        let footer_keys = hints::FooterKeys {
            hide: match on_request {
                true => keymap.key_for_action(&state, "ToggleKeyHints"),
                false => None,
            },
            help: None,
        };
        *cache = Some(hints::HintsCache::new(
            key,
            hints::group_by_action(&bindings),
            footer_keys,
        ));
    }
    let cache = cache.as_mut().expect("cache populated above");
    let subtitle = hints::key_count(cache.rows.len());
    hints::render_hints_grouped(
        title.unwrap_or(modal),
        &subtitle,
        cache,
        None,
        toggles,
        theme,
        area,
        status_rows,
        bands,
        buf,
        scene,
    );
}

/// A digit badge on a split pane that the `space_pane_display` chord reaches.
struct PaneBadge {
    rect: Rect,
    digit: char,
    /// The size stoatty floats the digit at, in cells per glyph side, 6 in a
    /// pane with room.
    scale: u8,
}

/// The badges the `space_pane_display` chord paints on `ws`'s split panes.
///
/// Panes are numbered 1-9 then 0 for the tenth in `split_panes` layout order,
/// and panes past the tenth get no badge. The focused pane gets no badge, and
/// its number goes unused, so a digit names the same pane wherever the focus
/// sits.
///
/// A badge's box sits in the center of its pane, as tall as its digit's scale
/// and three cells wide for each two rows, so it is 9 by 6 at full size. A
/// cell is twice as tall as it is wide, so the box keeps the board's 3:4
/// shape. A small pane takes the largest scale whose box fits, and a pane
/// with room for no box gets no badge.
fn pane_badges(ws: &Workspace) -> Vec<PaneBadge> {
    let focused = ws.panes.focus();
    ws.panes
        .split_panes()
        .enumerate()
        .take(10)
        .filter(|(_, (pane_id, pane))| {
            *pane_id != focused && pane.area.width > 0 && pane.area.height > 0
        })
        .filter_map(|(i, (_, pane))| {
            let digit = char::from_digit((i as u32 + 1) % 10, 10)?;
            let area = pane.area;
            let width = |scale: u16| (3 * scale).div_ceil(2);
            let scale = (1..=6)
                .rev()
                .find(|&scale| scale <= area.height && width(scale) <= area.width)?;

            let rect = Rect::new(
                area.x + area.width.saturating_sub(width(scale)) / 2,
                area.y + area.height.saturating_sub(scale) / 2,
                width(scale),
                scale,
            );
            Some(PaneBadge {
                rect,
                digit,
                scale: scale as u8,
            })
        })
        .collect()
}

/// Paint a large digit badge centered on each split pane while the
/// `space_pane_display` chord is active, so the reader focuses a pane by its
/// number.
///
/// The focused pane gets no badge, since it is where the chord started, and
/// every other badge paints in the fill [`pane_badge_colors`] picks, so the
/// reachable panes stand out. Each badge is a bold [`Popover`], so a plain
/// terminal draws the cell-fallback box and stoatty floats the scaled bold
/// glyph in a box with [`BADGE_CHROME`]. The digit sits in the center of the
/// box in both forms.
fn render_pane_id_badges(
    theme: &crate::theme::Theme,
    ws: &Workspace,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let (fill, ink) = pane_badge_colors(theme);

    for badge in pane_badges(ws) {
        let digit = badge.digit.to_string();
        let popover = Popover {
            fill,
            border: ink,
            content_fg: ink,
            scale: badge.scale,
            offset: [0, 0],
            bold: true,
            chrome: BADGE_CHROME,
            center: true,
            content: &digit,
            spans: &[],
        };
        // The live popover has rounded corners, and a cell fill under it shows
        // past them, as the hover card's clear does. Each host draws one form.
        if scene.live() {
            popover.draw_components(badge.rect, scene);
        } else {
            popover.draw_fallback(badge.rect, buf);
        }
    }
}

/// The fill of a Pane Picker badge and the ink of its digit and border.
///
/// The theme's `ui.pane_picker.badge` scope gives both, its background the
/// fill and its foreground the ink. A theme that sets no RGB pair there fills
/// the badge with the focused status bar's background and inks it with the
/// editor background.
pub(crate) fn pane_badge_colors(theme: &crate::theme::Theme) -> ([u8; 3], [u8; 3]) {
    let themed = theme
        .try_get(crate::theme::scope::UI_PANE_PICKER_BADGE)
        .map(|style| (paint::style_rgb(style.bg), paint::style_rgb(style.fg)));
    if let Some((Some(fill), Some(ink))) = themed {
        return (fill, ink);
    }

    let fill = paint::style_rgb(theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED).bg)
        .unwrap_or([90, 90, 110]);
    let ink =
        paint::style_rgb(theme.get(crate::theme::scope::UI_BACKGROUND).bg).unwrap_or([40, 44, 52]);
    (fill, ink)
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
    use super::{
        blank_buffer, clear_modal, clear_surface, clear_themed, modal_blank, modal_fill,
        themed_blank, Hold,
    };
    use crate::{app::Stoat, pane::PaneId, theme::Theme, workspace::WorkspaceId};
    use ratatui::{
        buffer::{Buffer, Cell},
        layout::Rect,
        style::{Color, Style},
    };
    use std::sync::Arc;

    /// A blank job puts each hold's capture back into its rect and blanks every
    /// other cell. It drops a hold whose rect leaves the buffer, since such a
    /// hold names cells the buffer does not hold.
    #[test]
    fn a_blank_job_restores_held_rects_and_blanks_the_rest() {
        let area = Rect::new(0, 0, 4, 3);
        let held = |rect: Rect, symbol: &'static str| Hold {
            pane: (WorkspaceId::default(), PaneId::default()),
            rect,
            capture: 1,
            cells: Arc::new(vec![
                Cell::new(symbol);
                usize::from(rect.width * rect.height)
            ]),
        };
        let (inside, outside) = (
            held(Rect::new(1, 1, 2, 1), "x"),
            held(Rect::new(3, 2, 2, 1), "y"),
        );
        let mut poison = Cell::new("#");
        poison.bg = Color::Red;

        let blanked = blank_buffer(
            Buffer::filled(area, poison),
            Cell::new(" "),
            vec![inside.clone(), outside],
        );

        let mut expected = Buffer::filled(area, Cell::new(" "));
        expected[(1u16, 1u16)] = Cell::new("x");
        expected[(2u16, 1u16)] = Cell::new("x");
        let kept: Vec<Rect> = blanked.holds.iter().map(|hold| hold.rect).collect();
        assert_eq!((blanked.buffer, kept), (expected, vec![inside.rect]));
    }

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

    /// A modal box blanks to its own style's background, and to the theme's
    /// where the style sets none, as default_dark's modal scopes do.
    #[test]
    fn clear_modal_takes_the_style_background_or_the_themed_blank() {
        let theme = {
            let src = r##"theme t { ui.background.bg = "#102030"; }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));

        let filled = Style::new().bg(Color::Rgb(1, 2, 3));
        clear_modal(Rect::new(0, 0, 1, 1), &mut buf, &theme, filled);
        let border_only = Style::new().fg(Color::Rgb(9, 9, 9));
        clear_modal(Rect::new(1, 0, 1, 1), &mut buf, &theme, border_only);

        assert_eq!(
            [buf[(0u16, 0u16)].bg, buf[(1u16, 0u16)].bg],
            [Color::Rgb(1, 2, 3), Color::Rgb(0x10, 0x20, 0x30)],
            "the style's background, then the theme's for a style with none",
        );
    }

    /// The window ground is the ground surface where a theme sets one, the
    /// chrome surface where it sets only that, and the editor surface where it
    /// sets neither.
    #[test]
    fn the_themed_blank_takes_the_ground_then_the_chrome_then_the_background() {
        let ground = load_theme(
            r##"theme t {
                ui.background.bg = "#102030";
                ui.surface.chrome.bg = "#010203";
                ui.surface.ground.bg = "#040506";
            }"##,
        );
        let chrome = load_theme(
            r##"theme t { ui.background.bg = "#102030"; ui.surface.chrome.bg = "#010203"; }"##,
        );
        let plain = load_theme(r##"theme t { ui.background.bg = "#102030"; }"##);

        assert_eq!(
            [&ground, &chrome, &plain].map(|theme| themed_blank(theme).bg),
            [
                Color::Rgb(4, 5, 6),
                Color::Rgb(1, 2, 3),
                Color::Rgb(0x10, 0x20, 0x30)
            ],
            "the ground surface, then the chrome, then the background",
        );
    }

    /// A pane's content clears to the editor surface, so a cleared cell under a
    /// chrome window takes the background while its neighbor keeps the ground.
    #[test]
    fn clear_surface_takes_the_background_under_a_chrome_window() {
        let theme = load_theme(
            r##"theme t {
                ui.background.bg = "#102030";
                ui.surface.chrome.bg = "#010203";
                ui.text.fg = "#405060";
            }"##,
        );
        let mut buf = Buffer::filled(Rect::new(0, 0, 2, 1), themed_blank(&theme));
        buf[(0u16, 0u16)].set_char('x');

        clear_surface(Rect::new(0, 0, 1, 1), &mut buf, &theme);

        let text = Color::Rgb(0x40, 0x50, 0x60);
        assert_eq!(
            [&buf[(0u16, 0u16)], &buf[(1u16, 0u16)]].map(|cell| (cell.symbol(), cell.fg, cell.bg)),
            [
                (" ", text, Color::Rgb(0x10, 0x20, 0x30)),
                (" ", text, Color::Rgb(1, 2, 3)),
            ],
            "the cleared cell on the background, beside a cell of the ground",
        );
    }

    /// A modal style with no background fills its box with the window ground,
    /// the color its blank cells take, so a box reads one surface inside and
    /// outside stoatty.
    #[test]
    fn a_modal_with_no_background_fills_with_the_window_ground() {
        let theme = load_theme(
            r##"theme t { ui.background.bg = "#102030"; ui.surface.chrome.bg = "#010203"; }"##,
        );
        let border_only = Style::new().fg(Color::Rgb(9, 9, 9));

        assert_eq!(
            (
                modal_fill(&theme, border_only),
                modal_blank(&theme, border_only).bg
            ),
            (Some([1, 2, 3]), Color::Rgb(1, 2, 3)),
            "the box's fill and its blank cells agree on the chrome ground",
        );
    }

    /// The theme `t` that `src` declares.
    fn load_theme(src: &str) -> Theme {
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
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
mod pane_badge_tests {
    use super::{paint, pane_badge_colors, pane_badges, BADGE_CHROME};
    use crate::test_harness::TestHarness;
    use ratatui::{layout::Rect, style::Color};
    use stoatty_protocol::command::{self, Command, PopoverCommand};

    /// The focused pane has no badge, since the chord starts there, and the
    /// other panes keep the numbers their places give them, so a digit names
    /// the same pane wherever the focus sits.
    #[test]
    fn the_focused_pane_has_no_badge_and_the_others_keep_their_numbers() {
        let mut h = TestHarness::with_size(80, 24);
        h.type_action("SplitRight()");
        h.type_action("SplitRight()");
        let digits_with_focus_on = |h: &mut TestHarness, place: usize| {
            let ws = h.stoat.active_workspace_mut();
            let pane = ws.panes.split_pane_ids()[place];
            ws.panes.set_focus(pane);
            pane_badges(ws)
                .iter()
                .map(|badge| badge.digit)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            [
                digits_with_focus_on(&mut h, 2),
                digits_with_focus_on(&mut h, 1)
            ],
            [vec!['1', '2'], vec!['1', '3']],
        );
    }

    /// A badge paints the digit on the accent fill, so the panes a digit
    /// reaches stand out, and the focused pane carries no badge.
    #[test]
    fn a_badge_paints_the_accent_fill() {
        let mut h = badge_chord(true);
        let buf = h.render_composited();

        let accent = accent_bg(&h);
        let ws = h.stoat.active_workspace();
        let badges = pane_badges(ws);
        let [badge] = badges.as_slice() else {
            panic!("one badge, on the pane without the focus");
        };
        let digit_cell = &buf[(
            badge.rect.x + (badge.rect.width - 1) / 2,
            badge.rect.y + (badge.rect.height - 1) / 2,
        )];
        let focused = ws.panes.pane(ws.panes.focus()).area;
        let focused_has_accent = (focused.y..focused.y + focused.height - 1)
            .flat_map(|y| (focused.x..focused.x + focused.width).map(move |x| (x, y)))
            .any(|cell| Some(buf[cell].bg) == accent);

        assert_eq!(
            (digit_cell.symbol(), Some(digit_cell.bg), focused_has_accent),
            (badge.digit.to_string().as_str(), accent, false),
            "the digit sits on the accent fill, and the focused pane above its status row has none",
        );
    }

    /// A live scene floats the rounded badge alone, so no square cell fill
    /// sits under its corners. The one popover carries the board's chrome and
    /// centers a scale 6 digit.
    #[test]
    fn a_live_scene_paints_no_badge_cells() {
        let mut h = badge_chord(true);
        let buf = h.stoat.render();

        let accent = accent_bg(&h);
        let badges = pane_badges(h.stoat.active_workspace());
        let [badge] = badges.as_slice() else {
            panic!("one badge, on the pane without the focus");
        };
        let sent: Vec<_> = popovers(&h)
            .into_iter()
            .map(|p| (p.fill, p.chrome, p.center, p.scale))
            .collect();

        assert_eq!(
            (
                rect_cells(badge.rect).any(|cell| Some(buf[cell].bg) == accent),
                sent
            ),
            (
                false,
                vec![(
                    paint::style_rgb(accent).expect("an RGB accent"),
                    BADGE_CHROME,
                    true,
                    6
                )]
            ),
            "the grid under the badge keeps its cells, and one popover carries the badge",
        );
    }

    /// A badge's box is 9 by 6 in a pane with room, centered on the pane. A
    /// small pane takes the largest box that fits, three cells wide for each
    /// two rows, and a pane with room for no box gets no badge.
    #[test]
    fn a_badge_is_nine_by_six_in_a_pane_with_room_and_shrinks_in_a_small_one() {
        let mut h = badge_chord(false);
        let mut badge_in = |width: u16, height: u16| {
            let ws = h.stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            let unfocused = ws
                .panes
                .split_pane_ids()
                .into_iter()
                .find(|&pane| pane != focused)
                .expect("a pane without the focus");
            ws.panes.pane_mut(unfocused).area = Rect::new(2, 3, width, height);
            pane_badges(ws)
                .first()
                .map(|badge| (badge.rect, badge.scale))
        };

        assert_eq!(
            [
                badge_in(40, 23),
                badge_in(5, 3),
                badge_in(3, 1),
                badge_in(1, 1)
            ],
            [
                Some((Rect::new(17, 11, 9, 6), 6)),
                Some((Rect::new(2, 3, 5, 3), 3)),
                Some((Rect::new(2, 3, 2, 1), 1)),
                None,
            ],
        );
    }

    /// A dead scene draws the cell box alone, since no host floats a popover.
    #[test]
    fn a_dead_scene_paints_the_badge_cells() {
        let mut h = badge_chord(false);
        let buf = h.stoat.render();

        let accent = accent_bg(&h);
        let badges = pane_badges(h.stoat.active_workspace());
        let [badge] = badges.as_slice() else {
            panic!("one badge, on the pane without the focus");
        };

        assert_eq!(
            (
                rect_cells(badge.rect).all(|cell| Some(buf[cell].bg) == accent),
                popovers(&h).len()
            ),
            (true, 0),
            "every cell of the badge box takes the fill, and no popover goes out",
        );
    }

    /// Two side-by-side panes in the pane picker chord, rendered for a host
    /// that floats popovers when `stoatty` is set.
    fn badge_chord(stoatty: bool) -> TestHarness {
        let mut h = TestHarness::with_size(80, 24);
        h.type_action("SplitRight()");
        h.stoat.stoatty = stoatty;
        h.stoat.set_focused_mode("space_pane_display".into());
        h
    }

    /// The fill a badge takes under the theme the harness loads.
    fn accent_bg(h: &TestHarness) -> Option<Color> {
        let [r, g, b] = pane_badge_colors(&h.stoat.theme).0;
        Some(Color::Rgb(r, g, b))
    }

    /// The popovers the last frame sent to the host.
    fn popovers(h: &TestHarness) -> Vec<PopoverCommand> {
        command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Popover(popover) => Some(popover),
                _ => None,
            })
            .collect()
    }

    /// Every cell position inside `rect`, row by row.
    fn rect_cells(rect: Rect) -> impl Iterator<Item = (u16, u16)> {
        (rect.y..rect.bottom()).flat_map(move |y| (rect.x..rect.right()).map(move |x| (x, y)))
    }
}

#[cfg(test)]
mod minimap_chrome_tests {
    use crate::{render::paint::style_rgb, test_harness::TestHarness, theme::scope};
    use stoatty_protocol::command::{self, Command};

    /// A pane's minimap strip is seven cells wide at six lines a cell, and its
    /// viewport thumb washes the focus accent at eighteen percent with an
    /// outline at 90 percent.
    #[test]
    fn the_strip_is_seven_cells_with_an_accent_thumb() {
        let mut h = TestHarness::with_size(120, 24);
        h.seed_focused_buffer(&"line\n".repeat(60));
        h.stoat.render();

        let strip = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .find_map(|cmd| match cmd {
                Command::Minimap(strip) => Some(strip),
                _ => None,
            })
            .expect("the pane declares a strip");
        let [r, g, b] = style_rgb(h.stoat.theme.get(scope::UI_BORDER_FOCUSED).fg)
            .expect("the theme's focus accent is RGB");
        assert_eq!(
            (
                strip.width,
                strip.lines_per_cell,
                strip.thumb,
                strip.thumb_border
            ),
            (7, 6, [r, g, b, 46], Some([r, g, b, 230]))
        );
    }
}

#[cfg(test)]
mod modal_dim_tests {
    use super::pane_fade;
    use crate::{
        render::paint::{dim_rgb, style_rgb},
        test_fixture::open_scratch_file,
        test_harness::TestHarness,
        theme::scope,
    };
    use ratatui::style::Color;
    use stoat_config::MinimapMode;
    use stoatty_protocol::command::{self, Command};

    /// An unfocused pane takes the deeper of the two fades, and a focused pane
    /// takes the modal fade alone.
    #[test]
    fn a_pane_takes_the_deeper_of_its_fades() {
        assert_eq!(
            [
                pane_fade(true, 0.8, 0.65),
                pane_fade(false, 0.8, 0.65),
                pane_fade(false, 0.1, 0.65),
                pane_fade(true, 0.8, 0.0),
            ],
            [0.65, 0.8, 0.65, 0.0]
        );
    }

    /// A modal that paints its own box fades every pane behind it by
    /// `ui.modal_dim`, the unfocused one past its own lighter fade. The search
    /// prompt paints no box, so the panes keep their fades.
    #[test]
    fn a_boxed_modal_fades_every_pane_behind_it() {
        let mut h = TestHarness::with_size(80, 24);
        let _path = open_scratch_file(&mut h, "abcdef\n");
        h.type_action("SplitRight()");
        h.stoat.settings.ui_inactive_dim = Some(0.1);
        h.stoat.settings.ui_modal_dim = Some(0.65);
        let glyphs = |h: &mut TestHarness| -> Vec<Color> {
            let buf = h.stoat.render();
            (0..80)
                .filter(|&x| buf[(x, 0)].symbol() == "c")
                .map(|x| buf[(x, 0)].fg)
                .collect()
        };

        let plain = glyphs(&mut h);
        h.type_keys("/");
        let searching = glyphs(&mut h);
        h.type_keys("escape");
        h.type_action("SwitchWorkspace()");
        let boxed = glyphs(&mut h);

        let bg = style_rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg).expect("an RGB ground");
        let Some(&Color::Rgb(r, g, b)) = plain.last() else {
            panic!("the focused pane paints its text in RGB, not {plain:?}");
        };
        let fade = |amount: f32| {
            let [r, g, b] = dim_rgb([r, g, b], bg, amount);
            Color::Rgb(r, g, b)
        };
        assert_eq!(
            [plain, searching, boxed],
            [
                vec![fade(0.1), fade(0.0)],
                vec![fade(0.1), fade(0.0)],
                vec![fade(0.65), fade(0.65)],
            ],
            "the unfocused left pane, then the focused right one"
        );
    }

    /// `ui.modal_dim` clamps to `0.0..=1.0`, and a setting left unset fades no
    /// pane.
    #[test]
    fn the_modal_fade_clamps_its_setting_and_needs_it_set() {
        let mut h = TestHarness::with_size(80, 24);
        h.type_action("SwitchWorkspace()");
        let fades = [None, Some(2.0), Some(-1.0)].map(|setting| {
            h.stoat.settings.ui_modal_dim = setting;
            h.stoat.modal_dim()
        });
        assert_eq!(fades, [0.0, 1.0, 0.0]);
    }

    /// A per-pane minimap strip fades with its pane under a boxed modal, its
    /// thumb and its palette alike.
    #[test]
    fn a_pane_strip_fades_with_its_pane_under_a_boxed_modal() {
        let mut h = TestHarness::with_size(120, 24);
        h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
        h.stoat.settings.ui_modal_dim = Some(0.65);
        h.seed_focused_buffer(&"line\n".repeat(60));
        let strip = |h: &mut TestHarness| {
            h.stoat.render();
            command::decode_stream(&h.stoat.apc_scene.all_bytes())
                .into_iter()
                .find_map(|cmd| match cmd {
                    Command::Minimap(strip) => Some(strip),
                    _ => None,
                })
                .expect("the pane declares a strip")
        };

        let plain = strip(&mut h);
        h.type_action("SwitchWorkspace()");
        let faded = strip(&mut h);

        let bg = style_rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg).expect("an RGB ground");
        let fade = |color: [u8; 3]| dim_rgb(color, bg, 0.65);
        let [r, g, b, alpha] = plain.thumb;
        let [r, g, b] = fade([r, g, b]);
        let palette: Vec<[u8; 3]> = plain.palette.iter().map(|&color| fade(color)).collect();
        assert_eq!((faded.thumb, faded.palette), ([r, g, b, alpha], palette));
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::{
        hints::FooterKeys, hints_cache_key, hints_subtitle, keeps_hints_up, screen_own_box, Flags,
        FocusFlags,
    };
    use crate::{
        action_handlers::dispatch, app::Stoat, keymap_state::modal_predicate,
        location_picker::LocationEntry,
    };
    use stoat_action::Action;

    /// The `modal` a frame painted, read back off the hints cache the painting arm
    /// populated.
    fn painted_modal_hints(h: &crate::test_harness::TestHarness) -> Option<u64> {
        h.stoat.hints_cache.as_ref().map(|c| c.key)
    }

    /// A chord's box names what it waits on, even over a screen that keeps a
    /// box up. A screen's own box says that it stays up, and any other box
    /// counts its keys.
    #[test]
    fn the_hints_subtitle_follows_the_chord_the_screen_and_the_key_count() {
        assert_eq!(
            [
                hints_subtitle(true, true, 5),
                hints_subtitle(false, true, 5),
                hints_subtitle(false, false, 5),
                hints_subtitle(false, false, 1),
            ],
            [
                "waiting on a char",
                "stays up on this view",
                "5 keys",
                "1 key"
            ],
        );
    }

    /// The conflict screen's own box shows in every mode. The diff and changes
    /// views share the `diff` box, which shows in a primary mode with no tour
    /// open, and a chord sub-mode over either view shows the mode's box.
    #[test]
    fn a_screen_s_own_box_shows_in_its_modes() {
        assert_eq!(
            [
                screen_own_box(Some("conflict"), "space", false),
                screen_own_box(Some("diff"), "normal", false),
                screen_own_box(Some("diff"), "select", false),
                screen_own_box(Some("diff"), "normal", true),
                screen_own_box(Some("diff"), "space", false),
                screen_own_box(Some("diff_changes"), "normal", false),
                screen_own_box(Some("diff_changes"), "normal", true),
                screen_own_box(Some("diff_changes"), "space", false),
                screen_own_box(Some("file"), "normal", false),
                screen_own_box(None, "normal", false),
            ],
            [
                Some("conflict"),
                Some("diff"),
                Some("diff"),
                None,
                None,
                Some("diff"),
                None,
                None,
                None,
                None
            ],
        );
    }

    /// The conflict screen and the diff and changes views keep a box up, and
    /// an open tour leaves the two diff views' box to the toggle.
    #[test]
    fn an_open_tour_leaves_the_diff_view_box_to_the_toggle() {
        assert_eq!(
            [
                keeps_hints_up(Some("conflict"), true),
                keeps_hints_up(Some("diff"), false),
                keeps_hints_up(Some("diff"), true),
                keeps_hints_up(Some("diff_changes"), false),
                keeps_hints_up(Some("diff_changes"), true),
                keeps_hints_up(Some("file"), false),
                keeps_hints_up(None, false),
            ],
            [true, true, false, true, false, false, false],
        );
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
            None,
            false,
        )
    }

    /// A modal that ends in its own footer of keys, and the palette, paint no
    /// hints box until the hints toggle asks for one. The jumplist has no
    /// footer, so it paints its box unasked.
    #[test]
    fn a_footed_modal_paints_its_hints_box_on_request() {
        let paints_box = |open: &dyn Action, modal: &str, scope: Option<&str>, toggle| {
            let mut h = Stoat::test();
            h.seed_focused_buffer("alpha\n");
            dispatch(&mut h.stoat, &stoat_action::SaveSelection);
            h.stoat.key_hints_visible = toggle;
            dispatch(&mut h.stoat, open);
            h.render_composited();
            let mode = h.stoat.focused_mode().to_string();
            painted_modal_hints(&h) == Some(modal_hints_key(&mode, modal, scope))
        };
        let modals = [
            (
                &stoat_action::OpenFileFinder as &dyn Action,
                "finder",
                Some("all"),
            ),
            (&stoat_action::OpenHelp, "help", None),
            (&stoat_action::SwitchWorkspace, "workspace_picker", None),
            (&stoat_action::OpenCommandPalette, "palette", None),
            (&stoat_action::OpenJumplistPicker, "jumplist", None),
        ];

        let shown: Vec<(bool, bool)> = modals
            .iter()
            .map(|&(action, modal, scope)| {
                (
                    paints_box(action, modal, scope, false),
                    paints_box(action, modal, scope, true),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (false, true),
                (false, true),
                (false, true),
                (false, true),
                (true, true)
            ],
            "the box unasked and on the toggle, per modal"
        );
    }

    /// A modal box names a footer key only where the key does what the footer
    /// says. The jumplist shows its box unasked, so the toggle does not hide
    /// it, and the file finder's box waits for the toggle, so it names the
    /// toggle's key. Neither names a help key, because the modal takes `?`
    /// first.
    #[test]
    fn a_modal_box_names_only_the_footer_keys_that_work() {
        let footer_keys = |open: &dyn Action| {
            let mut h = Stoat::test();
            h.seed_focused_buffer("alpha\n");
            dispatch(&mut h.stoat, &stoat_action::SaveSelection);
            h.stoat.key_hints_visible = true;
            dispatch(&mut h.stoat, open);
            h.render_composited();
            h.stoat
                .hints_cache
                .as_ref()
                .map(|cache| cache.footer_keys.clone())
        };

        assert_eq!(
            [
                footer_keys(&stoat_action::OpenJumplistPicker),
                footer_keys(&stoat_action::OpenFileFinder),
            ],
            [
                Some(FooterKeys::default()),
                Some(FooterKeys {
                    hide: Some("C-?".to_string()),
                    help: None,
                }),
            ],
        );
    }

    /// The frame used to walk the modal fields in its own order, so a second modal
    /// opened over a first could be the one painted while keys still routed to the
    /// other. Both now derive from the same authority.
    #[test]
    fn the_painted_modal_is_the_one_keys_route_to() {
        let mut h = Stoat::test();
        // The finder paints its hints box on the hints toggle alone.
        h.stoat.key_hints_visible = true;
        dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
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
    use crate::{
        action_handlers::{textobject::TextobjectMode, yank},
        app::UpdateEffect,
        lsp::LspSymbolKind,
        render::hints::FooterKeys,
        test_harness::{keys, TestHarness},
    };
    use crossterm::event::{Event, KeyCode};
    use std::{path::Path, sync::Arc};
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
                text.contains("+space"),
                text.contains("+goto"),
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
            "+goto",
            "+z",
            "+bracket next",
            "+bracket prev",
            "+match",
        ];
        let menu_rows = [
            ("space", "save selection"),
            ("space", "+clipboard"),
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

    /// The space box lists the keys the space mode binds, `?` among them, and
    /// leaves out the root keys every mode answers and the Escape that leaves
    /// the mode. A submenu row reads `+name`.
    #[test]
    fn a_chord_mode_box_omits_root_keys() {
        let mut h = TestHarness::with_size(150, 50);
        h.type_keys("space");
        h.render_composited();
        let rows: Vec<(String, String)> = h
            .stoat
            .hints_cache
            .as_ref()
            .expect("the space box laid out its rows")
            .rows
            .iter()
            .map(|row| (row.keys.clone(), row.action.clone()))
            .collect();
        let lists_key = |key: &str| {
            rows.iter()
                .any(|(keys, _)| keys.split(", ").any(|listed| listed == key))
        };

        assert_eq!(
            (
                ["C-?", "C-:", "C-;", "C-+", "C-_", "Esc"].map(lists_key),
                lists_key("?"),
                rows.contains(&("b".to_string(), "+buffer".to_string())),
            ),
            ([false; 6], true, true),
            "the box rows: {rows:?}",
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
        let scratch = chord_rows(&mut h, "space h");

        h.stage_review_scenario("/repo", &[("a.rs", "a\n", "b\n")]);
        h.type_keys("escape");
        let staged = chord_rows(&mut h, "space h");

        let listed = |rows: &[String], names: &[&str]| -> Vec<bool> {
            names
                .iter()
                .map(|name| rows.iter().any(|row| row == short_desc(name)))
                .collect()
        };
        assert_eq!(
            (
                listed(
                    &scratch,
                    &["ForceReloadAll", "LiveReload", "ChangeWalkWrap"]
                ),
                listed(&scratch, &["GitLs", "ToggleStageHunk", "GotoNextChange"]),
                listed(&staged, &["GitLs"]),
            ),
            (vec![true; 3], vec![false; 3], vec![true]),
            "a scratch root lists only the ungated rows, and a repository brings GitLs back",
        );
    }

    #[test]
    fn the_git_chord_box_lists_its_pin() {
        let mut h = TestHarness::with_size(150, 50);
        let pin = short_desc("PinMode");
        assert!(
            chord_rows(&mut h, "space h").iter().any(|row| row == pin),
            "the space h box lists the ~ pin",
        );
    }

    /// The chord waits on one char in a primary mode, so its box lists the
    /// chars the chord takes in place of the keymap's rows.
    #[test]
    fn the_textobject_chord_box_lists_its_chars() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "m i");
        h.type_keys("m i");
        let text = box_text(&mut h);

        let labels = [
            "word",
            "WORD",
            "paragraph",
            "function",
            "class",
            "parameter",
            "comment",
            "test",
            "entry",
            "xml element",
            "change",
            "closest pair",
            "pair",
            "cancel",
        ];
        assert_eq!(
            (rows, text.contains("inside")),
            (labels.map(String::from).to_vec(), true),
            "the box lists every type, the pair chars on one row, under its title",
        );
    }

    #[test]
    fn the_textobject_chord_box_leaves_with_the_chord() {
        let mut h = TestHarness::with_size(150, 50);
        h.type_keys("m a");
        let armed = box_text(&mut h);
        h.type_keys("w");
        let resolved = box_text(&mut h);

        assert_eq!(
            (
                armed.contains("around"),
                resolved.contains("around"),
                resolved.contains("paragraph"),
            ),
            (true, false, false),
            "the box shows while the chord waits and leaves with its char",
        );
    }

    /// The forced box caches the keymap's rows under the same primary mode the
    /// chord waits in, so the chord's part of the cache key is what brings its
    /// table in. The chord arms by hand, since a typed `m` repaints the match
    /// chord's box between the two frames.
    #[test]
    fn the_textobject_chord_box_replaces_the_forced_box() {
        let mut h = TestHarness::with_size(150, 50);
        h.stoat.key_hints_visible = true;
        h.render_composited();
        h.stoat.pending_textobject_select = Some((TextobjectMode::Inner, 1));
        h.render_composited();

        let first = h
            .stoat
            .hints_cache
            .as_ref()
            .and_then(|cache| cache.rows.first())
            .map(|row| row.action.as_str());
        assert_eq!(first, Some("word"), "the chord's table takes the box");
    }

    #[test]
    fn the_surround_add_chord_box_lists_the_pair_chars() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);
        h.type_keys("w");
        let rows = chord_rows(&mut h, "m s");
        h.type_keys("m s");
        let text = box_text(&mut h);

        let labels = [
            "pair",
            "quote",
            "own line",
            "that char both sides",
            "cancel",
        ];
        assert_eq!(
            (rows, text.contains("surround")),
            (labels.map(String::from).to_vec(), true),
            "the brackets and the quotes fold into a row each, under the box title",
        );
    }

    #[test]
    fn the_surround_replace_chord_box_follows_its_two_stages() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = (chord_rows(&mut h, "m r"), chord_rows(&mut h, "m r ("));
        h.type_keys("m r");
        let from = box_text(&mut h);
        h.type_keys("(");
        let to = box_text(&mut h);
        h.type_keys("[");
        let done = box_text(&mut h);

        let titles = |text: &str| (text.contains("replace pair"), text.contains("replace with"));
        let from_rows = ["closest pair", "pair", "cancel"];
        let to_rows = ["pair", "quote", "that char both sides", "cancel"];
        assert_eq!(
            (rows, titles(&from), titles(&to), titles(&done)),
            (
                (
                    from_rows.map(String::from).to_vec(),
                    to_rows.map(String::from).to_vec(),
                ),
                (true, false),
                (false, true),
                (false, false),
            ),
            "each stage shows its own box, and the box leaves with the second char",
        );
    }

    #[test]
    fn the_surround_delete_chord_box_lists_the_pair_chars() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "m d");
        h.type_keys("m d");
        let text = box_text(&mut h);

        let labels = ["closest pair", "pair", "cancel"];
        assert_eq!(
            (rows, text.contains("delete pair")),
            (labels.map(String::from).to_vec(), true),
            "the pair chars fold into one row under the box title",
        );
    }

    #[test]
    fn the_replace_chord_box_lists_its_keys() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "r");
        h.type_keys("r");
        let text = box_text(&mut h);

        let labels = [
            "replace each selected char",
            "replace with newline",
            "replace with tab",
            "cancel",
        ];
        assert_eq!(
            (rows, has_box_titled(&text, "replace with char")),
            (labels.map(String::from).to_vec(), true),
            "the box lists a char, Enter, Tab, and Esc under its title",
        );
    }

    #[test]
    fn the_find_chord_box_names_its_direction() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "f");
        h.type_keys("f");
        let find = box_text(&mut h);
        h.type_keys("escape T");
        let till = box_text(&mut h);

        let labels = ["that char", "line ending", "tab", "cancel"];
        assert_eq!(
            (
                rows,
                find.contains("find next"),
                till.contains("till prev"),
                till.contains("find next"),
            ),
            (labels.map(String::from).to_vec(), true, true, false),
            "each find kind titles the box with its own direction",
        );
    }

    /// Whether `text` holds the top border of a box titled `title`, which a
    /// row label holding the same word never matches.
    fn has_box_titled(text: &str, title: &str) -> bool {
        text.contains(&format!("\u{250c} {title} \u{2500}"))
    }

    /// The top and footer rows of the box titled `title`, each from the box's
    /// left edge on, with every run of blanks or of `─` read as one space.
    fn box_edges(h: &mut TestHarness, title: &str) -> Option<[String; 2]> {
        let text = box_text(h);
        let lines: Vec<&str> = text.lines().collect();
        let corner = format!("\u{250c} {title} ");
        let (top, col) = lines.iter().enumerate().find_map(|(y, line)| {
            let at = line.find(&corner)?;
            Some((y, line[..at].chars().count()))
        })?;
        let bottom = (top..lines.len()).find(|&y| lines[y].chars().nth(col) == Some('\u{2514}'))?;

        let words = |y: usize| {
            lines[y]
                .chars()
                .skip(col)
                .collect::<String>()
                .split(|c: char| c.is_whitespace() || c == '\u{2500}')
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        };
        Some([words(top), words(bottom - 1)])
    }

    /// The diff view keeps its box up with the toggle off, and the toggle hides
    /// the box and brings it back. The flag that forces a box on elsewhere
    /// shows none here. The box names the view and says that it stays, and its
    /// footer reads the hunk under the cursor, `-` above the first one. A chord
    /// over the view shows its own box, whose footer names no hunk.
    #[test]
    fn the_diff_view_keeps_its_box_up_with_the_hunk_position() {
        let mut h = TestHarness::with_size(150, 50);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .set_diff_view(true);
        let above = box_edges(&mut h, "diff");
        h.type_keys("j j j");
        let on = box_edges(&mut h, "diff");
        let toggle = |h: &mut TestHarness| {
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);
            box_edges(h, "diff")
        };
        let hidden = toggle(&mut h);
        h.stoat.key_hints_visible = true;
        let forced = box_edges(&mut h, "diff");
        let shown = toggle(&mut h);
        h.type_keys("space");
        let chord_footer = box_edges(&mut h, "space").map(|[_, footer]| footer);

        let edges = |at: &str| {
            Some([
                "\u{250c} diff stays up on this view \u{2510}".to_string(),
                format!("\u{2502} hunk {at}/1 C-? hide \u{b7} ? help \u{2502}"),
            ])
        };
        assert_eq!(
            (above, on, hidden, forced, shown, chord_footer),
            (
                edges("-"),
                edges("1"),
                None,
                None,
                edges("1"),
                Some("\u{2502} Esc ? help \u{2502}".to_string()),
            ),
        );
    }

    /// The changes view keeps the diff view's box up with the toggle off, and
    /// the toggle hides it. The footer reads the hunk under the cursor in the
    /// focused excerpt, `-` above the first one.
    #[test]
    fn the_changes_view_keeps_the_diff_box_up_with_the_hunk_position() {
        let mut h = TestHarness::with_size(150, 50);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle_diff_jobs();
        let above = box_edges(&mut h, "diff");
        h.type_keys("j j j");
        let on = box_edges(&mut h, "diff");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleKeyHints);
        let hidden = box_edges(&mut h, "diff");

        let edges = |at: &str| {
            Some([
                "\u{250c} diff stays up on this view \u{2510}".to_string(),
                format!("\u{2502} hunk {at}/1 C-? hide \u{b7} ? help \u{2502}"),
            ])
        };
        assert_eq!(
            (h.stoat.current_view(), above, on, hidden),
            (Some("diff_changes"), edges("-"), edges("1"), None),
        );
    }

    /// The diff views' own box lists the keys their view blocks bind, with a
    /// short label on each row, and none of the editor's other context keys.
    /// Keys with one label share a row. A wheel notch is no key to press, so
    /// the Alt notches stay off the box.
    #[test]
    fn the_diff_box_lists_the_view_s_own_keys() {
        let box_rows = |h: &mut TestHarness| {
            h.render_composited();
            h.stoat
                .hints_cache
                .as_ref()
                .expect("the view keeps its box up")
                .rows
                .iter()
                .map(|row| [row.keys.clone(), row.action.clone()])
                .collect::<Vec<_>>()
        };
        let mut h = TestHarness::with_size(150, 50);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .set_diff_view(true);
        let split = box_rows(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle_diff_jobs();
        let changes = box_rows(&mut h);

        let rows = |groups: &[&[[&str; 2]]]| -> Vec<[String; 2]> {
            groups
                .concat()
                .into_iter()
                .map(|row| row.map(str::to_owned))
                .collect()
        };
        let dials = [
            ["C-6", "underline changes"],
            ["C-7", "bold changes"],
            ["C-8", "syntax colors"],
            ["C-9", "tint down"],
            ["C-0", "tint up"],
        ];
        let walk = [["MBk", "next change"], ["MFw", "prev change"]];
        let hunks = [["S-MBk", "next hunk"], ["S-MFw", "prev hunk"]];
        assert_eq!(
            (split, changes),
            (
                rows(&[
                    &dials[..],
                    &walk[..],
                    &hunks[..],
                    &[["C-t", "files / changes"]],
                ]),
                rows(&[
                    &dials[..],
                    &[
                        ["q", "close"],
                        ["C-t", "files / changes"],
                        ["Ret", "open"],
                        ["j, Dn", "next row"],
                        ["k, Up", "prev row"],
                        ["C-f, PgDn", "page down"],
                        ["C-b, PgUp", "page up"],
                        ["C-d", "half page down"],
                        ["C-u", "half page up"],
                    ],
                    &walk[..],
                    &hunks[..],
                    &[[":", "commands"]],
                ]),
            ),
        );
    }

    /// The conflict screen's box footer names the chunk at the cursor, `-`
    /// on a line outside every chunk.
    #[test]
    fn the_conflict_box_footer_names_the_chunk_at_the_cursor() {
        let mut h = TestHarness::with_size(150, 50);
        h.stoat.active_workspace_mut().git_root = std::path::PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .conflicted_file(
                "f.txt",
                Some("a\nb\nc\nd\ne\n"),
                Some("a\nB\nc\nD\ne\n"),
                Some("a\nX\nc\nY\ne\n"),
            );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Conflict);
        h.settle();
        let footer = |h: &mut TestHarness| box_edges(h, "conflict").map(|[_, footer]| footer);

        let first = footer(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ConflictNextChunk);
        h.settle();
        let second = footer(&mut h);
        h.type_keys("g g");
        let outside = footer(&mut h);

        assert_eq!(
            [first, second, outside],
            ["1", "2", "-"].map(|at| Some(format!(
                "\u{2502} chunk {at} / 2 C-? hide \u{b7} ? help \u{2502}"
            ))),
        );
    }

    /// A box's footer names a key only where the key does what the footer
    /// says. An armed chord takes both keys as its char, so its box ends at
    /// its last row. A chord mode's box shows whatever the toggle holds, so it
    /// names no hide key, and its help key is the chord's own way to the help.
    /// A pin leaves the box to the toggle, so a pinned chord's box names the
    /// hide key again.
    #[test]
    fn a_hints_footer_names_only_the_keys_that_work() {
        let mut h = TestHarness::with_size(150, 50);
        let footer = |h: &mut TestHarness| {
            h.render_composited();
            h.stoat
                .hints_cache
                .as_ref()
                .expect("a box laid out")
                .footer_keys
                .clone()
        };
        h.type_keys("r");
        let chord = (footer(&mut h), box_edges(&mut h, "replace with char"));
        h.type_keys("escape space");
        let space = footer(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::PinMode);
        h.stoat.key_hints_visible = true;
        let pinned = footer(&mut h);
        h.type_keys("escape z");
        let help_chord = footer(&mut h);
        h.type_keys("escape");
        let forced = footer(&mut h);

        let keys = |hide: Option<&str>, help: Option<&str>| FooterKeys {
            hide: hide.map(String::from),
            help: help.map(String::from),
        };
        assert_eq!(
            (chord, [space, pinned, help_chord, forced]),
            (
                (
                    keys(None, None),
                    Some([
                        "\u{250c} replace with char waiting on a char \u{2510}".to_string(),
                        "\u{2502} Esc cancel \u{2502}".to_string(),
                    ]),
                ),
                [
                    keys(None, Some("Esc ?")),
                    keys(Some("C-?"), Some("Esc ?")),
                    keys(None, Some("?")),
                    keys(Some("C-?"), Some("?")),
                ],
            ),
        );
    }

    #[test]
    fn the_register_chord_box_lists_the_special_registers() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "\"");
        h.type_keys("\"");
        let text = box_text(&mut h);

        let labels = [
            "unnamed",
            "system clipboard",
            "primary clipboard",
            "search",
            "blackhole",
            "selection index",
            "selection contents",
            "document path",
            "command",
            "named register",
            "cancel",
        ];
        assert_eq!(
            (rows, has_box_titled(&text, "register")),
            (labels.map(String::from).to_vec(), true),
            "the box lists each special register by char under its title",
        );
    }

    /// The insert-mode chord waits in insert mode, where no keymap box shows,
    /// and names its register through the same table.
    #[test]
    fn the_insert_register_chord_shows_the_same_box() {
        let mut h = TestHarness::with_size(150, 50);
        h.type_keys("i ctrl-r");
        let armed = box_text(&mut h);
        h.type_keys("\"");
        let resolved = box_text(&mut h);

        let shows = |text: &str| (has_box_titled(text, "register"), text.contains("blackhole"));
        assert_eq!(
            (shows(&armed), shows(&resolved)),
            ((true, true), (false, false)),
            "the box shows while the chord waits and leaves with its char",
        );
    }

    #[test]
    fn the_macro_replay_chord_box_lists_the_registers() {
        let mut h = TestHarness::with_size(150, 50);
        let rows = chord_rows(&mut h, "q");
        h.type_keys("q");
        let text = box_text(&mut h);

        let labels: Vec<String> = yank::REGISTER_HINTS
            .iter()
            .map(|&(_, label)| label)
            .chain(["cancel"])
            .map(String::from)
            .collect();
        assert_eq!(
            (rows, has_box_titled(&text, "replay macro")),
            (labels, true),
            "the replay chord names its register through the register table",
        );
    }

    /// The mark actions have no default binding, so each one arms through
    /// dispatch, as the command palette arms it.
    #[test]
    fn the_mark_chord_box_names_its_request() {
        let mut h = TestHarness::with_size(150, 50);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SetMark);
        let set = box_text(&mut h);
        let rows: Vec<String> = h
            .stoat
            .hints_cache
            .as_ref()
            .map(|cache| cache.rows.iter().map(|row| row.action.clone()).collect())
            .unwrap_or_default();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoMark);
        let line = box_text(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoMarkExact);
        let exact = box_text(&mut h);

        assert_eq!(
            (
                rows,
                has_box_titled(&set, "set mark"),
                has_box_titled(&line, "goto mark line"),
                has_box_titled(&exact, "goto mark"),
            ),
            (
                ["buffer mark", "global mark", "cancel"]
                    .map(String::from)
                    .to_vec(),
                true,
                true,
                true,
            ),
            "the box lists the mark letters under the title of each request",
        );
    }

    /// A chord whose char finds nothing edits no text and moves no cursor, so
    /// the box leaving is the one change that frame has to paint.
    #[test]
    fn a_chord_key_that_finds_nothing_still_repaints() {
        let mut h = TestHarness::with_size(150, 50);
        open_foo_bar(&mut h);
        h.type_keys("m i");
        let effect = h.stoat.update(Event::Key(keys::key(KeyCode::Char('('))));
        assert_eq!(
            effect,
            UpdateEffect::Redraw,
            "the frame takes the inside box down"
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
                "OpenPaneFinder",
            ]
            .map(short_desc)
            .into_iter()
            .chain(["+pane nav new", "+pane display"])
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
        let recall = "last picker";
        let lists_recall = |rows: Vec<String>| rows.iter().any(|row| row == recall);
        let fresh = lists_recall(chord_rows(&mut h, "space"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.type_keys("escape");
        assert_eq!(
            (fresh, lists_recall(chord_rows(&mut h, "space"))),
            (false, true),
        );
    }

    /// The space box names each key with the config's short label, and a key
    /// that enters a submenu with the submenu's name, as the HintBox board
    /// lists them.
    #[test]
    fn the_space_box_names_its_keys_with_their_short_labels() {
        let mut h = TestHarness::with_size(150, 50);
        h.type_keys("space");
        h.render_composited();
        let rows: Vec<(String, String)> = h
            .stoat
            .hints_cache
            .as_ref()
            .expect("the space chord laid out its box")
            .rows
            .iter()
            .map(|row| (row.keys.clone(), row.action.clone()))
            .collect();

        let expected = [
            ("q", "quit"),
            ("a", "+pane nav"),
            ("t", "run"),
            ("c", "claude"),
            ("w", "+workspace"),
            ("b", "+buffer"),
            ("l", "+lsp"),
            ("\"", "+clipboard"),
            ("C", "block comment"),
            ("A-c", "line comment"),
            ("p", "files"),
            ("r", "reload all"),
            ("g", "+goto"),
            ("h", "+git"),
            ("W", "walkthroughs"),
            ("s", "save selection"),
            ("/", "code search"),
            ("?", "toggle hints"),
        ];
        assert_eq!(
            rows,
            expected.map(|(keys, label)| (keys.to_string(), label.to_string())),
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
