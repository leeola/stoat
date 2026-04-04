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
use stoat_text::Anchor;

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

/// Precedence order. Derived `Ord`: lower variant = applied first (overridden by later).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HighlightKey {
    ColorizeBracket(usize),
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

            for token in &tokens[start_ix..] {
                let s = resolve(&token.range.start);
                let e = resolve(&token.range.end);
                if s >= range.end {
                    break;
                }
                if s == e {
                    continue;
                }
                endpoints.push(HighlightEndpoint {
                    offset: s,
                    is_start: true,
                    key: HighlightKey::SemanticToken,
                    style: Some(interner[token.style].clone()),
                });
                endpoints.push(HighlightEndpoint {
                    offset: e,
                    is_start: false,
                    key: HighlightKey::SemanticToken,
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
        let split_at = next_boundary.min(remaining.len());

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

#[cfg(test)]
mod tests {
    use super::{
        create_highlight_endpoints, highlighted_chunks, HighlightKey, HighlightStyle,
        TextHighlights,
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
            HighlightKey::SearchHighlight,
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
            (HighlightKey::SyntaxToken, style_low, vec![2..8]),
            (HighlightKey::MatchingBracket, style_high, vec![4..6]),
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
            HighlightKey::SearchHighlight,
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
}
