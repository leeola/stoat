use crate::{
    buffer::BufferId,
    display_map::{
        fold_map,
        inlay_map::{InlayId, InlayKind},
        DiagnosticSeverity,
    },
};
use ratatui::style::{Color, Modifier, Style};
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    ops::{Deref, Range},
    sync::Arc,
};
use stoat_text::{patch::Patch, Anchor, ChunksInRange, Rope};

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct HighlightStyle {
    pub foreground: Option<Color>,
    pub background: Option<Color>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strikethrough: Option<bool>,
}

impl HighlightStyle {
    /// Merge `other` on top of `self`. Non-None fields in `other` override.
    pub fn merge(&mut self, other: &HighlightStyle) {
        if other.foreground.is_some() {
            self.foreground = other.foreground;
        }
        if other.background.is_some() {
            self.background = other.background;
        }
        if other.bold.is_some() {
            self.bold = other.bold;
        }
        if other.italic.is_some() {
            self.italic = other.italic;
        }
        if other.underline.is_some() {
            self.underline = other.underline;
        }
        if other.strikethrough.is_some() {
            self.strikethrough = other.strikethrough;
        }
    }

    pub fn to_ratatui_style(&self) -> Style {
        let mut style = Style::default();
        if let Some(fg) = self.foreground {
            style = style.fg(fg);
        }
        if let Some(bg) = self.background {
            style = style.bg(bg);
        }
        let mut modifiers = Modifier::empty();
        if self.bold == Some(true) {
            modifiers |= Modifier::BOLD;
        }
        if self.italic == Some(true) {
            modifiers |= Modifier::ITALIC;
        }
        if self.underline == Some(true) {
            modifiers |= Modifier::UNDERLINED;
        }
        if self.strikethrough == Some(true) {
            modifiers |= Modifier::CROSSED_OUT;
        }
        if !modifiers.is_empty() {
            style = style.add_modifier(modifiers);
        }
        style
    }
}

/// Precedence layer. Derived `Ord`: lower variant applied first, overridden by later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HighlightLayer {
    ColorizeBracket,
    SyntaxToken,
    SemanticToken,
    LspSemanticToken,
    SearchHighlight,
    DiffHighlight,
    DocumentHighlightRead,
    DocumentHighlightWrite,
    EditPredictionHighlight,
    Editor,
    HoverState,
    SelectionHighlight,
    MatchingBracket,
}

/// Key identifying a single highlight range within the merger.
///
/// The `layer` determines precedence (later layers override earlier ones). The
/// `slot` disambiguates ranges within a layer so overlapping captures at the
/// same layer stack instead of colliding in the active-styles map. For layers
/// with a single style source (e.g. [`HighlightLayer::SearchHighlight`]), use
/// `slot: 0` via [`HighlightKey::layer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HighlightKey {
    pub layer: HighlightLayer,
    pub slot: u32,
}

impl HighlightKey {
    pub const fn new(layer: HighlightLayer, slot: u32) -> Self {
        Self { layer, slot }
    }

    pub const fn layer(layer: HighlightLayer) -> Self {
        Self { layer, slot: 0 }
    }
}

pub type TextHighlights = Arc<HashMap<HighlightKey, Arc<(HighlightStyle, Vec<Range<Anchor>>)>>>;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct HighlightStyleId(u32);

#[derive(Default, Debug)]
pub struct HighlightStyleInterner {
    styles: Vec<HighlightStyle>,
    index: HashMap<HighlightStyle, u32>,
}

impl HighlightStyleInterner {
    pub fn intern(&mut self, style: HighlightStyle) -> HighlightStyleId {
        if let Some(&id) = self.index.get(&style) {
            return HighlightStyleId(id);
        }
        let id = self.styles.len() as u32;
        self.index.insert(style.clone(), id);
        self.styles.push(style);
        HighlightStyleId(id)
    }

    /// Append `style` and return its id, never reusing an existing entry.
    ///
    /// For interners built one entry per fixed table slot, where an id has to
    /// mean the same slot in every interner built that way. Deduping would
    /// break that. Whether two slots collapse onto one id depends on whether
    /// those slots happen to share a style, which differs from theme to theme,
    /// so an id recorded under one theme would index a different slot under the
    /// next.
    pub fn push(&mut self, style: HighlightStyle) -> HighlightStyleId {
        let id = self.styles.len() as u32;
        self.styles.push(style);
        HighlightStyleId(id)
    }
}

impl std::ops::Index<HighlightStyleId> for HighlightStyleInterner {
    type Output = HighlightStyle;
    fn index(&self, id: HighlightStyleId) -> &HighlightStyle {
        &self.styles[id.0 as usize]
    }
}

#[derive(Debug, Clone)]
pub struct SemanticTokenHighlight {
    pub range: Range<Anchor>,
    pub style: HighlightStyleId,
}

/// One parse's tokens as raw byte spans, in the parsed snapshot's coordinates.
///
/// The same tokens as [`SemanticTokenHighlight`] before anchoring. Retained so
/// the next parse can carry them across the buffer's edits and diff them.
/// Anchors cannot stand in here, because an anchor already follows every later
/// edit, and that movement is exactly what the diff needs to observe.
pub type SemanticTokenSpans = Arc<[(Range<usize>, HighlightStyleId)]>;

/// How many tokens one segment of a channel holds.
///
/// Sized so a buffer's tokens land in tens of segments rather than thousands,
/// keeping the per-segment bookkeeping small while still being fine enough
/// that an edit's covers touch only a few segments.
const SEGMENT_TOKENS: usize = 4096;

/// One buffer's semantic tokens for a highlight channel, plus a search index
/// that bounds the per-frame endpoint build to the viewport.
///
/// Tokens are sorted by `range.start` and held in fixed-size segments rather
/// than one whole-file allocation, so a rebuild can replace only the segments
/// an edit reached and let the rest carry by refcount.
///
/// The search index is an argmax over resolved token ends. Because anchor
/// resolution preserves relative order across edits, that argmax never changes
/// once computed, so it stays valid for every later frame without a rebuild.
#[derive(Debug, Clone)]
pub struct BufferSemanticTokens {
    segments: Arc<[TokenSegment]>,
    len: usize,
    pub interner: Arc<HighlightStyleInterner>,
}

/// One contiguous run of a channel's tokens, with each token's resolved end.
///
/// The unit a caller assembles a channel out of. A splice hands over the runs
/// it carried from the previous channel, whose `tokens` is an [`Arc`] clone
/// rather than a fresh allocation, alongside the ones it rebuilt.
pub struct TokenRun {
    pub tokens: Arc<[SemanticTokenHighlight]>,
    /// Resolved end of each token, ordered as a query-time resolver would
    /// order them. A carried run's ends still move with the text, so they are
    /// the caller's to supply either way.
    pub ends: Vec<usize>,
}

/// A run of tokens and the search index over them.
///
/// Segments are not a fixed stride. A rebuild that replaces one segment with a
/// different number of tokens shifts every later token's whole-channel index,
/// so each segment records where it starts rather than having that derived from
/// its position.
#[derive(Debug, Clone)]
struct TokenSegment {
    /// Whole-channel index of this segment's first token.
    base: usize,
    tokens: Arc<[SemanticTokenHighlight]>,
    /// Index *within this segment* of the greatest end among its `0..=i`.
    prefix_max_end: Arc<[u32]>,
    /// Index, in whole-channel terms, of the greatest end among every token in
    /// the segments before this one. [`None`] in the first segment.
    ///
    /// This is what keeps the composed argmax global. A token enclosing the
    /// viewport from an earlier segment is invisible to a segment's own index,
    /// so the search has to consider both.
    carried_max_end: Option<u32>,
}

impl BufferSemanticTokens {
    /// Build the channel, resolving each token's end once for the search index.
    ///
    /// `resolve` need only be order-consistent with the resolver used at query
    /// time. Any buffer snapshot satisfies that, since resolution preserves
    /// relative order.
    ///
    /// Costs one `resolve` call per token. A caller that already holds the
    /// resolved ends, or the byte offsets the anchors were just built from,
    /// should call [`Self::with_resolved_ends`] instead.
    pub fn new(
        tokens: Arc<[SemanticTokenHighlight]>,
        interner: Arc<HighlightStyleInterner>,
        resolve: impl Fn(&Anchor) -> usize,
    ) -> Self {
        let ends: Vec<usize> = tokens
            .iter()
            .map(|token| resolve(&token.range.end))
            .collect();
        Self::with_resolved_ends(tokens, interner, &ends)
    }

    /// Build the channel from token ends the caller already resolved.
    ///
    /// `ends` must hold one entry per token, ordered the way a query-time
    /// resolver would order them. Anchor resolution preserves relative order,
    /// so the byte offsets a batch of anchors was just created from satisfy
    /// that without any anchor being resolved.
    pub fn with_resolved_ends(
        tokens: Arc<[SemanticTokenHighlight]>,
        interner: Arc<HighlightStyleInterner>,
        ends: &[usize],
    ) -> Self {
        debug_assert_eq!(
            tokens.len(),
            ends.len(),
            "ends must have one entry per token",
        );

        let runs: Vec<TokenRun> = (0..tokens.len())
            .step_by(SEGMENT_TOKENS)
            .map(|base| {
                let upto = (base + SEGMENT_TOKENS).min(tokens.len());
                TokenRun {
                    tokens: Arc::from(&tokens[base..upto]),
                    ends: ends[base..upto].to_vec(),
                }
            })
            .collect();

        Self::from_runs(runs, interner)
    }

    /// Build the channel from runs the caller assembled.
    ///
    /// A splice hands over the runs it carried from the previous channel
    /// alongside the ones it rebuilt, so a carried run keeps its allocation and
    /// only takes a new base. Runs arrive in document order and are used as the
    /// segmentation, so a caller that carries most of a large channel pays for
    /// what it replaced rather than for the file.
    pub fn from_runs(runs: Vec<TokenRun>, interner: Arc<HighlightStyleInterner>) -> Self {
        let mut segments: Vec<TokenSegment> = Vec::with_capacity(runs.len());
        let mut base = 0;
        let mut carried: Option<(u32, usize)> = None;

        for run in runs {
            if run.tokens.is_empty() {
                continue;
            }

            debug_assert_eq!(
                run.tokens.len(),
                run.ends.len(),
                "a run's ends must have one entry per token",
            );

            let best = run.ends.iter().enumerate().fold(
                (0usize, 0usize),
                |(best_i, best_end), (i, &end)| {
                    if i == 0 || end > best_end {
                        (i, end)
                    } else {
                        (best_i, best_end)
                    }
                },
            );

            segments.push(TokenSegment {
                base,
                prefix_max_end: Arc::from(prefix_max_end_indices(&run.ends)),
                tokens: run.tokens,
                carried_max_end: carried.map(|(index, _)| index),
            });

            let local = ((base + best.0) as u32, best.1);
            carried = match carried {
                Some((_, end)) if end >= local.1 => carried,
                _ => Some(local),
            };
            base += segments.last().expect("just pushed").tokens.len();
        }

        Self {
            segments: Arc::from(segments),
            len: base,
            interner,
        }
    }

    /// The same tokens resolved through a different interner.
    ///
    /// For a theme switch, where style ids keep their meaning but the styles
    /// behind them change. The search index carries over because it is an
    /// argmax over resolved anchor ends, and re-interning moves no anchor.
    pub fn with_interner(&self, interner: Arc<HighlightStyleInterner>) -> Self {
        Self {
            segments: self.segments.clone(),
            len: self.len,
            interner,
        }
    }

    /// Whether `other` is this channel's own index rather than a rebuild of the
    /// same tokens.
    ///
    /// A channel's segments are shared by refcount, so two editors seeded from
    /// one build answer `true` where each building its own answers `false`.
    pub fn shares_index(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.segments, &other.segments)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Every token in the channel, in start order.
    pub fn iter(&self) -> impl Iterator<Item = &SemanticTokenHighlight> {
        self.segments.iter().flat_map(|s| s.tokens.iter())
    }

    /// The tokens at whole-channel indices `range`, in start order.
    ///
    /// Takes the flat range [`Self::overlap_bounds`] returns and walks it
    /// across however many segments it spans.
    pub fn range(&self, range: Range<usize>) -> impl Iterator<Item = &SemanticTokenHighlight> {
        let end = range.end.min(self.len);
        let start = range.start.min(end);

        // Enter at the segment holding `start` rather than walking to it, so a
        // viewport late in a large file costs its own tokens and no more.
        let first = self.locate(start);

        self.segments[first..]
            .iter()
            .take_while(move |segment| segment.base < end)
            .flat_map(move |segment| {
                let lo = start.saturating_sub(segment.base).min(segment.tokens.len());
                let hi = end.saturating_sub(segment.base).min(segment.tokens.len());
                segment.tokens[lo..hi].iter()
            })
    }

    /// The tokens at whole-channel indices `range`, as one or more runs.
    ///
    /// A segment lying wholly inside `range` is handed over by refcount, which
    /// is what lets a splice keep most of a large channel's allocations. The
    /// partial segments at either end are copied, so a caller carrying a range
    /// that does not align with the segmentation pays only for its edges.
    pub fn carve(&self, range: Range<usize>) -> Vec<Arc<[SemanticTokenHighlight]>> {
        let end = range.end.min(self.len);
        let start = range.start.min(end);
        if start == end {
            return Vec::new();
        }

        self.segments[self.locate(start)..]
            .iter()
            .take_while(|segment| segment.base < end)
            .map(|segment| {
                let lo = start.saturating_sub(segment.base);
                let hi = (end - segment.base).min(segment.tokens.len());
                if lo == 0 && hi == segment.tokens.len() {
                    segment.tokens.clone()
                } else {
                    Arc::from(&segment.tokens[lo..hi])
                }
            })
            .collect()
    }

    /// Which segment holds whole-channel index `index`.
    ///
    /// Segments are ordered by base and do not share one, so the last segment
    /// starting at or before `index` is the one holding it. An index past the
    /// end lands on the final segment, which then yields nothing.
    fn locate(&self, index: usize) -> usize {
        self.segments
            .partition_point(|segment| segment.base <= index)
            .saturating_sub(1)
    }

    /// The token at whole-channel index `index`.
    fn token(&self, index: usize) -> &SemanticTokenHighlight {
        let segment = &self.segments[self.locate(index)];
        &segment.tokens[index - segment.base]
    }

    /// The greatest resolved end among tokens `0..=index`.
    ///
    /// Composed from the containing segment's own argmax and the argmax
    /// carried in from every segment before it, which is what makes the bound
    /// global rather than segment-local.
    fn prefix_max_end(&self, index: usize, resolve: &impl Fn(&Anchor) -> usize) -> usize {
        let segment = &self.segments[self.locate(index)];
        let local = segment.base + segment.prefix_max_end[index - segment.base] as usize;
        let local_end = resolve(&self.token(local).range.end);

        match segment.carried_max_end {
            Some(carried) => local_end.max(resolve(&self.token(carried as usize).range.end)),
            None => local_end,
        }
    }

    /// The half-open whole-channel index range that can overlap `byte_range`.
    ///
    /// Tokens are start-sorted, so a binary search caps the upper bound at the
    /// first token starting at or past `byte_range.end`. The lower bound reads
    /// the argmax index, so a multi-line token enclosing the range from earlier
    /// is kept while tokens ending before it are skipped without resolving them.
    ///
    /// Tokens in the returned range still need a per-token end check. Some end at
    /// or before `byte_range.start` and only ride along under an enclosing token's
    /// max end.
    pub fn overlap_bounds(
        &self,
        byte_range: &Range<usize>,
        resolve: impl Fn(&Anchor) -> usize,
    ) -> Range<usize> {
        let hi = {
            let (mut left, mut right) = (0, self.len);
            while left < right {
                let mid = left + (right - left) / 2;
                if resolve(&self.token(mid).range.start) < byte_range.end {
                    left = mid + 1;
                } else {
                    right = mid;
                }
            }
            left
        };

        let (mut left, mut right) = (0, hi);
        while left < right {
            let mid = left + (right - left) / 2;
            if self.prefix_max_end(mid, &resolve) > byte_range.start {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        left..hi
    }
}

/// The running argmax of `ends`, as the search index
/// [`BufferSemanticTokens::overlap_bounds`] binary-searches.
///
/// Entry `i` is the index of the greatest end among `0..=i`. Ties keep the
/// earlier index. Which index wins a tie is not observable, since both name
/// a token with the same end and the search reads the same bound either way.
fn prefix_max_end_indices(ends: &[usize]) -> Vec<u32> {
    let mut indices = Vec::with_capacity(ends.len());
    let mut max_end = 0;
    let mut max_idx = 0;

    for (i, &end) in ends.iter().enumerate() {
        if i == 0 || end > max_end {
            max_end = end;
            max_idx = i as u32;
        }
        indices.push(max_idx);
    }

    indices
}

pub type SemanticTokensHighlights = Arc<HashMap<BufferId, BufferSemanticTokens>>;

pub type InlayHighlights =
    BTreeMap<HighlightKey, BTreeMap<InlayId, (HighlightStyle, InlayHighlight)>>;

#[derive(Debug, Clone)]
pub struct InlayHighlight {
    pub inlay: InlayId,
    pub range: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChunkRendererId {
    Fold(usize),
    Inlay(InlayId),
}

#[derive(Clone, Debug)]
pub struct ChunkRenderer {
    pub id: ChunkRendererId,
}

#[derive(Clone, Debug)]
pub enum ChunkReplacement {
    Renderer(ChunkRenderer),
    Str(Arc<str>),
}

#[derive(Debug, Clone)]
pub struct HighlightedChunk<'a> {
    pub text: &'a str,
    pub style: Option<HighlightStyle>,
    pub replacement: Option<ChunkReplacement>,
}

#[derive(Clone, Debug)]
pub struct Chunk<'a> {
    pub text: Cow<'a, str>,
    pub highlight_style: Option<HighlightStyle>,
    pub is_tab: bool,
    pub is_inlay: bool,
    pub inlay_kind: Option<InlayKind>,
    pub diagnostic_severity: Option<DiagnosticSeverity>,
    pub renderer: Option<ChunkRenderer>,
}

impl Default for Chunk<'_> {
    fn default() -> Self {
        Self {
            text: Cow::Borrowed(""),
            highlight_style: None,
            is_tab: false,
            is_inlay: false,
            inlay_kind: None,
            diagnostic_severity: None,
            renderer: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Highlights<'a> {
    pub text_highlights: Option<&'a TextHighlights>,
    pub inlay_highlights: Option<&'a InlayHighlights>,
    pub semantic_token_highlights: Option<&'a SemanticTokensHighlights>,
    pub lsp_token_highlights: Option<&'a SemanticTokensHighlights>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightEndpoint {
    offset: usize,
    is_start: bool,
    key: HighlightKey,
    style: Option<HighlightStyle>,
}

impl Ord for HighlightEndpoint {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.offset
            .cmp(&other.offset)
            .then(self.is_start.cmp(&other.is_start))
            .then(self.key.cmp(&other.key))
    }
}

impl PartialOrd for HighlightEndpoint {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// How many endpoints lie between two checkpoints of a [`HighlightEndpoints`],
/// which bounds what a stream replays before its first chunk.
const CHECKPOINT_STEP: usize = 32;

/// The styles in force at one offset, sorted by key, which is the order they
/// merge in.
///
/// A replay holds one to three of them at a time, so a sorted inline vector
/// serves each insert, remove, and copy with no allocation.
type ActiveStyles = SmallVec<[(HighlightKey, HighlightStyle); 4]>;

/// A sorted endpoint list, with the replay state recorded every
/// [`CHECKPOINT_STEP`] endpoints.
///
/// A stream works out the styles in force at its start by replaying the
/// endpoints before it. It resumes from the last checkpoint at or before its
/// start, so it replays at most one step rather than every endpoint before it.
///
/// Many streams open far past the first endpoint of their list. The cache pads
/// its window past the viewport, the inlay and fold layers open a stream per
/// segment between inlays and folds, and a painter that steps row by row opens
/// one per row.
#[derive(Debug)]
pub struct HighlightEndpoints {
    endpoints: Vec<HighlightEndpoint>,
    checkpoints: Vec<Checkpoint>,
}

/// The replay state after the first `ep_idx` endpoints, the last of which lies
/// at `last_offset`.
#[derive(Debug)]
struct Checkpoint {
    last_offset: usize,
    ep_idx: usize,
    active: ActiveStyles,
}

impl HighlightEndpoints {
    /// Records the checkpoints of `endpoints`, which must be sorted by offset.
    ///
    /// The result lives behind an `Arc` because every chunk stream over a frame
    /// reads the same list.
    pub fn shared(endpoints: Vec<HighlightEndpoint>) -> Arc<Self> {
        let mut checkpoints = Vec::with_capacity(endpoints.len() / CHECKPOINT_STEP);
        let mut active = ActiveStyles::new();
        for (index, endpoint) in endpoints.iter().enumerate() {
            if index > 0 && index % CHECKPOINT_STEP == 0 {
                checkpoints.push(Checkpoint {
                    last_offset: endpoints[index - 1].offset,
                    ep_idx: index,
                    active: active.clone(),
                });
            }
            replay(&mut active, endpoint);
        }

        Arc::new(Self {
            endpoints,
            checkpoints,
        })
    }

    /// The last checkpoint whose replayed endpoints all lie at or before
    /// `offset`, or `None` when the first checkpoint lies past it.
    fn resume_at(&self, offset: usize) -> Option<&Checkpoint> {
        let index = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.last_offset <= offset);
        index.checked_sub(1).map(|index| &self.checkpoints[index])
    }
}

impl Deref for HighlightEndpoints {
    type Target = [HighlightEndpoint];

    fn deref(&self) -> &[HighlightEndpoint] {
        &self.endpoints
    }
}

/// Apply `endpoint` to the styles in force.
fn replay(active: &mut ActiveStyles, endpoint: &HighlightEndpoint) {
    let slot = active.binary_search_by_key(&endpoint.key, |(key, _)| *key);
    match (&endpoint.style, slot) {
        (Some(style), Ok(index)) => active[index].1 = style.clone(),
        (Some(style), Err(index)) => active.insert(index, (endpoint.key, style.clone())),
        (None, Ok(index)) => {
            active.remove(index);
        },
        (None, Err(_)) => {},
    }
}

/// A viewport's endpoints plus one replay over them, for a painter working row
/// by row.
///
/// The review, diff and conflict views paint other things between rows, so they
/// cannot hold one chunk stream open across the viewport the way the plain
/// editor body does. Opening a stream per row instead makes each row re-derive
/// its opening styles from the nearest checkpoint. Holding this across the loop
/// replays each endpoint once.
///
/// `plain` records whether display offsets below the block layer are buffer
/// offsets. They are not once anything is folded or an inlay is shown, and a
/// replay measured in one coordinate says nothing about the other, so no seed is
/// offered at all in that case and each row resumes from a checkpoint instead.
pub struct RowHighlightCursor {
    pub(crate) endpoints: Arc<HighlightEndpoints>,
    cursor: HighlightCursor,
    plain: bool,
}

impl RowHighlightCursor {
    pub fn new(endpoints: Arc<HighlightEndpoints>, plain: bool) -> Self {
        Self {
            endpoints,
            cursor: HighlightCursor::default(),
            plain,
        }
    }

    /// Advance the replay to `offset` and hand it over as a seed.
    ///
    /// `None` when there is no offset to advance to, which is a block row, or
    /// when the coordinates do not line up.
    pub(crate) fn seed_at(&mut self, offset: Option<usize>) -> Option<&HighlightCursor> {
        if !self.plain {
            return None;
        }
        let offset = offset?;
        self.cursor.advance_to(offset, &self.endpoints);
        Some(&self.cursor)
    }
}

/// The counters a cached endpoint list is checked against, beside the `Arc`
/// pointers and the range.
///
/// Both exist because a pointer comparison answers a narrower question than it
/// looks like it does, each in its own direction.
///
/// `buffer` is the buffer content version. An in-place edit shifts the anchors
/// the endpoints resolved from without necessarily swapping the highlight
/// `Arc`s, so the pointers alone leave stale offsets in place.
///
/// `settings` is the display map's settings generation. The highlight setters
/// install through `Arc::make_mut`, which allocates only while a second
/// reference exists, so an install that finds none mutates in place and leaves
/// every pointer equal to what the cache stored. Every install moves the
/// generation, whatever the refcount did, except a tree-sitter install that
/// paints exactly what was installed. That one moves nothing, and the cache
/// outlives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightVersions {
    pub buffer: u64,
    pub settings: u64,
}

/// The most a scroll pads its endpoint window past each end of the requested
/// range. It bounds a padded build when the range itself is long, as a long
/// line that does not wrap makes it.
const MAX_PAD: usize = 16 * 1024;

/// Visible-range highlight endpoints memoized across repaints.
#[derive(Clone, Debug)]
pub struct CachedHighlightEndpoints {
    pub(super) versions: HighlightVersions,
    text_ptr: usize,
    lsp_ptr: Option<usize>,
    /// The bytes the endpoints cover. They hold the range the endpoints were
    /// built for and, when a scroll built them, a pad past each end.
    range: Range<usize>,
    endpoints: Arc<HighlightEndpoints>,
    /// Whether the window holds a highlight whose range an edit emptied, which
    /// a build skips and a carry drops.
    ///
    /// An undo that brings the text back resolves that highlight's anchors to
    /// a range again, with no pair for it in the window. A rebuild is what
    /// brings the pair back, and the next tree-sitter install rebuilds unless
    /// it paints exactly what was installed. See [`Self::forget_if_collapsed`].
    collapsed: bool,
}

impl CachedHighlightEndpoints {
    /// Whether the endpoints came from these versions and highlight sets.
    ///
    /// The tree-sitter channels are checked by the settings generation in
    /// `versions` alone, so an install that moves no generation keeps the
    /// cache although it swapped the channel's `Arc`.
    fn current(
        &self,
        versions: HighlightVersions,
        highlights: &TextHighlights,
        lsp: Option<&SemanticTokensHighlights>,
    ) -> bool {
        self.versions == versions
            && self.text_ptr == Arc::as_ptr(highlights) as usize
            && self.lsp_ptr == lsp.map(|s| Arc::as_ptr(s) as usize)
    }

    pub fn endpoints(&self) -> &[HighlightEndpoint] {
        &self.endpoints
    }

    /// Drop `cache` when its window holds a highlight an edit emptied.
    ///
    /// Call this before a tree-sitter install that moves no generation. Such
    /// an install keeps the cache, and a collapsed window then misses the pair
    /// for any text an undo brings back until something else rebuilds it.
    pub fn forget_if_collapsed(cache: &mut Option<Self>) {
        if cache.as_ref().is_some_and(|cached| cached.collapsed) {
            *cache = None;
        }
    }
}

/// How a caller turns anchors into byte offsets.
///
/// Two forms because the bounds searches probe one anchor at a time by nature,
/// while everything between the bounds is known before any of it is needed. A
/// batch walks the anchor trees once instead of descending per anchor, so the
/// split is what lets each half be answered the cheaper way.
pub struct AnchorResolver<'a> {
    pub one: &'a dyn Fn(&Anchor) -> usize,
    pub many: &'a dyn Fn(&[Anchor]) -> Vec<usize>,
}

/// The endpoints for `range`, answered from `cache` while its window holds the
/// range and its versions and highlight sets are current.
///
/// A cache that only the buffer moved past is first carried across `edits`,
/// the patch since its buffer version, so the frame that shows a typed
/// character builds no endpoints. The carry declines when a highlight set
/// moved or an edit reaches the window's edge, and the miss builds as below.
///
/// A padded window costs a few exact builds, so only a scroll builds one. A
/// scroll is a miss on current versions whose range overlaps the window. Its
/// window reaches half the range's length past each end, up to [`MAX_PAD`], so
/// the scrolls after it stay inside the window until the viewport moves half
/// its length.
///
/// Any other miss builds the exact range. A jump gives no sign that a scroll
/// follows, and a change to the highlights arrives on every key.
#[allow(clippy::too_many_arguments)]
pub fn create_highlight_endpoints_cached(
    versions: HighlightVersions,
    range: &Range<usize>,
    highlights: &TextHighlights,
    semantic_highlights: Option<&SemanticTokensHighlights>,
    lsp_highlights: Option<&SemanticTokensHighlights>,
    resolver: &AnchorResolver<'_>,
    edits: Option<&Patch<usize>>,
    cache: &mut Option<CachedHighlightEndpoints>,
) -> Arc<HighlightEndpoints> {
    if let Some(edits) = edits {
        carry_cache(
            cache,
            versions,
            highlights,
            semantic_highlights,
            lsp_highlights,
            resolver,
            edits,
        );
    }

    let current = cache
        .as_ref()
        .filter(|cached| cached.current(versions, highlights, lsp_highlights));
    if let Some(cached) = current
        && cached.range.start <= range.start
        && range.end <= cached.range.end
    {
        return cached.endpoints.clone();
    }

    let scroll = current
        .is_some_and(|cached| range.start < cached.range.end && cached.range.start < range.end);
    let window = match scroll {
        true => {
            let pad = (range.len() / 2).min(MAX_PAD);
            range.start.saturating_sub(pad)..range.end.saturating_add(pad)
        },
        false => range.clone(),
    };
    let (endpoints, collapsed) = build_highlight_endpoints(
        &window,
        highlights,
        semantic_highlights,
        lsp_highlights,
        resolver,
    );
    let arc = HighlightEndpoints::shared(endpoints);
    *cache = Some(CachedHighlightEndpoints {
        versions,
        text_ptr: Arc::as_ptr(highlights) as usize,
        lsp_ptr: lsp_highlights.map(|s| Arc::as_ptr(s) as usize),
        range: window,
        endpoints: arc.clone(),
        collapsed,
    });
    arc
}

/// Bring `cache` to `versions` across `edits` when the buffer is all that moved
/// since the cache was built.
///
/// Leaves the cache stale, for the hit test to refuse, when a highlight set
/// moved, when an edit touches either end of the window, or when an edit
/// touches an endpoint whose anchor no token names. An edit at the window's
/// edge moves ranges across that edge, which only a build sees.
///
/// Text that an undo brings back holds the anchors it had before its removal.
/// A pair that left the window when the text went does not come back with it,
/// so it paints unstyled until the window is built again. The carry marks such
/// a cache collapsed, and the next tree-sitter install drops a collapsed cache
/// whatever that install restyled.
fn carry_cache(
    cache: &mut Option<CachedHighlightEndpoints>,
    versions: HighlightVersions,
    highlights: &TextHighlights,
    semantic: Option<&SemanticTokensHighlights>,
    lsp: Option<&SemanticTokensHighlights>,
    resolver: &AnchorResolver<'_>,
    edits: &Patch<usize>,
) {
    let Some(cached) = cache.as_mut() else {
        return;
    };
    let only_the_buffer_moved = cached.versions.buffer < versions.buffer
        && cached.current(
            HighlightVersions {
                buffer: cached.versions.buffer,
                ..versions
            },
            highlights,
            lsp,
        );
    // A text highlight on a token layer has a slot that names no token.
    let slots_name_tokens = !highlights.keys().any(|key| {
        matches!(
            key.layer,
            HighlightLayer::SemanticToken | HighlightLayer::LspSemanticToken
        )
    });
    let window = cached.range.clone();
    let inside_or_apart = edits.edits().iter().all(|edit| {
        edit.old.end < window.start
            || window.end < edit.old.start
            || (window.start < edit.old.start && edit.old.end < window.end)
    });
    if !(only_the_buffer_moved && slots_name_tokens && inside_or_apart) {
        return;
    }
    let Some((endpoints, emptied)) =
        carry_endpoints(&cached.endpoints, edits, semantic, lsp, resolver)
    else {
        return;
    };

    let mut bounds = [window.start, window.end];
    fold_map::carry_offsets(&mut bounds, &mut [false; 2], edits);
    cached.versions = versions;
    cached.range = bounds[0]..bounds[1];
    cached.endpoints = HighlightEndpoints::shared(endpoints);
    cached.collapsed |= emptied;
}

/// `endpoints` as they lie after `edits`, and whether the edits emptied a
/// pair, or `None` when an edit touches an endpoint whose anchor no token
/// names.
///
/// An endpoint that no edit touches shifts by the edits before it. One that an
/// edit touches is resolved from its anchor, because its bias decides which
/// side of new text it falls on, and the buffer puts new text ahead of the
/// fragments deleted where it lands. A pair the edits left empty goes, the way
/// a fresh build skips an empty range.
fn carry_endpoints(
    endpoints: &[HighlightEndpoint],
    edits: &Patch<usize>,
    semantic: Option<&SemanticTokensHighlights>,
    lsp: Option<&SemanticTokensHighlights>,
    resolver: &AnchorResolver<'_>,
) -> Option<(Vec<HighlightEndpoint>, bool)> {
    let mut offsets: Vec<usize> = endpoints.iter().map(|endpoint| endpoint.offset).collect();
    let mut flags = vec![false; offsets.len()];
    fold_map::carry_offsets(&mut offsets, &mut flags, edits);

    let touched: Vec<usize> = flags
        .iter()
        .enumerate()
        .filter_map(|(index, &flag)| flag.then_some(index))
        .collect();
    let anchors = touched
        .iter()
        .map(|&index| endpoint_anchor(&endpoints[index], semantic, lsp))
        .collect::<Option<Vec<Anchor>>>()?;
    for (&index, offset) in touched.iter().zip((resolver.many)(&anchors)) {
        offsets[index] = offset;
    }

    // An edit empties a pair only by reaching across all of it, so both of its
    // endpoints are among the touched ones.
    let touched_starts: HashMap<HighlightKey, usize> = touched
        .iter()
        .filter(|&&index| endpoints[index].is_start)
        .map(|&index| (endpoints[index].key, offsets[index]))
        .collect();
    let emptied: HashSet<HighlightKey> = touched
        .iter()
        .filter(|&&index| {
            !endpoints[index].is_start
                && touched_starts.get(&endpoints[index].key) == Some(&offsets[index])
        })
        .map(|&index| endpoints[index].key)
        .collect();

    let mut carried: Vec<HighlightEndpoint> = endpoints
        .iter()
        .zip(offsets)
        .filter(|(endpoint, _)| !emptied.contains(&endpoint.key))
        .map(|(endpoint, offset)| HighlightEndpoint {
            offset,
            ..endpoint.clone()
        })
        .collect();
    if touched.is_empty() {
        debug_assert!(
            carried.is_sorted(),
            "a carry that resolves nothing keeps the order"
        );
    } else {
        carried.sort();
    }
    Some((carried, !emptied.is_empty()))
}

/// The anchor `endpoint` was resolved from, when its slot names one.
///
/// A token endpoint's slot is the token's index in its channel, which names the
/// token only while its layer holds one channel, as a singleton buffer's does.
/// A text highlight endpoint names no anchor.
fn endpoint_anchor(
    endpoint: &HighlightEndpoint,
    semantic: Option<&SemanticTokensHighlights>,
    lsp: Option<&SemanticTokensHighlights>,
) -> Option<Anchor> {
    let channels = match endpoint.key.layer {
        HighlightLayer::SemanticToken => semantic?,
        HighlightLayer::LspSemanticToken => lsp?,
        _ => return None,
    };
    let mut values = channels.values();
    let (Some(channel), None) = (values.next(), values.next()) else {
        return None;
    };

    let index = endpoint.key.slot as usize;
    (index < channel.len()).then(|| {
        let range = &channel.token(index).range;
        if endpoint.is_start {
            range.start
        } else {
            range.end
        }
    })
}

/// Build the endpoint list for `range`.
///
/// `resolve` answers the bounds searches, which probe one anchor at a time by
/// nature. `resolve_batch` answers everything between those bounds, where the
/// whole set is known before any of it is needed and one walk of the anchor
/// trees beats a descent per anchor.
pub fn create_highlight_endpoints(
    range: &Range<usize>,
    highlights: &TextHighlights,
    semantic_highlights: Option<&SemanticTokensHighlights>,
    lsp_highlights: Option<&SemanticTokensHighlights>,
    resolver: &AnchorResolver<'_>,
) -> Vec<HighlightEndpoint> {
    build_highlight_endpoints(
        range,
        highlights,
        semantic_highlights,
        lsp_highlights,
        resolver,
    )
    .0
}

/// [`create_highlight_endpoints`], and whether the build skipped a highlight
/// whose range resolved empty, which the cache records as collapsed.
fn build_highlight_endpoints(
    range: &Range<usize>,
    highlights: &TextHighlights,
    semantic_highlights: Option<&SemanticTokensHighlights>,
    lsp_highlights: Option<&SemanticTokensHighlights>,
    resolver: &AnchorResolver<'_>,
) -> (Vec<HighlightEndpoint>, bool) {
    let mut endpoints = Vec::new();
    let mut collapsed = false;

    for (&key, hl) in highlights.iter() {
        let style = &hl.0;
        let ranges = &hl.1;

        let start_ix = ranges
            .binary_search_by(|probe| {
                (resolver.one)(&probe.end)
                    .cmp(&range.start)
                    .then(std::cmp::Ordering::Less)
            })
            .unwrap_or_else(|i| i);
        // The walk used to stop at the first range starting past the viewport,
        // which meant resolving to learn where to stop. Finding that bound up
        // front is the same sortedness the stop assumed, and it is what lets
        // the anchors between the bounds go out together.
        let end_ix = start_ix
            + ranges[start_ix..].partition_point(|probe| (resolver.one)(&probe.start) < range.end);

        let bounded = &ranges[start_ix..end_ix];
        for (s, e) in resolve_pairs(bounded.iter().map(|r| (&r.start, &r.end)), resolver) {
            if s == e {
                collapsed = true;
                continue;
            }
            endpoints.push(HighlightEndpoint {
                offset: s,
                is_start: true,
                key,
                style: Some(style.clone()),
            });
            endpoints.push(HighlightEndpoint {
                offset: e,
                is_start: false,
                key,
                style: None,
            });
        }
    }

    if let Some(semantic) = semantic_highlights {
        collapsed |= push_semantic_endpoints(
            &mut endpoints,
            semantic,
            HighlightLayer::SemanticToken,
            range,
            resolver,
        );
    }

    // LSP semantic tokens ride a higher layer than the tree-sitter tokens above,
    // so their styles merge on top of the syntactic baseline per slot.
    if let Some(lsp) = lsp_highlights {
        collapsed |= push_semantic_endpoints(
            &mut endpoints,
            lsp,
            HighlightLayer::LspSemanticToken,
            range,
            resolver,
        );
    }

    endpoints.sort();
    (endpoints, collapsed)
}

/// Resolve both ends of each range, in order.
///
/// One call rather than two because the resolver's cost is walking its cursors,
/// and a single walk over twice the anchors beats two walks over half each.
fn resolve_pairs<'a>(
    ranges: impl Iterator<Item = (&'a Anchor, &'a Anchor)>,
    resolver: &AnchorResolver<'_>,
) -> Vec<(usize, usize)> {
    let (mut anchors, mut ends): (Vec<Anchor>, Vec<Anchor>) = ranges.map(|(s, e)| (*s, *e)).unzip();
    let count = anchors.len();
    anchors.append(&mut ends);

    let offsets = (resolver.many)(&anchors);
    let (starts, ends) = offsets.split_at(count);
    starts.iter().copied().zip(ends.iter().copied()).collect()
}

/// Emit start/end endpoints for one semantic-token channel at `layer`, bounding
/// the walk to the tokens that can overlap `range`.
///
/// Tokens are start-sorted, so a `partition_point` caps the walk at the first
/// token starting past the viewport. The lower bound reads the channel's
/// `prefix_max_end` index, so an enclosing token whose nested children end
/// before the viewport is still emitted while tokens that end before it are
/// skipped without resolving them. Each token keeps a unique slot so nested
/// captures (e.g. an escape inside a string) occupy distinct entries of the
/// merger's active map.
///
/// Returns whether the walk skipped a token whose range resolved empty.
fn push_semantic_endpoints(
    endpoints: &mut Vec<HighlightEndpoint>,
    semantic: &SemanticTokensHighlights,
    layer: HighlightLayer,
    range: &Range<usize>,
    resolver: &AnchorResolver<'_>,
) -> bool {
    let mut collapsed = false;
    for channel in semantic.values() {
        let bounds = channel.overlap_bounds(range, resolver.one);
        let lo = bounds.start;

        let tokens: Vec<&SemanticTokenHighlight> = channel.range(bounds).collect();
        let resolved = resolve_pairs(
            tokens.iter().map(|t| (&t.range.start, &t.range.end)),
            resolver,
        );

        for (offset, (token, (s, e))) in tokens.iter().zip(resolved).enumerate() {
            if s == e {
                collapsed = true;
                continue;
            }
            if e <= range.start {
                continue;
            }
            let key = HighlightKey::new(layer, (lo + offset) as u32);
            endpoints.push(HighlightEndpoint {
                offset: s,
                is_start: true,
                key,
                style: Some(channel.interner[token.style].clone()),
            });
            endpoints.push(HighlightEndpoint {
                offset: e,
                is_start: false,
                key,
                style: None,
            });
        }
    }
    collapsed
}

/// Iterate text chunks with merged highlight styles applied.
pub fn highlighted_chunks<'a>(
    text: &'a str,
    text_offset: usize,
    endpoints: &'a [HighlightEndpoint],
) -> impl Iterator<Item = HighlightedChunk<'a>> {
    let mut active = ActiveStyles::new();
    let mut ep_idx = 0;
    let mut pos = text_offset;
    let mut remaining = text;

    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }

        // Process all endpoints at the current position
        while ep_idx < endpoints.len() && endpoints[ep_idx].offset <= pos {
            replay(&mut active, &endpoints[ep_idx]);
            ep_idx += 1;
        }

        // Find next boundary
        let next_boundary = if ep_idx < endpoints.len() {
            endpoints[ep_idx].offset - pos
        } else {
            remaining.len()
        };
        let raw_split = next_boundary.min(remaining.len());
        // Snap to a UTF-8 char boundary; see [`BufferChunks::next`] for the
        // rationale. No-op for tree-sitter-derived endpoints.
        let split_at = remaining.ceil_char_boundary(raw_split);

        let chunk_text = &remaining[..split_at];
        remaining = &remaining[split_at..];
        pos += split_at;

        let merged_style = if active.is_empty() {
            None
        } else {
            let mut merged = HighlightStyle::default();
            for (_, style) in &active {
                merged.merge(style);
            }
            Some(merged)
        };

        Some(HighlightedChunk {
            text: chunk_text,
            style: merged_style,
            replacement: None,
        })
    })
}

/// Streaming chunk iterator over a rope segment that merges in highlight
/// styles on the fly.
///
/// Holds a single [`ChunksInRange`] cursor over the rope and an `Arc`-shared
/// [`HighlightEndpoints`] list. Emits [`Chunk`]s without any per-chunk heap
/// allocation: `text` is always a borrow of the rope's own chunk storage, the
/// endpoints vector is built once by the caller, and only a short sorted list
/// of active styles, held inline, is carried across calls.
///
/// This is the bottom layer of the display map chunks pipeline. Higher layers
/// ([`super::inlay_map::InlaySnapshot::chunks`], [`super::fold_map::FoldSnapshot::chunks`],
/// etc.) wrap a `BufferChunks` and transform the chunk stream.
pub struct BufferChunks<'a> {
    text_chunks: ChunksInRange<'a>,
    pending: &'a str,
    offset: usize,
    end: usize,
    endpoints: Arc<HighlightEndpoints>,
    ep_idx: usize,
    active: ActiveStyles,
}

/// How far an endpoint replay has got, and what it left active.
///
/// A [`BufferChunks`] works out which styles apply at its range start by
/// replaying every endpoint at or below that offset. A caller opening one stream
/// per row repeats that replay per row, each time from the beginning of the
/// endpoint list. Keeping one of these across the rows instead, advanced to each
/// row's start, replays each endpoint once for the whole run.
///
/// The offset it describes travels with it because the active set only means
/// anything at that offset. A stream that starts elsewhere must not be seeded
/// with it.
#[derive(Clone, Default)]
pub struct HighlightCursor {
    offset: usize,
    ep_idx: usize,
    active: ActiveStyles,
}

impl HighlightCursor {
    /// Replay `endpoints` forward until the cursor describes `offset`.
    ///
    /// A move past a checkpoint resumes from that checkpoint, so a long move
    /// replays at most one step.
    ///
    /// Only moves forward. An offset behind the cursor leaves it where it is,
    /// because the active set is accumulated and cannot be unwound, and a stream
    /// seeded from an unmoved cursor renders correctly anyway. The seed no
    /// longer describes the range being opened, so it is ignored and that range
    /// replays for itself.
    ///
    /// Going backwards is ordinary rather than a misuse. A caller stepping down
    /// display rows steps back whenever a row's chunks come from a run that
    /// started above it, as a wrapped line's continuation rows do.
    pub fn advance_to(&mut self, offset: usize, endpoints: &HighlightEndpoints) {
        if offset < self.offset {
            return;
        }
        if let Some(checkpoint) = endpoints.resume_at(offset)
            && checkpoint.ep_idx > self.ep_idx
        {
            self.ep_idx = checkpoint.ep_idx;
            self.active = checkpoint.active.clone();
        }
        while self.ep_idx < endpoints.len() && endpoints[self.ep_idx].offset <= offset {
            replay(&mut self.active, &endpoints[self.ep_idx]);
            self.ep_idx += 1;
        }
        self.offset = offset;
    }
}

impl<'a> BufferChunks<'a> {
    /// Construct a new iterator over `rope[range]` applying `endpoints`.
    ///
    /// `endpoints` must be sorted by offset. Use [`create_highlight_endpoints`]
    /// (or the cached variant) to build them.
    pub fn new(rope: &'a Rope, range: Range<usize>, endpoints: Arc<HighlightEndpoints>) -> Self {
        Self::with_seed(rope, range, endpoints, None)
    }

    /// Like [`Self::new`], starting from a replay `seed` already advanced to the
    /// range's start.
    ///
    /// Saves re-deriving the active styles from the nearest checkpoint below
    /// `range.start`, which is worth doing for a caller opening a stream per row
    /// over one shared endpoint list.
    ///
    /// A seed describing any other offset is ignored and the replay runs as
    /// usual, so a caller that gets the pairing wrong loses the shortcut rather
    /// than the styling.
    pub fn with_seed(
        rope: &'a Rope,
        range: Range<usize>,
        endpoints: Arc<HighlightEndpoints>,
        seed: Option<&HighlightCursor>,
    ) -> Self {
        let start = range.start;
        let end = range.end;
        let (ep_idx, active) = match seed.filter(|cursor| cursor.offset == start) {
            Some(cursor) => (cursor.ep_idx, cursor.active.clone()),
            None => match endpoints.resume_at(start) {
                Some(checkpoint) => (checkpoint.ep_idx, checkpoint.active.clone()),
                None => (0, ActiveStyles::new()),
            },
        };
        Self {
            text_chunks: rope.chunks_in_range(range),
            pending: "",
            offset: start,
            end,
            endpoints,
            ep_idx,
            active,
        }
    }

    fn merged_style(&self) -> Option<HighlightStyle> {
        if self.active.is_empty() {
            return None;
        }
        let mut merged = HighlightStyle::default();
        for (_, style) in &self.active {
            merged.merge(style);
        }
        Some(merged)
    }
}

impl<'a> Iterator for BufferChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        if self.offset >= self.end {
            return None;
        }
        while self.pending.is_empty() {
            self.pending = self.text_chunks.next()?;
        }

        while self.ep_idx < self.endpoints.len()
            && self.endpoints[self.ep_idx].offset <= self.offset
        {
            replay(&mut self.active, &self.endpoints[self.ep_idx]);
            self.ep_idx += 1;
        }

        let next_ep_offset = if self.ep_idx < self.endpoints.len() {
            self.endpoints[self.ep_idx].offset
        } else {
            usize::MAX
        };
        let raw_split = self
            .pending
            .len()
            .min(next_ep_offset.saturating_sub(self.offset))
            .min(self.end.saturating_sub(self.offset));
        // Tree-sitter byte ranges always align to UTF-8 boundaries, so this
        // ceil is a no-op for the common case. Defensive against any future
        // endpoint source that lands mid-codepoint: rounding up guarantees
        // forward progress and that `split_at` never panics.
        let split = self.pending.ceil_char_boundary(raw_split);
        let (emit, rest) = self.pending.split_at(split);
        self.pending = rest;
        self.offset += split;

        Some(Chunk {
            text: Cow::Borrowed(emit),
            highlight_style: self.merged_style(),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        create_highlight_endpoints, create_highlight_endpoints_cached, highlighted_chunks,
        AnchorResolver, BufferChunks, CachedHighlightEndpoints, Chunk, HighlightCursor,
        HighlightEndpoint, HighlightEndpoints, HighlightKey, HighlightLayer, HighlightStyle,
        HighlightVersions, TextHighlights, MAX_PAD,
    };
    use ratatui::style::Color;
    use std::{collections::HashMap, iter, ops::Range, sync::Arc};
    use stoat_text::{Anchor, Bias, Rope};

    const VERSIONS: HighlightVersions = HighlightVersions {
        buffer: 1,
        settings: 1,
    };

    /// Endpoints turning three overlapping styles on and off across the text,
    /// so a replay stopped part-way holds more than one active entry.
    fn layered_endpoints() -> Arc<HighlightEndpoints> {
        endpoints_of([
            (
                HighlightKey::layer(HighlightLayer::SearchHighlight),
                colored(Color::Red),
                2..30,
            ),
            (
                HighlightKey::layer(HighlightLayer::SelectionHighlight),
                colored(Color::Green),
                8..22,
            ),
            (
                HighlightKey::layer(HighlightLayer::MatchingBracket),
                colored(Color::Blue),
                14..40,
            ),
        ])
    }

    fn colored(color: Color) -> HighlightStyle {
        HighlightStyle {
            foreground: Some(color),
            ..Default::default()
        }
    }

    /// The sorted endpoints that open and close each span.
    fn endpoints_of(
        spans: impl IntoIterator<Item = (HighlightKey, HighlightStyle, Range<usize>)>,
    ) -> Arc<HighlightEndpoints> {
        let mut endpoints = Vec::new();
        for (key, style, range) in spans {
            endpoints.push(HighlightEndpoint {
                offset: range.start,
                is_start: true,
                key,
                style: Some(style),
            });
            endpoints.push(HighlightEndpoint {
                offset: range.end,
                is_start: false,
                key,
                style: None,
            });
        }
        endpoints.sort();
        HighlightEndpoints::shared(endpoints)
    }

    fn styled_text(chunks: BufferChunks<'_>) -> Vec<(String, Option<HighlightStyle>)> {
        chunks
            .map(|c| (c.text.into_owned(), c.highlight_style))
            .collect()
    }

    /// A stream seeded from a carried replay must render exactly what one that
    /// replayed the endpoints itself renders.
    ///
    /// The seed exists only to skip that walk, so any difference between the two
    /// is a colouring bug, and one that reads as plausible output rather than as
    /// a crash. Every endpoint offset is checked, plus the positions either side
    /// of each, because the replay's boundary condition is `<=` and an off-by-one
    /// there changes which styles a row opens with.
    #[test]
    fn a_seeded_stream_renders_what_an_unseeded_one_does() {
        let rope = Rope::from("a".repeat(48).as_str());
        let endpoints = layered_endpoints();

        let mut offsets: Vec<usize> = endpoints.iter().map(|e| e.offset).collect();
        offsets.extend(offsets.clone().iter().filter_map(|o| o.checked_sub(1)));
        offsets.extend(offsets.clone().iter().map(|o| o + 1));
        offsets.push(0);
        offsets.push(48);
        offsets.sort_unstable();
        offsets.dedup();

        let mut cursor = HighlightCursor::default();
        for start in offsets {
            cursor.advance_to(start, &endpoints);
            let seeded =
                BufferChunks::with_seed(&rope, start..rope.len(), endpoints.clone(), Some(&cursor));
            let plain = BufferChunks::new(&rope, start..rope.len(), endpoints.clone());
            assert_eq!(
                styled_text(seeded),
                styled_text(plain),
                "seeded at {start} diverged from replaying from the start",
            );
        }
    }

    /// A seed describing some other offset is ignored rather than believed.
    ///
    /// It is the guard that keeps a mis-paired seed from silently styling a row
    /// with whatever was active somewhere else.
    #[test]
    fn a_seed_for_a_different_offset_is_ignored() {
        let rope = Rope::from("a".repeat(48).as_str());
        let endpoints = layered_endpoints();

        let mut elsewhere = HighlightCursor::default();
        elsewhere.advance_to(20, &endpoints);

        assert_eq!(
            styled_text(BufferChunks::with_seed(
                &rope,
                4..rope.len(),
                endpoints.clone(),
                Some(&elsewhere),
            )),
            styled_text(BufferChunks::new(&rope, 4..rope.len(), endpoints)),
            "the mismatched seed was used instead of being replayed past",
        );
    }

    /// Advancing in steps has to land where advancing in one go lands, since
    /// the caller advances row by row and never in one jump.
    #[test]
    fn advancing_in_steps_matches_advancing_at_once() {
        let rope = Rope::from("a".repeat(48).as_str());
        let endpoints = layered_endpoints();

        let mut stepped = HighlightCursor::default();
        for offset in [3, 9, 15, 21, 33] {
            stepped.advance_to(offset, &endpoints);
        }
        let mut at_once = HighlightCursor::default();
        at_once.advance_to(33, &endpoints);

        let render = |cursor: &HighlightCursor| {
            styled_text(BufferChunks::with_seed(
                &rope,
                33..rope.len(),
                endpoints.clone(),
                Some(cursor),
            ))
        };
        assert_eq!(render(&stepped), render(&at_once), "the walk lost state");
    }

    /// A style that opens inside one on a higher layer merges under it, so
    /// precedence follows the layer and not the order the styles opened in.
    #[test]
    fn a_lower_layer_opened_later_merges_under_a_higher_one() {
        let endpoints = endpoints_of([
            (
                HighlightKey::layer(HighlightLayer::SelectionHighlight),
                colored(Color::Green),
                0..10,
            ),
            (
                HighlightKey::new(HighlightLayer::SyntaxToken, 0),
                colored(Color::Red),
                2..6,
            ),
        ]);
        let rope = Rope::from("0123456789");

        assert_eq!(
            styled_text(BufferChunks::new(&rope, 0..10, endpoints)),
            ["01", "2345", "6789"]
                .map(|text| (text.to_string(), Some(colored(Color::Green))))
                .to_vec(),
            "the selection wins over the syntax token inside it",
        );
    }

    /// A text with endpoints for several checkpoints, and the style each byte
    /// takes from a replay of them from the first endpoint.
    ///
    /// Forty spans open on one offset, so a checkpoint lands inside a run of
    /// equal offsets. That is where a resume one endpoint early or late shows.
    fn checkpointed_fixture() -> (Rope, Arc<HighlightEndpoints>, Vec<Option<HighlightStyle>>) {
        let red = (0..80).map(|i| {
            let key = HighlightKey::layer(HighlightLayer::SearchHighlight);
            (key, colored(Color::Red), i * 7..i * 7 + 4)
        });
        let green = (0..50).map(|i| {
            let key = HighlightKey::layer(HighlightLayer::SelectionHighlight);
            (key, colored(Color::Green), i * 11 + 3..i * 11 + 9)
        });
        let nested = (0..40u8).map(|slot| {
            let key = HighlightKey::new(HighlightLayer::SemanticToken, u32::from(slot));
            (
                key,
                colored(Color::Indexed(slot)),
                200..210 + usize::from(slot),
            )
        });
        let endpoints = endpoints_of(red.chain(green).chain(nested));
        assert!(
            endpoints
                .checkpoints
                .iter()
                .any(|checkpoint| endpoints[checkpoint.ep_idx].offset == checkpoint.last_offset),
            "no checkpoint lands inside a run of equal offsets",
        );

        let text = "a".repeat(600);
        let replayed = byte_styles(
            highlighted_chunks(&text, 0, &endpoints)
                .map(|chunk| (chunk.text.to_owned(), chunk.style)),
        );
        (Rope::from(text.as_str()), endpoints, replayed)
    }

    /// The style each byte renders with, so streams that split their chunks at
    /// different places compare equal when they style each byte alike.
    fn byte_styles(
        runs: impl IntoIterator<Item = (String, Option<HighlightStyle>)>,
    ) -> Vec<Option<HighlightStyle>> {
        runs.into_iter()
            .flat_map(|(text, style)| iter::repeat_n(style, text.len()))
            .collect()
    }

    /// A stream that resumes from a checkpoint must style each byte as a replay
    /// from the first endpoint does, wherever the stream opens.
    #[test]
    fn a_stream_opened_anywhere_styles_as_a_full_replay() {
        let (rope, endpoints, replayed) = checkpointed_fixture();
        for start in 0..=rope.len() {
            let streamed = BufferChunks::new(&rope, start..rope.len(), endpoints.clone());
            assert_eq!(
                byte_styles(styled_text(streamed)),
                replayed[start..],
                "opened at {start}",
            );
        }
    }

    /// A cursor that jumps to a checkpoint must seed the replay a walk from the
    /// first endpoint reaches, for short and long strides alike.
    #[test]
    fn a_cursor_that_jumps_to_checkpoints_seeds_a_full_replay() {
        let (rope, endpoints, replayed) = checkpointed_fixture();
        for stride in [1, 13, 97] {
            let mut cursor = HighlightCursor::default();
            for start in (0..=rope.len()).step_by(stride) {
                cursor.advance_to(start, &endpoints);
                let seeded = BufferChunks::with_seed(
                    &rope,
                    start..rope.len(),
                    endpoints.clone(),
                    Some(&cursor),
                );
                assert_eq!(
                    byte_styles(styled_text(seeded)),
                    replayed[start..],
                    "stride {stride}, seeded at {start}",
                );
            }
        }
    }

    fn anchor(offset: usize) -> Anchor {
        Anchor {
            timestamp: 0,
            offset: offset as u32,
            bias: Bias::Left,
            buffer_id: None,
        }
    }

    fn make_highlights(
        entries: Vec<(HighlightKey, HighlightStyle, Vec<Range<usize>>)>,
    ) -> TextHighlights {
        let mut map = HashMap::new();
        for (key, style, ranges) in entries {
            let anchor_ranges: Vec<Range<Anchor>> = ranges
                .into_iter()
                .map(|r| anchor(r.start)..anchor(r.end))
                .collect();
            map.insert(key, Arc::new((style, anchor_ranges)));
        }
        Arc::new(map)
    }

    /// The viewport's end is exclusive, so a highlight starting exactly on it
    /// belongs to the next viewport. That boundary used to be a break reading a
    /// resolved start and is now the bound the batch is collected between, which
    /// is why it is worth pinning rather than left to the ranges that clearly
    /// straddle it.
    #[test]
    fn a_highlight_starting_at_the_range_end_is_left_out() {
        let style = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        let highlights = make_highlights(vec![(
            HighlightKey::layer(HighlightLayer::SearchHighlight),
            style,
            vec![2..4, 10..12, 14..16],
        )]);
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_batch = |a: &[Anchor]| a.iter().map(&resolve).collect::<Vec<usize>>();
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };

        let eps = create_highlight_endpoints(&(0..10), &highlights, None, None, &resolver);
        assert_eq!(
            eps.iter().map(|e| e.offset).collect::<Vec<_>>(),
            vec![2, 4],
            "only the range inside the viewport, not the one opening at its end"
        );
    }

    /// Ten-byte highlights that open every thousand bytes up to 100_000.
    fn spaced_highlights() -> TextHighlights {
        make_highlights(vec![(
            HighlightKey::layer(HighlightLayer::SearchHighlight),
            colored(Color::Red),
            (0..100).map(|i| i * 1000..i * 1000 + 10).collect(),
        )])
    }

    /// The endpoint offsets of the [`spaced_highlights`] that overlap `window`.
    fn spaced_offsets(window: Range<usize>) -> Vec<usize> {
        (0..100)
            .map(|i| i * 1000)
            .filter(|start| *start < window.end && start + 10 > window.start)
            .flat_map(|start| [start, start + 10])
            .collect()
    }

    fn cached(
        range: Range<usize>,
        versions: HighlightVersions,
        highlights: &TextHighlights,
        cache: &mut Option<CachedHighlightEndpoints>,
    ) -> Arc<HighlightEndpoints> {
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_batch = |a: &[Anchor]| a.iter().map(&resolve).collect::<Vec<usize>>();
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };
        create_highlight_endpoints_cached(
            versions, &range, highlights, None, None, &resolver, None, cache,
        )
    }

    fn offsets(endpoints: &HighlightEndpoints) -> Vec<usize> {
        endpoints.iter().map(|endpoint| endpoint.offset).collect()
    }

    /// A scroll pads its window by half the viewport past each end, so the
    /// scrolls that stay inside the window rebuild nothing, forward or back.
    #[test]
    fn a_scroll_inside_the_window_reuses_its_endpoints() {
        let highlights = spaced_highlights();
        let mut cache = None;

        let cold = cached(40_000..44_000, VERSIONS, &highlights, &mut cache);
        assert_eq!(
            offsets(&cold),
            spaced_offsets(40_000..44_000),
            "a cold build"
        );

        let scrolled = cached(40_100..44_100, VERSIONS, &highlights, &mut cache);
        assert_eq!(
            offsets(&scrolled),
            spaced_offsets(38_100..46_100),
            "a scroll"
        );
        for range in [40_200..44_200, 38_500..42_500] {
            let inside = cached(range.clone(), VERSIONS, &highlights, &mut cache);
            assert!(
                Arc::ptr_eq(&scrolled, &inside),
                "a scroll to {range:?} rebuilt inside the window",
            );
        }

        let past = cached(42_500..46_500, VERSIONS, &highlights, &mut cache);
        assert_eq!(
            offsets(&past),
            spaced_offsets(40_500..48_500),
            "a scroll past the window",
        );
    }

    /// A jump or a change to the text or the highlights builds the viewport
    /// alone, even where the window holds the viewport.
    ///
    /// A padded build costs a few exact ones, and an edit makes its change on
    /// every key.
    #[test]
    fn a_jump_or_a_change_builds_the_viewport_alone() {
        let highlights = spaced_highlights();
        let mut cache = None;
        cached(40_000..44_000, VERSIONS, &highlights, &mut cache);
        cached(40_100..44_100, VERSIONS, &highlights, &mut cache);

        let jumped = cached(70_000..74_000, VERSIONS, &highlights, &mut cache);
        assert_eq!(offsets(&jumped), spaced_offsets(70_000..74_000), "a jump");

        cached(70_100..74_100, VERSIONS, &highlights, &mut cache);
        let edited = HighlightVersions {
            buffer: 2,
            ..VERSIONS
        };
        let after_edit = cached(70_200..74_200, edited, &highlights, &mut cache);
        assert_eq!(
            offsets(&after_edit),
            spaced_offsets(70_200..74_200),
            "an edit"
        );

        cached(70_300..74_300, edited, &highlights, &mut cache);
        let reinstalled = spaced_highlights();
        let after_install = cached(70_400..74_400, edited, &reinstalled, &mut cache);
        assert_eq!(
            offsets(&after_install),
            spaced_offsets(70_400..74_400),
            "a new highlight set",
        );
    }

    /// A scroll over a long range pads each end by [`MAX_PAD`] at most.
    #[test]
    fn a_scroll_over_a_long_range_pads_at_most_the_cap() {
        let highlights = spaced_highlights();
        let mut cache = None;
        cached(20_000..60_000, VERSIONS, &highlights, &mut cache);

        let scrolled = cached(20_100..60_100, VERSIONS, &highlights, &mut cache);
        assert_eq!(
            offsets(&scrolled),
            spaced_offsets(20_100 - MAX_PAD..60_100 + MAX_PAD),
            "a scroll over a long range",
        );
    }

    #[test]
    fn no_highlights() {
        let text = "hello world";
        let highlights = Arc::new(HashMap::new());
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_batch = |a: &[Anchor]| a.iter().map(&resolve).collect::<Vec<usize>>();
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };
        let eps = create_highlight_endpoints(&(0..text.len()), &highlights, None, None, &resolver);
        let chunks: Vec<_> = highlighted_chunks(text, 0, &eps).collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
        assert!(chunks[0].style.is_none());
    }

    #[test]
    fn single_highlight() {
        let text = "hello world";
        let style = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        #[allow(clippy::single_range_in_vec_init)]
        let highlights = make_highlights(vec![(
            HighlightKey::layer(HighlightLayer::SearchHighlight),
            style.clone(),
            vec![6..11],
        )]);
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_batch = |a: &[Anchor]| a.iter().map(&resolve).collect::<Vec<usize>>();
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };
        let eps = create_highlight_endpoints(&(0..text.len()), &highlights, None, None, &resolver);
        let chunks: Vec<_> = highlighted_chunks(text, 0, &eps).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "hello ");
        assert!(chunks[0].style.is_none());
        assert_eq!(chunks[1].text, "world");
        assert_eq!(
            chunks[1].style.as_ref().unwrap().foreground,
            Some(Color::Red)
        );
    }

    #[test]
    fn overlapping_highlights_precedence() {
        let text = "abcdefghij";
        let style_low = HighlightStyle {
            foreground: Some(Color::Blue),
            bold: Some(true),
            ..Default::default()
        };
        let style_high = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        #[allow(clippy::single_range_in_vec_init)]
        let highlights = make_highlights(vec![
            (
                HighlightKey::layer(HighlightLayer::SyntaxToken),
                style_low,
                vec![2..8],
            ),
            (
                HighlightKey::layer(HighlightLayer::MatchingBracket),
                style_high,
                vec![4..6],
            ),
        ]);
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_batch = |a: &[Anchor]| a.iter().map(&resolve).collect::<Vec<usize>>();
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };
        let eps = create_highlight_endpoints(&(0..text.len()), &highlights, None, None, &resolver);
        let chunks: Vec<_> = highlighted_chunks(text, 0, &eps).collect();

        // "ab" (no style), "cd" (blue+bold), "ef" (red+bold, red overrides blue fg),
        // "gh" (blue+bold), "ij" (no style)
        assert_eq!(chunks.len(), 5);
        assert_eq!(chunks[0].text, "ab");
        assert!(chunks[0].style.is_none());

        assert_eq!(chunks[1].text, "cd");
        let s1 = chunks[1].style.as_ref().unwrap();
        assert_eq!(s1.foreground, Some(Color::Blue));
        assert_eq!(s1.bold, Some(true));

        assert_eq!(chunks[2].text, "ef");
        let s2 = chunks[2].style.as_ref().unwrap();
        assert_eq!(s2.foreground, Some(Color::Red));
        assert_eq!(s2.bold, Some(true));

        assert_eq!(chunks[3].text, "gh");
        let s3 = chunks[3].style.as_ref().unwrap();
        assert_eq!(s3.foreground, Some(Color::Blue));

        assert_eq!(chunks[4].text, "ij");
        assert!(chunks[4].style.is_none());
    }

    #[test]
    fn empty_range_ignored() {
        let text = "hello";
        #[allow(clippy::single_range_in_vec_init)]
        let highlights = make_highlights(vec![(
            HighlightKey::layer(HighlightLayer::SearchHighlight),
            HighlightStyle {
                foreground: Some(Color::Red),
                ..Default::default()
            },
            vec![2..2],
        )]);
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_batch = |a: &[Anchor]| a.iter().map(&resolve).collect::<Vec<usize>>();
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };
        let eps = create_highlight_endpoints(&(0..text.len()), &highlights, None, None, &resolver);
        let chunks: Vec<_> = highlighted_chunks(text, 0, &eps).collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello");
    }

    #[test]
    fn highlight_merge() {
        let mut s = HighlightStyle::default();
        s.merge(&HighlightStyle {
            foreground: Some(Color::Blue),
            bold: Some(true),
            ..Default::default()
        });
        s.merge(&HighlightStyle {
            foreground: Some(Color::Red),
            italic: Some(true),
            ..Default::default()
        });
        assert_eq!(s.foreground, Some(Color::Red));
        assert_eq!(s.bold, Some(true));
        assert_eq!(s.italic, Some(true));
    }

    #[test]
    fn nested_semantic_tokens_stack_per_slot() {
        use super::{
            BufferSemanticTokens, HighlightStyleInterner, SemanticTokenHighlight,
            SemanticTokensHighlights,
        };
        use crate::buffer::BufferId;

        let text = "abcdefghij";
        let outer_style = HighlightStyle {
            foreground: Some(Color::Blue),
            bold: Some(true),
            ..Default::default()
        };
        let inner_style = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        let mut interner = HighlightStyleInterner::default();
        let outer_id = interner.intern(outer_style.clone());
        let inner_id = interner.intern(inner_style.clone());

        let tokens: Arc<[SemanticTokenHighlight]> = Arc::from(vec![
            SemanticTokenHighlight {
                range: anchor(2)..anchor(8),
                style: outer_id,
            },
            SemanticTokenHighlight {
                range: anchor(4)..anchor(6),
                style: inner_id,
            },
        ]);

        let mut semantic_map = HashMap::new();
        semantic_map.insert(
            BufferId::new(0),
            BufferSemanticTokens::new(tokens, Arc::new(interner), |a: &Anchor| a.offset as usize),
        );
        let semantic: SemanticTokensHighlights = Arc::new(semantic_map);
        let text_hl: TextHighlights = Arc::new(HashMap::new());
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_batch = |a: &[Anchor]| a.iter().map(&resolve).collect::<Vec<usize>>();
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };

        let eps = create_highlight_endpoints(
            &(0..text.len()),
            &text_hl,
            Some(&semantic),
            None,
            &resolver,
        );
        let chunks: Vec<_> = highlighted_chunks(text, 0, &eps).collect();

        // "ab" unstyled; "cd" outer only (blue+bold); "ef" outer+inner (inner slot
        // wins: red over blue, bold preserved from outer); "gh" outer only again;
        // "ij" unstyled.
        assert_eq!(chunks.len(), 5);
        assert_eq!(chunks[0].text, "ab");
        assert!(chunks[0].style.is_none());

        assert_eq!(chunks[1].text, "cd");
        let s1 = chunks[1].style.as_ref().unwrap();
        assert_eq!(s1.foreground, Some(Color::Blue));
        assert_eq!(s1.bold, Some(true));

        assert_eq!(chunks[2].text, "ef");
        let s2 = chunks[2].style.as_ref().unwrap();
        assert_eq!(s2.foreground, Some(Color::Red));
        assert_eq!(s2.bold, Some(true), "outer bold must survive inner merge");

        assert_eq!(chunks[3].text, "gh");
        let s3 = chunks[3].style.as_ref().unwrap();
        assert_eq!(s3.foreground, Some(Color::Blue));

        assert_eq!(chunks[4].text, "ij");
        assert!(chunks[4].style.is_none());
    }

    #[test]
    fn enclosing_semantic_token_styles_scrolled_viewport() {
        use super::{
            BufferChunks, BufferSemanticTokens, HighlightStyleInterner, SemanticTokenHighlight,
            SemanticTokensHighlights,
        };
        use crate::buffer::BufferId;
        use stoat_text::Rope;

        let text = "abcdefghij".repeat(4);
        let rope = Rope::from(text.as_str());

        let outer_style = HighlightStyle {
            foreground: Some(Color::Blue),
            bold: Some(true),
            ..Default::default()
        };
        let inner_style = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        let mut interner = HighlightStyleInterner::default();
        let outer_id = interner.intern(outer_style);
        let inner_id = interner.intern(inner_style);

        // Start-sorted, with the enclosing 0..40 token first and three short
        // leading tokens that all end before byte 20.
        let tokens: Arc<[SemanticTokenHighlight]> = Arc::from(vec![
            SemanticTokenHighlight {
                range: anchor(0)..anchor(40),
                style: outer_id,
            },
            SemanticTokenHighlight {
                range: anchor(1)..anchor(5),
                style: inner_id,
            },
            SemanticTokenHighlight {
                range: anchor(2)..anchor(8),
                style: inner_id,
            },
            SemanticTokenHighlight {
                range: anchor(3)..anchor(10),
                style: inner_id,
            },
        ]);

        let mut semantic_map = HashMap::new();
        semantic_map.insert(
            BufferId::new(0),
            BufferSemanticTokens::new(tokens, Arc::new(interner), |a: &Anchor| a.offset as usize),
        );
        let semantic: SemanticTokensHighlights = Arc::new(semantic_map);
        let text_hl: TextHighlights = Arc::new(HashMap::new());
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_batch = |a: &[Anchor]| a.iter().map(&resolve).collect::<Vec<usize>>();
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };

        let eps = create_highlight_endpoints(&(20..40), &text_hl, Some(&semantic), None, &resolver);
        let endpoints = HighlightEndpoints::shared(eps);
        let chunks: Vec<Chunk<'_>> = BufferChunks::new(&rope, 20..40, endpoints).collect();

        let first = chunks.first().expect("scrolled viewport must emit chunks");
        let style = first
            .highlight_style
            .as_ref()
            .expect("enclosing token must style the scrolled viewport");
        assert_eq!(style.foreground, Some(Color::Blue));
        assert_eq!(style.bold, Some(true));
    }

    #[test]
    fn overlap_bounds_brackets_the_tokens_reaching_a_range() {
        use super::{BufferSemanticTokens, HighlightStyleInterner, SemanticTokenHighlight};

        let make = |ranges: &[(usize, usize)]| -> BufferSemanticTokens {
            let mut interner = HighlightStyleInterner::default();
            let id = interner.intern(HighlightStyle::default());
            let tokens: Arc<[SemanticTokenHighlight]> = ranges
                .iter()
                .map(|&(s, e)| SemanticTokenHighlight {
                    range: anchor(s)..anchor(e),
                    style: id,
                })
                .collect();
            BufferSemanticTokens::new(tokens, Arc::new(interner), |a: &Anchor| a.offset as usize)
        };
        let resolve = |a: &Anchor| a.offset as usize;

        // Tokens ending before the range are dropped from the lower bound.
        let short = make(&[(0, 5), (6, 10), (22, 28), (30, 38)]);
        assert_eq!(
            short.overlap_bounds(&(20..40), resolve),
            2..4,
            "tokens ending before the range are excluded"
        );

        // A long token starting before the range but reaching into it is kept via
        // prefix_max_end, even though the nearer token ends earlier.
        let enclosing = make(&[(0, 40), (22, 28)]);
        assert_eq!(
            enclosing.overlap_bounds(&(20..40), resolve),
            0..2,
            "an enclosing token reaching the range is kept"
        );
    }

    /// Every other channel test fits inside one segment, so nothing else here
    /// exercises a lookup that has to compose across them.
    #[test]
    fn a_channel_spanning_segments_bounds_like_a_flat_one() {
        use super::{
            BufferSemanticTokens, HighlightStyleInterner, SemanticTokenHighlight, SEGMENT_TOKENS,
        };

        let mut interner = HighlightStyleInterner::default();
        let style = interner.intern(HighlightStyle::default());
        let interner = Arc::new(interner);

        // Two and a bit segments of small tokens, with one at the very front
        // reaching past the end of them all. That leading token is what the
        // carried argmax exists for. A lookup late in the channel has to see
        // it, and no segment's own index knows about it.
        let count = SEGMENT_TOKENS * 2 + 17;
        let mut spans: Vec<(usize, usize)> = vec![(0, count * 4 + 8)];
        spans.extend((1..count).map(|i| (i * 4, i * 4 + 3)));

        let tokens: Arc<[SemanticTokenHighlight]> = spans
            .iter()
            .map(|&(s, e)| SemanticTokenHighlight {
                range: anchor(s)..anchor(e),
                style,
            })
            .collect();
        let resolve = |a: &Anchor| a.offset as usize;
        let channel = BufferSemanticTokens::new(tokens, interner, resolve);
        assert!(
            channel.segments.len() > 2,
            "the fixture must actually span segments",
        );

        // Probe around each segment seam, plus the ends, where a composed
        // lookup is most likely to read the wrong segment's index.
        let seams = [
            0,
            1,
            SEGMENT_TOKENS - 1,
            SEGMENT_TOKENS,
            SEGMENT_TOKENS + 1,
            SEGMENT_TOKENS * 2 - 1,
            SEGMENT_TOKENS * 2,
            SEGMENT_TOKENS * 2 + 1,
            count - 1,
        ];
        for token_index in seams {
            let start = token_index * 4;
            for range in [start..start + 1, start..start + 9, start.max(1) - 1..start] {
                let expected = {
                    let hi = spans.partition_point(|(s, _)| *s < range.end);
                    let mut running = 0;
                    let lo = (0..hi)
                        .find(|&i| {
                            running = running.max(spans[i].1);
                            running > range.start
                        })
                        .unwrap_or(hi);
                    lo..hi
                };
                assert_eq!(
                    channel.overlap_bounds(&range, resolve),
                    expected,
                    "segmented bounds must match a flat scan for {range:?}",
                );
            }
        }

        // The flat-range walk has to cross segments too, not just index one.
        let all: Vec<_> = channel.range(0..channel.len()).collect();
        assert_eq!(all.len(), count, "every token is reachable in order");
        let crossing: Vec<_> = channel
            .range(SEGMENT_TOKENS - 2..SEGMENT_TOKENS + 2)
            .map(|t| resolve(&t.range.start))
            .collect();
        assert_eq!(
            crossing,
            [
                (SEGMENT_TOKENS - 2) * 4,
                (SEGMENT_TOKENS - 1) * 4,
                SEGMENT_TOKENS * 4,
                (SEGMENT_TOKENS + 1) * 4,
            ],
            "a range straddling a seam yields both sides in order",
        );
    }

    /// The parse path builds the search index straight from the byte offsets it
    /// anchored, never resolving an anchor. That is only sound if it lands on
    /// the same index the resolving constructor computes.
    #[test]
    fn with_resolved_ends_matches_the_resolving_constructor() {
        use super::{BufferSemanticTokens, HighlightStyleInterner, SemanticTokenHighlight};

        // Ends run deliberately out of order relative to starts. An enclosing
        // token comes first, two nest inside it, one reaches past everything,
        // and a short one trails. Two tokens share an end so the tie-break is
        // pinned as well.
        let spans = [
            (0usize, 20usize),
            (2, 6),
            (4, 20),
            (10, 12),
            (11, 40),
            (30, 35),
        ];

        let mut interner = HighlightStyleInterner::default();
        let style = interner.intern(HighlightStyle::default());
        let interner = Arc::new(interner);
        let tokens: Arc<[SemanticTokenHighlight]> = spans
            .iter()
            .map(|&(s, e)| SemanticTokenHighlight {
                range: anchor(s)..anchor(e),
                style,
            })
            .collect();

        let resolve = |a: &Anchor| a.offset as usize;
        let resolving = BufferSemanticTokens::new(tokens.clone(), interner.clone(), resolve);

        let ends: Vec<usize> = spans.iter().map(|&(_, e)| e).collect();
        let batched = BufferSemanticTokens::with_resolved_ends(tokens, interner, &ends);

        for i in 0..batched.len() {
            assert_eq!(
                batched.prefix_max_end(i, &resolve),
                resolving.prefix_max_end(i, &resolve),
                "byte-offset ends must yield the same argmax as resolved ends at {i}",
            );
        }
        for start in 0..=40 {
            for end in start..=40 {
                assert_eq!(
                    batched.overlap_bounds(&(start..end), resolve),
                    resolving.overlap_bounds(&(start..end), resolve),
                    "bounds must agree for {start}..{end}",
                );
            }
        }
    }

    #[test]
    fn bottom_viewport_emits_only_overlapping_endpoints() {
        use super::{
            BufferSemanticTokens, HighlightStyleInterner, SemanticTokenHighlight,
            SemanticTokensHighlights,
        };
        use crate::buffer::BufferId;

        let style = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        let mut interner = HighlightStyleInterner::default();
        let id = interner.intern(style);

        // Ten disjoint 4-wide tokens marching down a 100-byte file.
        let tokens: Arc<[SemanticTokenHighlight]> = Arc::from(
            (0..10)
                .map(|k| SemanticTokenHighlight {
                    range: anchor(k * 10)..anchor(k * 10 + 4),
                    style: id,
                })
                .collect::<Vec<_>>(),
        );

        let mut semantic_map = HashMap::new();
        semantic_map.insert(
            BufferId::new(0),
            BufferSemanticTokens::new(tokens, Arc::new(interner), |a: &Anchor| a.offset as usize),
        );
        let semantic: SemanticTokensHighlights = Arc::new(semantic_map);
        let text_hl: TextHighlights = Arc::new(HashMap::new());
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_batch = |a: &[Anchor]| a.iter().map(&resolve).collect::<Vec<usize>>();
        let resolver = AnchorResolver {
            one: &resolve,
            many: &resolve_batch,
        };

        // A viewport over the last token only. Every earlier token ends at or
        // before byte 84, so none may contribute an endpoint.
        let eps =
            create_highlight_endpoints(&(92..100), &text_hl, Some(&semantic), None, &resolver);

        let offsets: Vec<usize> = eps.iter().map(|e| e.offset).collect();
        assert_eq!(
            offsets,
            vec![90, 94],
            "only the token overlapping the bottom viewport is emitted"
        );
    }

    #[test]
    fn buffer_chunks_spans_multiple_rope_chunks() {
        use super::{BufferChunks, HighlightEndpoint};
        use stoat_text::Rope;

        // A rope large enough to be split across multiple internal chunks.
        // Chunks in stoat_text cap around 384 bytes, so 1500 bytes
        // definitely spans multiple storage chunks.
        let text: String = "abcdefghij".repeat(150);
        let rope = Rope::from(text.as_str());
        assert!(
            rope.chunks().count() > 1,
            "test precondition: need multi-chunk rope",
        );

        // Highlight bytes 50..55 red and 60..65 blue. These spans may lie on
        // either side of a rope chunk boundary depending on chunk split.
        let red = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        let blue = HighlightStyle {
            foreground: Some(Color::Blue),
            ..Default::default()
        };
        let endpoints = HighlightEndpoints::shared(vec![
            HighlightEndpoint {
                offset: 50,
                is_start: true,
                key: HighlightKey::new(HighlightLayer::SyntaxToken, 0),
                style: Some(red.clone()),
            },
            HighlightEndpoint {
                offset: 55,
                is_start: false,
                key: HighlightKey::new(HighlightLayer::SyntaxToken, 0),
                style: None,
            },
            HighlightEndpoint {
                offset: 60,
                is_start: true,
                key: HighlightKey::new(HighlightLayer::SyntaxToken, 1),
                style: Some(blue.clone()),
            },
            HighlightEndpoint {
                offset: 65,
                is_start: false,
                key: HighlightKey::new(HighlightLayer::SyntaxToken, 1),
                style: None,
            },
        ]);

        let chunks: Vec<Chunk<'_>> = BufferChunks::new(&rope, 0..text.len(), endpoints).collect();

        let recovered: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        assert_eq!(recovered, text, "chunks must reassemble to the rope text");

        // Walk chunks by byte offset and collect each byte's resolved foreground
        // color. Position-based assertions avoid false matches on repeated text.
        let mut colors: Vec<Option<Color>> = Vec::with_capacity(text.len());
        for chunk in &chunks {
            let color = chunk.highlight_style.as_ref().and_then(|s| s.foreground);
            for _ in chunk.text.as_bytes() {
                colors.push(color);
            }
        }
        assert_eq!(colors.len(), text.len());

        for (byte, color) in colors.iter().enumerate().take(55).skip(50) {
            assert_eq!(*color, Some(Color::Red), "byte {byte} must be red");
        }
        for (byte, color) in colors.iter().enumerate().take(60).skip(55) {
            assert_eq!(*color, None, "byte {byte} must be unstyled (gap)");
        }
        for (byte, color) in colors.iter().enumerate().take(65).skip(60) {
            assert_eq!(*color, Some(Color::Blue), "byte {byte} must be blue");
        }
        assert_eq!(colors[49], None, "byte before red span is unstyled");
        assert_eq!(colors[65], None, "byte after blue span is unstyled");
    }

    #[test]
    fn buffer_chunks_no_highlights_fast_path() {
        use super::BufferChunks;
        use stoat_text::Rope;

        let text = "hello world";
        let rope = Rope::from(text);
        let chunks: Vec<Chunk<'_>> =
            BufferChunks::new(&rope, 0..text.len(), HighlightEndpoints::shared(Vec::new()))
                .collect();
        let joined: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        assert_eq!(joined, text);
        for c in &chunks {
            assert!(c.highlight_style.is_none());
        }
    }

    #[test]
    fn buffer_chunks_endpoint_inside_multibyte_char_does_not_panic() {
        use super::{BufferChunks, HighlightEndpoint};
        use stoat_text::Rope;

        // "h\u{e9}llo": the second byte lands inside the two-byte 'e-acute'
        // codepoint. A correctly defended chunk splitter must round to a
        // UTF-8 boundary instead of panicking on split_at(2).
        let text = "h\u{e9}llo";
        let rope = Rope::from(text);
        let red = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        let endpoints = HighlightEndpoints::shared(vec![
            HighlightEndpoint {
                offset: 2,
                is_start: true,
                key: HighlightKey::new(HighlightLayer::SyntaxToken, 0),
                style: Some(red.clone()),
            },
            HighlightEndpoint {
                offset: text.len(),
                is_start: false,
                key: HighlightKey::new(HighlightLayer::SyntaxToken, 0),
                style: None,
            },
        ]);

        let chunks: Vec<Chunk<'_>> = BufferChunks::new(&rope, 0..text.len(), endpoints).collect();
        let joined: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        assert_eq!(joined, text, "all bytes must be emitted exactly once");
    }

    #[test]
    fn highlighted_chunks_endpoint_inside_multibyte_char_does_not_panic() {
        use super::HighlightEndpoint;

        let text = "h\u{e9}llo";
        let red = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };
        let endpoints = vec![
            HighlightEndpoint {
                offset: 2,
                is_start: true,
                key: HighlightKey::new(HighlightLayer::SyntaxToken, 0),
                style: Some(red),
            },
            HighlightEndpoint {
                offset: text.len(),
                is_start: false,
                key: HighlightKey::new(HighlightLayer::SyntaxToken, 0),
                style: None,
            },
        ];
        let chunks: Vec<_> = highlighted_chunks(text, 0, &endpoints).collect();
        let joined: String = chunks.iter().map(|c| c.text).collect();
        assert_eq!(joined, text);
    }

    /// A cached window carried across real buffer edits, checked against a
    /// fresh build of the same window after each one.
    mod carry {
        use crate::{
            buffer::{BufferId, SharedBuffer, TextBuffer},
            display_map::{
                highlights::{
                    create_highlight_endpoints, create_highlight_endpoints_cached, AnchorResolver,
                    BufferSemanticTokens, CachedHighlightEndpoints, HighlightEndpoint,
                    HighlightStyle, HighlightStyleInterner, HighlightVersions,
                    SemanticTokenHighlight, SemanticTokensHighlights, TextHighlights,
                },
                sampler::Sampler,
            },
            multi_buffer::MultiBuffer,
        };
        use ratatui::style::Color;
        use std::{
            cell::Cell,
            cmp::Reverse,
            collections::HashMap,
            ops::Range,
            sync::{Arc, RwLock},
        };
        use stoat_text::{Anchor, Bias};

        const TEXT: &str = "fn main() {\n    let alpha = beta;\n    let gamma = alpha + 1;\n}\n";

        /// A buffer with one tree-sitter token per word and per punctuation
        /// byte, and one over the braces that encloses the rest, anchored the
        /// way a parse anchors them.
        struct Fixture {
            shared: SharedBuffer,
            multi: MultiBuffer,
            text_highlights: TextHighlights,
            semantic: SemanticTokensHighlights,
        }

        impl Fixture {
            fn new() -> Self {
                let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), TEXT)));
                let multi = MultiBuffer::singleton(shared.clone());
                let snap = multi.snapshot();

                let mut interner = HighlightStyleInterner::default();
                let style = interner.intern(HighlightStyle {
                    foreground: Some(Color::Blue),
                    ..Default::default()
                });
                let tokens: Arc<[SemanticTokenHighlight]> = token_ranges(TEXT)
                    .into_iter()
                    .map(|range| SemanticTokenHighlight {
                        range: snap.anchor_at(range.start, Bias::Right)
                            ..snap.anchor_at(range.end, Bias::Left),
                        style,
                    })
                    .collect();
                let channel =
                    BufferSemanticTokens::new(tokens, Arc::new(interner), |a: &Anchor| {
                        snap.resolve_anchor(a)
                    });

                Self {
                    shared,
                    multi,
                    text_highlights: Arc::new(HashMap::new()),
                    semantic: Arc::new(HashMap::from([(BufferId::new(0), channel)])),
                }
            }

            /// Ask `cache` for the whole buffer with the edits since its
            /// version, and report whether a bounds search ran, which only a
            /// build does.
            fn ask(&self, cache: &mut Option<CachedHighlightEndpoints>) -> bool {
                let snap = self.multi.snapshot();
                let edits = cache
                    .as_ref()
                    .map(|cached| snap.edits_since(cached.versions.buffer));
                let searches = Cell::new(0);
                let resolve = |a: &Anchor| {
                    searches.set(searches.get() + 1);
                    snap.resolve_anchor(a)
                };
                let resolve_batch = |a: &[Anchor]| snap.resolve_anchors_batch(a);
                create_highlight_endpoints_cached(
                    HighlightVersions {
                        buffer: snap.version(),
                        settings: 1,
                    },
                    &(0..snap.rope().len()),
                    &self.text_highlights,
                    Some(&self.semantic),
                    None,
                    &AnchorResolver {
                        one: &resolve,
                        many: &resolve_batch,
                    },
                    edits.as_ref(),
                    cache,
                );
                searches.get() > 0
            }

            /// The endpoints a fresh build gives for the whole buffer.
            fn fresh(&self) -> Vec<HighlightEndpoint> {
                let snap = self.multi.snapshot();
                let resolve = |a: &Anchor| snap.resolve_anchor(a);
                let resolve_batch = |a: &[Anchor]| snap.resolve_anchors_batch(a);
                create_highlight_endpoints(
                    &(0..snap.rope().len()),
                    &self.text_highlights,
                    Some(&self.semantic),
                    None,
                    &AnchorResolver {
                        one: &resolve,
                        many: &resolve_batch,
                    },
                )
            }

            /// Replace `range` with `text`, then assert that `cache` carries
            /// across the edit, with no bounds search, to the window and the
            /// endpoints a fresh build gives.
            fn edit_and_carry(
                &self,
                cache: &mut Option<CachedHighlightEndpoints>,
                range: Range<usize>,
                text: &str,
                label: &str,
            ) {
                self.shared.write().expect("poisoned").edit(range, text);
                let searched = self.ask(cache);
                let len = self.multi.snapshot().rope().len();
                let cached = cache.as_ref().expect("a cache");
                assert_eq!(
                    (searched, cached.range.clone(), cached.endpoints.to_vec()),
                    (false, 0..len, self.fresh()),
                    "{label}",
                );
            }
        }

        fn token_ranges(text: &str) -> Vec<Range<usize>> {
            let bytes = text.as_bytes();
            let mut ranges = Vec::new();
            ranges.push(text.find('{').expect("braces")..text.rfind('}').expect("braces") + 1);
            let mut at = 0;
            while at < bytes.len() {
                let start = at;
                if bytes[at].is_ascii_alphanumeric() {
                    while at < bytes.len() && bytes[at].is_ascii_alphanumeric() {
                        at += 1;
                    }
                    ranges.push(start..at);
                    continue;
                }
                if bytes[at].is_ascii_punctuation() {
                    ranges.push(start..start + 1);
                }
                at += 1;
            }
            ranges.sort_by_key(|range| (range.start, Reverse(range.end)));
            ranges
        }

        /// A cache over the whole buffer, built fresh.
        fn built(fixture: &Fixture) -> Option<CachedHighlightEndpoints> {
            let mut cache = None;
            fixture.ask(&mut cache);
            cache
        }

        #[test]
        fn an_insertion_carries_the_window_to_a_fresh_build() {
            let indent = TEXT.find("    let").expect("fixture") + 2;
            let alpha = TEXT.find("alpha").expect("fixture");
            for (at, label) in [
                (indent, "before every token of the line"),
                (alpha + 2, "inside a token"),
                (alpha, "at a token's start"),
                (alpha + 5, "at a token's end"),
            ] {
                let fixture = Fixture::new();
                let mut cache = built(&fixture);
                fixture.edit_and_carry(&mut cache, at..at, "xy", label);
            }
        }

        #[test]
        fn a_deletion_that_swallows_a_token_drops_its_pair() {
            let fixture = Fixture::new();
            let mut cache = built(&fixture);
            let plus = TEXT.find('+').expect("fixture");
            fixture.edit_and_carry(&mut cache, plus..plus + 1, "", "the + token");

            let gamma = TEXT.find("gamma").expect("fixture");
            fixture.edit_and_carry(&mut cache, gamma..gamma + 8, "", "gamma, = and both spaces");
        }

        /// A deletion of the closing brace lands the end of the token before
        /// it and the end of the token the braces enclose on one offset. Their
        /// old order is the reverse of their key order, so the carry must sort
        /// them again.
        #[test]
        fn a_deletion_that_lands_two_ends_on_one_offset_sorts_them() {
            let fixture = Fixture::new();
            let mut cache = built(&fixture);
            let brace = TEXT.rfind('}').expect("fixture");
            fixture.edit_and_carry(
                &mut cache,
                brace - 1..brace + 1,
                "",
                "the brace and the break before it",
            );
        }

        /// A backspace leaves the byte it removed behind at the cursor, and text
        /// typed there goes ahead of it. A token end anchored to that byte then
        /// lands past the new text, which only its anchor knows.
        #[test]
        fn typing_after_a_backspace_carries_the_end_the_removed_byte_held() {
            let fixture = Fixture::new();
            let mut cache = built(&fixture);
            let end = TEXT.find("beta").expect("fixture") + 4;
            fixture.edit_and_carry(
                &mut cache,
                end - 1..end,
                "",
                "backspace over beta's last byte",
            );
            fixture.edit_and_carry(&mut cache, end - 1..end - 1, "z", "typing where it was");
        }

        #[test]
        fn a_window_carried_across_random_edits_matches_a_fresh_build() {
            for seed in 1..=4 {
                let fixture = Fixture::new();
                let mut cache = built(&fixture);
                let mut sampler = Sampler::new(seed);
                for step in 0..10 {
                    // Inside the window, which spans the buffer, and clear of
                    // both of its ends.
                    let len = fixture.multi.snapshot().rope().len() as u32;
                    let start = 1 + sampler.below(len - 2);
                    let end = (start + sampler.below(4)).min(len - 1);
                    let text: String = (0..sampler.below(3))
                        .map(|_| char::from(b'a' + sampler.below(26) as u8))
                        .collect();
                    fixture.edit_and_carry(
                        &mut cache,
                        start as usize..end as usize,
                        &text,
                        &format!("seed {seed} step {step}"),
                    );
                }
            }
        }

        /// An edit at the window's edge moves ranges across that edge, so the
        /// cache builds the window again instead of carrying it.
        #[test]
        fn an_edit_at_the_window_edge_builds_the_window_again() {
            let fixture = Fixture::new();
            let mut cache = built(&fixture);
            fixture.shared.write().expect("poisoned").edit(0..0, "//");

            let searched = fixture.ask(&mut cache);
            let cached = cache.as_ref().expect("a cache");
            assert_eq!(
                (searched, cached.endpoints.to_vec()),
                (true, fixture.fresh()),
                "an insertion at the window's start",
            );
        }

        /// The tree-sitter channels are checked by generation, so an install
        /// that swaps in an equal channel without moving one keeps the cache.
        #[test]
        fn an_equal_channel_under_the_same_generation_keeps_the_cache() {
            let mut fixture = Fixture::new();
            let mut cache = built(&fixture);
            fixture.semantic = Arc::new((*fixture.semantic).clone());

            assert!(!fixture.ask(&mut cache), "no bounds search, so no build");
        }

        /// A carry that empties a pair marks the cache, and only a marked
        /// cache goes before an install that keeps the cache.
        #[test]
        fn only_a_carry_that_empties_a_pair_forgets_the_cache() {
            let alpha = TEXT.find("alpha").expect("fixture");
            let plus = TEXT.find('+').expect("fixture");
            let kept: Vec<bool> = [(alpha + 2..alpha + 2, "xy"), (plus..plus + 1, "")]
                .into_iter()
                .map(|(range, text)| {
                    let fixture = Fixture::new();
                    let mut cache = built(&fixture);
                    fixture.edit_and_carry(&mut cache, range, text, "the edit carries");
                    CachedHighlightEndpoints::forget_if_collapsed(&mut cache);
                    cache.is_some()
                })
                .collect();
            assert_eq!(kept, [true, false], "an insertion, then a deleted token");
        }

        /// A build that skips a token an edit emptied marks the cache as well.
        #[test]
        fn a_build_over_an_emptied_token_forgets_the_cache() {
            let fixture = Fixture::new();
            let plus = TEXT.find('+').expect("fixture");
            fixture
                .shared
                .write()
                .expect("poisoned")
                .edit(plus..plus + 1, "");
            let mut cache = built(&fixture);
            CachedHighlightEndpoints::forget_if_collapsed(&mut cache);

            assert!(cache.is_none());
        }
    }
}
