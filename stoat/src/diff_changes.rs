//! The changes view, which shows every change against the diff base as a
//! stack of per-file diff excerpts.
//!
//! Each changed file gets one editor in the per-file diff view, over the
//! workspace's own buffer of the file. The editor folds every unchanged run
//! that lies more than three rows from a hunk, so the excerpt shows the file's
//! hunks with their context. A refined hunk shows only the rows the tree pass
//! marked and its last row, so the unmarked rows of a block the formatter
//! re-indented fold too. The stack paints the excerpts in path order, each
//! under a header row that names its file. The focused excerpt's editor is the
//! focused editor, so every editor key acts on the real file at the real line
//! and column.
//!
//! See also:
//! - [`pump_diff_changes_stack`] for the list of files, the folds, and the landings.
//! - [`layout_stack`] for the rows the paint lays the excerpts on.

use crate::{
    action_handlers::{review, view},
    app::Stoat,
    buffer::{BufferId, SharedBuffer},
    buffer_lifecycle,
    buffer_registry::{BufferRegistry, OpenOrigin},
    diff_map::{DiffHunk, DiffHunkStatus, DiffMap},
    display_map::{
        BlockRowKind, DisplayPoint, DisplaySnapshot, FoldPlaceholder, FoldScope, FoldSnapshot,
        InlayPoint, InlaySnapshot, ScopeMark,
    },
    editor_state::{EditorId, EditorState},
    multi_buffer::MultiBufferSnapshot,
    pane::View,
    render,
    theme::scope,
    workspace::{diff::DiffBase, Workspace},
};
use ratatui::layout::Rect;
use slotmap::{new_key_type, SlotMap};
use std::{
    cell::OnceCell,
    collections::BTreeMap,
    mem,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_language::{
    scope_chain,
    structural_diff::{parse_memoized, TreeCache},
    Language, Query, ScopeSegment, SyntaxState, Tree,
};
use stoat_text::{Anchor, Bias, Point, Rope};
use stoat_widgets::text_run;

/// Rows of context an excerpt shows on each side of a hunk, and on each side
/// of a marked run of a refined hunk.
const CONTEXT_ROWS: u32 = 3;

/// Lines one expand opens of a fold. A fold of this many lines or fewer opens
/// whole.
const EXPAND_STEP: u32 = 10;

/// The space left of a fold row's first Fold Button, in sixteenths of a cell.
///
/// The ExpandOptions board draws each button as an 18 px square, 3 px in from
/// the row's left and 1 px from the next. One board px is two sixteenths
/// across.
pub(crate) const FOLD_BUTTON_MARGIN: u16 = 6;
/// The width of a Fold Button's square box, in sixteenths of a cell.
pub(crate) const FOLD_BUTTON_WIDTH: u16 = 36;
/// The space between two Fold Buttons, in sixteenths of a cell.
pub(crate) const FOLD_BUTTON_GAP: u16 = 2;

/// The seconds a revealed row's mark holds at full strength, as the
/// ExpandFlow board times it.
pub(crate) const REVEAL_HOLD: f32 = 0.6;
/// The seconds a revealed row's mark takes to fade out after its hold.
pub(crate) const REVEAL_FADE: f32 = 0.4;

/// The fewest rows a changes view shows for its first row to pin a File
/// Header.
///
/// A pinned row hides the stack row under it. In a short view that row is too
/// large a share of the view, so a shorter view shows the stack alone.
const PINNED_MIN_VIEWPORT: usize = 8;

/// A fold's buffer range and the Scope its fold row centers.
type HeldFold = (Range<Point>, Option<Arc<FoldScope>>);

new_key_type! {
    /// Key for one changes view pane's [`DiffChangesState`].
    pub struct DiffChangesId;
}

/// One changes view pane's stack of excerpts, its scroll, and the view the
/// changes view replaced in that pane.
pub(crate) struct DiffChangesState {
    /// One excerpt per changed file, in path order. The focused excerpt stays
    /// though an edit took its file off the list, until the focus moves off it.
    pub excerpts: Vec<Excerpt>,
    /// The index in [`Self::excerpts`] of the excerpt whose editor has focus.
    pub focused: usize,
    /// The first stack row on screen.
    ///
    /// A stack row is a header row or a display row of an excerpt, in the
    /// order [`layout_stack`] lays them out.
    pub scroll: usize,
    /// The stack's row count as of the last [`layout_stack`].
    pub rows: usize,
    /// The workspace stamp the excerpts were listed at, or `None` before the
    /// first list.
    pub synced: Option<StackStamp>,
    /// The focused excerpt's path as of the last list, or `None` before the
    /// first list and on an empty stack.
    ///
    /// A relist keeps that excerpt though its file is off the list, while its
    /// buffer holds an edit made since the excerpt opened. An edit that removes
    /// the file's last change thus leaves the cursor where it is. A focus move
    /// to another excerpt makes the list stale, and the next relist drops the
    /// excerpt if its file is still off the list.
    pub listed_focus: Option<String>,
    /// Rows visible on the most recent paint. Zero until the first paint.
    pub viewport_rows: usize,
    /// The minimap strip the most recent paint reserved at the pane's right
    /// edge, or `None` where no per-pane strip shows.
    pub minimap_rect: Option<Rect>,
    /// The rect the most recent paint laid the stack's rows into, or `None`
    /// before the first paint.
    ///
    /// Every excerpt paints from its left edge, so a press finds a fold row's
    /// Fold Buttons by the cells it lands past that edge.
    pub content_rect: Option<Rect>,
    /// The view the changes view replaced in its pane, which closing the
    /// changes view puts back.
    pub covered: Option<View>,
    /// True when the covered editor showed the diff view, so closing the
    /// changes view returns to the per-file diff it came from.
    pub covered_diff: bool,
    /// The place the cursor moves to once its file's excerpt is folded.
    ///
    /// A changes view opens before its excerpts exist, so the editor position
    /// it opened from waits here.
    pub pending_landing: Option<Landing>,
    /// The row of the view that the next paint puts the cursor's row on, in
    /// place of the least scroll that shows it.
    ///
    /// A landing sets it from the screen row the cursor held in the layout the
    /// changes view opened from. The paint applies it after it fits each
    /// excerpt to the width, so the rows it counts are the rows it paints.
    pub held_screen_row: Option<u16>,
    /// The text a reload keeps on a row of the view until the reader next
    /// acts.
    pub view_hold: Option<StackHold>,
    /// A reload's seat placed the stack with the text under it, so the
    /// terminal takes the next scroll target with no ease. The emit drops the
    /// stack's pool when that target moved and declares it again on the
    /// target.
    pub pool_swap: bool,
    /// The focused excerpt's editor and its cursor as of the last paint.
    ///
    /// The paint scrolls to the cursor only when the cursor moved since, so a
    /// wheel scroll leaves the cursor off screen as an editor's wheel does,
    /// while a cursor move that no key made still comes into view.
    pub followed_cursor: Option<(EditorId, Point)>,
    /// The stack row of the focused excerpt's cursor as of the last emit to
    /// stoatty, or `None` before one.
    ///
    /// A selection step patches the page that the cursor left, so that page
    /// sheds the old selection even when no composite reads it.
    pub last_cursor_row: Option<usize>,
    /// The Scope that [`Self::cursor_scope`] last read, which it answers again
    /// while the cursor's row and the parse hold.
    pub scope_cache: Option<CursorScope>,
}

/// The Scope of the cursor's row in one excerpt, with the row and the parse
/// it was read from.
pub(crate) struct CursorScope {
    editor: EditorId,
    row: u32,
    /// The syntax version of the parse the chain was read from.
    syntax: u64,
    scope: Arc<FoldScope>,
}

/// The text a reload pins to a row of a changes view, so the stack rows that
/// the old and the new files share keep their screen rows.
///
/// Each paint finds the seat's stack row and scrolls so that row sits on
/// [`Self::screen_row`]. The reload edit, the relist, and the refold that
/// follows a landed diff map each move that row, and the paint after each one
/// scrolls to match. The hold ends at the reader's next input, and when
/// another writer moved the scroll from where the last paint left it.
pub(crate) struct StackHold {
    pub(crate) seat: StackSeat,
    pub(crate) screen_row: usize,
    /// The scroll the last paint left, `None` before the first one.
    pub(crate) rested: Option<usize>,
}

/// What a [`StackHold`] keeps on its screen row.
pub(crate) enum StackSeat {
    /// The focused excerpt's cursor, wherever the reload's landings put it.
    Cursor,
    /// A text position in the excerpt that the editor shows.
    Text(EditorId, Anchor),
}

/// The rows at the top of a changes view that repeat the context of the stack
/// row under them, and hold while the stack scrolls.
///
/// The first row repeats the File Header of the excerpt under the stack's top
/// edge, so a long excerpt always says which file it shows. The second row
/// repeats the Scope of the hunk under it, so a long hunk always says which
/// item it is in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PinnedHeader {
    /// The index in [`DiffChangesState::excerpts`] of the excerpt whose header
    /// the first row repeats, or `None` when the first row shows the stack.
    pub file: Option<usize>,
    /// The Scope row that the second row repeats, or `None` when the second
    /// row shows the stack.
    pub scope: Option<PinnedScope>,
}

/// The Scope that a changes view's second row repeats, with the stack row of
/// the Scope row it repeats, which a press on the pinned row acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PinnedScope {
    pub scope: Arc<FoldScope>,
    pub row: usize,
}

impl PinnedHeader {
    /// The rows at the top of the view that pinned rows cover.
    pub(crate) fn rows(&self) -> usize {
        usize::from(self.file.is_some()) + usize::from(self.scope.is_some())
    }
}

/// One changed file in a changes view's stack.
pub(crate) struct Excerpt {
    pub buffer_id: BufferId,
    /// The editor that shows the file in the diff view, which no pane shows.
    pub editor_id: EditorId,
    /// The file's path relative to the repository's working directory, which
    /// the header row shows and the stack sorts by.
    pub rel_path: String,
    /// The version of the buffer's diff map the folds were cut from, or `None`
    /// until the map lands.
    pub folded_version: Option<usize>,
    /// The buffer's syntax version when the folds were cut, or `None` before a
    /// parse. A parse that lands after the cut cuts again, so the fold rows
    /// take the Scopes that parse reads.
    pub folded_syntax: Option<u64>,
    /// The stack row of the excerpt's header, which [`layout_stack`] writes.
    pub top: usize,
    /// Whether the stack opened the buffer, so the buffer closes with the
    /// excerpt. A buffer that was open before the excerpt stays open.
    pub owns_buffer: bool,
    /// The buffer's text version when the excerpt opened.
    ///
    /// A focused excerpt whose buffer moved past this version stays though its
    /// file left the list, because an edit there took the file's last change.
    /// A file that a commit or a base change took off the list took no edit,
    /// so its excerpt goes though it holds the focus.
    pub opened_version: u64,
    /// The row spans a reader opened out of the folds, which every refold keeps
    /// open until a collapse drops them.
    ///
    /// Each span is an anchor at the start of its first row and an anchor at
    /// the end of its last row, so an edit above a span carries the span with
    /// its lines.
    pub revealed: Vec<(Anchor, Anchor)>,
    /// The Scope of the first change when no fold lies above it, which a row
    /// under the header shows.
    ///
    /// `None` when a fold row names the first change. It is `None` too when
    /// the first change lies at the top level, or the language has no outline
    /// query, since the row then has no Scope to show.
    pub scope_row: Option<Arc<FoldScope>>,
}

/// The size of one excerpt's diff, which its header row counts out.
///
/// The line counts take every row a hunk marks. A modified line counts once on
/// each side. A moved line counts as added where it went and as removed where
/// it left. A reindented line counts on each side too, though the tree pass
/// finds no change in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExcerptStats {
    /// Live rows the hunks mark.
    pub added: u32,
    /// Base lines the hunks remove.
    pub removed: u32,
    /// Every hunk the line pass found, which the git walk stops on.
    pub hunks: usize,
    /// Hunks the tree pass narrowed to the rows whose tokens changed.
    pub structural: usize,
}

impl ExcerptStats {
    /// Counts the hunks of `map`.
    pub(crate) fn of(map: &DiffMap) -> Self {
        map.hunks().fold(Self::default(), |stats, hunk| {
            // A moved-to hunk's base range names the lines it came from. The
            // moved-away hunk, which holds no live rows, removes those lines,
            // so only that hunk counts them.
            let removed = match hunk.status {
                DiffHunkStatus::Deleted | DiffHunkStatus::Modified => map.base_line_count(hunk),
                DiffHunkStatus::Moved if hunk.buffer_line_range.is_empty() => {
                    map.base_line_count(hunk)
                },
                DiffHunkStatus::Moved | DiffHunkStatus::Added => 0,
            };
            Self {
                added: stats.added + hunk.buffer_line_range.len() as u32,
                removed: stats.removed + removed,
                hunks: stats.hunks + 1,
                structural: stats.structural + usize::from(hunk.refined()),
            }
        })
    }
}

/// The editor position a changes view lands its cursor on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Landing {
    /// The buffer the position is in, whose diff map decides whether the file
    /// is still to join the list.
    pub buffer_id: BufferId,
    pub rel_path: String,
    pub row: u32,
    pub column: u32,
    /// The row of the view the cursor sat on in the layout the landing came
    /// from, so a flip keeps the line where the eyes are. `None` when the
    /// cursor sat above the view.
    pub screen_row: Option<u16>,
}

/// The state of a workspace that a changes view's excerpts were listed from.
///
/// A list is current while its stamp equals the workspace's stamp now.
/// `epoch` moves when the diff base, the git state, or the repository tally
/// changes. `maps` moves with the diff map of every open buffer, which
/// decides whether an unsaved edit puts its file in the list, and `paths`
/// moves when a buffer opens, closes, or changes its path.
///
/// Each part is a counter its owner moves, so a read walks no buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StackStamp {
    epoch: u64,
    maps: u64,
    paths: u64,
}

/// The way an expand opens the unchanged lines of a fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExpandDir {
    /// The nearest fold above the cursor, from its bottom edge.
    Up,
    /// The nearest fold below the cursor, from its top edge.
    Down,
    /// The whole fold under the cursor, or the nearest fold each way whole.
    All,
}

/// The edge of a fold that an expand opens lines from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FoldEdge {
    /// The fold's first lines, so the hunk above the fold grows down.
    Top,
    /// The fold's last lines, so the hunk below the fold grows up.
    Bottom,
    /// The whole fold.
    Whole,
}

/// The Fold Buttons a fold row carries, by the hunks on either side of its
/// fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FoldButtons {
    /// The fold above a file's first hunk, which only grows up.
    Up,
    /// The fold below a file's last hunk, which only grows down.
    Down,
    /// A fold between two hunks, which grows from either edge.
    Both,
    /// A fold of [`EXPAND_STEP`] lines or fewer, which opens whole.
    Whole,
}

impl FoldButtons {
    /// The arrow of each button from left to right, with the edge a press on
    /// it opens the fold from.
    pub(crate) fn glyphs(self) -> &'static [(char, FoldEdge)] {
        match self {
            Self::Up => &[('\u{2191}', FoldEdge::Bottom)],
            Self::Down => &[('\u{2193}', FoldEdge::Top)],
            Self::Both => &[('\u{2193}', FoldEdge::Top), ('\u{2191}', FoldEdge::Bottom)],
            Self::Whole => &[('\u{2195}', FoldEdge::Whole)],
        }
    }
}

/// The mark on the rows an expand just opened, which fades out so the opened
/// lines then read as plain context.
///
/// An excerpt's editor holds one per expand. The frame timer ages it and drops
/// it once its fade ends, at [`REVEAL_HOLD`] plus [`REVEAL_FADE`].
#[derive(Debug, PartialEq)]
pub(crate) struct RevealFlash {
    /// The buffer rows the expand opened.
    pub rows: Range<u32>,
    /// Seconds since the expand.
    pub age: f32,
}

impl RevealFlash {
    /// The share of the mark's full color the rows take at the flash's age.
    ///
    /// The full mark holds through [`REVEAL_HOLD`], so the eye finds the
    /// opened rows. It then eases out over [`REVEAL_FADE`] on the curve a
    /// reload flash fades on, so the mark leaves no hard edge as it ends.
    pub(crate) fn strength(&self) -> f32 {
        // The end reads the whole age, the sum the frame timer drops a flash
        // at. At that age `age - REVEAL_HOLD` rounds below `REVEAL_FADE` in
        // f32, which leaves a sliver of strength.
        if self.age >= REVEAL_HOLD + REVEAL_FADE {
            return 0.0;
        }
        let fading = self.age - REVEAL_HOLD;
        if fading <= 0.0 {
            return 1.0;
        }
        (1.0 - fading / REVEAL_FADE).powi(2)
    }
}

/// One run of the Scope chip a fold row centers, with the role that colors it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScopePart {
    pub text: String,
    pub role: ScopeRole,
}

/// The theme scope a [`ScopePart`] takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScopeRole {
    /// An item's keyword, in `diff.scope.keyword`.
    Keyword,
    /// An item's name, in `diff.scope.name`.
    Name,
    /// A mark between items or a label, in `diff.scope.separator`.
    Separator,
    /// The mark of an added item, in `diff.added`.
    Added,
    /// The mark of a removed item, in `diff.deleted`.
    Removed,
}

impl ScopePart {
    fn new(text: impl Into<String>, role: ScopeRole) -> Self {
        Self {
            text: text.into(),
            role,
        }
    }
}

impl ScopeRole {
    /// The theme scope whose foreground colors a part of this role.
    pub(crate) fn theme_scope(self) -> &'static str {
        match self {
            Self::Keyword => scope::DIFF_SCOPE_KEYWORD,
            Self::Name => scope::DIFF_SCOPE_NAME,
            Self::Separator => scope::DIFF_SCOPE_SEPARATOR,
            Self::Added => scope::DIFF_ADDED,
            Self::Removed => scope::DIFF_DELETED,
        }
    }
}

/// A buffer's parse and its language, which a fold row reads its Scope from,
/// with the memo of trees that a parse of the diff's base text reads through.
pub(crate) struct ScopeParse {
    syntax: SyntaxState,
    language: Arc<Language>,
    cache: TreeCache,
}

impl ScopeParse {
    /// The parse of buffer `id`, or `None` while the buffer has no parse and
    /// for a language with no outline query. A base parse reads through
    /// `cache`, the memo the workspace's diffs share.
    ///
    /// The parse is a clone, which bumps a refcount and copies no tree.
    pub(crate) fn of(buffers: &BufferRegistry, id: BufferId, cache: &TreeCache) -> Option<Self> {
        let language = buffers.language_for(id)?;
        language.outline_query()?;
        Some(Self {
            syntax: buffers.syntax(id)?.clone(),
            language,
            cache: cache.clone(),
        })
    }
}

/// The reader of one cut's Scopes, from the buffer's parse and, on the first
/// change that needs it, a parse of the diff's base text.
struct ScopeReader<'a> {
    parse: &'a ScopeParse,
    query: &'a Query,
    map: &'a DiffMap,
    rope: &'a Rope,
    /// The rows of each change with its hunk, in buffer order.
    changes: Vec<(Range<u32>, &'a DiffHunk)>,
    /// The base text's tree and rope once a change reads them, holding `None`
    /// when the diff has no base text or the base does not parse.
    base: OnceCell<Option<(Tree, Rope)>>,
}

impl<'a> ScopeReader<'a> {
    /// The reader of `map`'s changes over `rope`, which `parse` parsed, or
    /// `None` for a language with no outline query.
    fn new(parse: &'a ScopeParse, map: &'a DiffMap, rope: &'a Rope) -> Option<Self> {
        Some(Self {
            parse,
            query: parse.language.outline_query()?,
            map,
            rope,
            changes: changed_rows(map),
            base: OnceCell::new(),
        })
    }

    /// The Scope of the fold over `gap` in a buffer of `line_count` rows, the
    /// items that hold the first change below the gap, or the end of the file
    /// for a gap that reaches it.
    ///
    /// `None` for a gap with no change below it that still ends before the file
    /// does, such as a gap above a row that the cursor holds open.
    fn gap_scope(&self, gap: &Range<u32>, line_count: u32) -> Option<FoldScope> {
        if gap.end >= line_count {
            return Some(FoldScope {
                segments: Vec::new(),
                split: None,
                end_of_file: true,
                mark: ScopeMark::None,
            });
        }
        let (rows, hunk) = self
            .changes
            .iter()
            .find(|(rows, _)| rows.start >= gap.end)?;
        Some(self.change_scope(rows, hunk))
    }

    /// The Scope of a change over buffer `rows` of `hunk`.
    ///
    /// The chain is the one at the first character of the first row. A last row
    /// in another innermost item keeps its own chain as the split. When one
    /// chain holds the other, the change lies inside the shorter chain's
    /// innermost item, so the Scope is the shorter chain.
    ///
    /// A deletion reads its chain from the base text, and takes the removed
    /// mark when it removes the innermost item whole. An added hunk that adds
    /// an item of its chain whole takes the added mark, and its chain ends at
    /// the outermost such item. A modified hunk whose first row holds the
    /// innermost item's name takes the renamed mark when the base names that
    /// item otherwise.
    fn change_scope(&self, rows: &Range<u32>, hunk: &DiffHunk) -> FoldScope {
        let scope = |segments, split, mark| FoldScope {
            segments,
            split,
            end_of_file: false,
            mark,
        };
        if hunk.buffer_line_range.is_empty()
            && let Some(segments) = self.base_chain(hunk.base_byte_range.start)
        {
            let whole = segments
                .last()
                .is_some_and(|item| covers(&hunk.base_byte_range, &item.item_range));
            let mark = match whole {
                true => ScopeMark::Removed,
                false => ScopeMark::None,
            };
            return scope(segments, None, mark);
        }

        let (first, last) = (rows.start, rows.end - 1);
        let mut segments = self.live_chain(first);
        if hunk.status == DiffHunkStatus::Added {
            let bytes = row_bytes(self.rope, first..last + 1);
            if let Some(whole) = segments
                .iter()
                .position(|item| covers(&bytes, &item.item_range))
            {
                segments.truncate(whole + 1);
                return scope(segments, None, ScopeMark::Added);
            }
        }

        let split = match last == first {
            true => None,
            false => {
                let tail = self.live_chain(last);
                let common = segments
                    .iter()
                    .zip(&tail)
                    .take_while(|(head, tail)| head.item_range == tail.item_range)
                    .count();
                match common == segments.len().min(tail.len()) {
                    true => {
                        segments.truncate(common);
                        None
                    },
                    false => Some(tail),
                }
            },
        };
        let mark = match hunk.status == DiffHunkStatus::Modified
            && first == hunk.buffer_line_range.start
        {
            true => self.renamed(&segments, first, hunk),
            false => ScopeMark::None,
        };
        scope(segments, split, mark)
    }

    /// The renamed mark of `hunk`, a modified hunk from buffer row `first`,
    /// when that row holds the name of the innermost item of `segments` and
    /// the base chain at the hunk's base start ends on an item of the same
    /// keyword with another name.
    fn renamed(&self, segments: &[ScopeSegment], first: u32, hunk: &DiffHunk) -> ScopeMark {
        let Some(item) = segments
            .last()
            .filter(|item| self.rope.offset_to_point(item.name_range.start).row == first)
        else {
            return ScopeMark::None;
        };
        let base = self.base_chain(hunk.base_byte_range.start);
        match base.as_ref().and_then(|base| base.last()) {
            Some(old) if old.keyword == item.keyword && old.name != item.name => {
                ScopeMark::Renamed(old.name.clone())
            },
            _ => ScopeMark::None,
        }
    }

    /// The chain at the first character of buffer row `row`.
    fn live_chain(&self, row: u32) -> Vec<ScopeSegment> {
        scope_chain(
            self.query,
            self.parse.syntax.tree.root_node(),
            self.rope,
            first_text_byte(self.rope, row),
        )
    }

    /// The chain at the first character of the base line that holds base byte
    /// `byte`, or `None` when the diff has no base text or the base does not
    /// parse.
    ///
    /// The first call parses the base text through the memo the workspace's
    /// diffs share, and later calls read that parse.
    fn base_chain(&self, byte: usize) -> Option<Vec<ScopeSegment>> {
        let (tree, rope) = self
            .base
            .get_or_init(|| {
                let text = self.map.base_text()?;
                let tree = parse_memoized(&self.parse.language, text, Some(&self.parse.cache))?;
                Some((tree, Rope::from(text.as_str())))
            })
            .as_ref()?;
        let row = rope.offset_to_point(byte.min(rope.len())).row;
        Some(scope_chain(
            self.query,
            tree.root_node(),
            rope,
            first_text_byte(rope, row),
        ))
    }
}

impl DiffChangesState {
    /// Scrolls the least distance that keeps stack row `row` on screen in a
    /// viewport of `rows` rows, clear of the rows that pin at its top.
    ///
    /// A zero-row viewport, as before the first paint, leaves the scroll as it
    /// is.
    pub(crate) fn ensure_row_visible(
        &mut self,
        editors: &mut SlotMap<EditorId, EditorState>,
        row: usize,
        rows: usize,
    ) {
        if rows == 0 {
            return;
        }
        if row >= self.scroll + rows {
            self.scroll = row + 1 - rows;
        }
        self.uncover_row(editors, row);
    }

    /// Scrolls up the least distance that shows stack row `row` clear of the
    /// rows that pin at the top of the view, as [`pinned_header`] names them.
    ///
    /// A row above the view comes into view this way too. A row clear of the
    /// pinned rows leaves the scroll as it is.
    pub(crate) fn uncover_row(&mut self, editors: &mut SlotMap<EditorId, EditorState>, row: usize) {
        if row < self.scroll + pinned_header(self, editors, self.scroll).rows() {
            self.scroll = clear_scroll(self, editors, row, 0);
        }
    }

    /// The excerpt whose editor has focus, or `None` on an empty stack.
    pub(crate) fn focused_excerpt(&self) -> Option<&Excerpt> {
        self.excerpts.get(self.focused)
    }

    /// The lines the stack's hunks add and remove, as `(added, removed)`.
    ///
    /// Each count is the sum of [`ExcerptStats`] over the excerpts, so the
    /// stack counts what its file headers count.
    pub(crate) fn line_counts(&self, buffers: &BufferRegistry) -> (u32, u32) {
        self.excerpts
            .iter()
            .map(|excerpt| excerpt_stats(excerpt, buffers))
            .fold((0, 0), |(added, removed), stats| {
                (added + stats.added, removed + stats.removed)
            })
    }

    /// The Scope of the row under the focused excerpt's cursor, which the
    /// status row names.
    ///
    /// The chain is the one at the row's first character, so every column of
    /// a row names the same items. While the parse lags an edit, the Scope read
    /// before the edit holds. The status row thus keeps the name between a
    /// keystroke and the parse after it.
    ///
    /// `None` for a language with no outline query, and while the buffer has
    /// no parse.
    pub(crate) fn cursor_scope(
        &mut self,
        editors: &mut SlotMap<EditorId, EditorState>,
        buffers: &BufferRegistry,
    ) -> Option<Arc<FoldScope>> {
        let excerpt = self.focused_excerpt()?;
        let (editor_id, buffer_id) = (excerpt.editor_id, excerpt.buffer_id);
        let editor = editors.get_mut(editor_id)?;
        let row = cursor_point(editor).row;
        let language = buffers.language_for(buffer_id)?;
        let query = language.outline_query()?;
        let syntax = buffers.syntax(buffer_id)?;

        let held = self
            .scope_cache
            .as_ref()
            .filter(|held| held.editor == editor_id);
        let snapshot = editor.display_map.snapshot();
        let buffer = snapshot.buffer_snapshot();
        // A parse of older text puts its items at the wrong bytes.
        if syntax.version != buffer.version() {
            return held.map(|held| held.scope.clone());
        }
        if let Some(held) = held.filter(|held| held.row == row && held.syntax == syntax.version) {
            return Some(held.scope.clone());
        }

        let rope = buffer.rope();
        let scope = Arc::new(FoldScope {
            segments: scope_chain(
                query,
                syntax.tree.root_node(),
                rope,
                first_text_byte(rope, row),
            ),
            split: None,
            end_of_file: false,
            mark: ScopeMark::None,
        });
        self.scope_cache = Some(CursorScope {
            editor: editor_id,
            row,
            syntax: syntax.version,
            scope: scope.clone(),
        });
        Some(scope)
    }
}

impl Default for DiffChangesState {
    /// An empty stack at its top, that covers no view and waits on no landing.
    fn default() -> Self {
        Self {
            excerpts: Vec::new(),
            focused: 0,
            scroll: 0,
            rows: 0,
            synced: None,
            listed_focus: None,
            viewport_rows: 0,
            minimap_rect: None,
            content_rect: None,
            covered: None,
            covered_diff: false,
            pending_landing: None,
            held_screen_row: None,
            view_hold: None,
            pool_swap: false,
            followed_cursor: None,
            last_cursor_row: None,
            scope_cache: None,
        }
    }
}

impl StackStamp {
    /// Read the stamp of `ws` as it stands.
    pub(crate) fn read(ws: &Workspace) -> Self {
        Self {
            epoch: ws.diff_changes_epoch,
            maps: ws.diff_maps_epoch(),
            paths: ws.buffers.paths_epoch(),
        }
    }
}

/// Bring every changes view of the active workspace in step with the
/// workspace, and report whether anything moved.
///
/// A moved stamp or a moved focus lists each stale stack's files anew. An
/// excerpt whose file left the list goes, unless an edit there took the file
/// off the list and the focus is still on it. A file with no excerpt gets one.
/// An excerpt whose buffer's diff map moved refolds, and each waiting landing
/// lands once its excerpt is folded. The list reads the repository tally,
/// which the pump drives too, and the buffers a disposed changes view released
/// close here.
/// Answers on the contract every pump in [`Stoat::drive_pumps`] answers.
pub(crate) fn pump_diff_changes_stack(stoat: &mut Stoat) -> bool {
    let released = mem::take(&mut stoat.active_workspace_mut().diff_changes_released);
    for &buffer_id in &released {
        buffer_lifecycle::close_released_buffer(stoat, buffer_id);
    }
    if stoat.active_workspace().diff_changes_views.is_empty() {
        return !released.is_empty();
    }

    let tallied = {
        let executor = stoat.executor.clone();
        let git_host = stoat.git_host.clone();
        let redraw = stoat.redraw_notify.clone();
        stoat
            .active_workspace_mut()
            .pump_tally(&executor, &git_host, &redraw)
    };
    let relisted = relist(stoat);
    let ws = stoat.active_workspace_mut();
    let refolded = refold(ws);
    let landed = land_pending(ws);
    !released.is_empty() || tallied || relisted || refolded || landed
}

/// Lay the excerpts of `state` out in stack rows, and answer the total.
///
/// Each excerpt takes a header row and then its display rows, and
/// [`Excerpt::top`] records its header's row. An excerpt that waits on its
/// diff map takes its header row alone, so the stack never shows a whole
/// unfolded file.
pub(crate) fn layout_stack(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> usize {
    let mut row = 0;
    for excerpt in &mut state.excerpts {
        excerpt.top = row;
        row += header_rows(excerpt) + excerpt_rows(excerpt, editors);
    }
    state.rows = row;
    row
}

/// The stack rows above `excerpt`'s first display row, its header row and the
/// row that [`Excerpt::scope_row`] holds a Scope for.
pub(crate) fn header_rows(excerpt: &Excerpt) -> usize {
    1 + usize::from(excerpt.scope_row.is_some())
}

/// The display rows of `excerpt`'s editor that the stack's window shows, as of
/// the last [`layout_stack`], or `None` when the window shows none of them.
///
/// `rows` is the excerpt's display row count, from [`excerpt_rows`]. The
/// header row above them is no row of the excerpt's editor.
pub(crate) fn shown_excerpt_rows(
    state: &DiffChangesState,
    excerpt: &Excerpt,
    rows: usize,
) -> Option<Range<u32>> {
    let first = excerpt.top + header_rows(excerpt);
    let start = state.scroll.max(first);
    let end = (state.scroll + state.viewport_rows).min(first + rows);
    (start < end).then(|| (start - first) as u32..(end - first) as u32)
}

/// The rows that pin at the top of `state`'s view when the stack scrolls to
/// stack row `scroll`, as of the last [`layout_stack`].
///
/// The first row pins the header of the excerpt that holds row `scroll` when
/// that row lies past the header and before the excerpt's last row. A header
/// row at the top shows in place. The excerpt's last row shows in place too,
/// so the next header shows under it and two headers never stack. A view under
/// [`PINNED_MIN_VIEWPORT`] rows pins nothing.
///
/// Under a pinned header, the second row pins the nearest Scope row above the
/// stack row it covers. A Scope row on the second row itself shows in place.
/// The stack row there shows in place too when the row under it is the next
/// Scope row or the excerpt's end, since no row of the pinned Scope's section
/// then shows under it. A fold row over a top-level hunk names no Scope, so
/// the second row pins nothing while that fold row is the nearest above.
pub(crate) fn pinned_header(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    scroll: usize,
) -> PinnedHeader {
    if state.viewport_rows < PINNED_MIN_VIEWPORT {
        return PinnedHeader::default();
    }
    let Some(index) = state
        .excerpts
        .iter()
        .rposition(|excerpt| excerpt.top <= scroll)
    else {
        return PinnedHeader::default();
    };
    let top = state.excerpts[index].top;
    let end = state
        .excerpts
        .get(index + 1)
        .map_or(state.rows, |next| next.top);
    if top >= scroll || scroll + 1 >= end {
        return PinnedHeader::default();
    }

    let slot = scroll + 1;
    let scope = scope_row_at_or_above(state, editors, index, slot)
        .filter(|(_, scope)| !scope.is_top_level())
        .filter(|&(at, _)| at < slot && slot + 1 < end)
        .filter(|_| {
            !matches!(
                scope_row_at_or_above(state, editors, index, slot + 1),
                Some((at, _)) if at == slot + 1
            )
        })
        .map(|(row, scope)| PinnedScope { scope, row });
    PinnedHeader {
        file: Some(index),
        scope,
    }
}

/// The stack row and the Scope of the nearest Scope row at or above stack row
/// `row` in excerpt `index`, as of the last [`layout_stack`].
///
/// A Scope row is a fold row whose placeholder carries a Scope, or the
/// excerpt's bare Scope row under its header. `None` on the header row and
/// where no Scope row lies at or above `row`.
fn scope_row_at_or_above(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    index: usize,
    row: usize,
) -> Option<(usize, Arc<FoldScope>)> {
    let excerpt = state.excerpts.get(index)?;
    let bare = excerpt
        .scope_row
        .clone()
        .map(|scope| (excerpt.top + 1, scope));
    let rows_top = excerpt.top + header_rows(excerpt);
    if row < rows_top {
        return bare.filter(|&(at, _)| at == row);
    }

    let display_row = (row - rows_top) as u32;
    let editor = editors.get_mut(excerpt.editor_id)?;
    let snapshot = editor.display_map.snapshot();
    let folds = snapshot.fold_snapshot();
    let inlays = folds.inlay_snapshot();
    let point = snapshot
        .row_start_at_or_after(display_row)
        .unwrap_or_else(|| snapshot.buffer_snapshot().rope().max_point());
    let inlay_row = inlays.to_inlay_point(point).row();
    folds
        .folds_in_range(InlayPoint::default()..InlayPoint::new(inlay_row + 1, 0))
        .into_iter()
        .rev()
        .find_map(|fold| {
            let scope = fold.placeholder.scope.clone()?;
            let start = inlays.to_buffer_point(fold.range.start);
            let fold_row = snapshot.buffer_to_display(start).row;
            (fold_row <= display_row).then(|| (rows_top + fold_row as usize, scope))
        })
        .or(bare)
}

/// The greatest scroll that shows stack row `row` at least `margin` rows under
/// the rows that pin at that scroll, or 0 where no scroll does.
///
/// A scroll one row up sometimes pins a row less, so the walk steps up one row
/// at a time.
fn clear_scroll(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
    margin: usize,
) -> usize {
    let mut scroll = row.saturating_sub(margin);
    while scroll > 0 && row < scroll + pinned_header(state, editors, scroll).rows() + margin {
        scroll -= 1;
    }
    scroll
}

/// The stack row that a press on stack row `row` acts on, and whether the
/// press lands on a pinned row.
///
/// A press on a pinned row acts on the row it repeats. The pinned header
/// repeats its excerpt's header, and the pinned Scope repeats its Scope row.
/// Every other row acts on itself.
pub(crate) fn pinned_row(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
) -> (usize, bool) {
    let pinned = pinned_header(state, editors, state.scroll);
    match (row.checked_sub(state.scroll), pinned.file, pinned.scope) {
        (Some(0), Some(index), _) => (state.excerpts[index].top, true),
        (Some(1), _, Some(scope)) => (scope.row, true),
        _ => (row, false),
    }
}

/// The display rows `excerpt` shows, zero while it waits on its diff map.
///
/// The count is current with every edit, fold, and wrap, and reads with no
/// snapshot clone while none moved, so a layout per frame pays the version
/// checks alone.
pub(crate) fn excerpt_rows(
    excerpt: &Excerpt,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> usize {
    if excerpt.folded_version.is_none() {
        return 0;
    }
    editors
        .get_mut(excerpt.editor_id)
        .map_or(0, |editor| editor.display_map.line_count() as usize)
}

/// The size of `excerpt`'s diff, all zero while its buffer has no diff map.
pub(crate) fn excerpt_stats(excerpt: &Excerpt, buffers: &BufferRegistry) -> ExcerptStats {
    let Some(buffer) = buffers.get(excerpt.buffer_id) else {
        return ExcerptStats::default();
    };
    let buffer = buffer.read().expect("buffer poisoned");
    buffer
        .diff_map
        .as_ref()
        .map_or_else(ExcerptStats::default, ExcerptStats::of)
}

/// The stack row of the focused excerpt's cursor, as of the last
/// [`layout_stack`].
///
/// `None` on an empty stack, and while the focused excerpt waits on its diff
/// map.
pub(crate) fn cursor_stack_row(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> Option<usize> {
    let excerpt = state.focused_excerpt()?;
    excerpt.folded_version?;
    let editor = editors.get_mut(excerpt.editor_id)?;
    Some(excerpt.top + header_rows(excerpt) + view::cursor_display_row(editor) as usize)
}

/// The signed `j` or `k` steps from the focused excerpt's cursor to the row
/// before each excerpt's display row 0, one per excerpt in stack order.
///
/// A step lands on each display row that maps to a buffer position, as the
/// changes view's `j` and `k` walk the stack. So a removed base row and an
/// excerpt header take no step, and a fold placeholder takes one. An origin is
/// negative for the focused excerpt and the excerpts above it.
///
/// `None` on an empty stack, and while the focused excerpt waits on its diff
/// map.
pub(crate) fn step_origins(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> Option<Vec<i64>> {
    let focused = state.focused_excerpt()?;
    focused.folded_version?;
    let through_cursor = {
        let editor = editors.get_mut(focused.editor_id)?;
        let cursor_row = view::cursor_display_row(editor);
        editor
            .display_map
            .snapshot()
            .buffer_rows_above(cursor_row + 1)
    };

    // An excerpt that waits on its diff map paints no row, so the walk passes
    // over it.
    let totals: Vec<i64> = state
        .excerpts
        .iter()
        .map(|excerpt| match excerpt.folded_version {
            None => 0,
            Some(_) => editors.get_mut(excerpt.editor_id).map_or(0, |editor| {
                let snapshot = editor.display_map.snapshot();
                i64::from(snapshot.buffer_rows_above(snapshot.line_count()))
            }),
        })
        .collect();

    let mut origins = vec![0; totals.len()];
    origins[state.focused] = -i64::from(through_cursor);
    for index in state.focused + 1..totals.len() {
        origins[index] = origins[index - 1] + totals[index - 1];
    }
    for index in (0..state.focused).rev() {
        origins[index] = origins[index + 1] - totals[index];
    }
    Some(origins)
}

/// Focus the excerpt that stack row `row` falls in and put its cursor at the
/// start of the row, as of the last [`layout_stack`], and report whether the
/// row held one.
///
/// A header row puts the cursor on its excerpt's first row, and a row past the
/// stack on the last row.
pub(crate) fn select_row(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
) -> bool {
    let Some((index, display_row)) = excerpt_at(state, editors, row) else {
        return false;
    };
    let Some(editor) = editors.get_mut(state.excerpts[index].editor_id) else {
        return false;
    };
    let Some(point) = editor
        .display_map
        .snapshot()
        .row_start_at_or_after(display_row)
    else {
        return false;
    };
    place_cursor(editor, point);
    state.focused = index;
    true
}

/// Focus the excerpt that stack row `row` falls in and put its cursor on the
/// cell at screen column `column` of the row, as a press in an editor does,
/// and report whether the row held one.
///
/// The column maps against the excerpt's text column as of its last paint, so
/// a press left of the text lands at the start of the row. A header row or a
/// Scope row puts the cursor at the start of its excerpt's first row, and a
/// row that shows no buffer line, as a base row spliced in for a removal, at
/// the start of the first row below it that does.
pub(crate) fn press_cell(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
    column: u16,
) -> bool {
    let Some((index, display_row)) = excerpt_at(state, editors, row) else {
        return false;
    };
    let excerpt = &state.excerpts[index];
    let header = row < excerpt.top + header_rows(excerpt);
    let Some(editor) = editors.get_mut(state.excerpts[index].editor_id) else {
        return false;
    };

    let snapshot = editor.display_map.snapshot();
    let cell = editor
        .text_rect
        .filter(|_| !header)
        .and_then(|text| {
            let column = column.saturating_sub(text.x);
            render::editor::display_cell_to_offset(&snapshot, display_row, 0, column, 0)
        })
        .map(|offset| snapshot.buffer_snapshot().rope().offset_to_point(offset));
    let Some(point) = cell.or_else(|| snapshot.row_start_at_or_after(display_row)) else {
        return false;
    };
    place_cursor(editor, point);
    state.focused = index;
    true
}

/// The fold that a press on screen column `column` of stack row `row` opens
/// through a Fold Button.
///
/// The answer holds the excerpt's index, the buffer rows the fold hides, and
/// the edge the button opens the fold from.
///
/// The buttons' cells count from the stack's left edge as of the last paint,
/// as [`fold_button_cells`] lays them. A header row, a row past the stack, a
/// row that is no fold row, and a cell past the buttons answer `None`.
pub(crate) fn fold_button_at(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
    column: u16,
) -> Option<(usize, Range<u32>, FoldEdge)> {
    let cell = column.checked_sub(state.content_rect?.x)?;
    let (index, display_row) = excerpt_at(state, editors, row)?;
    let excerpt = &state.excerpts[index];
    if excerpt.top + header_rows(excerpt) + display_row as usize != row {
        return None;
    }

    // The press reads the row as the paint does, so a spliced base row above
    // a fold row opens nothing.
    let snapshot = editors.get_mut(excerpt.editor_id)?.display_map.snapshot();
    let BlockRowKind::BufferRow { buffer_row } = snapshot.classify_row(display_row) else {
        return None;
    };
    if snapshot.is_wrap_continuation(display_row) {
        return None;
    }
    let gap = fold_gap(&snapshot, buffer_row)?;

    let rope = snapshot.buffer_snapshot().rope();
    let buttons = fold_buttons(&gap, fold_lines(rope, &gap), rope.max_point().row + 1);
    let (_, &(_, edge)) = buttons
        .glyphs()
        .iter()
        .enumerate()
        .find(|(slot, _)| fold_button_cells(*slot).contains(&cell))?;
    Some((index, gap, edge))
}

/// The focused excerpt's editor and its cursor's buffer position.
///
/// `None` on an empty stack.
pub(crate) fn focused_cursor(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> Option<(EditorId, Point)> {
    let editor_id = state.focused_excerpt()?.editor_id;
    let editor = editors.get_mut(editor_id)?;
    Some((editor_id, cursor_point(editor)))
}

/// The excerpt that stack row `row` falls in, as an index, and the display row
/// of that excerpt the row shows, as of the last [`layout_stack`].
///
/// A header row answers its excerpt's first display row, and a row past the
/// stack answers the last row. `None` on a stack with no row to show.
fn excerpt_at(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
) -> Option<(usize, u32)> {
    let index = state
        .excerpts
        .iter()
        .rposition(|excerpt| excerpt.top <= row)?;
    let excerpt = &state.excerpts[index];
    let rows = excerpt_rows(excerpt, editors);
    if rows == 0 {
        return None;
    }
    let display_row = row
        .saturating_sub(excerpt.top + header_rows(excerpt))
        .min(rows - 1);
    Some((index, display_row as u32))
}

/// Seat each changes view of `ws` that a paint has sized and that no reload
/// holds yet on the text it shows, before a reload edits a buffer under it.
///
/// A stack with its cursor on screen holds the cursor, which then keeps its
/// screen row through the reload's landings. Any other stack holds the first
/// row under its pinned rows that shows buffer text. A stack that a reload
/// already holds keeps its seat, since a reload of every open buffer edits one
/// buffer after another with no paint to correct the rows between them.
pub(crate) fn seat_stacks(ws: &mut Workspace) {
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    for (_, state) in diff_changes_views.iter_mut() {
        if state.view_hold.is_none() && state.viewport_rows > 0 {
            state.view_hold = stack_seat(state, editors);
        }
    }
}

/// The seat a reload holds `state` on, or `None` when no row of its view
/// shows buffer text.
///
/// A header row and a Scope row show no buffer text, so the walk passes them.
fn stack_seat(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
) -> Option<StackHold> {
    layout_stack(state, editors);
    let view = state.scroll..state.scroll + state.viewport_rows;
    let hold = |seat, row: usize| StackHold {
        seat,
        screen_row: row - state.scroll,
        rested: None,
    };
    if let Some(row) = cursor_stack_row(state, editors).filter(|row| view.contains(row)) {
        return Some(hold(StackSeat::Cursor, row));
    }

    let start = view.start + pinned_header(state, editors, state.scroll).rows();
    (start..view.end).find_map(|row| {
        let excerpt = state
            .excerpts
            .iter()
            .rev()
            .find(|excerpt| excerpt.top <= row)?;
        let display_row = row.checked_sub(excerpt.top + header_rows(excerpt))?;
        if display_row >= excerpt_rows(excerpt, editors) {
            return None;
        }
        let editor = editors.get_mut(excerpt.editor_id)?;
        let snapshot = editor.display_map.snapshot();
        let point = snapshot.display_to_buffer(DisplayPoint::new(display_row as u32, 0))?;
        let buffer_snapshot = snapshot.buffer_snapshot();
        let offset = buffer_snapshot.rope().point_to_offset(point);
        let anchor = buffer_snapshot.anchor_at(offset, Bias::Right);
        Some(hold(StackSeat::Text(excerpt.editor_id, anchor), row))
    })
}

/// Scroll `state` so the text its reload hold keeps sits on its screen row,
/// as of the last [`layout_stack`] of `total` rows.
///
/// A held screen row or a pending landing drops the hold, since each is a
/// deliberate move. So do a scroll that another writer moved since the last
/// paint and a seat whose text left the stack. The scroll stops at the last
/// page, and the move marks the stack's pool for a swap and records the
/// cursor as followed, so the paint scrolls to it no further.
pub(crate) fn apply_stack_hold(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    total: usize,
) {
    if state.held_screen_row.is_some() || state.pending_landing.is_some() {
        state.view_hold = None;
        return;
    }
    let Some(mut hold) = state.view_hold.take() else {
        return;
    };
    if hold.rested.is_some_and(|scroll| scroll != state.scroll) {
        return;
    }
    let Some(row) = hold_stack_row(state, editors, &hold) else {
        return;
    };

    state.scroll = row
        .saturating_sub(hold.screen_row)
        .min(total.saturating_sub(state.viewport_rows));
    state.pool_swap = true;
    state.followed_cursor = focused_cursor(state, editors);
    hold.rested = Some(state.scroll);
    state.view_hold = Some(hold);
}

/// The stack row of the text `hold` keeps, as of the last [`layout_stack`].
///
/// `None` when the seat's excerpt left the stack or waits on its diff map.
fn hold_stack_row(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    hold: &StackHold,
) -> Option<usize> {
    let StackSeat::Text(editor_id, anchor) = &hold.seat else {
        return cursor_stack_row(state, editors);
    };
    let excerpt = state
        .excerpts
        .iter()
        .find(|excerpt| excerpt.editor_id == *editor_id)?;
    excerpt.folded_version?;
    let editor = editors.get_mut(*editor_id)?;
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let point = buffer_snapshot
        .rope()
        .offset_to_point(buffer_snapshot.resolve_anchor(anchor));
    let display_row = snapshot.buffer_to_display(point).row as usize;
    Some(excerpt.top + header_rows(excerpt) + display_row)
}

/// Scroll the focused changes view so its cursor sits inside the scrolloff
/// band, as an editor follows its cursor, and report whether the scroll moved.
///
/// The band is `scrolloff` rows at each edge, capped to half the rows that the
/// pinned rows leave. The top band starts under the pinned rows, and the
/// scroll stops at the stack's last page.
pub(crate) fn follow_cursor(stoat: &mut Stoat, scrolloff: u32) -> bool {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return false;
    };
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return false;
    };
    let total = layout_stack(state, editors);
    let Some(row) = cursor_stack_row(state, editors) else {
        return false;
    };

    let viewport = state.viewport_rows.max(1);
    let scrolloff = scrolloff as usize;
    let band = pinned_header(state, editors, state.scroll).rows();
    let usable = viewport.saturating_sub(band).max(1);
    let top = scrolloff.min(usable.saturating_sub(1) / 2);
    let bottom = scrolloff.min(usable / 2);
    let before = state.scroll;
    if row < state.scroll + band + top {
        state.scroll = clear_scroll(state, editors, row, top);
    } else if row + bottom >= state.scroll + viewport {
        state.scroll = (row + bottom + 1)
            .saturating_sub(viewport)
            .min(total.saturating_sub(viewport));
    }
    state.scroll != before
}

/// Open the rows around the focused changes view's cursor where a fold hides
/// it, fold them again once the cursor leaves them, and report whether a fold
/// changed.
///
/// A fold change moves the rows above the cursor, so the next paint holds the
/// cursor's line on the screen row it held before the change.
///
/// See also:
/// - [`follow_cursor`], which the key epilogue runs after this.
pub(crate) fn reveal_cursor(stoat: &mut Stoat) -> bool {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return false;
    };
    let cache = ws.diff_tree_cache();
    let Workspace {
        diff_changes_views,
        editors,
        buffers,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return false;
    };
    layout_stack(state, editors);
    let Some((editor_id, buffer_id, revealed)) = state
        .focused_excerpt()
        .filter(|excerpt| excerpt.folded_version.is_some())
        .map(|excerpt| {
            (
                excerpt.editor_id,
                excerpt.buffer_id,
                excerpt.revealed.clone(),
            )
        })
    else {
        return false;
    };
    let screen_row = cursor_stack_row(state, editors)
        .and_then(|row| row.checked_sub(state.scroll))
        .filter(|&row| row < state.viewport_rows)
        .and_then(|row| u16::try_from(row).ok());

    let Some(editor) = editors.get_mut(editor_id) else {
        return false;
    };
    let insert = editor.mode == "insert";
    let row = reveal_row(editor, insert, &revealed);
    let parse = ScopeParse::of(buffers, buffer_id, &cache);
    let Some((_, _, changed)) = fold_excerpt(
        editor,
        row,
        &revealed,
        &mut state.excerpts[state.focused].scope_row,
        parse.as_ref(),
        false,
    ) else {
        return false;
    };
    if changed && screen_row.is_some() {
        state.held_screen_row = screen_row;
    }
    changed
}

/// The row that `editor`'s cursor holds open, or `None` when the folds the
/// diff cuts leave the cursor in sight.
///
/// A fold hides its rows past its first position, and that first position
/// shows as the fold row. So a cursor past a fold's start holds its row open,
/// and so does a cursor on the start while `insert` is set, because an insert
/// there types into the hidden line. A fold of the empty row after a final
/// newline hides no line, so it holds nothing open.
///
/// The test reads the folds the diff's own runs and the `revealed` spans cut,
/// not the folds the display holds. A row this holds open shows no fold, so a
/// test of the display's folds releases the row on the next key and hides the
/// cursor again.
fn reveal_row(
    editor: &mut EditorState,
    insert: bool,
    revealed: &[(Anchor, Anchor)],
) -> Option<u32> {
    let cursor = cursor_point(editor);
    let snapshot = editor.display_map.snapshot();
    let map = snapshot.diff_map()?;
    let buffer = snapshot.buffer_snapshot();
    let rope = buffer.rope();
    let line_count = rope.max_point().row + 1;
    let fold = gaps(&shown_runs(map, buffer, None, revealed), line_count)
        .into_iter()
        .map(|gap| gap_fold(rope, gap))
        .filter(|(_, placeholder)| !placeholder.text.is_empty())
        .map(|(range, _)| range)
        .find(|fold| fold.start <= cursor && cursor < fold.end)?;
    (cursor > fold.start || insert).then_some(cursor.row)
}

/// Open unchanged lines of a fold of the focused changes view's focused
/// excerpt, as `dir` says.
///
/// A fold row under the cursor is the fold every `dir` opens. On any other row
/// `Up` opens the nearest fold above the cursor and `Down` the nearest below
/// it, [`EXPAND_STEP`] lines at a time, and `All` opens the nearest fold each
/// way whole. An expand stays inside the cursor's file, and the opened lines
/// stay open through every refold until [`collapse_at_cursor`] folds them.
///
/// Fails with the status row's text when no fold lies that way.
pub(crate) fn expand_from_cursor(stoat: &mut Stoat, dir: ExpandDir) -> Result<(), &'static str> {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return Err("no fold to open");
    };
    let cache = ws.diff_tree_cache();
    let Workspace {
        diff_changes_views,
        editors,
        buffers,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return Err("no fold to open");
    };
    let Some(editor) = state
        .focused_excerpt()
        .filter(|excerpt| excerpt.folded_version.is_some())
        .and_then(|excerpt| editors.get_mut(excerpt.editor_id))
    else {
        return Err("no fold to open");
    };

    let targets: Vec<(Range<u32>, FoldEdge)> = {
        let cursor = cursor_point(editor).row;
        let snapshot = editor.display_map.snapshot();
        let rope = snapshot.buffer_snapshot().rope();
        let line_count = rope.max_point().row + 1;
        let folds = fold_gaps(&snapshot);
        let edge = |gap: &Range<u32>| fold_edge(gap, fold_lines(rope, gap), line_count, dir);
        let above = folds.iter().rev().find(|gap| gap.end <= cursor);
        let below = folds.iter().find(|gap| gap.start > cursor);
        match (folds.iter().find(|gap| gap.start == cursor), dir) {
            (Some(gap), _) => vec![(gap.clone(), edge(gap))],
            (None, ExpandDir::Up) => {
                let gap = above.ok_or("no fold above")?;
                vec![(gap.clone(), edge(gap))]
            },
            (None, ExpandDir::Down) => {
                let gap = below.ok_or("no fold below")?;
                vec![(gap.clone(), edge(gap))]
            },
            (None, ExpandDir::All) => above
                .into_iter()
                .chain(below)
                .map(|gap| (gap.clone(), FoldEdge::Whole))
                .collect(),
        }
    };
    if targets.is_empty() {
        return Err("no fold to open");
    }

    let index = state.focused;
    let parse = ScopeParse::of(buffers, state.excerpts[index].buffer_id, &cache);
    for (gap, edge) in targets {
        expand(state, editors, parse.as_ref(), index, gap, edge);
    }
    Ok(())
}

/// Fold again the rows that expands opened around the focused changes view's
/// cursor, and report whether a span went.
///
/// On a shown row it drops every opened span that meets the run of shown rows
/// that holds the cursor. On a fold row it drops the spans that end where the
/// fold starts or start where the fold ends. A cursor that the new folds hide
/// moves to the fold row that hides it.
pub(crate) fn collapse_at_cursor(stoat: &mut Stoat) -> bool {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return false;
    };
    let cache = ws.diff_tree_cache();
    let Workspace {
        diff_changes_views,
        editors,
        buffers,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return false;
    };
    let Some(excerpt) = state
        .excerpts
        .get_mut(state.focused)
        .filter(|excerpt| excerpt.folded_version.is_some())
    else {
        return false;
    };
    let Some(editor) = editors.get_mut(excerpt.editor_id) else {
        return false;
    };

    let cursor = cursor_point(editor).row;
    let dropped: Vec<bool> = {
        let snapshot = editor.display_map.snapshot();
        let Some(map) = snapshot.diff_map() else {
            return false;
        };
        let buffer = snapshot.buffer_snapshot();
        let spans = revealed_rows(buffer, &excerpt.revealed);
        match fold_gaps(&snapshot)
            .into_iter()
            .find(|gap| gap.start == cursor)
        {
            Some(gap) => spans
                .iter()
                .map(|rows| rows.end == gap.start || rows.start == gap.end)
                .collect(),
            None => {
                let shown = shown_runs(map, buffer, None, &excerpt.revealed);
                let run = shown.into_iter().find(|run| run.contains(&cursor));
                spans
                    .iter()
                    .map(|rows| {
                        run.as_ref()
                            .is_some_and(|run| rows.start < run.end && run.start < rows.end)
                    })
                    .collect()
            },
        }
    };
    if !dropped.contains(&true) {
        return false;
    }

    let mut dropped = dropped.into_iter();
    excerpt
        .revealed
        .retain(|_| !dropped.next().unwrap_or(false));
    let parse = ScopeParse::of(buffers, excerpt.buffer_id, &cache);
    fold_excerpt(
        editor,
        None,
        &excerpt.revealed,
        &mut excerpt.scope_row,
        parse.as_ref(),
        false,
    );
    let hidden_by = fold_gap(&editor.display_map.snapshot(), cursor);
    if let Some(gap) = hidden_by {
        place_cursor(editor, Point::new(gap.start, 0));
    }
    true
}

/// Center the focused changes view on a change walk's landed `span`, as an
/// editor centers a jump, and report whether the scroll moved.
///
/// `span` is the landed chunk's display rows in the focused excerpt, with an
/// exclusive end. The stack takes the band and the bias of
/// [`view::centered_scroll`], so the key epilogue's [`follow_cursor`] and the
/// paint's follow find the landing inside the band and leave it. A span taller
/// than the view centers the cursor's row alone, as
/// [`view::center_jump_on_span`] does.
///
/// Before the first paint the stack has no height to center in, and while the
/// focused excerpt waits on its diff map it has no rows. Either answers
/// `false`, and the paint shows the row.
pub(crate) fn center_jump(
    stoat: &mut Stoat,
    span: Range<u32>,
    down: bool,
    center_off: u32,
) -> bool {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return false;
    };
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return false;
    };
    let total = layout_stack(state, editors);
    let viewport = state.viewport_rows;
    let top = match state.focused_excerpt() {
        Some(excerpt) if viewport > 0 && excerpt.folded_version.is_some() => {
            excerpt.top + header_rows(excerpt)
        },
        _ => return false,
    };
    let rows = if span.len() > viewport {
        let Some(row) = cursor_stack_row(state, editors) else {
            return false;
        };
        row..row + 1
    } else {
        top + span.start as usize..top + span.end as usize
    };

    let Some(scroll) = view::centered_scroll(
        rows.start as u32..rows.end as u32,
        viewport as u32,
        state.scroll as u32,
        total.saturating_sub(viewport) as u32,
        down,
        center_off,
    ) else {
        return false;
    };
    let before = state.scroll;
    state.scroll = scroll as usize;
    state.scroll != before
}

/// Remove `excerpts`' editors, and close each buffer the stack opened when
/// nothing else needs it.
pub(crate) fn release_excerpts(stoat: &mut Stoat, excerpts: Vec<Excerpt>) {
    for excerpt in excerpts {
        drop_excerpt(stoat, excerpt);
    }
}

/// Remove `excerpts`' editors from `ws`, and queue each buffer the stack
/// opened for [`pump_diff_changes_stack`] to close.
///
/// For a caller that holds the workspace alone, since a buffer's close reaches
/// the language servers.
pub(crate) fn dispose_excerpts(ws: &mut Workspace, excerpts: Vec<Excerpt>) {
    for excerpt in excerpts {
        ws.editors.remove(excerpt.editor_id);
        if excerpt.owns_buffer {
            ws.diff_changes_released.push(excerpt.buffer_id);
        }
    }
}

/// Put `editor`'s one cursor on `point`, held inside the buffer.
pub(crate) fn place_cursor(editor: &mut EditorState, point: Point) {
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let rope = buffer.rope();
    let row = point.row.min(rope.max_point().row);
    let column = point.column.min(rope.line_len(row));
    let offset = rope.point_to_offset(Point::new(row, column));
    editor.selections.set_block_cursor(offset, buffer);
}

/// The buffer position of `editor`'s cursor.
pub(crate) fn cursor_point(editor: &mut EditorState) -> Point {
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let selection = editor.selections.newest_anchor();
    let offset = stoat_text::cursor_offset(
        buffer.rope(),
        buffer.resolve_anchor(&selection.tail()),
        buffer.resolve_anchor(&selection.head()),
    );
    buffer.rope().offset_to_point(offset)
}

/// The buffer row each hunk of `editor`'s diff map starts on, in buffer order.
pub(crate) fn hunk_starts(editor: &mut EditorState) -> Vec<u32> {
    let snapshot = editor.display_map.snapshot();
    snapshot.diff_map().map_or_else(Vec::new, |map| {
        map.hunks().map(|hunk| hunk.buffer_start_line).collect()
    })
}

/// The rows `editor`'s folds leave showing, from its buffer's diff map and the
/// spans its excerpt's `revealed` opens.
pub(crate) fn kept_rows(
    editor: &mut EditorState,
    revealed: &[(Anchor, Anchor)],
) -> Vec<Range<u32>> {
    let snapshot = editor.display_map.snapshot();
    snapshot.diff_map().map_or_else(Vec::new, |map| {
        shown_runs(map, snapshot.buffer_snapshot(), None, revealed)
    })
}

/// The row a landing on `row` takes in an excerpt that shows `kept`.
///
/// A row the excerpt shows stays. A row a fold hides moves to the first row of
/// the next shown run below it, and below the last run to the first row of
/// the first run.
pub(crate) fn landing_row(kept: &[Range<u32>], row: u32) -> u32 {
    if kept.iter().any(|run| run.contains(&row)) {
        return row;
    }
    kept.iter()
        .find(|run| run.start > row)
        .or(kept.first())
        .map_or(row, |run| run.start)
}

/// List the changed files anew for every changes view whose list is stale, and
/// report whether any was.
///
/// A list is stale when the workspace stamp moved since it, or when the focus
/// moved to another excerpt since it. The second case drops an excerpt that
/// only the focus kept, once the focus leaves it.
fn relist(stoat: &mut Stoat) -> bool {
    let stale: Vec<DiffChangesId> = {
        let ws = stoat.active_workspace();
        let stamp = StackStamp::read(ws);
        ws.diff_changes_views
            .iter()
            .filter(|(_, state)| {
                let focus = state.focused_excerpt().map(|excerpt| &excerpt.rel_path);
                state.synced != Some(stamp) || focus != state.listed_focus.as_ref()
            })
            .map(|(id, _)| id)
            .collect()
    };
    if stale.is_empty() {
        return false;
    }

    stoat.sync_repo_root();
    let listed = listed_files(stoat);
    for &id in &stale {
        sync_excerpts(stoat, id, &listed);
    }

    // Read after the sync, since an excerpt that opens a buffer moves the
    // stamp.
    let ws = stoat.active_workspace_mut();
    let stamp = StackStamp::read(ws);
    for id in stale {
        if let Some(state) = ws.diff_changes_views.get_mut(id) {
            state.synced = Some(stamp);
        }
    }
    true
}

/// The files a diff over the working tree shows, keyed by their path relative
/// to the working directory, so the keys run in path order.
///
/// The repository's tally names the files changed against the base: the files
/// with an unstaged hunk under the index base, and the files that differ from
/// HEAD under any other base. Under a revision base the files changed since
/// the revision take the tally's place.
///
/// An untracked file that holds content joins, since the tally counts it as
/// one unstaged hunk. Under a revision base it joins too, since the files
/// changed since the revision include it. Its excerpt shows the whole file as
/// one added hunk, which the staging keys add to the index.
///
/// An open buffer under the working directory whose diff map holds a hunk
/// joins the list, so an unsaved edit shows. A buffer with no unsaved edit
/// joins only while its map is current for its content and the git state. A
/// file that a commit cleaned then leaves the list when the tally lands. Under
/// the index base only an unstaged hunk counts, as the tally counts.
///
/// Outside a repository the list is empty. The working directory is the
/// workspace's resolved [`Workspace::repo_root`], so a list opens no
/// repository.
pub(crate) fn listed_files(stoat: &Stoat) -> BTreeMap<String, PathBuf> {
    let ws = stoat.active_workspace();
    let Some(workdir) = ws.repo_root.clone() else {
        return BTreeMap::new();
    };
    let index_base = ws.diff_base().is_none();
    let listed: Vec<PathBuf> = match ws.diff_base() {
        Some(DiffBase::Rev { sha: Some(_), .. }) => ws
            .rev_changed_files()
            .map(<[PathBuf]>::to_vec)
            .unwrap_or_default(),
        _ => {
            let totals = match index_base {
                true => ws.repo_unstaged_hunk_totals(),
                false => ws.repo_hunk_totals(),
            };
            totals
                .unwrap_or_default()
                .iter()
                .filter(|(_, count)| *count > 0)
                .map(|(path, _)| workdir.join(path))
                .collect()
        },
    };
    let open = ws.buffers.open_paths().into_iter().filter(|path| {
        if !path.starts_with(&workdir) {
            return false;
        }
        let Some(id) = ws.buffers.id_for_path(path) else {
            return false;
        };
        // A clean buffer's content is the disk's, which the tally reads. Its map
        // adds a file only while the map is current. An unsaved edit shows in
        // the map alone.
        let current = ws.diff_map_current(id) || ws.hunks_installed(id);
        ws.buffers.get(id).is_some_and(|buffer| {
            let buffer = buffer.read().expect("buffer poisoned");
            (buffer.dirty || current)
                && buffer
                    .diff_map
                    .as_ref()
                    .is_some_and(|map| map.hunks().any(|hunk| !index_base || !hunk.staged()))
        })
    });

    listed
        .into_iter()
        .chain(open)
        .filter_map(|path| {
            let rel_path = path.strip_prefix(&workdir).ok()?.display().to_string();
            Some((rel_path, path))
        })
        .collect()
}

/// Bring the excerpts of changes view `id` in step with `listed`.
///
/// An excerpt whose file left the list goes, as [`drop_excerpt`] drops it,
/// unless it holds the focus and its buffer took an edit since it opened, as
/// [`Excerpt::opened_version`] tells. A listed file with no excerpt gets one.
/// The excerpts sort by path, the focus stays on its file, and
/// [`DiffChangesState::listed_focus`] records that file.
fn sync_excerpts(stoat: &mut Stoat, id: DiffChangesId, listed: &BTreeMap<String, PathBuf>) {
    let Workspace {
        diff_changes_views,
        buffers,
        ..
    } = stoat.active_workspace_mut();
    let Some(state) = diff_changes_views.get_mut(id) else {
        return;
    };
    let focused_path = state
        .focused_excerpt()
        .map(|excerpt| excerpt.rel_path.clone());
    let (kept, gone): (Vec<Excerpt>, Vec<Excerpt>) = mem::take(&mut state.excerpts)
        .into_iter()
        .partition(|excerpt| {
            listed.contains_key(&excerpt.rel_path)
                || (focused_path.as_ref() == Some(&excerpt.rel_path)
                    && edited_since_open(excerpt, buffers))
        });
    let held_before = kept.len() + gone.len();
    let kept_count = kept.len();
    let gone_paths: Vec<String> = gone
        .iter()
        .map(|excerpt| excerpt.rel_path.clone())
        .collect();
    state.excerpts = kept;
    release_excerpts(stoat, gone);

    let missing: Vec<(&String, &PathBuf)> = {
        let held = stoat
            .active_workspace()
            .diff_changes_views
            .get(id)
            .map(|state| &state.excerpts[..])
            .unwrap_or_default();
        listed
            .iter()
            .filter(|(rel_path, _)| !held.iter().any(|excerpt| &excerpt.rel_path == *rel_path))
            .collect()
    };
    let mut added_paths = Vec::new();
    for (rel_path, path) in missing {
        let Some(excerpt) = open_excerpt(stoat, id, rel_path.clone(), path) else {
            continue;
        };
        match stoat.active_workspace_mut().diff_changes_views.get_mut(id) {
            Some(state) => {
                state.excerpts.push(excerpt);
                added_paths.push(rel_path);
            },
            None => drop_excerpt(stoat, excerpt),
        }
    }

    let Some(state) = stoat.active_workspace_mut().diff_changes_views.get_mut(id) else {
        return;
    };
    state.excerpts.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    state.focused = focused_path
        .as_deref()
        .and_then(|path| {
            state
                .excerpts
                .iter()
                .position(|excerpt| excerpt.rel_path == path)
        })
        .unwrap_or(state.focused)
        .min(state.excerpts.len().saturating_sub(1));
    state.listed_focus = state
        .focused_excerpt()
        .map(|excerpt| excerpt.rel_path.clone());

    tracing::debug!(
        target: "stoat::diff_changes",
        kept = kept_count,
        gone = ?gone_paths,
        added = ?added_paths,
        focused_before = ?focused_path,
        focused_after = ?state.focused_excerpt().map(|excerpt| &excerpt.rel_path),
        excerpts = state.excerpts.len(),
        "changes view resynced"
    );
    if held_before > 0 && state.excerpts.is_empty() {
        tracing::info!(target: "stoat::diff_changes", "changes view stack is empty");
    }
}

/// Whether `excerpt`'s buffer took an edit since the excerpt opened.
fn edited_since_open(excerpt: &Excerpt, buffers: &BufferRegistry) -> bool {
    buffers.get(excerpt.buffer_id).is_some_and(|buffer| {
        buffer.read().expect("buffer poisoned").version() != excerpt.opened_version
    })
}

/// An excerpt of changes view `id` over the file at `path`, in the diff view,
/// with its diff map wanted.
///
/// `None` when the file reads as no buffer, as an image or an unreadable file
/// does. `None` too for a large file not yet open, which reads on the pool,
/// and [`adopt_excerpt`] adds its excerpt once the read lands.
fn open_excerpt(
    stoat: &mut Stoat,
    id: DiffChangesId,
    rel_path: String,
    path: &Path,
) -> Option<Excerpt> {
    let workspace = stoat.active_workspace;
    let owns_buffer = stoat.active_workspace().buffers.id_for_path(path).is_none();
    if owns_buffer && buffer_lifecycle::defer_excerpt_open(stoat, path, id, &rel_path) {
        return None;
    }
    let (buffer_id, buffer) =
        buffer_lifecycle::open_buffer_unshown(stoat, workspace, path, OpenOrigin::Visited)?;
    let opened_version = buffer.read().expect("buffer poisoned").version();
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let mut editor = ws.seeded_editor(buffer_id, buffer, executor);
    editor.set_diff_view(true);
    let editor_id = ws.editors.insert(editor);
    review::ensure_diff_map(stoat, buffer_id, None);
    Some(Excerpt {
        buffer_id,
        editor_id,
        rel_path,
        folded_version: None,
        folded_syntax: None,
        scope_row: None,
        top: 0,
        owns_buffer,
        opened_version,
        revealed: Vec::new(),
    })
}

/// Add the excerpt of the file at `rel_path` to changes view `id` of the
/// active workspace, in path order.
///
/// `buffer_id` is the buffer a read on the pool registered for the file. The
/// stack owns it, since the read opened it for the excerpt, and the focus
/// stays on its file. A view that is gone, or that already holds an excerpt of
/// the file, takes nothing, and the buffer closes when nothing else needs it.
pub(crate) fn adopt_excerpt(
    stoat: &mut Stoat,
    id: DiffChangesId,
    rel_path: String,
    buffer_id: BufferId,
    buffer: SharedBuffer,
) {
    let held = stoat
        .active_workspace()
        .diff_changes_views
        .get(id)
        .map(|state| state.excerpts.iter().any(|e| e.rel_path == rel_path));
    if held != Some(false) {
        buffer_lifecycle::close_released_buffer(stoat, buffer_id);
        return;
    }

    let opened_version = buffer.read().expect("buffer poisoned").version();
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let mut editor = ws.seeded_editor(buffer_id, buffer, executor);
    editor.set_diff_view(true);
    let editor_id = ws.editors.insert(editor);
    review::ensure_diff_map(stoat, buffer_id, None);

    let Some(state) = stoat.active_workspace_mut().diff_changes_views.get_mut(id) else {
        return;
    };
    let focused_path = state
        .focused_excerpt()
        .map(|excerpt| excerpt.rel_path.clone());
    let at = state
        .excerpts
        .partition_point(|excerpt| excerpt.rel_path < rel_path);
    state.excerpts.insert(
        at,
        Excerpt {
            buffer_id,
            editor_id,
            rel_path,
            folded_version: None,
            folded_syntax: None,
            scope_row: None,
            top: 0,
            owns_buffer: true,
            opened_version,
            revealed: Vec::new(),
        },
    );
    tracing::debug!(
        target: "stoat::diff_changes",
        rel_path = %state.excerpts[at].rel_path,
        "excerpt adopted"
    );
    state.focused = focused_path
        .and_then(|path| {
            state
                .excerpts
                .iter()
                .position(|excerpt| excerpt.rel_path == path)
        })
        .unwrap_or(state.focused);
}

/// Remove `excerpt`'s editor, and close its buffer when the stack opened it
/// and nothing else needs it.
fn drop_excerpt(stoat: &mut Stoat, excerpt: Excerpt) {
    stoat
        .active_workspace_mut()
        .editors
        .remove(excerpt.editor_id);
    if excerpt.owns_buffer {
        buffer_lifecycle::close_released_buffer(stoat, excerpt.buffer_id);
    }
}

/// Cut the folds of every excerpt whose buffer's diff map or parse moved since
/// its folds were cut, and report whether any did.
///
/// An excerpt cut for the first time puts its cursor on a row it shows, by
/// [`landing_row`], unless a landing waits on its file. A later cut keeps the
/// cursor's row open where [`reveal_row`] holds it.
fn refold(ws: &mut Workspace) -> bool {
    let cache = ws.diff_tree_cache();
    let Workspace {
        diff_changes_views,
        editors,
        buffers,
        ..
    } = ws;
    let mut any = false;
    for state in diff_changes_views.values_mut() {
        for excerpt in &mut state.excerpts {
            let Some(version) = map_version(buffers, excerpt.buffer_id) else {
                continue;
            };
            let syntax = buffers.syntax_version(excerpt.buffer_id);
            if excerpt.folded_version == Some(version) && excerpt.folded_syntax == syntax {
                continue;
            }
            let Some(editor) = editors.get_mut(excerpt.editor_id) else {
                continue;
            };
            // A first cut comes before the cursor is placed, so no cursor holds
            // a row open yet.
            let first = excerpt.folded_version.is_none();
            let cursor = match first {
                true => None,
                false => {
                    let insert = editor.mode == "insert";
                    reveal_row(editor, insert, &excerpt.revealed)
                },
            };
            let parse = ScopeParse::of(buffers, excerpt.buffer_id, &cache);
            let Some((folded, kept, _)) = fold_excerpt(
                editor,
                cursor,
                &excerpt.revealed,
                &mut excerpt.scope_row,
                parse.as_ref(),
                true,
            ) else {
                continue;
            };
            excerpt.folded_version = Some(folded);
            excerpt.folded_syntax = syntax;
            any = true;

            let landing_waits = state
                .pending_landing
                .as_ref()
                .is_some_and(|landing| landing.rel_path == excerpt.rel_path);
            if first && !landing_waits {
                let row = cursor_point(editor).row;
                place_cursor(editor, Point::new(landing_row(&kept, row), 0));
            }
        }
    }
    any
}

/// Land the cursor of each changes view of `ws` that waits on a landing, and
/// report whether any landed.
///
/// A landing waits until the excerpt of its file is folded. A landing whose
/// file has no excerpt waits until the list is settled for the file. The list
/// is settled when it is current, the tally is in, and the file's buffer has a
/// current diff map. The landing then lands on the first excerpt's first row.
fn land_pending(ws: &mut Workspace) -> bool {
    let stamp = StackStamp::read(ws);
    let tally_pending = ws.tally_pending();
    let waiting: Vec<(DiffChangesId, bool)> = ws
        .diff_changes_views
        .iter()
        .filter_map(|(id, state)| {
            let buffer_id = state.pending_landing.as_ref()?.buffer_id;
            let map_settled = ws.buffers.get(buffer_id).is_none()
                || ws.diff_map_current(buffer_id)
                || ws.hunks_installed(buffer_id);
            Some((
                id,
                state.synced == Some(stamp) && !tally_pending && map_settled,
            ))
        })
        .collect();

    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let mut landed = false;
    for (id, settled) in waiting {
        if let Some(state) = diff_changes_views.get_mut(id) {
            landed |= land(state, editors, settled);
        }
    }
    landed
}

/// Land `state`'s waiting landing if its excerpt is ready, and report whether
/// it landed.
///
/// The landing focuses the excerpt of its file and puts the cursor on its row
/// and column, or on the row [`landing_row`] picks when a fold hides that row.
/// A file with no excerpt lands, once `settled`, on the first excerpt's first
/// row, and a stack with no excerpt drops the landing.
///
/// A landing on its own row holds its screen row for the next paint. A landing
/// that a fold or a missing excerpt moved shows another line, so the paint
/// scrolls the least distance instead.
fn land(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    settled: bool,
) -> bool {
    let Some(landing) = state.pending_landing.clone() else {
        return false;
    };
    let target = state
        .excerpts
        .iter()
        .position(|excerpt| excerpt.rel_path == landing.rel_path);
    let (index, row, column, screen_row) = match target {
        Some(index) => (index, landing.row, landing.column, landing.screen_row),
        None if !settled => return false,
        None => (0, 0, 0, None),
    };
    let Some(excerpt) = state.excerpts.get(index) else {
        state.pending_landing = None;
        return true;
    };
    if excerpt.folded_version.is_none() {
        return false;
    }
    let Some(editor) = editors.get_mut(excerpt.editor_id) else {
        return false;
    };

    let shown = landing_row(&kept_rows(editor, &excerpt.revealed), row);
    let column = if shown == row { column } else { 0 };
    place_cursor(editor, Point::new(shown, column));
    state.focused = index;
    state.pending_landing = None;
    state.held_screen_row = screen_row.filter(|_| shown == row);
    true
}

/// Fold every run of `editor`'s buffer that [`kept_runs`] and the `revealed`
/// spans leave out, and answer the diff map version the folds were cut from,
/// the rows left showing, and whether a fold changed.
///
/// A fold that already hides a wanted gap stays. Only the gaps that opened or
/// closed since the last cut change the display map, so a landing that moves no
/// gap changes nothing. A `cursor` row stays open with its context, as
/// [`kept_runs`] keeps it, and each revealed span stays open as it is.
///
/// Each fold row takes the Scope of the change below it, read from `parse`
/// while `parse` is a parse of the buffer's text. A held fold keeps its Scope
/// unless `rescope` asks for every Scope again, which a cut after the text,
/// the diff, or the parse moved does. A held fold whose Scope differs from the
/// Scope read again is cut again. With no such parse the held folds keep their
/// Scopes, and a new fold takes none.
///
/// `scope_row` takes the Scope of the first change when the first kept run
/// starts at row 0 and holds that change, so no fold lies above it, and
/// `None` otherwise. It is `None` too for a first change at the top level,
/// which shows no Scope. It keeps and rereads its Scope by the same rules. A
/// `scope_row` that comes or goes counts as a fold change, since it moves every
/// row of the excerpt.
///
/// `None`, with nothing folded, while the buffer has no diff map.
fn fold_excerpt(
    editor: &mut EditorState,
    cursor: Option<u32>,
    revealed: &[(Anchor, Anchor)],
    scope_row: &mut Option<Arc<FoldScope>>,
    parse: Option<&ScopeParse>,
    rescope: bool,
) -> Option<(usize, Vec<Range<u32>>, bool)> {
    let (version, kept, stale, fresh, next_row) = {
        let snapshot = editor.display_map.snapshot();
        let map = snapshot.diff_map()?;
        let buffer = snapshot.buffer_snapshot();
        let rope = buffer.rope();
        let line_count = rope.max_point().row + 1;
        let kept = shown_runs(map, buffer, cursor, revealed);

        // The folds sit where their anchors carried them across every edit
        // since their cut, so a gap whose rows moved still finds its fold.
        let held = held_folds(snapshot.fold_snapshot());
        // A parse of older text puts its items at the wrong bytes.
        let reader = parse
            .filter(|parse| parse.syntax.version == buffer.version())
            .and_then(|parse| ScopeReader::new(parse, map, rope));
        let reread = rescope && reader.is_some();
        let wanted: Vec<(Range<Point>, FoldPlaceholder)> = gaps(&kept, line_count)
            .into_iter()
            .map(|gap| {
                let (range, mut placeholder) = gap_fold(rope, gap.clone());
                if placeholder.is_fold_row() {
                    let kept_scope = held_scope(&held, &range)
                        .filter(|scope| !reread && (scope.is_some() || reader.is_none()));
                    placeholder.scope = match kept_scope {
                        Some(scope) => scope.clone(),
                        None => reader
                            .as_ref()
                            .and_then(|reader| reader.gap_scope(&gap, line_count))
                            .map(Arc::new),
                    };
                }
                (range, placeholder)
            })
            .collect();
        let wanted_keys: Vec<HeldFold> = wanted
            .iter()
            .map(|(range, placeholder)| (range.clone(), placeholder.scope.clone()))
            .collect();

        let stale: Vec<Range<Point>> = held
            .iter()
            .filter(|fold| !holds(&wanted_keys, fold))
            .map(|(range, _)| range.clone())
            .collect();
        let fresh: Vec<(Range<Point>, FoldPlaceholder)> = wanted
            .into_iter()
            .zip(&wanted_keys)
            .filter(|(_, key)| !holds(&held, key))
            .map(|(fold, _)| fold)
            .collect();

        let top_open = first_change_row(map).is_some_and(|row| {
            kept.first()
                .is_some_and(|run| run.start == 0 && row < run.end)
        });
        let next_row = match top_open {
            false => None,
            true => match (&*scope_row, &reader) {
                (Some(scope), _) if !reread => Some(scope.clone()),
                (_, Some(reader)) => reader.gap_scope(&(0..0), line_count).map(Arc::new),
                (held, None) => held.clone(),
            }
            .filter(|scope| !scope.is_top_level()),
        };
        (map.version(), kept, stale, fresh, next_row)
    };
    let row_changed = *scope_row != next_row;
    *scope_row = next_row;
    let changed = !stale.is_empty() || !fresh.is_empty() || row_changed;
    if !stale.is_empty() {
        editor.display_map.unfold(stale);
    }
    if !fresh.is_empty() {
        editor.display_map.fold_with(fresh);
    }
    Some((version, kept, changed))
}

/// The buffer range and the Scope of each fold of `folds`, sorted by start and
/// then end.
fn held_folds(folds: &FoldSnapshot) -> Vec<HeldFold> {
    let inlays = folds.inlay_snapshot();
    let mut held: Vec<HeldFold> = folds
        .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
        .into_iter()
        .map(|fold| {
            (
                inlays.to_buffer_point(fold.range.start)..inlays.to_buffer_point(fold.range.end),
                fold.placeholder.scope.clone(),
            )
        })
        .collect();
    held.sort_unstable_by_key(|(range, _)| (range.start, range.end));
    held
}

/// Whether `folds`, sorted by start and then end, holds `fold`'s range with the
/// same Scope.
fn holds(folds: &[HeldFold], fold: &HeldFold) -> bool {
    held_scope(folds, &fold.0).is_some_and(|scope| *scope == fold.1)
}

/// The Scope of the fold of `folds`, sorted by start and then end, that hides
/// `range`, or `None` when no fold hides it.
fn held_scope<'a>(
    folds: &'a [HeldFold],
    range: &Range<Point>,
) -> Option<&'a Option<Arc<FoldScope>>> {
    let index = folds
        .binary_search_by_key(&(range.start, range.end), |(probe, _)| {
            (probe.start, probe.end)
        })
        .ok()?;
    Some(&folds[index].1)
}

/// The rows an excerpt shows, as runs in buffer order.
///
/// Each hunk's rows widen by [`CONTEXT_ROWS`] on each side, stay inside the
/// buffer's `line_count` rows, and merge with a run they touch. A deletion's
/// rows are empty, so it keeps the row it sits on.
///
/// A refined modified hunk keeps only each run the tree pass marked and its
/// last live row, each widened the same way. Its unmarked rows, such as the
/// body of a block the formatter re-indented, fold away. The last live row
/// stays because, where the layout pairs base rows with live rows, the base
/// lines left over hang below it.
///
/// A `cursor` row keeps its context as a hunk's rows do, so a cursor outside
/// every hunk's run shows with the rows around it.
fn kept_runs(map: &DiffMap, line_count: u32, cursor: Option<u32>) -> Vec<Range<u32>> {
    let mut wanted: Vec<Range<u32>> = Vec::new();
    for hunk in map.hunks() {
        let rows = &hunk.buffer_line_range;
        if rows.is_empty() {
            wanted.push(hunk.buffer_start_line..hunk.buffer_start_line + 1);
        } else if hunk.status == DiffHunkStatus::Modified && hunk.refined() {
            wanted.extend(hunk.marked_rows.iter().cloned());
            wanted.push(rows.end - 1..rows.end);
        } else {
            wanted.push(rows.clone());
        }
    }
    wanted.extend(cursor.map(|row| row..row + 1));
    // The merge below joins a run only to the one before it, so the rows go in
    // in buffer order. A cursor row falls anywhere, even between a refined
    // hunk's marked runs.
    wanted.sort_by_key(|rows| rows.start);

    let mut runs: Vec<Range<u32>> = Vec::new();
    for rows in wanted {
        let run =
            rows.start.saturating_sub(CONTEXT_ROWS)..(rows.end + CONTEXT_ROWS).min(line_count);
        if run.is_empty() {
            continue;
        }
        match runs.last_mut() {
            Some(last) if run.start <= last.end => last.end = last.end.max(run.end),
            _ => runs.push(run),
        }
    }
    runs
}

/// The rows an excerpt shows, as runs in buffer order.
///
/// The runs are the ones [`kept_runs`] keeps for `map` and `cursor` and the
/// rows of the `revealed` spans, merged where they touch.
fn shown_runs(
    map: &DiffMap,
    buffer: &MultiBufferSnapshot,
    cursor: Option<u32>,
    revealed: &[(Anchor, Anchor)],
) -> Vec<Range<u32>> {
    let line_count = buffer.rope().max_point().row + 1;
    let mut runs = kept_runs(map, line_count, cursor);
    runs.extend(
        revealed_rows(buffer, revealed)
            .into_iter()
            .filter(|rows| !rows.is_empty()),
    );
    merge_runs(runs)
}

/// The buffer rows each span of `revealed` covers, in span order.
///
/// A span whose end resolves above its start lost its lines to an edit, so its
/// rows are empty.
fn revealed_rows(buffer: &MultiBufferSnapshot, revealed: &[(Anchor, Anchor)]) -> Vec<Range<u32>> {
    let anchors: Vec<Anchor> = revealed
        .iter()
        .flat_map(|&(start, end)| [start, end])
        .collect();
    buffer
        .points_for_anchors_batch(&anchors)
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[start, end]| match end.row >= start.row {
            true => start.row..end.row + 1,
            false => start.row..start.row,
        })
        .collect()
}

/// `runs` sorted by start, with each run merged into the one before it where
/// the two touch or overlap.
fn merge_runs(mut runs: Vec<Range<u32>>) -> Vec<Range<u32>> {
    runs.sort_by_key(|run| run.start);
    let mut merged: Vec<Range<u32>> = Vec::with_capacity(runs.len());
    for run in runs {
        match merged.last_mut() {
            Some(last) if run.start <= last.end => last.end = last.end.max(run.end),
            _ => merged.push(run),
        }
    }
    merged
}

/// The rows of `0..line_count` that no run of `kept` holds, as runs.
fn gaps(kept: &[Range<u32>], line_count: u32) -> Vec<Range<u32>> {
    let mut gaps = Vec::new();
    let mut start = 0;
    for run in kept {
        if run.start > start {
            gaps.push(start..run.start);
        }
        start = run.end;
    }
    if start < line_count {
        gaps.push(start..line_count);
    }
    gaps
}

/// The fold that hides the rows of `gap`, with the placeholder it shows.
///
/// A gap folds from the start of its first row, so the fold takes a display
/// row of its own that names how many lines it hides. The diff view paints that
/// row as a fold row. The empty row after a final newline holds no line, so the
/// count leaves it out.
///
/// A gap of that empty row alone folds from the end of the row above under an
/// empty placeholder instead, so it takes no row, and the excerpt's last line
/// ends as it reads.
fn gap_fold(rope: &Rope, gap: Range<u32>) -> (Range<Point>, FoldPlaceholder) {
    let end = Point::new(gap.end - 1, rope.line_len(gap.end - 1));
    match fold_lines(rope, &gap) {
        0 => {
            // An empty file's one row is the gap, so the fold starts on it.
            let above = gap.start.saturating_sub(1);
            (
                Point::new(above, rope.line_len(above))..end,
                FoldPlaceholder {
                    text: Arc::from(""),
                    ..FoldPlaceholder::default()
                },
            )
        },
        lines => (
            Point::new(gap.start, 0)..end,
            render::review::fold_row_placeholder(lines),
        ),
    }
}

/// The first row of `map`'s first change, as [`changed_rows`] reads it, or
/// `None` for a map with no hunk.
fn first_change_row(map: &DiffMap) -> Option<u32> {
    let hunk = map.hunks().next()?;
    let range = &hunk.buffer_line_range;
    Some(if range.is_empty() {
        hunk.buffer_start_line
    } else if hunk.status == DiffHunkStatus::Modified && hunk.refined() {
        hunk.marked_rows
            .first()
            .map_or(range.start, |run| run.start)
    } else {
        range.start
    })
}

/// The rows each change of `map` marks, with the change's hunk, in buffer
/// order.
///
/// A deletion marks the row it sits on, and a refined modified hunk marks the
/// runs the tree pass found, as [`kept_runs`] reads them.
fn changed_rows(map: &DiffMap) -> Vec<(Range<u32>, &DiffHunk)> {
    let mut rows: Vec<(Range<u32>, &DiffHunk)> = Vec::new();
    for hunk in map.hunks() {
        let range = &hunk.buffer_line_range;
        if range.is_empty() {
            rows.push((hunk.buffer_start_line..hunk.buffer_start_line + 1, hunk));
        } else if hunk.status == DiffHunkStatus::Modified && hunk.refined() {
            rows.extend(hunk.marked_rows.iter().map(|run| (run.clone(), hunk)));
        } else {
            rows.push((range.clone(), hunk));
        }
    }
    rows.sort_by_key(|(rows, _)| rows.start);
    rows
}

/// The bytes of buffer `rows` of `rope`, from the first row's start to the
/// start of the row after the last, or to the end of the text.
fn row_bytes(rope: &Rope, rows: Range<u32>) -> Range<usize> {
    let end = match rows.end > rope.max_point().row {
        true => rope.len(),
        false => rope.point_to_offset(Point::new(rows.end, 0)),
    };
    rope.point_to_offset(Point::new(rows.start, 0))..end
}

/// Whether `outer` holds every byte of `inner`.
fn covers(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// The byte of the first character on `row` of `rope` that is not whitespace,
/// or the end of the row for a blank row.
fn first_text_byte(rope: &Rope, row: u32) -> usize {
    let row = row.min(rope.max_point().row);
    let start = rope.point_to_offset(Point::new(row, 0));
    let end = rope.point_to_offset(Point::new(row, rope.line_len(row)));
    let indent: usize = rope
        .chunks_in_range(start..end)
        .flat_map(str::chars)
        .take_while(|ch| ch.is_whitespace())
        .map(char::len_utf8)
        .sum();
    start + indent
}

/// The runs of `scope`'s chip that fit `room` sixteenths at text `scale`, or
/// none when not even the innermost item fits.
///
/// Items join with `U+203A`. A chain too wide for the room drops items from its
/// middle for one `U+2026`, the item after the outermost first, and drops the
/// outermost last, so the innermost item always shows. A split names the two
/// innermost items after their common containers, joined with `+`. An empty
/// chain gives no parts, since a hunk at the top level shows no Scope, and a
/// fold with no change below it reads `end of file`.
///
/// A whole added item takes `+` before the chain, and a whole removed item
/// takes `U+2212`. A renamed item reads its old name, `U+2192`, and its new
/// name.
pub(crate) fn scope_parts(scope: &FoldScope, room: u16, scale: u16) -> Vec<ScopePart> {
    if scope.is_top_level() {
        return Vec::new();
    }
    let items = scope_items(scope);
    let mark = mark_part(&scope.mark);
    let last = items.len() - 1;
    let mut shapes: Vec<Vec<Option<usize>>> = vec![(0..=last).map(Some).collect()];
    for cut in 2..=last {
        shapes.push(
            [Some(0), None]
                .into_iter()
                .chain((cut..=last).map(Some))
                .collect(),
        );
    }
    if last > 0 {
        shapes.push(vec![None, Some(last)]);
    }
    shapes
        .iter()
        .map(|shape| {
            mark.iter()
                .cloned()
                .chain(join_items(&items, shape))
                .collect::<Vec<_>>()
        })
        .find(|parts| parts_advance(parts, scale) <= room)
        .unwrap_or_default()
}

/// The run before the chain that marks a whole added or removed item, with the
/// space after it.
fn mark_part(mark: &ScopeMark) -> Option<ScopePart> {
    match mark {
        ScopeMark::Added => Some(ScopePart::new("+ ", ScopeRole::Added)),
        ScopeMark::Removed => Some(ScopePart::new("\u{2212} ", ScopeRole::Removed)),
        ScopeMark::None | ScopeMark::Renamed(_) => None,
    }
}

/// The sixteenths that `parts` advance at text `scale`.
pub(crate) fn parts_advance(parts: &[ScopePart], scale: u16) -> u16 {
    parts.iter().fold(0, |advance: u16, part| {
        advance.saturating_add(text_run::advance_sixteenths(
            part.text.chars().count(),
            scale,
        ))
    })
}

/// The items a Scope chip names, outermost first, each as its runs. A fold
/// with no change below it reads as one label item.
fn scope_items(scope: &FoldScope) -> Vec<Vec<ScopePart>> {
    let label = |text: &str| vec![vec![ScopePart::new(text, ScopeRole::Separator)]];
    if scope.end_of_file {
        return label("end of file");
    }
    let old_name = match &scope.mark {
        ScopeMark::Renamed(old) => Some(old.as_str()),
        _ => None,
    };
    let innermost = scope.segments.len().checked_sub(1);
    let item = |index: usize| {
        segment_parts(
            &scope.segments[index],
            old_name.filter(|_| Some(index) == innermost),
        )
    };
    match &scope.split {
        Some(tail) => {
            let common = scope
                .segments
                .iter()
                .zip(tail)
                .take_while(|(head, tail)| head.item_range == tail.item_range)
                .count();
            let mut items: Vec<Vec<ScopePart>> = (0..common).map(item).collect();
            let mut pair = innermost.map(item).unwrap_or_default();
            pair.push(ScopePart::new(" + ", ScopeRole::Separator));
            pair.extend(
                tail.last()
                    .map(|segment| segment_parts(segment, None))
                    .unwrap_or_default(),
            );
            items.push(pair);
            items
        },
        None => (0..scope.segments.len()).map(item).collect(),
    }
}

/// The runs of one item, its keyword with a space and then its name. An
/// `old_name` comes before the name with `U+2192`, for an item a hunk renamed.
fn segment_parts(segment: &ScopeSegment, old_name: Option<&str>) -> Vec<ScopePart> {
    let mut parts: Vec<ScopePart> = segment
        .keyword
        .iter()
        .map(|keyword| ScopePart::new(format!("{keyword} "), ScopeRole::Keyword))
        .collect();
    if let Some(old_name) = old_name {
        parts.push(ScopePart::new(old_name, ScopeRole::Name));
        parts.push(ScopePart::new(" \u{2192} ", ScopeRole::Separator));
    }
    parts.push(ScopePart::new(segment.name.clone(), ScopeRole::Name));
    parts
}

/// The runs of the items `shape` names, joined with `U+203A`. Each entry of
/// `shape` is an index into `items`, or `None` for the `U+2026` that stands
/// for the items the shape drops.
fn join_items(items: &[Vec<ScopePart>], shape: &[Option<usize>]) -> Vec<ScopePart> {
    let mut parts = Vec::new();
    for (position, slot) in shape.iter().enumerate() {
        if position > 0 {
            parts.push(ScopePart::new(" \u{203a} ", ScopeRole::Separator));
        }
        match slot {
            Some(index) => parts.extend(items[*index].iter().cloned()),
            None => parts.push(ScopePart::new("\u{2026}", ScopeRole::Separator)),
        }
    }
    parts
}

/// The lines `gap` hides. The empty row after a final newline holds no line,
/// so the count leaves it out.
pub(crate) fn fold_lines(rope: &Rope, gap: &Range<u32>) -> u32 {
    let line_count = rope.max_point().row + 1;
    let after_final_newline = gap.end == line_count && rope.line_len(line_count - 1) == 0;
    gap.end - gap.start - u32::from(after_final_newline)
}

/// Open the rows of `gap` that an expand from `edge` reveals in excerpt `index`
/// of `state`, and scroll the opened rows into view.
///
/// The scroll first shows the far edge of the opened rows, the first row of a
/// fold above the cursor or the last row of a fold below it. Then it shows the
/// cursor, so a short view keeps the cursor in sight.
///
/// The opened rows carry a [`RevealFlash`] on the excerpt's editor until its
/// fade ends.
pub(crate) fn expand(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    parse: Option<&ScopeParse>,
    index: usize,
    gap: Range<u32>,
    edge: FoldEdge,
) {
    let Some(excerpt) = state.excerpts.get_mut(index) else {
        return;
    };
    let Some(editor) = editors.get_mut(excerpt.editor_id) else {
        return;
    };
    let cursor = cursor_point(editor).row;
    let rows = {
        let snapshot = editor.display_map.snapshot();
        let buffer = snapshot.buffer_snapshot();
        let rope = buffer.rope();
        let rows = reveal_rows(&gap, fold_lines(rope, &gap), edge);
        let last = rows.end - 1;
        let start = rope.point_to_offset(Point::new(rows.start, 0));
        let end = rope.point_to_offset(Point::new(last, rope.line_len(last)));
        excerpt.revealed.push((
            buffer.anchor_at(start, Bias::Left),
            buffer.anchor_at(end, Bias::Right),
        ));
        rows
    };
    let insert = editor.mode == "insert";
    let held = reveal_row(editor, insert, &excerpt.revealed);
    fold_excerpt(
        editor,
        held,
        &excerpt.revealed,
        &mut excerpt.scope_row,
        parse,
        false,
    );
    editor.reveal_flashes.push(RevealFlash {
        rows: rows.clone(),
        age: 0.0,
    });

    let far = match gap.start >= cursor {
        true => rows.end - 1,
        false => rows.start,
    };
    let far_row = editor
        .display_map
        .snapshot()
        .buffer_to_display(Point::new(far, 0))
        .row as usize;
    layout_stack(state, editors);
    let first = state.excerpts[index].top + header_rows(&state.excerpts[index]);
    state.ensure_row_visible(editors, first + far_row, state.viewport_rows);
    if let Some(row) = cursor_stack_row(state, editors) {
        state.ensure_row_visible(editors, row, state.viewport_rows);
    }
}

/// The edge an expand toward `dir` opens `gap` from, a fold of `lines` lines in
/// a buffer of `line_count` rows.
///
/// A fold of [`EXPAND_STEP`] lines or fewer opens whole, as Expand All opens
/// any fold. The fold above a file's first hunk only grows up and the fold
/// below its last hunk only grows down, since each has a hunk on one side
/// alone.
fn fold_edge(gap: &Range<u32>, lines: u32, line_count: u32, dir: ExpandDir) -> FoldEdge {
    if lines <= EXPAND_STEP || dir == ExpandDir::All {
        return FoldEdge::Whole;
    }
    if gap.start == 0 {
        return FoldEdge::Bottom;
    }
    if gap.end == line_count {
        return FoldEdge::Top;
    }
    match dir {
        ExpandDir::Up => FoldEdge::Bottom,
        ExpandDir::Down | ExpandDir::All => FoldEdge::Top,
    }
}

/// The rows an expand from `edge` opens of `gap`, a fold of `lines` lines.
///
/// A bottom reveal runs to the gap's end, so it takes the empty row after a
/// final newline with it and leaves no gap of that row alone.
fn reveal_rows(gap: &Range<u32>, lines: u32, edge: FoldEdge) -> Range<u32> {
    match edge {
        FoldEdge::Whole => gap.clone(),
        FoldEdge::Top => gap.start..gap.start + EXPAND_STEP,
        FoldEdge::Bottom => gap.start + lines - EXPAND_STEP..gap.end,
    }
}

/// The Fold Buttons of `gap`, a fold of `lines` lines in a buffer of
/// `line_count` rows.
///
/// A fold of [`EXPAND_STEP`] lines or fewer opens whole, and so does a fold
/// with no hunk on either side.
pub(crate) fn fold_buttons(gap: &Range<u32>, lines: u32, line_count: u32) -> FoldButtons {
    let (top, bottom) = (gap.start == 0, gap.end == line_count);
    if lines <= EXPAND_STEP || (top && bottom) {
        return FoldButtons::Whole;
    }
    match (top, bottom) {
        (true, _) => FoldButtons::Up,
        (_, true) => FoldButtons::Down,
        _ => FoldButtons::Both,
    }
}

/// The cells a press on the Fold Button at `slot` lands in, counted from the
/// fold row's first cell.
///
/// A box that ends partway into a cell takes that cell, and the next box's
/// cells start after it. The first button takes cells 0 to 2, and the second
/// takes cells 3 and 4.
pub(crate) fn fold_button_cells(slot: usize) -> Range<u16> {
    let end = |slot: u16| {
        (FOLD_BUTTON_MARGIN + slot * (FOLD_BUTTON_WIDTH + FOLD_BUTTON_GAP) + FOLD_BUTTON_WIDTH)
            .div_ceil(16)
    };
    let slot = slot as u16;
    let start = match slot {
        0 => 0,
        slot => end(slot - 1),
    };
    start..end(slot)
}

/// The buffer rows of every fold row of `snapshot`, in buffer order.
fn fold_gaps(snapshot: &DisplaySnapshot) -> Vec<Range<u32>> {
    let folds = snapshot.fold_snapshot();
    folds
        .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
        .into_iter()
        .filter(|fold| fold.placeholder.is_fold_row())
        .map(|fold| buffer_rows(folds.inlay_snapshot(), &fold.range))
        .collect()
}

/// The buffer rows of the fold row whose fold holds buffer row `row`, or `None`
/// when no fold row holds it.
pub(crate) fn fold_gap(snapshot: &DisplaySnapshot, row: u32) -> Option<Range<u32>> {
    let folds = snapshot.fold_snapshot();
    let inlays = folds.inlay_snapshot();
    let start = inlays.to_inlay_point(Point::new(row, 0));
    folds
        .folds_in_range(start..InlayPoint::new(start.row() + 1, 0))
        .into_iter()
        .find(|fold| fold.placeholder.is_fold_row())
        .map(|fold| buffer_rows(inlays, &fold.range))
}

/// The buffer rows from the first row of `range` to its last.
fn buffer_rows(inlays: &InlaySnapshot, range: &Range<InlayPoint>) -> Range<u32> {
    inlays.to_buffer_point(range.start).row..inlays.to_buffer_point(range.end).row + 1
}

/// The version of buffer `id`'s diff map, or `None` while it has none.
fn map_version(buffers: &BufferRegistry, id: BufferId) -> Option<usize> {
    let buffer = buffers.get(id)?;
    let buffer = buffer.read().expect("buffer poisoned");
    buffer.diff_map.as_ref().map(DiffMap::version)
}

#[cfg(test)]
mod tests {
    use super::{
        cursor_point, cursor_stack_row, excerpt_rows, expand_from_cursor, fold_edge, fold_excerpt,
        follow_cursor, gap_fold, gaps, header_rows, kept_runs, landing_row, layout_stack,
        listed_files, place_cursor, press_cell, pump_diff_changes_stack, reveal_rows, scope_parts,
        select_row, DiffChangesState, ExcerptStats, ExpandDir, FoldEdge, Landing, ScopeParse,
        StackStamp,
    };
    use crate::{
        action_handlers::dispatch,
        auto_reload,
        buffer::{BufferId, TextBuffer},
        buffer_registry::AutoReloadMode,
        diff_map::{DiffHunk, DiffHunkStatus, DiffMap, TokenDetail},
        display_map::{DisplayPoint, FoldScope, InlayPoint, ScopeMark},
        editor_state::{EditorId, EditorState},
        host::FakeRepoBuilder,
        test_harness::TestHarness,
        theme::FULL_CELL_SCALE,
        workspace::{
            diff::{DiffBase, WorktreeBase},
            Workspace,
        },
    };
    use ratatui::buffer::Buffer;
    use slotmap::SlotMap;
    use std::{
        ops::{Range, RangeInclusive},
        path::{Path, PathBuf},
        sync::{Arc, RwLock},
    };
    use stoat_action::{CloseDiffChanges, DiffChanges, DiffChangesPrev};
    use stoat_config::MinimapMode;
    use stoat_language::{
        parse_rope,
        structural_diff::{self, TreeCache},
        LanguageRegistry, ScopeSegment, SyntaxState,
    };
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::{Point, Rope};

    /// The map a structural diff of `base` against `text` gives.
    fn map_of(base: &str, text: &str) -> DiffMap {
        DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        )
    }

    /// Lines `1` to `n`, one per line.
    fn numbered(n: u32) -> String {
        (1..=n).map(|line| format!("{line}\n")).collect()
    }

    /// A `status` hunk over live rows `rows` and base bytes `base`, which the
    /// tree pass narrowed to `marked_rows`.
    fn hunk(
        status: DiffHunkStatus,
        rows: Range<u32>,
        base: Range<usize>,
        marked_rows: Vec<Range<u32>>,
    ) -> DiffHunk {
        DiffHunk {
            status,
            unstaged_lines: Vec::new(),
            marked_rows,
            buffer_start_line: rows.start,
            buffer_line_range: rows,
            base_byte_range: base,
            anchor_range: None,
            token_detail: None,
        }
    }

    /// The counts take the live rows and the base lines each hunk marks. A move
    /// adds at the hunk it went to and removes at the hunk it left, and only a
    /// refined hunk is structural. Each base line `bN` takes three bytes.
    #[test]
    fn excerpt_stats_count_both_sides_of_every_hunk() {
        let base: String = (0..10).map(|line| format!("b{line}\n")).collect();
        let map = DiffMap::from_hunks(
            [
                hunk(DiffHunkStatus::Added, 0..4, 0..0, Vec::new()),
                hunk(
                    DiffHunkStatus::Modified,
                    4..6,
                    3..11,
                    std::iter::once(4..5).collect(),
                ),
                hunk(DiffHunkStatus::Modified, 6..7, 15..17, Vec::new()),
                hunk(DiffHunkStatus::Deleted, 8..8, 18..20, Vec::new()),
                hunk(DiffHunkStatus::Moved, 9..10, 24..26, Vec::new()),
                hunk(DiffHunkStatus::Moved, 12..12, 21..26, Vec::new()),
            ],
            Some(Arc::new(base)),
        );

        assert_eq!(
            ExcerptStats::of(&map),
            ExcerptStats {
                added: 8,
                removed: 7,
                hunks: 6,
                structural: 1
            },
        );
    }

    /// A hunk keeps three rows each side, a run held at the buffer's ends, and
    /// two runs that touch merge into one. A deletion keeps the row it sits on.
    #[test]
    fn kept_runs_widen_each_hunk_and_merge_where_they_touch() {
        let base = numbered(30);
        let text = base
            .replace("\n2\n", "\nTWO\n")
            .replace("\n9\n", "\nNINE\n")
            .replace("\n20\n", "\n")
            .replace("\n30\n", "\nTHIRTY\n");
        // TWO sits on row 1, NINE on row 8, the deletion of 20 before row 19,
        // and THIRTY on row 28. The buffer has 30 rows, the empty last one
        // included.
        assert_eq!(
            kept_runs(&map_of(&base, &text), 30, None),
            [0..12, 16..23, 25..30],
        );
    }

    /// The cursor's row keeps three rows each side as a hunk does, alone where
    /// it sits apart from every hunk and merged where its run touches one.
    #[test]
    fn kept_runs_keep_the_cursor_row_with_its_context() {
        let base = numbered(60);
        let map = map_of(&base, &base.replace("\n41\n", "\nFORTY-ONE\n"));
        let runs = |cursor| {
            kept_runs(&map, 61, Some(cursor))
                .into_iter()
                .map(|run| (run.start, run.end))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            [runs(10), runs(36)],
            [vec![(7, 14), (37, 44)], vec![(33, 44)]],
            "the runs as start and end rows, for a cursor apart from the hunk and one touching it",
        );
    }

    /// A refined modified hunk keeps each marked run and its last live row,
    /// each with its context, and leaves out the unmarked rows between them.
    #[test]
    fn a_refined_modified_hunk_keeps_its_marked_runs_and_its_last_row() {
        let map = |marked_rows: Vec<Range<u32>>| {
            DiffMap::from_hunks(
                [DiffHunk {
                    status: DiffHunkStatus::Modified,
                    unstaged_lines: Vec::new(),
                    marked_rows,
                    buffer_start_line: 10,
                    buffer_line_range: 10..60,
                    base_byte_range: 0..0,
                    anchor_range: None,
                    token_detail: Some(Arc::new(TokenDetail {
                        buffer_spans: Vec::new(),
                        base_spans: Vec::new(),
                        tree: false,
                    })),
                }],
                None,
            )
        };
        assert_eq!(
            [
                kept_runs(&map(vec![10..12, 58..60]), 80, None),
                kept_runs(&map(std::iter::once(10..12).collect()), 80, None),
            ],
            [vec![7..15, 55..63], vec![7..15, 56..63]],
        );
    }

    /// The gaps are every row no run holds, at the top, between runs, and at
    /// the bottom.
    #[test]
    fn gaps_are_the_rows_between_and_around_the_runs() {
        let every_row = |line_count: u32| 0..line_count;
        assert_eq!(
            [
                gaps(&[2..5, 8..9], 12),
                gaps(&[every_row(12)], 12),
                gaps(&[], 3)
            ],
            [vec![0..2, 5..8, 9..12], vec![], vec![every_row(3)]],
        );
    }

    /// A gap folds from the start of its first row under a fold row's
    /// placeholder, which counts the gap's lines without the empty row after
    /// the final newline. That row alone folds from the end of the row above
    /// under an empty placeholder, and so does an empty file's one row.
    #[test]
    fn a_gap_folds_from_its_first_row_under_a_count_of_its_lines() {
        let rope = Rope::from("one\ntwo\nthree\nfour\n");
        let fold = |rope: &Rope, gap: Range<u32>| {
            let (range, placeholder) = gap_fold(rope, gap);
            (
                range,
                placeholder.text.to_string(),
                placeholder.is_fold_row(),
            )
        };
        let lines = |n: &str| format!("\u{22ef} {n}");

        assert_eq!(
            [
                fold(&rope, 1..3),
                fold(&rope, 0..1),
                fold(&rope, 2..5),
                fold(&rope, 4..5),
                fold(&Rope::from(""), 0..1),
            ],
            [
                (
                    Point::new(1, 0)..Point::new(2, 5),
                    lines("2") + " lines",
                    true
                ),
                (
                    Point::new(0, 0)..Point::new(0, 3),
                    lines("1") + " line",
                    true
                ),
                (
                    Point::new(2, 0)..Point::new(4, 0),
                    lines("2") + " lines",
                    true
                ),
                (Point::new(3, 4)..Point::new(4, 0), String::new(), false),
                (Point::new(0, 0)..Point::new(0, 0), String::new(), false),
            ],
        );
    }

    /// A fold between two hunks opens from the edge an expand asks for. The
    /// fold above the first hunk only grows up, the fold below the last only
    /// grows down, and a fold of ten lines or fewer opens whole.
    #[test]
    fn a_fold_opens_from_the_edge_its_place_allows() {
        let edges = |gap: Range<u32>, lines| {
            [ExpandDir::Up, ExpandDir::Down, ExpandDir::All]
                .map(|dir| fold_edge(&gap, lines, 61, dir))
        };
        assert_eq!(
            [
                edges(24..37, 13),
                edges(0..37, 37),
                edges(44..61, 16),
                edges(30..40, 10)
            ],
            [
                [FoldEdge::Bottom, FoldEdge::Top, FoldEdge::Whole],
                [FoldEdge::Bottom, FoldEdge::Bottom, FoldEdge::Whole],
                [FoldEdge::Top, FoldEdge::Top, FoldEdge::Whole],
                [FoldEdge::Whole; 3],
            ],
        );
    }

    /// A top reveal takes a fold's first ten rows, a bottom reveal its last
    /// ten, and a whole reveal the fold.
    #[test]
    fn a_reveal_takes_ten_rows_from_its_edge() {
        assert_eq!(
            [FoldEdge::Top, FoldEdge::Bottom, FoldEdge::Whole].map(|edge| reveal_rows(
                &(6..30),
                24,
                edge
            )),
            [6..16, 20..30, 6..30],
        );
    }

    /// A gap inside a refined hunk takes a fold row that counts its lines, as a
    /// gap of unchanged rows does.
    ///
    /// Rows 10 to 49 form one hunk, marked at rows 10 and 11 and at 40 and 41,
    /// so the gaps at rows 15 to 36 and at row 45 lie inside it.
    #[test]
    fn a_gap_inside_a_refined_hunk_takes_a_fold_row() {
        let text: String = (1..=60).map(|n| format!("{n}\n")).collect();
        let map = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Modified,
                unstaged_lines: Vec::new(),
                marked_rows: vec![10..12, 40..42],
                buffer_start_line: 10,
                buffer_line_range: 10..50,
                base_byte_range: 0..0,
                anchor_range: None,
                token_detail: None,
            }],
            None,
        );
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut buffer = TextBuffer::with_text(BufferId::new(0), &text);
        buffer.diff_map = Some(map);
        let shared = Arc::new(RwLock::new(buffer));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
        fold_excerpt(&mut editor, None, &[], &mut None, None, false);

        let snapshot = editor.display_map.snapshot();
        let folds = snapshot.fold_snapshot();
        let inlays = folds.inlay_snapshot();
        let held: Vec<(Point, String)> = folds
            .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
            .into_iter()
            .map(|fold| {
                (
                    inlays.to_buffer_point(fold.range.start),
                    fold.placeholder.text.to_string(),
                )
            })
            .collect();
        let count = |lines: &str| format!("\u{22ef} {lines}");
        assert_eq!(
            held,
            [
                (Point::new(0, 0), count("7 lines")),
                (Point::new(15, 0), count("22 lines")),
                (Point::new(45, 0), count("1 line")),
                (Point::new(53, 0), count("7 lines")),
            ],
        );
    }

    const SCOPED: &str = "\
impl Theme {
    fn panel(&self) -> u32 {
        let a = 1;
        let b = 2;
        let c = 3;
        a + b + c
    }
}

// one
// two
// three
fn palette_title() -> u32 {
    1
}
fn hint_width() -> u32 {
    2
}
// four
// five
// six
// seven
";

    /// Each fold row takes the Scope of the change below it. A method's edited
    /// row names the method and its impl, a hunk across two functions names
    /// both, and the fold after the last hunk names the end of the file.
    #[test]
    fn each_fold_row_takes_the_scope_of_the_change_below_it() {
        let modified = |rows: Range<u32>| DiffHunk {
            status: DiffHunkStatus::Modified,
            unstaged_lines: Vec::new(),
            marked_rows: Vec::new(),
            buffer_start_line: rows.start,
            buffer_line_range: rows,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        };
        let map = DiffMap::from_hunks([modified(5..6), modified(13..17)], None);
        assert_eq!(
            rust_fold_scopes(SCOPED, map),
            [
                (0, "impl Theme \u{203a} fn panel"),
                (9, "fn palette_title + fn hint_width"),
                (20, "end of file"),
            ]
            .map(|(row, text)| (row, text.to_string())),
        );
    }

    /// A hunk that adds a whole function marks it with `+`. A hunk that removes
    /// a whole struct marks it with `U+2212` and names it from the base text,
    /// and a hunk that renames a function names it by its old and new names.
    #[test]
    fn a_whole_added_removed_or_renamed_item_takes_its_mark() {
        let around = |middle: &str| {
            format!("fn first() -> u32 {{\n    1\n}}\n// a\n// b\n// c\n// d\n{middle}// e\n// f\n// g\n// h\n")
        };
        let first_fold = |base: &str, text: &str| {
            rust_fold_scopes(text, map_of(base, text))
                .into_iter()
                .next()
        };
        let renamed = |name: &str| around(&format!("fn {name}() -> u32 {{\n    2\n}}\n"));
        assert_eq!(
            [
                first_fold(&around(""), &around("fn panel_role() -> u32 {\n    3\n}\n")),
                first_fold(
                    &around("struct OldPanel {\n    width: u32,\n}\n"),
                    &around("")
                ),
                first_fold(&renamed("modal_box"), &renamed("modal_frame")),
            ],
            [
                "+ fn panel_role",
                "\u{2212} struct OldPanel",
                "fn modal_box \u{2192} modal_frame",
            ]
            .map(|text| Some((0, text.to_string()))),
        );
    }

    /// Each fold row's first buffer row and Scope text after a cut of `text`
    /// under `map`, with `text` parsed as Rust.
    fn rust_fold_scopes(text: &str, map: DiffMap) -> Vec<(u32, String)> {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut buffer = TextBuffer::with_text(BufferId::new(0), text);
        buffer.diff_map = Some(map);
        let shared = Arc::new(RwLock::new(buffer));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());

        let rope = Rope::from(text);
        let language = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .expect("the rust language");
        let parse = ScopeParse {
            syntax: SyntaxState {
                tree: parse_rope(&language, &rope, None).expect("a parse"),
                version: editor.display_map.buffer_snapshot().version(),
                rope_snapshot: rope,
            },
            language,
            cache: TreeCache::default(),
        };
        fold_excerpt(&mut editor, None, &[], &mut None, Some(&parse), true);

        let snapshot = editor.display_map.snapshot();
        let folds = snapshot.fold_snapshot();
        let inlays = folds.inlay_snapshot();
        folds
            .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
            .into_iter()
            .filter(|fold| fold.placeholder.is_fold_row())
            .map(|fold| {
                (
                    inlays.to_buffer_point(fold.range.start).row,
                    fold.placeholder
                        .scope
                        .as_deref()
                        .map(scope_text)
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    /// A chain too wide for its room drops items from its middle for one
    /// ellipsis, the outermost item last, and keeps the innermost. Nothing
    /// shows where even the innermost does not fit or where a Scope names no
    /// item, and the end of the file reads as a label.
    #[test]
    fn a_scope_drops_middle_items_until_it_fits() {
        let item = |keyword: &str, name: &str| ScopeSegment {
            keyword: Some(keyword.to_string()),
            name: name.to_string(),
            item_range: 0..0,
            name_range: 0..0,
        };
        let scope = |segments: Vec<ScopeSegment>, end_of_file| FoldScope {
            segments,
            split: None,
            end_of_file,
            mark: ScopeMark::None,
        };
        let chain = scope(
            vec![item("mod", "a"), item("impl", "B"), item("fn", "c")],
            false,
        );
        let fit = |scope: &FoldScope, chars: u16| -> String {
            scope_parts(scope, chars * 16, FULL_CELL_SCALE)
                .iter()
                .map(|part| part.text.as_str())
                .collect()
        };

        assert_eq!(
            [
                fit(&chain, 21),
                fit(&chain, 20),
                fit(&chain, 15),
                fit(&chain, 7),
                fit(&scope(Vec::new(), false), 21),
                fit(&scope(Vec::new(), true), 21),
            ],
            [
                "mod a \u{203a} impl B \u{203a} fn c",
                "mod a \u{203a} \u{2026} \u{203a} fn c",
                "\u{2026} \u{203a} fn c",
                "",
                "",
                "end of file",
            ],
        );
    }

    /// A hunk on a file's first line has no fold above it, so its Scope takes a
    /// row under the header. The excerpt's rows and its cursor's stack row
    /// start past that row, and a press on the row lands as a press on the
    /// header does.
    #[test]
    fn a_first_line_hunk_takes_a_scope_row_under_its_header() {
        let base = "fn first() -> u32 {\n    1\n}\n// a\n// b\n// c\n// d\n// e\n// f\n// g\n";
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.rs", base, &base.replacen("u32", "u64", 1))]);
        stacked(&mut h);
        // The paint asks for the excerpt's parse, and the refold after it lands
        // reads the Scope.
        h.render_composited();
        for _ in 0..2 {
            h.stoat.drive_background();
            h.settle();
        }

        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = h.stoat.active_workspace_mut();
        let state = diff_changes_views
            .values_mut()
            .next()
            .expect("a changes view");
        let total = layout_stack(state, editors);
        let (editor_id, scope) = {
            let excerpt = &state.excerpts[0];
            (
                excerpt.editor_id,
                excerpt.scope_row.as_deref().map(scope_text),
            )
        };
        let rows = excerpt_rows(&state.excerpts[0], editors);
        place_cursor(&mut editors[editor_id], Point::new(1, 4));
        let display_row = editors[editor_id]
            .display_map
            .snapshot()
            .buffer_to_display(Point::new(1, 4))
            .row as usize;
        let cursor = cursor_stack_row(state, editors);
        let pressed = press_cell(state, editors, 1, 40);

        assert_eq!(
            (
                scope,
                total,
                cursor,
                pressed,
                cursor_point(&mut editors[editor_id])
            ),
            (
                Some("fn first".to_string()),
                2 + rows,
                Some(2 + display_row),
                true,
                Point::new(0, 0)
            ),
        );
    }

    /// A first-line hunk outside every item names no Scope, so no row shows
    /// under the header and the excerpt's rows start right under it.
    #[test]
    fn a_top_level_first_line_hunk_takes_no_scope_row() {
        let base = "use a::B;\nfn first() -> u32 {\n    1\n}\n";
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario(
            "/repo",
            &[("a.rs", base, &base.replacen("a::B", "a::C", 1))],
        );
        stacked(&mut h);
        h.render_composited();
        for _ in 0..2 {
            h.stoat.drive_background();
            h.settle();
        }

        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = h.stoat.active_workspace_mut();
        let state = diff_changes_views
            .values_mut()
            .next()
            .expect("a changes view");
        let total = layout_stack(state, editors);
        let excerpt = &state.excerpts[0];

        assert_eq!(
            (excerpt.scope_row.clone(), header_rows(excerpt), total),
            (None, 1, 1 + excerpt_rows(excerpt, editors)),
        );
    }

    /// The Scope of the cursor's row follows the row. It holds while the parse
    /// lags an edit, and reads again once a parse of the edited text lands.
    #[test]
    fn the_cursor_scope_holds_until_a_parse_of_the_edit_lands() {
        let base =
            "mod outer {\n    impl Theme {\n        fn panel() -> u32 {\n            1\n        \
                    }\n    }\n}\n";
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.rs", base, &base.replacen('1', "2", 1))]);
        stacked(&mut h);
        let parse = |h: &mut TestHarness| {
            h.render_composited();
            for _ in 0..2 {
                h.stoat.drive_background();
                h.settle();
            }
        };
        let read = |h: &mut TestHarness, row: Option<u32>| {
            let Workspace {
                diff_changes_views,
                editors,
                buffers,
                ..
            } = h.stoat.active_workspace_mut();
            let state = diff_changes_views
                .values_mut()
                .next()
                .expect("a changes view");
            if let Some(row) = row {
                place_cursor(
                    &mut editors[state.excerpts[0].editor_id],
                    Point::new(row, 0),
                );
            }
            state
                .cursor_scope(editors, buffers)
                .as_deref()
                .map(scope_text)
        };

        parse(&mut h);
        let body = read(&mut h, Some(3));
        let header = read(&mut h, Some(1));
        {
            let ws = h.stoat.active_workspace();
            let id = ws
                .buffers
                .id_for_path(Path::new("/repo/a.rs"))
                .expect("an open buffer");
            ws.buffers
                .get(id)
                .expect("buffer")
                .write()
                .expect("buffer poisoned")
                .edit(0..0, "//\n");
        }
        let held = read(&mut h, None);
        parse(&mut h);
        let reread = read(&mut h, Some(4));

        let function = Some("mod outer \u{203a} impl Theme \u{203a} fn panel".to_string());
        let block = Some("mod outer \u{203a} impl Theme".to_string());
        assert_eq!(
            [body, header, held, reread],
            [function.clone(), block.clone(), block, function],
        );
    }

    /// The text of every run of `scope`'s chip, with room for all of it.
    fn scope_text(scope: &FoldScope) -> String {
        scope_parts(scope, u16::MAX, FULL_CELL_SCALE)
            .iter()
            .map(|part| part.text.as_str())
            .collect()
    }

    /// A shown row stays, a hidden row moves to the next run below, and a
    /// row below every run moves to the first run.
    #[test]
    fn a_landing_on_a_hidden_row_moves_to_a_shown_one() {
        let kept = [2..5, 8..9];
        assert_eq!(
            [3, 0, 6, 10].map(|row| landing_row(&kept, row)),
            [3, 2, 8, 2]
        );
    }

    /// A landing on a row its excerpt shows holds its screen row for the next
    /// paint. A landing that a fold moves to another row shows another line,
    /// so it holds none.
    #[test]
    fn only_a_landing_on_a_shown_row_holds_its_screen_row() {
        let mut h = TestHarness::with_size(100, 24);
        let base = numbered(20);
        h.stage_review_scenario(
            "/repo",
            &[("a.txt", &base, &base.replace("\n10\n", "\nTEN\n"))],
        );
        stacked(&mut h);
        let land_on = |row| {
            let state = h
                .stoat
                .active_workspace_mut()
                .focused_diff_changes_mut()
                .expect("a changes view");
            state.pending_landing = Some(Landing {
                buffer_id: state.excerpts[0].buffer_id,
                rel_path: "a.txt".to_string(),
                row,
                column: 0,
                screen_row: Some(3),
            });
            pump_diff_changes_stack(&mut h.stoat);
            h.stoat
                .active_workspace()
                .focused_diff_changes()
                .and_then(|state| state.held_screen_row)
        };
        // TEN sits on row 9, inside the shown run of rows 6 to 12, and row 2
        // lies in the fold above it.
        assert_eq!([9, 2].map(land_on), [Some(3), None]);
    }

    /// The scroll moves the least distance that shows the row, and stays for a
    /// row already in view or for a viewport of no rows.
    #[test]
    fn ensure_row_visible_scrolls_the_least_distance() {
        let scrolled = |row, rows| {
            let mut state = DiffChangesState {
                scroll: 5,
                ..DiffChangesState::default()
            };
            state.ensure_row_visible(&mut SlotMap::with_key(), row, rows);
            state.scroll
        };
        assert_eq!(
            [
                scrolled(2, 4),
                scrolled(9, 4),
                scrolled(8, 4),
                scrolled(9, 0)
            ],
            [2, 6, 5, 5],
        );
    }

    /// A row under the pinned header uncovers to the first row under it, or to
    /// its file's first row with the header in place. The follow keeps its
    /// scrolloff band under the pinned row.
    ///
    /// Each file takes a header row and twelve paired rows, with no final
    /// newline to add an empty row. So a.txt takes stack rows 0 to 12 and
    /// b.txt rows 13 to 25, in a view of eleven rows.
    #[test]
    fn the_scroll_keeps_rows_clear_of_the_pinned_header() {
        let base = (1..=12)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let text = (1..=12)
            .map(|n| format!("{n}!"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut h = TestHarness::with_size(120, 12);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
        h.stage_review_scenario("/repo", &[("a.txt", &base, &text), ("b.txt", &base, &text)]);
        stacked(&mut h);
        h.render_composited();
        let state = |h: &mut TestHarness| {
            let views = &mut h.stoat.active_workspace_mut().diff_changes_views;
            views.values_mut().next().expect("a changes view").scroll
        };

        let uncovered = [(5, 5), (1, 1), (15, 14)].map(|(scroll, row)| {
            let Workspace {
                diff_changes_views,
                editors,
                ..
            } = h.stoat.active_workspace_mut();
            let state = diff_changes_views
                .values_mut()
                .next()
                .expect("a changes view");
            state.scroll = scroll;
            state.uncover_row(editors, row);
            state.scroll
        });
        let followed = [(0, 7, 7), (0, 9, 7), (1, 2, 15)].map(|(index, buffer_row, scroll)| {
            {
                let Workspace {
                    diff_changes_views,
                    editors,
                    ..
                } = h.stoat.active_workspace_mut();
                let state = diff_changes_views
                    .values_mut()
                    .next()
                    .expect("a changes view");
                state.focused = index;
                state.scroll = scroll;
                let editor = &mut editors[state.excerpts[index].editor_id];
                place_cursor(editor, Point::new(buffer_row, 0));
            }
            follow_cursor(&mut h.stoat, 3);
            state(&mut h)
        });

        assert_eq!((uncovered, followed), ([4, 0, 13], [4, 6, 13]));
    }

    /// The repository at /repo, with the harness's fake files under it.
    fn repo(h: &mut TestHarness) -> FakeRepoBuilder<'_> {
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo(PathBuf::from("/repo"))
            .with_fs(h.fake_fs())
    }

    /// The focused changes view's excerpts as each file's path and whether its
    /// folds are cut.
    fn excerpts(h: &TestHarness) -> Vec<(String, bool)> {
        h.stoat
            .active_workspace()
            .focused_diff_changes()
            .expect("a focused changes view")
            .excerpts
            .iter()
            .map(|excerpt| (excerpt.rel_path.clone(), excerpt.folded_version.is_some()))
            .collect()
    }

    fn listed(paths: &[&str]) -> Vec<(String, bool)> {
        paths.iter().map(|path| (path.to_string(), true)).collect()
    }

    /// A changes view over the harness's focused view, settled.
    fn stacked(h: &mut TestHarness) {
        h.cover_with_diff_changes(false);
        h.settle();
    }

    /// Each changed file and each untracked file with content gets an excerpt,
    /// folded, in path order.
    #[test]
    fn changed_and_untracked_files_build_folded_excerpts_in_path_order() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario(
            "/repo",
            &[
                ("c.txt", "c\n", "C\n"),
                ("a.txt", "a\n", "A\n"),
                ("b.txt", "b\n", "B\n"),
            ],
        );
        repo(&mut h).untracked("z.txt");
        h.fake_fs()
            .insert_file(Path::new("/repo/z.txt"), b"scratch\n");

        stacked(&mut h);
        assert_eq!(excerpts(&h), listed(&["a.txt", "b.txt", "c.txt", "z.txt"]));
    }

    /// Under a revision base an untracked file with content stacks beside the
    /// files changed since the revision, as the change walk under that base
    /// visits it.
    #[test]
    fn a_revision_base_lists_an_untracked_file() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        repo(&mut h)
            .untracked("z.txt")
            .commit("base0", &[("a.txt", "a\n")]);
        h.fake_fs()
            .insert_file(Path::new("/repo/z.txt"), b"scratch\n");
        dispatch(
            &mut h.stoat,
            &stoat_action::DiffFiles {
                rev: Some("base0".into()),
            },
        );
        h.settle();

        stacked(&mut h);
        assert_eq!(excerpts(&h), listed(&["a.txt", "z.txt"]));
    }

    /// An excerpt shows its hunk with three rows of context each side, and the
    /// rows beyond them fold away.
    #[test]
    fn an_excerpt_folds_every_row_beyond_the_context_of_its_hunks() {
        let mut h = TestHarness::with_size(100, 24);
        let base = numbered(20);
        h.stage_review_scenario(
            "/repo",
            &[("a.txt", &base, &base.replace("\n10\n", "\nTEN\n"))],
        );
        stacked(&mut h);

        let ws = h.stoat.active_workspace_mut();
        let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
        let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
        let snapshot = editor.display_map.snapshot();
        let shown: Vec<u32> = (0..snapshot.line_count())
            .filter_map(|row| snapshot.display_to_buffer(DisplayPoint::new(row, 0)))
            .map(|point| point.row)
            .collect();
        // The fold row at row 0 stands for the folded top, rows 6 to 12 show the
        // hunk at row 9 with its context, and the fold row at row 13 stands for
        // the rows below. The base line the hunk replaced takes a row with no
        // buffer row.
        assert_eq!(shown, [0, 6, 7, 8, 9, 10, 11, 12, 13]);
    }

    /// An unsaved edit to a clean open file puts the file in the list, which
    /// reads the open buffer's diff map.
    #[test]
    fn an_unsaved_edit_to_a_clean_file_gets_an_excerpt() {
        let mut h = TestHarness::with_size(100, 24);
        h.stoat.set_diff_warm_auto(true);
        repo(&mut h)
            .head_file("d.txt", "same\n")
            .head_file("f.txt", "kept\n");
        h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
        h.fake_fs().insert_file(Path::new("/repo/f.txt"), b"kept\n");
        h.open_file(Path::new("/repo/f.txt"));
        h.open_file(Path::new("/repo/d.txt"));
        h.type_keys("i n e w Escape");
        h.settle_diff_jobs();

        stacked(&mut h);
        assert_eq!(excerpts(&h), listed(&["d.txt"]));
    }

    /// A stack over the changed a.txt with the clean d.txt open under it, and
    /// d.txt's buffer.
    fn stack_beside_clean_file(h: &mut TestHarness) -> BufferId {
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        repo(h).head_file("d.txt", "same\n");
        h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
        h.open_file(Path::new("/repo/d.txt"));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        stacked(h);
        buffer_id
    }

    /// Write `text` at the start of buffer `id`, unsaved.
    fn prepend(h: &TestHarness, id: BufferId, text: &str) {
        h.stoat
            .active_workspace()
            .buffers
            .get(id)
            .expect("buffer")
            .write()
            .expect("buffer poisoned")
            .edit(0..0, text);
    }

    /// An excerpt's rows follow its editor's display rows, after a refold and
    /// after an edit that adds a row before any diff map lands.
    #[test]
    fn excerpt_rows_follow_a_refold_and_an_edit() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        stacked(&mut h);
        let rows = |h: &mut TestHarness| {
            let ws = h.stoat.active_workspace_mut();
            let id = ws.focused_diff_changes_id().expect("a changes view");
            let Workspace {
                diff_changes_views,
                editors,
                ..
            } = ws;
            let excerpt = &diff_changes_views[id].excerpts[0];
            let shown = excerpt_rows(excerpt, editors);
            let display = editors[excerpt.editor_id].display_map.snapshot();
            (shown, display.line_count() as usize)
        };

        let folded = rows(&mut h);
        prepend(&h, buffer_id, "new\n");
        let edited = rows(&mut h);
        let count = folded.1;
        assert_eq!([folded, edited], [(count, count), (count + 1, count + 1)]);
    }

    /// The rows an expand opened follow their lines through an edit above them,
    /// so the refold the edit brings keeps them open.
    #[test]
    fn a_reveal_follows_its_lines_through_an_edit() {
        let mut h = TestHarness::with_size(100, 24);
        h.stoat.set_diff_warm_auto(true);
        let base = numbered(60);
        let working = base.replace("\n41\n", "\nFORTY-ONE\n");
        h.stage_review_scenario("/repo", &[("a.txt", &base, &working)]);
        h.open_file(Path::new("/repo/a.txt"));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        stacked(&mut h);
        h.settle_diff_jobs();
        {
            let ws = h.stoat.active_workspace_mut();
            let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
            let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
            place_cursor(editor, Point::new(40, 0));
        }
        let opened = expand_from_cursor(&mut h.stoat, ExpandDir::Up);

        prepend(&h, buffer_id, "new\n");
        h.settle_diff_jobs();
        assert_eq!(
            (opened, shown_rows(&mut h)),
            (Ok(()), rows_of(&[0..=3, 4..=4, 28..=44, 45..=45])),
        );
    }

    /// The buffer row each display row of the focused excerpt starts with, so a
    /// fold row reads as the first row it hides.
    fn shown_rows(h: &mut TestHarness) -> Vec<u32> {
        let ws = h.stoat.active_workspace_mut();
        let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
        let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
        let snapshot = editor.display_map.snapshot();
        (0..snapshot.line_count())
            .filter_map(|row| snapshot.display_to_buffer(DisplayPoint::new(row, 0)))
            .map(|point| point.row)
            .collect()
    }

    /// The rows of `runs`, one after another.
    fn rows_of(runs: &[RangeInclusive<u32>]) -> Vec<u32> {
        runs.iter().flat_map(|run| run.clone()).collect()
    }

    /// Excerpts over long files that paint before their hunks land show the
    /// rows of excerpts that waited for the hunks to paint.
    ///
    /// One scheduler pass lands the tally, which opens each excerpt unfolded
    /// with its hunks still on the pool. A paint then gives each excerpt its
    /// first width, which a file this long wraps on the pool, and the folds
    /// that the hunks bring land while that rewrap runs.
    #[test]
    fn excerpts_folded_under_a_pending_wrap_show_their_folded_rows() {
        let stack_rows = |paint_before_hunks: bool| {
            let mut h = TestHarness::with_size(160, 48);
            let base = numbered(120);
            let working = base.replace("\n60\n", "\nSIXTY\n");
            h.stage_review_scenario(
                "/repo",
                &[("a.txt", &base, &working), ("b.txt", &base, &working)],
            );
            h.open_file(Path::new("/repo/a.txt"));
            h.cover_with_diff_changes(false);
            h.run_until_parked();
            h.stoat.drive_pumps();
            if paint_before_hunks {
                h.stoat.render();
            }
            h.settle();
            h.stoat.render();
            h.stoat.render();

            let ws = h.stoat.active_workspace_mut();
            let id = ws.focused_diff_changes_id().expect("a changes view");
            let Workspace {
                diff_changes_views,
                editors,
                ..
            } = ws;
            diff_changes_views[id]
                .excerpts
                .iter()
                .map(|excerpt| excerpt_rows(excerpt, editors))
                .collect::<Vec<_>>()
        };
        let waited = stack_rows(false);
        let folded = waited.iter().all(|rows| (1..121).contains(rows));
        assert_eq!((stack_rows(true), folded), (waited, true));
    }

    /// An unsaved edit to a clean open file joins a stack that already
    /// exists, once the file's diff map lands.
    #[test]
    fn a_landing_outside_the_stack_relists_it() {
        let mut h = TestHarness::with_size(100, 24);
        let clean = stack_beside_clean_file(&mut h);
        let before = excerpts(&h);

        prepend(&h, clean, "new ");
        h.settle_diff_map(clean);
        assert_eq!(
            (before, excerpts(&h)),
            (listed(&["a.txt"]), listed(&["a.txt", "d.txt"])),
        );
    }

    /// A relist reads the root the workspace resolved, so a moved stamp lists
    /// the files again with no repository opened.
    #[test]
    fn a_relist_opens_no_repository() {
        let mut h = TestHarness::with_size(100, 24);
        let clean = stack_beside_clean_file(&mut h);
        let opened = h.fake_git().discover_calls();

        prepend(&h, clean, "new ");
        h.stoat
            .active_workspace_mut()
            .install_test_diff_map(clean, map_of("same\n", "new same\n"));
        h.settle();
        assert_eq!(
            (excerpts(&h), h.fake_git().discover_calls() - opened),
            (listed(&["a.txt", "d.txt"]), 0),
        );
    }

    /// A base switch moves every excerpt's diff map, so each excerpt cuts its
    /// folds again.
    #[test]
    fn a_base_switch_refolds_the_excerpts() {
        let mut h = TestHarness::with_size(100, 24);
        h.stoat.set_diff_warm_auto(true);
        h.stage_index_scenario("/repo", &[("e.txt", "head\n", "index\n", "work\n")]);
        // Set straight on the workspace, since the base actions also turn the
        // diff view on in the focused pane.
        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Index);
        stacked(&mut h);
        let folded = |h: &TestHarness| {
            h.stoat
                .active_workspace()
                .focused_diff_changes()
                .and_then(|state| state.excerpts.first())
                .and_then(|excerpt| excerpt.folded_version)
        };
        let index = folded(&h);

        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Head);
        h.settle_diff_jobs();
        let head = folded(&h);
        assert_eq!(
            (index.is_some(), head.is_some(), index != head),
            (true, true, true)
        );
    }

    /// One excerpt's folds: the diff map version they were cut from, the fold
    /// snapshot's version, and each fold's id with the buffer rows it hides.
    #[derive(Debug, PartialEq)]
    struct Cut {
        map: Option<usize>,
        version: usize,
        folds: Vec<(usize, Range<u32>)>,
    }

    /// A changes view over a file of 60 numbered lines with a hunk on rows 10,
    /// 30, and 50, so its folds hide rows 0..7, 14..27, 34..47, and 54..61.
    /// Diff maps stay warm, as in the app, so an edit refolds the excerpt.
    fn three_hunk_stack() -> TestHarness {
        let mut h = TestHarness::with_size(100, 24);
        h.stoat.set_diff_warm_auto(true);
        let base = numbered(60);
        let text = base
            .replace("\n11\n", "\nELEVEN\n")
            .replace("\n31\n", "\nTHIRTY-ONE\n")
            .replace("\n51\n", "\nFIFTY-ONE\n");
        h.stage_review_scenario("/repo", &[("a.txt", &base, &text)]);
        h.open_file(Path::new("/repo/a.txt"));
        h.cover_with_diff_changes(false);
        h.settle_diff_jobs();
        h
    }

    /// Replace the first `from` in /repo/a.txt with `to`, and let the changes
    /// view refold over the diff map that follows.
    fn edit(h: &mut TestHarness, from: &str, to: &str) {
        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(Path::new("/repo/a.txt"))
            .expect("an open buffer");
        let start = h
            .buffer_text(buffer_id)
            .find(from)
            .expect("the replaced text");
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("a buffer")
            .write()
            .expect("buffer poisoned")
            .edit(start..start + from.len(), to);
        h.settle_diff_jobs();
    }

    /// The folds of the focused changes view's first excerpt.
    fn cut(h: &mut TestHarness) -> Cut {
        let ws = h.stoat.active_workspace_mut();
        let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
        let map = excerpt.folded_version;
        let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
        let snapshot = editor.display_map.snapshot();
        let folds = snapshot.fold_snapshot();
        let inlays = folds.inlay_snapshot();
        // A fold from a row's start hides that row under its fold row, and a
        // fold from a row's end hides the rows below it.
        let hidden = |range: &Range<InlayPoint>| {
            let start = inlays.to_buffer_point(range.start);
            let first = match start.column {
                0 => start.row,
                _ => start.row + 1,
            };
            first..inlays.to_buffer_point(range.end).row + 1
        };
        Cut {
            map,
            version: folds.version(),
            folds: folds
                .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
                .into_iter()
                .map(|fold| (fold.id.0, hidden(&fold.range)))
                .collect(),
        }
    }

    /// The rows each fold of `after` hides, with whether `before` held it.
    fn survivors(before: &Cut, after: &Cut) -> Vec<(bool, Range<u32>)> {
        after
            .folds
            .iter()
            .map(|(id, rows)| {
                (
                    before.folds.iter().any(|(held, _)| held == id),
                    rows.clone(),
                )
            })
            .collect()
    }

    /// A landing whose map shows the same rows leaves every fold, so the
    /// display map does no work for it.
    #[test]
    fn a_landing_that_keeps_the_shown_rows_toggles_no_fold() {
        let mut h = three_hunk_stack();
        let before = cut(&mut h);
        edit(&mut h, "ELEVEN", "ELEVEN!");
        let after = cut(&mut h);
        assert_eq!(
            (after.map != before.map, after.version, after.folds),
            (true, before.version, before.folds),
        );
    }

    /// A hunk that lands in a gap refolds that gap, and the other folds stay.
    #[test]
    fn a_hunk_landing_in_a_gap_refolds_that_gap_only() {
        let mut h = three_hunk_stack();
        let before = cut(&mut h);
        // Row 36 lies three rows below the run around row 30, so the run grows
        // to row 40 and the gap below it shrinks.
        edit(&mut h, "\n37\n", "\nTHIRTY-SEVEN\n");
        assert_eq!(
            survivors(&before, &cut(&mut h)),
            [
                (true, 0..7),
                (true, 14..27),
                (false, 40..47),
                (true, 54..61)
            ],
        );
    }

    /// A run whose only hunk goes leaves one fold over both gaps around it,
    /// and the other folds stay.
    #[test]
    fn a_run_whose_hunk_goes_joins_the_gaps_around_it() {
        let mut h = three_hunk_stack();
        let before = cut(&mut h);
        edit(&mut h, "THIRTY-ONE", "31");
        assert_eq!(
            survivors(&before, &cut(&mut h)),
            [(true, 0..7), (false, 14..47), (true, 54..61)],
        );
    }

    /// A row added inside a hunk moves the folds below it a row down, and every
    /// fold stays.
    #[test]
    fn a_row_added_inside_a_hunk_keeps_every_fold() {
        let mut h = three_hunk_stack();
        let before = cut(&mut h);
        edit(&mut h, "ELEVEN", "ELEVEN\nMORE");
        assert_eq!(
            survivors(&before, &cut(&mut h)),
            [(true, 0..7), (true, 15..28), (true, 35..48), (true, 55..62)],
        );
    }

    /// An edit that takes the focused file's last hunk leaves its excerpt in
    /// the stack, with the cursor where the edit was.
    #[test]
    fn a_revert_of_a_files_last_hunk_keeps_its_excerpt_under_the_cursor() {
        let mut h = TestHarness::with_size(100, 24);
        stack_on_an_unsaved_edit(&mut h);
        let before = focus(&mut h);

        h.edit_focused(0..4, "");
        h.settle_diff_jobs();
        assert_eq!(
            (before, excerpts(&h), focus(&mut h)),
            (at("d.txt", 0), listed(&["a.txt", "d.txt"]), at("d.txt", 0)),
        );
    }

    /// The excerpt the stack kept for the focus goes once the focus moves to
    /// another excerpt.
    #[test]
    fn a_hunkless_excerpt_leaves_once_the_focus_moves_off() {
        let mut h = TestHarness::with_size(100, 24);
        stack_on_an_unsaved_edit(&mut h);
        h.edit_focused(0..4, "");
        h.settle_diff_jobs();
        let kept = excerpts(&h);

        dispatch(&mut h.stoat, &DiffChangesPrev);
        h.settle();
        assert_eq!(
            (kept, excerpts(&h), focus(&mut h).0),
            (
                listed(&["a.txt", "d.txt"]),
                listed(&["a.txt"]),
                "a.txt".to_string()
            ),
        );
    }

    /// A changes view over the changed a.txt and the clean d.txt, which an
    /// unsaved edit puts in the list, with the cursor on d.txt's edit.
    ///
    /// The tally reads the disk, so the list holds d.txt through its buffer's
    /// diff map alone, and an edit that takes the map's last hunk takes the
    /// file off the list.
    fn stack_on_an_unsaved_edit(h: &mut TestHarness) {
        h.stoat.set_diff_warm_auto(true);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        repo(h).head_file("d.txt", "same\n");
        h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
        h.open_file(Path::new("/repo/d.txt"));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        prepend(h, buffer_id, "new ");
        h.settle_diff_jobs();
        dispatch(&mut h.stoat, &DiffChanges);
        h.settle();
    }

    /// The focused excerpt's file and its cursor's row.
    fn focus(h: &mut TestHarness) -> (String, u32) {
        let ws = h.stoat.active_workspace_mut();
        let id = ws.focused_diff_changes_id().expect("a changes view");
        let excerpt = ws.diff_changes_views[id]
            .focused_excerpt()
            .expect("a focused excerpt");
        let (rel_path, editor_id) = (excerpt.rel_path.clone(), excerpt.editor_id);
        let editor = ws.editors.get_mut(editor_id).expect("an editor");
        (rel_path, cursor_point(editor).row)
    }

    fn at(rel_path: &str, row: u32) -> (String, u32) {
        (rel_path.to_string(), row)
    }

    /// A stage and a `.git` event move the base where no diff map moved yet,
    /// so each moves the stamp, and the list follows.
    #[test]
    fn a_stage_and_a_git_event_each_move_the_stamp() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        let stamp = |h: &TestHarness| StackStamp::read(h.stoat.active_workspace());

        let first = stamp(&h);
        h.stoat
            .active_workspace_mut()
            .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
        let staged = stamp(&h);
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        assert_eq!((first != staged, staged != stamp(&h)), (true, true));
    }

    /// A clean buffer's content is the disk's, so after a commit cleans its
    /// file, the map a hidden buffer keeps from before the commit lists nothing.
    #[test]
    fn a_clean_buffer_with_a_stale_map_lists_from_the_tally_alone() {
        let h = commit_behind_a_hidden_buffer(|_| {});
        assert_eq!(listed_rel_paths(&h), ["b.txt"]);
    }

    /// The tally reads the disk, which an unsaved edit has not reached, so the
    /// edited buffer stays listed from its map whatever the map's age.
    #[test]
    fn an_unsaved_buffer_with_a_stale_map_stays_listed() {
        let h = commit_behind_a_hidden_buffer(|h| h.type_keys("i x escape"));
        assert_eq!(listed_rel_paths(&h), ["a.txt", "b.txt"]);
    }

    /// A repo where `a.txt` and `b.txt` differ from HEAD, and a commit of
    /// `a.txt` lands while `b.txt` hides it, with the diff jobs and the tally
    /// settled after it.
    ///
    /// The hidden `a.txt` keeps the map from before the commit. `before_hide`
    /// runs on `a.txt` while it has the focus.
    fn commit_behind_a_hidden_buffer(before_hide: impl FnOnce(&mut TestHarness)) -> TestHarness {
        let mut h = TestHarness::with_size(100, 24);
        h.stoat.set_diff_warm_auto(true);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.stoat.sync_repo_root();
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();
        before_hide(&mut h);
        h.open_file(Path::new("/repo/b.txt"));

        h.fake_git()
            .add_repo("/repo")
            .clear_changes()
            .head_file("a.txt", "A\n")
            .modified("b.txt", "b\n", "B\n");
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.settle_diff_jobs();
        h
    }

    /// The files the changes view lists, by their path in the repository.
    fn listed_rel_paths(h: &TestHarness) -> Vec<String> {
        listed_files(&h.stoat).into_keys().collect()
    }

    /// An open and a close move the stamp's path part, and a diff landing its
    /// map part, since the list reads the open buffers' diff maps.
    #[test]
    fn an_open_a_landing_and_a_close_move_their_stamp_parts() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        let parts = |h: &TestHarness| {
            let stamp = StackStamp::read(h.stoat.active_workspace());
            (stamp.maps, stamp.paths)
        };
        let mut seen = vec![parts(&h)];

        h.open_file(Path::new("/repo/a.txt"));
        seen.push(parts(&h));
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
        h.settle_diff_map(buffer_id);
        seen.push(parts(&h));
        h.stoat.active_workspace_mut().buffers.remove(buffer_id);
        seen.push(parts(&h));
        let moved: Vec<(bool, bool)> = seen
            .windows(2)
            .map(|pair| (pair[0].0 != pair[1].0, pair[0].1 != pair[1].1))
            .collect();
        assert_eq!(moved, [(false, true), (true, false), (false, true)]);
    }

    /// Under a revision base the stack lists what changed since the revision,
    /// a file committed since and clean in the tree included.
    #[test]
    fn a_revision_base_lists_what_changed_since_the_revision() {
        let mut h = TestHarness::with_size(100, 24);
        repo(&mut h)
            .head_file("committed.txt", "after\n")
            .commit("base0", &[("committed.txt", "before\n")]);
        h.fake_fs()
            .insert_file(Path::new("/repo/committed.txt"), b"after\n");
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::named("base0", "base0".to_string())));

        stacked(&mut h);
        assert_eq!(excerpts(&h), listed(&["committed.txt"]));
    }

    /// A close drops the buffers only the stack opened, and keeps the one a
    /// pane opened before.
    #[test]
    fn a_close_drops_the_buffers_only_the_stack_opened() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        stacked(&mut h);
        let open = |h: &TestHarness, path: &str| {
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(Path::new(path))
                .is_some()
        };
        let before = (open(&h, "/repo/a.txt"), open(&h, "/repo/b.txt"));

        dispatch(&mut h.stoat, &CloseDiffChanges);
        assert_eq!(
            (before, open(&h, "/repo/a.txt"), open(&h, "/repo/b.txt")),
            ((true, true), true, false),
        );
    }

    /// Lines `line 0` to `line 38`, and with `edit` the lines from 4 on edited
    /// in every other block of five, so a stack shows each line but the first.
    /// `appended` new lines follow them.
    fn blocks(edit: bool, appended: usize) -> String {
        let line = |n: usize| match edit && n >= 4 && ((n - 4) / 5).is_multiple_of(2) {
            true => format!("line {n} edit\n"),
            false => format!("line {n}\n"),
        };
        let new: String = (0..appended).map(|n| format!("new {n}\n")).collect();
        (0..39).map(line).collect::<String>() + &new
    }

    /// A painted changes view over `a.txt` and `b.txt`, two [`blocks`] files,
    /// with its view inside `b.txt`'s excerpt `offset` rows past its header.
    ///
    /// With `cursor_there`, the cursor sits on screen five rows under the
    /// view's top in `b.txt`, else on `a.txt`'s tenth stack row above the
    /// view. A paint follows the cursor before the scroll moves, so the scroll
    /// stays where it is set.
    fn seated_stack(cursor_there: bool, offset: usize) -> TestHarness {
        let mut h = TestHarness::with_size(120, 24);
        let (base, work) = (blocks(false, 0), blocks(true, 0));
        h.stage_review_scenario("/repo", &[("a.txt", &base, &work), ("b.txt", &base, &work)]);
        stacked(&mut h);
        let _ = h.stoat.render();
        with_stack(&mut h, |state, editors| {
            let row = match cursor_there {
                true => state.excerpts[1].top + offset + 5,
                false => state.excerpts[0].top + 10,
            };
            select_row(state, editors, row);
        });
        let _ = h.stoat.render();
        with_stack(&mut h, |state, _| {
            state.scroll = state.excerpts[1].top + offset;
        });
        let _ = h.stoat.render();
        h
    }

    /// The edited [`blocks`] text with three new lines after line `after`.
    fn with_mid_lines(after: usize) -> String {
        let work = blocks(true, 0);
        let lines: Vec<&str> = work.split_inclusive('\n').collect();
        lines[..=after].concat() + "mid 0\nmid 1\nmid 2\n" + &lines[after + 1..].concat()
    }

    /// Write `text` to `/repo/<file>`, reload every buffer, settle, and return
    /// the first paint.
    fn reload_file(h: &mut TestHarness, file: &str, text: &str) -> Buffer {
        h.fake_fs()
            .insert_file(Path::new(&format!("/repo/{file}")), text.as_bytes());
        auto_reload::reload_all(&mut h.stoat, true);
        h.settle();
        h.stoat.render()
    }

    /// Run `f` over the focused changes view and the workspace's editors.
    fn with_stack<T>(
        h: &mut TestHarness,
        f: impl FnOnce(&mut DiffChangesState, &mut SlotMap<EditorId, EditorState>) -> T,
    ) -> T {
        let ws = h.stoat.active_workspace_mut();
        let id = ws
            .focused_diff_changes_id()
            .expect("a focused changes view");
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = ws;
        f(&mut diff_changes_views[id], editors)
    }

    /// The text on each row of the focused stack's view, a header row as its
    /// file's path.
    fn stack_screen_text(h: &mut TestHarness) -> Vec<String> {
        with_stack(h, |state, editors| {
            layout_stack(state, editors);
            (state.scroll..state.scroll + state.viewport_rows)
                .map(|row| {
                    let Some(excerpt) = state.excerpts.iter().rev().find(|e| e.top <= row) else {
                        return String::new();
                    };
                    match row.checked_sub(excerpt.top + header_rows(excerpt)) {
                        None => excerpt.rel_path.clone(),
                        Some(display_row) => editors
                            .get_mut(excerpt.editor_id)
                            .map(|editor| {
                                editor
                                    .display_map
                                    .snapshot()
                                    .display_line(display_row as u32)
                            })
                            .unwrap_or_default(),
                    }
                })
                .collect()
        })
    }

    /// The focused stack's scroll and whether a reload holds it.
    fn stack_view(h: &mut TestHarness) -> (usize, bool) {
        with_stack(h, |state, _| (state.scroll, state.view_hold.is_some()))
    }

    /// Append twenty lines to `a.txt` on disk, reload every buffer, settle,
    /// and paint.
    fn grow_first_file(h: &mut TestHarness) {
        reload_file(h, "a.txt", &blocks(true, 20));
    }

    /// Twenty lines that arrive at the end of the first file leave the second
    /// file's text on its screen rows. The cursor there keeps its screen row,
    /// so the stack moves down twenty rows.
    #[test]
    fn a_reload_holds_the_stack_when_lines_arrive_above_the_view() {
        let mut h = seated_stack(true, 5);
        let (before, (scroll, _)) = (stack_screen_text(&mut h), stack_view(&mut h));

        grow_first_file(&mut h);

        assert_eq!(
            (stack_screen_text(&mut h), stack_view(&mut h)),
            (before, (scroll + 20, true)),
        );
    }

    /// A stack whose cursor sits above the view holds the first text row under
    /// its pinned header, so the text stays put though the cursor does not
    /// move. A view that starts on the file header holds the text row under
    /// the header, and the header stays on the view's top row.
    #[test]
    fn a_reload_holds_the_stack_with_the_cursor_off_screen() {
        for offset in [5, 0] {
            let mut h = seated_stack(false, offset);
            let (before, (scroll, _)) = (stack_screen_text(&mut h), stack_view(&mut h));

            grow_first_file(&mut h);

            assert_eq!(
                (stack_screen_text(&mut h), stack_view(&mut h).0),
                (before, scroll + 20),
                "with the view {offset} rows past the second header",
            );
        }
    }

    /// The cursor on screen keeps its screen row though three lines arrive in
    /// its own file between the view's top and the cursor.
    #[test]
    fn a_reload_keeps_the_stack_cursor_on_its_screen_row() {
        let mut h = seated_stack(true, 5);
        let cursor_screen_row = |h: &mut TestHarness| {
            with_stack(h, |state, editors| {
                layout_stack(state, editors);
                cursor_stack_row(state, editors).map(|row| row - state.scroll)
            })
        };
        let before = cursor_screen_row(&mut h);

        reload_file(&mut h, "b.txt", &with_mid_lines(7));

        assert_eq!((before, cursor_screen_row(&mut h)), (Some(5), Some(5)));
    }

    /// Lines that arrive in an off-screen cursor's own file above it move the
    /// cursor, and the stack holds its text with no follow to the cursor.
    #[test]
    fn a_reload_that_moves_an_off_screen_cursor_holds_the_stack() {
        let mut h = seated_stack(false, 5);
        let before = stack_screen_text(&mut h);

        reload_file(&mut h, "a.txt", &with_mid_lines(2));

        assert_eq!(stack_screen_text(&mut h), before);
    }

    /// An auto-reload poll holds the stack as a reload does.
    #[test]
    fn a_poll_reload_holds_the_stack() {
        let mut h = seated_stack(false, 5);
        let before = stack_screen_text(&mut h);
        let id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(Path::new("/repo/a.txt"))
            .expect("a.txt is open");
        h.stoat
            .active_workspace_mut()
            .buffers
            .set_auto_reload(id, AutoReloadMode::Tail);
        auto_reload::ensure_auto_reload_poll(&mut h.stoat);

        h.fake_fs()
            .insert_file(Path::new("/repo/a.txt"), blocks(true, 20).as_bytes());
        auto_reload::pump_auto_reload(&mut h.stoat);
        h.run_until_parked();
        auto_reload::pump_auto_reload_install(&mut h.stoat);
        h.settle();
        let _ = h.stoat.render();

        assert_eq!(stack_screen_text(&mut h), before);
    }

    /// A deliberate move after the hold's first paint releases the stack.
    /// Another writer's scroll keeps its row, and a held screen row puts the
    /// cursor on screen row 3.
    #[test]
    fn a_deliberate_move_releases_the_stack() {
        let after = |deliberate: fn(&mut DiffChangesState)| {
            let mut h = seated_stack(true, 5);
            grow_first_file(&mut h);
            with_stack(&mut h, |state, _| deliberate(state));
            let _ = h.stoat.render();
            with_stack(&mut h, |state, editors| {
                let cursor = cursor_stack_row(state, editors).map(|row| row - state.scroll);
                (state.scroll, cursor, state.view_hold.is_some())
            })
        };
        let (scrolled, _, scrolled_held) = after(|state| state.scroll = 10);
        let (_, cursor, held_row_held) = after(|state| state.held_screen_row = Some(3));

        assert_eq!(
            ((scrolled, scrolled_held), (cursor, held_row_held)),
            ((10, false), (Some(3), false)),
        );
    }

    /// A diff map that lands after the reload folds most of the first file
    /// away, and the second file's text stays on its screen rows.
    #[test]
    fn a_refold_after_a_reload_keeps_the_stack_rows() {
        let mut h = seated_stack(true, 5);
        grow_first_file(&mut h);
        let before = stack_screen_text(&mut h);
        let id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(Path::new("/repo/a.txt"))
            .expect("a.txt is open");

        h.stoat
            .active_workspace_mut()
            .install_test_diff_map(id, map_of(&blocks(true, 0), &blocks(true, 20)));
        pump_diff_changes_stack(&mut h.stoat);
        let _ = h.stoat.render();

        assert_eq!(stack_screen_text(&mut h), before);
    }

    /// The reader's next key releases the stack, so a reload never fights a
    /// move the reader makes.
    #[test]
    fn a_key_after_a_reload_releases_the_stack() {
        let mut h = seated_stack(true, 5);
        grow_first_file(&mut h);
        let held = stack_view(&mut h).1;

        h.type_keys("j");

        assert_eq!((held, stack_view(&mut h).1), (true, false));
    }

    /// The first paint after a reload shows the stack as a steady paint does,
    /// since the stack alone places the rows of its excerpts. Here the second
    /// file's header sits four rows down the view, and three lines arrive in
    /// that file between its header and its cursor.
    #[test]
    fn the_first_paint_after_a_reload_matches_a_steady_paint() {
        let mut h = seated_stack(true, 0);
        with_stack(&mut h, |state, _| state.scroll -= 4);
        let _ = h.stoat.render();

        let first = reload_file(&mut h, "b.txt", &with_mid_lines(1));
        h.stoat.pane_cache.clear();

        assert_eq!(first, h.stoat.render());
    }

    /// The paint that applies a reload's hold moves the stack, so it caches no
    /// pane under the scroll it leaves. A move back to that scroll paints the
    /// rows there, as a fresh paint does.
    ///
    /// A steady paint before the reload lets the reload's paint capture, since
    /// a capture skips the second of two frames whose keys moved. The first
    /// file grows by five lines, so it stays out of the view at that scroll
    /// and no reload flash there denies the paint a key.
    #[test]
    fn a_move_back_after_a_reload_paints_the_rows_there() {
        let mut h = seated_stack(true, 10);
        let scroll = stack_view(&mut h).0;
        let _ = h.stoat.render();
        reload_file(&mut h, "a.txt", &blocks(true, 5));

        with_stack(&mut h, |state, _| state.scroll = scroll);
        let moved_back = h.stoat.render();
        h.stoat.pane_cache.clear();

        assert_eq!(moved_back, h.stoat.render());
    }
}
