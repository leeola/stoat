use crate::{
    buffer::BufferId,
    display_map::{
        inlay_map::{InlayId, InlayKind},
        DiagnosticSeverity,
    },
};
use ratatui::style::{Color, Modifier, Style};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    ops::Range,
    sync::Arc,
};
use stoat_text::{Anchor, ChunksInRange, Rope};

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

/// A viewport's endpoints plus one replay over them, for a painter working row
/// by row.
///
/// The review, diff and conflict views paint other things between rows, so they
/// cannot hold one chunk stream open across the viewport the way the plain
/// editor body does. Opening a stream per row instead makes each row re-derive
/// its opening styles from the first endpoint, so the frame costs rows times
/// endpoints. Holding this across the loop makes it cost the endpoints once.
///
/// `plain` records whether display offsets below the block layer are buffer
/// offsets. They are not once anything is folded or an inlay is shown, and a
/// replay measured in one coordinate says nothing about the other, so no seed is
/// offered at all in that case and the rows simply cost what they used to.
pub struct RowHighlightCursor {
    pub(crate) endpoints: Arc<[HighlightEndpoint]>,
    cursor: HighlightCursor,
    plain: bool,
}

impl RowHighlightCursor {
    pub fn new(endpoints: Arc<[HighlightEndpoint]>, plain: bool) -> Self {
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
/// every pointer equal to what the cache stored. The generation moves on each
/// install whatever the refcount did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightVersions {
    pub buffer: u64,
    pub settings: u64,
}

/// Visible-range highlight endpoints memoized across repaints.
#[derive(Clone, Debug)]
pub struct CachedHighlightEndpoints {
    versions: HighlightVersions,
    text_ptr: usize,
    semantic_ptr: Option<usize>,
    lsp_ptr: Option<usize>,
    range: Range<usize>,
    endpoints: Arc<[HighlightEndpoint]>,
}

impl CachedHighlightEndpoints {
    fn is_valid(
        &self,
        versions: HighlightVersions,
        highlights: &TextHighlights,
        semantic: Option<&SemanticTokensHighlights>,
        lsp: Option<&SemanticTokensHighlights>,
        range: &Range<usize>,
    ) -> bool {
        self.versions == versions
            && self.text_ptr == Arc::as_ptr(highlights) as usize
            && self.semantic_ptr == semantic.map(|s| Arc::as_ptr(s) as usize)
            && self.lsp_ptr == lsp.map(|s| Arc::as_ptr(s) as usize)
            && self.range == *range
    }

    pub fn endpoints(&self) -> &[HighlightEndpoint] {
        &self.endpoints
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

pub fn create_highlight_endpoints_cached(
    versions: HighlightVersions,
    range: &Range<usize>,
    highlights: &TextHighlights,
    semantic_highlights: Option<&SemanticTokensHighlights>,
    lsp_highlights: Option<&SemanticTokensHighlights>,
    resolver: &AnchorResolver<'_>,
    cache: &mut Option<CachedHighlightEndpoints>,
) -> Arc<[HighlightEndpoint]> {
    if let &mut Some(ref cached) = cache
        && cached.is_valid(
            versions,
            highlights,
            semantic_highlights,
            lsp_highlights,
            range,
        )
    {
        return cached.endpoints.clone();
    }
    let endpoints = create_highlight_endpoints(
        range,
        highlights,
        semantic_highlights,
        lsp_highlights,
        resolver,
    );
    let arc: Arc<[HighlightEndpoint]> = Arc::from(endpoints);
    *cache = Some(CachedHighlightEndpoints {
        versions,
        text_ptr: Arc::as_ptr(highlights) as usize,
        semantic_ptr: semantic_highlights.map(|s| Arc::as_ptr(s) as usize),
        lsp_ptr: lsp_highlights.map(|s| Arc::as_ptr(s) as usize),
        range: range.clone(),
        endpoints: arc.clone(),
    });
    arc
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
    let mut endpoints = Vec::new();

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
        push_semantic_endpoints(
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
        push_semantic_endpoints(
            &mut endpoints,
            lsp,
            HighlightLayer::LspSemanticToken,
            range,
            resolver,
        );
    }

    endpoints.sort();
    endpoints
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
fn push_semantic_endpoints(
    endpoints: &mut Vec<HighlightEndpoint>,
    semantic: &SemanticTokensHighlights,
    layer: HighlightLayer,
    range: &Range<usize>,
    resolver: &AnchorResolver<'_>,
) {
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
}

/// Iterate text chunks with merged highlight styles applied.
pub fn highlighted_chunks<'a>(
    text: &'a str,
    text_offset: usize,
    endpoints: &'a [HighlightEndpoint],
) -> impl Iterator<Item = HighlightedChunk<'a>> {
    let mut active: BTreeMap<HighlightKey, HighlightStyle> = BTreeMap::new();
    let mut ep_idx = 0;
    let mut pos = text_offset;
    let mut remaining = text;

    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }

        // Process all endpoints at the current position
        while ep_idx < endpoints.len() && endpoints[ep_idx].offset <= pos {
            let ep = &endpoints[ep_idx];
            if let Some(ref style) = ep.style {
                active.insert(ep.key, style.clone());
            } else {
                active.remove(&ep.key);
            }
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
            for style in active.values() {
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
/// slice of pre-computed [`HighlightEndpoint`]s. Emits [`Chunk`]s without any
/// per-chunk heap allocation: `text` is always a borrow of the rope's own
/// chunk storage, the endpoints vector is built once by the caller, and only
/// a small [`BTreeMap`] of active styles is carried across calls.
///
/// This is the bottom layer of the display map chunks pipeline. Higher layers
/// ([`super::inlay_map::InlaySnapshot::chunks`], [`super::fold_map::FoldSnapshot::chunks`],
/// etc.) wrap a `BufferChunks` and transform the chunk stream.
pub struct BufferChunks<'a> {
    text_chunks: ChunksInRange<'a>,
    pending: &'a str,
    offset: usize,
    end: usize,
    endpoints: Arc<[HighlightEndpoint]>,
    ep_idx: usize,
    active: BTreeMap<HighlightKey, HighlightStyle>,
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
    active: BTreeMap<HighlightKey, HighlightStyle>,
}

impl HighlightCursor {
    /// Replay `endpoints` forward until the cursor describes `offset`.
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
    pub fn advance_to(&mut self, offset: usize, endpoints: &[HighlightEndpoint]) {
        if offset < self.offset {
            return;
        }
        while self.ep_idx < endpoints.len() && endpoints[self.ep_idx].offset <= offset {
            let ep = &endpoints[self.ep_idx];
            match &ep.style {
                Some(style) => {
                    self.active.insert(ep.key, style.clone());
                },
                None => {
                    self.active.remove(&ep.key);
                },
            }
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
    pub fn new(rope: &'a Rope, range: Range<usize>, endpoints: Arc<[HighlightEndpoint]>) -> Self {
        Self::with_seed(rope, range, endpoints, None)
    }

    /// Like [`Self::new`], starting from a replay `seed` already advanced to the
    /// range's start.
    ///
    /// Saves re-deriving the active styles by walking the endpoints below
    /// `range.start`, which is worth doing for a caller opening a stream per row
    /// over one shared endpoint list.
    ///
    /// A seed describing any other offset is ignored and the replay runs as
    /// usual, so a caller that gets the pairing wrong loses the shortcut rather
    /// than the styling.
    pub fn with_seed(
        rope: &'a Rope,
        range: Range<usize>,
        endpoints: Arc<[HighlightEndpoint]>,
        seed: Option<&HighlightCursor>,
    ) -> Self {
        let start = range.start;
        let end = range.end;
        let (ep_idx, active) = match seed.filter(|cursor| cursor.offset == start) {
            Some(cursor) => (cursor.ep_idx, cursor.active.clone()),
            None => (0, BTreeMap::new()),
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
        for style in self.active.values() {
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
            let ep = &self.endpoints[self.ep_idx];
            match &ep.style {
                Some(style) => {
                    self.active.insert(ep.key, style.clone());
                },
                None => {
                    self.active.remove(&ep.key);
                },
            }
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
        create_highlight_endpoints, highlighted_chunks, AnchorResolver, BufferChunks, Chunk,
        HighlightCursor, HighlightEndpoint, HighlightKey, HighlightLayer, HighlightStyle,
        TextHighlights,
    };
    use ratatui::style::Color;
    use std::{collections::HashMap, ops::Range, sync::Arc};
    use stoat_text::{Anchor, Bias, Rope};

    /// Endpoints turning three overlapping styles on and off across the text,
    /// so a replay stopped part-way holds more than one active entry.
    fn layered_endpoints() -> Arc<[HighlightEndpoint]> {
        let colored = |color| HighlightStyle {
            foreground: Some(color),
            ..Default::default()
        };
        let spans = [
            (HighlightLayer::SearchHighlight, colored(Color::Red), 2..30),
            (
                HighlightLayer::SelectionHighlight,
                colored(Color::Green),
                8..22,
            ),
            (
                HighlightLayer::MatchingBracket,
                colored(Color::Blue),
                14..40,
            ),
        ];

        let mut endpoints = Vec::new();
        for (layer, style, range) in spans {
            endpoints.push(HighlightEndpoint {
                offset: range.start,
                is_start: true,
                key: HighlightKey::layer(layer),
                style: Some(style),
            });
            endpoints.push(HighlightEndpoint {
                offset: range.end,
                is_start: false,
                key: HighlightKey::layer(layer),
                style: None,
            });
        }
        endpoints.sort();
        Arc::from(endpoints)
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
            BufferChunks, BufferSemanticTokens, HighlightEndpoint, HighlightStyleInterner,
            SemanticTokenHighlight, SemanticTokensHighlights,
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
        let endpoints: Arc<[HighlightEndpoint]> = Arc::from(eps);
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
        let endpoints: Arc<[HighlightEndpoint]> = Arc::from(vec![
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
            BufferChunks::new(&rope, 0..text.len(), Arc::from(Vec::new())).collect();
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
        let endpoints: Arc<[HighlightEndpoint]> = Arc::from(vec![
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
}
