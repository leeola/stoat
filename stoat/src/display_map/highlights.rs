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

pub type SemanticTokensHighlights =
    Arc<HashMap<BufferId, (Arc<[SemanticTokenHighlight]>, Arc<HighlightStyleInterner>)>>;

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

#[derive(Clone, Debug)]
pub struct CachedHighlightEndpoints {
    text_ptr: usize,
    semantic_ptr: Option<usize>,
    range: Range<usize>,
    endpoints: Arc<[HighlightEndpoint]>,
}

impl CachedHighlightEndpoints {
    fn is_valid(
        &self,
        highlights: &TextHighlights,
        semantic: Option<&SemanticTokensHighlights>,
        range: &Range<usize>,
    ) -> bool {
        self.text_ptr == Arc::as_ptr(highlights) as usize
            && self.semantic_ptr == semantic.map(|s| Arc::as_ptr(s) as usize)
            && self.range == *range
    }

    pub fn endpoints(&self) -> &[HighlightEndpoint] {
        &self.endpoints
    }
}

pub fn create_highlight_endpoints_cached(
    range: &Range<usize>,
    highlights: &TextHighlights,
    semantic_highlights: Option<&SemanticTokensHighlights>,
    resolve: &impl Fn(&Anchor) -> usize,
    cache: &mut Option<CachedHighlightEndpoints>,
) -> Arc<[HighlightEndpoint]> {
    if let Some(ref cached) = cache {
        if cached.is_valid(highlights, semantic_highlights, range) {
            return cached.endpoints.clone();
        }
    }
    let endpoints = create_highlight_endpoints(range, highlights, semantic_highlights, resolve);
    let arc: Arc<[HighlightEndpoint]> = Arc::from(endpoints);
    *cache = Some(CachedHighlightEndpoints {
        text_ptr: Arc::as_ptr(highlights) as usize,
        semantic_ptr: semantic_highlights.map(|s| Arc::as_ptr(s) as usize),
        range: range.clone(),
        endpoints: arc.clone(),
    });
    arc
}

pub fn create_highlight_endpoints(
    range: &Range<usize>,
    highlights: &TextHighlights,
    semantic_highlights: Option<&SemanticTokensHighlights>,
    resolve: &impl Fn(&Anchor) -> usize,
) -> Vec<HighlightEndpoint> {
    let mut endpoints = Vec::new();

    for (&key, hl) in highlights.iter() {
        let style = &hl.0;
        let ranges = &hl.1;

        let start_ix = ranges
            .binary_search_by(|probe| {
                resolve(&probe.end)
                    .cmp(&range.start)
                    .then(std::cmp::Ordering::Less)
            })
            .unwrap_or_else(|i| i);

        for anchor_range in &ranges[start_ix..] {
            let s = resolve(&anchor_range.start);
            let e = resolve(&anchor_range.end);
            if s >= range.end {
                break;
            }
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
        for (_buffer_id, (tokens, interner)) in semantic.iter() {
            let start_ix = tokens
                .binary_search_by(|probe| {
                    resolve(&probe.range.end)
                        .cmp(&range.start)
                        .then(std::cmp::Ordering::Less)
                })
                .unwrap_or_else(|i| i);

            for (offset_in_slice, token) in tokens[start_ix..].iter().enumerate() {
                let s = resolve(&token.range.start);
                let e = resolve(&token.range.end);
                if s >= range.end {
                    break;
                }
                if s == e {
                    continue;
                }
                // Unique slot per token keeps nested captures (e.g. escape
                // inside string) in distinct entries of the merger's active map.
                let key = HighlightKey::new(
                    HighlightLayer::SemanticToken,
                    (start_ix + offset_in_slice) as u32,
                );
                endpoints.push(HighlightEndpoint {
                    offset: s,
                    is_start: true,
                    key,
                    style: Some(interner[token.style].clone()),
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

    endpoints.sort();
    endpoints
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

impl<'a> BufferChunks<'a> {
    /// Construct a new iterator over `rope[range]` applying `endpoints`.
    ///
    /// `endpoints` must be sorted by offset and must only cover offsets within
    /// `range`. Use [`create_highlight_endpoints`] (or the cached variant) to
    /// build them.
    pub fn new(rope: &'a Rope, range: Range<usize>, endpoints: Arc<[HighlightEndpoint]>) -> Self {
        let start = range.start;
        let end = range.end;
        Self {
            text_chunks: rope.chunks_in_range(range),
            pending: "",
            offset: start,
            end,
            endpoints,
            ep_idx: 0,
            active: BTreeMap::new(),
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
        create_highlight_endpoints, highlighted_chunks, Chunk, HighlightKey, HighlightLayer,
        HighlightStyle, TextHighlights,
    };
    use ratatui::style::Color;
    use std::{collections::HashMap, ops::Range, sync::Arc};
    use stoat_text::{Anchor, Bias};

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

    #[test]
    fn no_highlights() {
        let text = "hello world";
        let highlights = Arc::new(HashMap::new());
        let resolve = |a: &Anchor| a.offset as usize;
        let eps = create_highlight_endpoints(&(0..text.len()), &highlights, None, &resolve);
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
        let highlights = make_highlights(vec![(
            HighlightKey::layer(HighlightLayer::SearchHighlight),
            style.clone(),
            vec![6..11],
        )]);
        let resolve = |a: &Anchor| a.offset as usize;
        let eps = create_highlight_endpoints(&(0..text.len()), &highlights, None, &resolve);
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
        let eps = create_highlight_endpoints(&(0..text.len()), &highlights, None, &resolve);
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
        let highlights = make_highlights(vec![(
            HighlightKey::layer(HighlightLayer::SearchHighlight),
            HighlightStyle {
                foreground: Some(Color::Red),
                ..Default::default()
            },
            vec![2..2],
        )]);
        let resolve = |a: &Anchor| a.offset as usize;
        let eps = create_highlight_endpoints(&(0..text.len()), &highlights, None, &resolve);
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
        use super::{HighlightStyleInterner, SemanticTokenHighlight, SemanticTokensHighlights};
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
        semantic_map.insert(BufferId::new(0), (tokens, Arc::new(interner)));
        let semantic: SemanticTokensHighlights = Arc::new(semantic_map);
        let text_hl: TextHighlights = Arc::new(HashMap::new());
        let resolve = |a: &Anchor| a.offset as usize;

        let eps = create_highlight_endpoints(&(0..text.len()), &text_hl, Some(&semantic), &resolve);
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

        for byte in 50..55 {
            assert_eq!(colors[byte], Some(Color::Red), "byte {byte} must be red");
        }
        for byte in 55..60 {
            assert_eq!(colors[byte], None, "byte {byte} must be unstyled (gap)");
        }
        for byte in 60..65 {
            assert_eq!(colors[byte], Some(Color::Blue), "byte {byte} must be blue");
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
