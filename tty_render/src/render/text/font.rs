//! Font resolution and text shaping, independent of any render pass.
//!
//! Turning a character or a run of them into glyph cache keys needs a font
//! database, a coverage test, and a cosmic-text shaping buffer, none of which
//! depend on GPU state. This module holds that half so the pass above it deals
//! only in the keys it gets back.

use crate::render::CellMetrics;
use cosmic_text::{
    fontdb::{Database, Query, Weight},
    Attrs, AttrsList, Buffer as CosmicBuffer, CacheKey, Ellipsize, Family, Font, FontSystem,
    Hinting, LayoutLine, Metrics, ShapeBuffer, ShapeLine, Shaping, Wrap,
};
use rustc_hash::FxHashMap;
use std::sync::Arc;
use ttf_parser::{
    gsub::SubstitutionSubtable,
    opentype_layout::{ClassDefinition, Coverage},
    Face as TtfFace,
};

/// Family name of the bundled text face, registered by [`load_bundled_fonts`].
///
/// The shipped config shapes with it, and it is what the generic monospace
/// resolves to, so it is present whatever the system holds.
pub(super) const BUNDLED_FAMILY: &str = "JetBrains Mono";

/// Family name of the bundled Nerd Font, registered by [`load_bundled_fonts`].
///
/// Carries the Private-Use-Area powerline separators and icon glyphs that
/// programming fonts omit, so it serves as the symbol fallback ahead of any
/// system font (see [`glyph_family`]).
pub(super) const SYMBOLS_FAMILY: &str = "Symbols Nerd Font Mono";

/// Words a [`CoveredSet`] holds, one bit per Unicode codepoint.
const UNICODE_WORDS: usize = 0x11_0000 / 64;

/// Shape `ch` on its own at `scale` times the cell size and return its glyph
/// cache key, or `None` if it produces no glyph.
///
/// One character maps to one cell, so each is shaped independently rather than
/// through proportional line layout. The cache key encodes the rasterization
/// size, so each scale of a character keys a distinct atlas entry.
///
/// `primary` is the preferred family; glyphs it lacks are shaped with the
/// bundled symbols font instead (see [`glyph_family`]).
pub(super) fn shape_char(
    font_system: &mut FontSystem,
    ch: char,
    scale: f32,
    metrics: CellMetrics,
    primary: Family<'_>,
    weight: Weight,
    covered: &mut Option<CoveredSet>,
) -> Option<CacheKey> {
    let (family, any_face_covers) = glyph_family(font_system, ch, primary, covered);
    // Nothing in the database maps this character, so a fallback walk would
    // load and shape every face only to end on the symbols face's glyph zero.
    // The basic path maps through that face's own charmap and reaches the same
    // key without the walk.
    let shaping = match any_face_covers {
        true => Shaping::Advanced,
        false => Shaping::Basic,
    };
    let size = scale;
    let mut buffer = CosmicBuffer::new(
        font_system,
        Metrics::new(metrics.font_size * size, metrics.height * size),
    );
    let mut encoded = [0u8; 4];
    let text = ch.encode_utf8(&mut encoded);
    buffer.set_text(
        font_system,
        text,
        &Attrs::new().family(family).weight(weight),
        shaping,
        None,
    );
    buffer.shape_until_scroll(font_system, false);

    let run = buffer.layout_runs().next()?;
    let glyph = run.glyphs.first()?;
    Some(glyph.physical((0.0, 0.0), 1.0).cache_key)
}

/// GSUB features a shaping run turns on for horizontal text without being
/// asked, so a lookup only some other feature reaches never fires here.
///
/// The list errs long rather than short. Leaving one out drops whatever
/// ligature it forms, where an extra one only keeps a run on the shaping path
/// it was already on.
const DEFAULT_FEATURES: [ttf_parser::Tag; 8] = [
    ttf_parser::Tag::from_bytes(b"ccmp"),
    ttf_parser::Tag::from_bytes(b"locl"),
    ttf_parser::Tag::from_bytes(b"rvrn"),
    ttf_parser::Tag::from_bytes(b"rlig"),
    ttf_parser::Tag::from_bytes(b"rclt"),
    ttf_parser::Tag::from_bytes(b"calt"),
    ttf_parser::Tag::from_bytes(b"liga"),
    ttf_parser::Tag::from_bytes(b"clig"),
];

/// What a run must hold for the face to substitute inside it.
///
/// A coverage says only that a glyph can open some rule. Every glyph of a
/// `calt` sequence carries its own rule with its partners in backtrack or
/// lookahead, so a coverage alone sends every run holding a bracket to the
/// shaper. A rule's other positions say whether the run could satisfy it at
/// all.
///
/// Over-approximates in one direction only, so a ligature is never dropped. A
/// position matching the catch-all class is left out rather than enumerated,
/// and a subtable kind this does not model keeps its coverage as an
/// unconditional trigger. Either way a run is sometimes shaped for nothing, and
/// never left unshaped where a substitution was there to make.
#[derive(Default)]
pub(crate) struct SubstitutionRules {
    /// Rules by the glyph their first input position names, so a run tests only
    /// the rules its own glyphs could open.
    by_first: FxHashMap<u16, Vec<Rule>>,
    /// Glyphs of a subtable kind the rules do not model, which trigger on their
    /// own. Sorted.
    always: Vec<u16>,
}

/// The positions a run must satisfy for one rule to fire.
struct Rule {
    positions: Vec<Position>,
}

/// One position of a rule, as what satisfies it.
enum Position {
    /// This glyph, `count` times over the whole rule. A rule over two dots
    /// needs two of them, which is what separates `..` from a lone `.`.
    One { glyph: u16, count: u16 },
    /// Any one of these, sorted. A class, or a coverage of several glyphs.
    /// Shared between the rules of one subtable, which all read the same sides.
    Any(Arc<[u16]>),
}

impl SubstitutionRules {
    /// Whether the face could substitute inside a run of `glyphs`.
    ///
    /// False means shaping the run returns exactly what shaping each glyph
    /// alone returns, so the caller may skip the shaper.
    pub(crate) fn reshapes(&self, glyphs: &[u16]) -> bool {
        glyphs.iter().any(|id| {
            self.always.binary_search(id).is_ok()
                || self
                    .by_first
                    .get(id)
                    .is_some_and(|rules| rules.iter().any(|rule| rule.satisfied(glyphs)))
        })
    }

    /// How many rules the table holds, for a caller reporting what it built.
    pub(crate) fn len(&self) -> usize {
        self.by_first.values().map(Vec::len).sum::<usize>() + self.always.len()
    }

    /// File a rule under the glyph its first input position names.
    fn push(&mut self, first: u16, positions: Vec<Position>) {
        self.by_first
            .entry(first)
            .or_default()
            .push(Rule { positions });
    }
}

impl Rule {
    fn satisfied(&self, glyphs: &[u16]) -> bool {
        self.positions.iter().all(|position| match position {
            Position::One { glyph, count } => {
                glyphs.iter().filter(|&held| held == glyph).count() >= usize::from(*count)
            },
            Position::Any(set) => glyphs.iter().any(|held| set.binary_search(held).is_ok()),
        })
    }
}

/// Every rule the face can fire under [`DEFAULT_FEATURES`], or an empty table
/// where it can fire none.
///
/// Empty for a face with no GSUB table, one that fails to parse, or one inside
/// a collection whose index does not resolve. Each of those reads as
/// "substitutes nothing", which costs shaping the caller paid anyway.
pub(super) fn substitution_rules(font: &Font) -> SubstitutionRules {
    let data = font.data();
    let mut rules = SubstitutionRules::default();

    let Some(index) = face_index(data, font.as_swash().offset) else {
        return rules;
    };
    let Ok(face) = TtfFace::parse(data, index) else {
        return rules;
    };
    let Some(gsub) = face.tables().gsub else {
        return rules;
    };

    for index in default_lookups(&gsub) {
        let Some(lookup) = gsub.lookups.get(index) else {
            continue;
        };
        for subtable in lookup.subtables.into_iter::<SubstitutionSubtable<'_>>() {
            collect_rules(&face, &subtable, &mut rules);
        }
    }

    rules.always.sort_unstable();
    rules.always.dedup();
    rules
}

/// Lookup indices a horizontal Latin run reaches without asking.
///
/// Through the default language system of `DFLT` and `latn` rather than the
/// whole feature list. The bundled face files its Turkish `locl` singles under
/// a language system only a Turkish run selects, and reading the feature list
/// instead puts every one of those glyphs on the shaping path.
fn default_lookups(gsub: &ttf_parser::opentype_layout::LayoutTable<'_>) -> Vec<u16> {
    let mut features: Vec<u16> = Vec::new();
    for tag in [
        ttf_parser::Tag::from_bytes(b"DFLT"),
        ttf_parser::Tag::from_bytes(b"latn"),
    ] {
        let Some(script) = gsub.scripts.find(tag) else {
            continue;
        };
        let Some(language) = script.default_language else {
            continue;
        };
        features.extend(language.required_feature);
        features.extend(language.feature_indices);
    }

    let mut lookups: Vec<u16> = Vec::new();
    for index in features {
        let Some(feature) = gsub.features.get(index) else {
            continue;
        };
        if !DEFAULT_FEATURES.contains(&feature.tag) {
            continue;
        }
        lookups.extend(feature.lookup_indices);
    }
    lookups.sort_unstable();
    lookups.dedup();
    lookups
}

/// Read one subtable's rules into `rules`.
fn collect_rules(
    face: &TtfFace<'_>,
    subtable: &SubstitutionSubtable<'_>,
    rules: &mut SubstitutionRules,
) {
    match subtable {
        // One glyph in, one or more out. Nothing else has to be present.
        SubstitutionSubtable::Single(_)
        | SubstitutionSubtable::Multiple(_)
        | SubstitutionSubtable::Alternate(_) => {
            for glyph in coverage_glyphs(&subtable.coverage()) {
                rules.push(glyph, vec![Position::One { glyph, count: 1 }]);
            }
        },
        SubstitutionSubtable::Ligature(ligature) => {
            for (index, glyph) in coverage_glyphs(&ligature.coverage).into_iter().enumerate() {
                let Some(set) = ligature.ligature_sets.get(index as u16) else {
                    continue;
                };
                for entry in set {
                    let named =
                        std::iter::once(glyph).chain(entry.components.into_iter().map(|id| id.0));
                    rules.push(glyph, positions(named.map(Named::One)));
                }
            }
        },
        SubstitutionSubtable::ChainContext(chain) => collect_chain_rules(face, chain, rules),
        // A context or a reverse-chain rule this does not read, so its glyphs
        // stay triggers on their own.
        SubstitutionSubtable::Context(_) | SubstitutionSubtable::ReverseChainSingle(_) => {
            rules.always.extend(coverage_glyphs(&subtable.coverage()));
        },
    }
}

/// What one position of a rule names, before the counts are folded together.
enum Named {
    One(u16),
    Any(Arc<[u16]>),
    /// The catch-all class, or anything else this does not enumerate. No
    /// constraint on the run.
    Anything,
}

/// Read one chained-context subtable's rules into `rules`.
fn collect_chain_rules(
    face: &TtfFace<'_>,
    chain: &ttf_parser::opentype_layout::ChainedContextLookup<'_>,
    rules: &mut SubstitutionRules,
) {
    use ttf_parser::opentype_layout::ChainedContextLookup;

    match chain {
        ChainedContextLookup::Format1 { coverage, sets } => {
            for (index, glyph) in coverage_glyphs(coverage).into_iter().enumerate() {
                let Some(set) = sets.get(index as u16) else {
                    continue;
                };
                for rule in set {
                    if rule.lookups.is_empty() {
                        continue;
                    }
                    let named = std::iter::once(glyph).chain(
                        rule.backtrack
                            .into_iter()
                            .chain(rule.input)
                            .chain(rule.lookahead),
                    );
                    rules.push(glyph, positions(named.map(Named::One)));
                }
            }
        },
        ChainedContextLookup::Format2 {
            coverage,
            backtrack_classes,
            input_classes,
            lookahead_classes,
            sets,
        } => {
            let backtrack = class_members(face, backtrack_classes);
            let input = class_members(face, input_classes);
            let lookahead = class_members(face, lookahead_classes);
            let named_class =
                |members: &FxHashMap<u16, Arc<[u16]>>, class: u16| match members.get(&class) {
                    Some(glyphs) if glyphs.len() == 1 => Named::One(glyphs[0]),
                    Some(glyphs) => Named::Any(Arc::clone(glyphs)),
                    None => Named::Anything,
                };

            for glyph in coverage_glyphs(coverage) {
                let class = input_classes.get(ttf_parser::GlyphId(glyph));
                let Some(set) = sets.get(class) else {
                    continue;
                };
                for rule in set {
                    if rule.lookups.is_empty() {
                        continue;
                    }
                    let named = std::iter::once(Named::One(glyph))
                        .chain(
                            rule.backtrack
                                .into_iter()
                                .map(|c| named_class(&backtrack, c)),
                        )
                        .chain(rule.input.into_iter().map(|c| named_class(&input, c)))
                        .chain(
                            rule.lookahead
                                .into_iter()
                                .map(|c| named_class(&lookahead, c)),
                        );
                    rules.push(glyph, positions(named));
                }
            }
        },
        ChainedContextLookup::Format3 {
            coverage,
            backtrack_coverages,
            input_coverages,
            lookahead_coverages,
            lookups,
        } => {
            if lookups.is_empty() {
                return;
            }
            // Read once per subtable rather than once per coverage glyph. Every
            // rule of a format 3 subtable reads the same sides.
            let sides: Vec<Named> = [backtrack_coverages, input_coverages, lookahead_coverages]
                .into_iter()
                .flat_map(|side| (0..side.len()).filter_map(|i| side.get(i)))
                .map(|coverage| named_coverage(&coverage))
                .collect();

            for glyph in coverage_glyphs(coverage) {
                let named =
                    std::iter::once(Named::One(glyph)).chain(sides.iter().map(Named::clone_of));
                rules.push(glyph, positions(named));
            }
        },
    }
}

impl Named {
    /// A copy of `self`, sharing the glyph list of a wide position rather than
    /// copying it.
    fn clone_of(&self) -> Named {
        match self {
            Named::One(glyph) => Named::One(*glyph),
            Named::Any(glyphs) => Named::Any(Arc::clone(glyphs)),
            Named::Anything => Named::Anything,
        }
    }
}

/// Fold the positions a rule names into the run test, counting the glyphs a
/// position names alone and dropping the ones that constrain nothing.
fn positions(named: impl Iterator<Item = Named>) -> Vec<Position> {
    let mut out: Vec<Position> = Vec::new();
    for name in named {
        match name {
            Named::Anything => {},
            Named::Any(glyphs) => out.push(Position::Any(glyphs)),
            Named::One(glyph) => {
                match out.iter_mut().find(
                    |held| matches!(held, Position::One { glyph: held, .. } if *held == glyph),
                ) {
                    Some(Position::One { count, .. }) => *count += 1,
                    _ => out.push(Position::One { glyph, count: 1 }),
                }
            },
        }
    }
    out
}

/// What `coverage` names as one position of a rule.
fn named_coverage(coverage: &Coverage<'_>) -> Named {
    let mut glyphs = coverage_glyphs(coverage);
    match glyphs.len() {
        0 => Named::Anything,
        1 => Named::One(glyphs[0]),
        _ => {
            glyphs.sort_unstable();
            Named::Any(glyphs.into())
        },
    }
}

/// The glyphs `coverage` names, in order.
fn coverage_glyphs(coverage: &Coverage<'_>) -> Vec<u16> {
    match coverage {
        Coverage::Format1 { glyphs } => glyphs.into_iter().map(|id| id.0).collect(),
        Coverage::Format2 { records } => records
            .into_iter()
            .flat_map(|record| record.start.0..=record.end.0)
            .collect(),
    }
}

/// The glyphs of each class the definition names, sorted, by class.
///
/// Class zero holds every glyph the definition does not name, which constrains
/// nothing, so it is left out rather than enumerated.
fn class_members(face: &TtfFace<'_>, classes: &ClassDefinition<'_>) -> FxHashMap<u16, Arc<[u16]>> {
    let mut members: FxHashMap<u16, Vec<u16>> = FxHashMap::default();
    for id in 1..face.number_of_glyphs() {
        let class = classes.get(ttf_parser::GlyphId(id));
        if class != 0 {
            members.entry(class).or_default().push(id);
        }
    }
    members
        .into_iter()
        .map(|(class, glyphs)| (class, glyphs.into()))
        .collect()
}

/// The index `ttf_parser` selects a face by, given the byte offset of that
/// face's table directory.
///
/// A single font file has one face at index zero. A collection lists its
/// faces' offsets after the `ttcf` tag, so the index is the position of the
/// offset in that list. Nothing else identifies a face across the two crates:
/// one names a face by index and the other by offset.
fn face_index(data: &[u8], offset: u32) -> Option<u32> {
    let Some(faces) = ttf_parser::fonts_in_collection(data) else {
        return Some(0);
    };

    (0..faces).find(|index| {
        // The header is the tag, the version, the count, then one big-endian
        // offset per face.
        let at = 12 + *index as usize * 4;
        data.get(at..at + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .is_some_and(|bytes| u32::from_be_bytes(bytes) == offset)
    })
}

/// Allocations [`shape_run`] reuses from one call to the next.
///
/// A novel screen shapes one run per word, some six hundred times a frame, and
/// building the line and its layout afresh each time is about a third of what
/// that costs. A caller holds one of these for as long as it shapes.
///
/// `line` is an option because `ShapeLine` has no `Default` and cosmic-text
/// keeps its empty constructor to itself, so the first call seeds it through
/// the public one.
#[derive(Default)]
pub(super) struct ShapeScratch {
    line: Option<ShapeLine>,
    buffer: ShapeBuffer,
    layout: Vec<LayoutLine>,
    /// Characters [`shape_run`] has laid out through this scratch. A cache
    /// holding a run says nothing about whether the shaper built it, since a
    /// caller may fill an entry from per-character keys instead.
    #[cfg(test)]
    shaped_chars: usize,
}

/// Shape `text` as one run with `family`, returning each glyph's source byte
/// offset paired with its cache key.
///
/// Shaping the run as a single string lets the font's contextual alternates
/// merge adjacent characters into ligature glyphs. The returned byte offset is
/// the start of each glyph's source cluster, which maps the glyph back to the
/// column it begins at. A ligature glyph maps to the column of its first
/// character. Each glyph is keyed at subpixel bin zero, since the grid draws it
/// at an integer cell origin.
pub(super) fn shape_run(
    scratch: &mut ShapeScratch,
    font_system: &mut FontSystem,
    text: &str,
    metrics: CellMetrics,
    family: Family<'_>,
) -> Vec<(usize, CacheKey)> {
    #[cfg(test)]
    {
        scratch.shaped_chars += text.chars().count();
    }

    let attrs = AttrsList::new(&Attrs::new().family(family));
    let line = match &mut scratch.line {
        Some(line) => {
            line.build(font_system, text, &attrs, Shaping::Advanced, TAB_WIDTH);
            line
        },
        none => none.insert(ShapeLine::new(
            font_system,
            text,
            &attrs,
            Shaping::Advanced,
            TAB_WIDTH,
        )),
    };

    // No width bound, so the run lays out as one line and nothing wraps. The
    // layout is cosmic-text's rather than ours because a fallback font whose
    // monospace em width differs from the primary's has its size adjusted here,
    // and that adjusted size is part of the cache key below.
    scratch.layout.clear();
    line.layout_to_buffer(
        &mut scratch.buffer,
        metrics.font_size,
        None,
        Wrap::None,
        Ellipsize::None,
        None,
        &mut scratch.layout,
        None,
        Hinting::Disabled,
    );

    let Some(first) = scratch.layout.first() else {
        return Vec::new();
    };
    first
        .glyphs
        .iter()
        .map(|glyph| {
            let pixel_aligned = (-(glyph.x + glyph.font_size * glyph.x_offset), 0.0);
            (glyph.start, glyph.physical(pixel_aligned, 1.0).cache_key)
        })
        .collect()
}

/// Columns a tab advances by while shaping, matching [`CosmicBuffer`]'s own
/// default.
///
/// A run reaching here holds one grid row's text, where the terminal has
/// already expanded tabs into spaces, so nothing depends on this. It matches
/// the buffer's default so a run that did carry one shapes the way it always
/// has.
const TAB_WIDTH: u16 = 8;

/// The number of shaped runs the cache holds before it evicts to make room.
///
/// A run is one word, and a screenful of distinct words is under a thousand,
/// so this holds several screens of scrolled-past content and still bounds the
/// cache at a couple of megabytes of run text and glyph vectors. A stream of
/// unique words, which is what log, hex, and UUID output is, settles here and
/// evicts rather than growing.
const RUN_SHAPE_CACHE_CAP: usize = 4096;

/// The glyphs `text` lays out as one run, from `cache` or from `miss`.
///
/// The run text alone keys the result, since runs group only same-scale
/// primary-covered cells in one family. On a miss `miss` builds the glyphs and
/// they are stored, evicting a run nothing has asked for lately once the cache
/// is full.
///
/// `miss` decides how to build them, because the two ways cost very different
/// things. A run holding something the face reshapes has to go through
/// [`shape_run`], which rebuilds a cosmic-text buffer. A run holding nothing it
/// reshapes lays out as its characters do, which a per-character cache answers
/// far more cheaply. Either way the answer is cached, so a hit costs one hash
/// and neither.
pub(super) fn shape_run_cached<'a>(
    cache: &'a mut RunShapeCache,
    text: &str,
    miss: impl FnOnce(&mut ShapeScratch) -> Vec<(usize, CacheKey)>,
) -> &'a [(usize, CacheKey)] {
    // The slot copies out, so the lookup's borrow ends before the miss path
    // needs the cache mutably. A map holding the glyphs themselves cannot do that,
    // and pays a second hash to read what the first already found.
    if let Some(&slot) = cache.at.get(text) {
        cache.runs[slot].asked_for = true;
        return &cache.runs[slot].glyphs;
    }

    let glyphs = miss(&mut cache.scratch);
    let slot = cache.store(text, glyphs);
    &cache.runs[slot].glyphs
}

/// One cached run, holding the text it was shaped from, its glyphs, and
/// whether anyone has asked for it since the eviction hand last passed it.
struct CachedRun {
    /// Shares its allocation with the key [`RunShapeCache::at`] holds, so the
    /// slot unmaps itself on eviction without storing the text twice.
    key: Arc<str>,
    glyphs: Vec<(usize, CacheKey)>,
    asked_for: bool,
}

/// Shaped glyphs of each ligature run, found by the run's text.
///
/// The glyphs live in a Vec and the map holds positions into it, so a hit costs one
/// hash and an index. Holding the glyphs in the map instead would cost a second hash
/// on every hit, to read what the first lookup already proved was there.
///
/// Past [`RUN_SHAPE_CACHE_CAP`] runs the Vec stops growing and becomes the ring
/// a second-chance sweep evicts around. Terminal content repeats, so a
/// flush-everything bound throws away precisely the rows still on screen.
/// Evicting only runs nothing asked for keeps those and drops the stream of
/// one-off runs that pushed the cache to its bound.
#[derive(Default)]
pub struct RunShapeCache {
    at: FxHashMap<Arc<str>, usize>,
    runs: Vec<CachedRun>,
    /// Shaping allocations the misses reuse, held here because this is already
    /// the thing a caller keeps for as long as it shapes.
    scratch: ShapeScratch,
    /// Slot the next eviction sweep starts at, which trails the most recent
    /// insert so a fresh run gets a full pass around the ring before it is
    /// considered.
    hand: usize,
}

#[cfg(test)]
impl RunShapeCache {
    /// Every run text held, sorted, so a caller can assert what was shaped
    /// rather than only how much was.
    pub(super) fn cached_texts(&self) -> Vec<String> {
        let mut texts: Vec<String> = self.at.keys().map(|key| key.to_string()).collect();
        texts.sort();
        texts
    }

    /// The glyphs held for `text`, or `None` when it was never shaped.
    pub(super) fn cached_glyphs(&self, text: &str) -> Option<&[(usize, CacheKey)]> {
        self.at.get(text).map(|&slot| &self.runs[slot].glyphs[..])
    }

    /// Characters the shaper laid out to fill this cache, which is the work a
    /// screen of text costs. A run whose entry was built from per-character keys
    /// counts nothing, since the shaper never saw it.
    pub(super) fn shaped_chars(&self) -> usize {
        self.scratch.shaped_chars
    }
}

impl RunShapeCache {
    /// Drop every shaped run, so the two halves cannot disagree about what is here.
    pub(super) fn clear(&mut self) {
        self.at.clear();
        self.runs.clear();
        self.hand = 0;
        #[cfg(test)]
        {
            self.scratch.shaped_chars = 0;
        }
    }

    /// Store `glyphs` under `text` and return the slot holding them.
    ///
    /// Below the cap this appends. At the cap it evicts through [`Self::sweep`]
    /// and reuses the slot that comes back.
    ///
    /// `text` must not already be cached. Storing it twice leaves the earlier
    /// slot holding the same key with nothing mapped to it, and evicting that
    /// slot then unmaps the later one. The only caller is
    /// [`shape_run_cached`]'s miss path, which has just proved the key absent.
    fn store(&mut self, text: &str, glyphs: Vec<(usize, CacheKey)>) -> usize {
        let key: Arc<str> = Arc::from(text);
        let run = CachedRun {
            key: Arc::clone(&key),
            glyphs,
            asked_for: false,
        };

        let slot = if self.runs.len() < RUN_SHAPE_CACHE_CAP {
            self.runs.push(run);
            self.runs.len() - 1
        } else {
            let slot = self.sweep();
            self.runs[slot] = run;
            slot
        };

        self.at.insert(key, slot);
        slot
    }

    /// Evict one run and return its now-free slot.
    ///
    /// The hand walks the ring and clears the mark on every run it finds asked
    /// for, which is the second chance those runs get. The first unmarked run it
    /// reaches is evicted. One full pass clears every mark, so the walk ends
    /// within two laps however hot the cache is.
    fn sweep(&mut self) -> usize {
        loop {
            let slot = self.hand;
            self.hand = (self.hand + 1) % self.runs.len();

            if self.runs[slot].asked_for {
                self.runs[slot].asked_for = false;
                continue;
            }

            self.at.remove(&self.runs[slot].key);
            return slot;
        }
    }
}

/// Fill `text` and `col_of_byte` with the run's shaping string and a per-byte
/// map from string offset to grid column, clearing both first.
///
/// Each cell contributes its character. Every byte of that character maps to the
/// cell's column, so a shaped glyph's [`start`](cosmic_text::LayoutGlyph::start)
/// byte resolves to the column it originates at, even across multi-byte
/// characters.
pub(super) fn run_text_and_columns_into(
    cells: &[(usize, char)],
    text: &mut String,
    col_of_byte: &mut Vec<usize>,
) {
    text.clear();
    col_of_byte.clear();
    for &(col, ch) in cells {
        text.push(ch);
        col_of_byte.resize(text.len(), col);
    }
}

/// The cosmic-text family to shape `ch` with: `primary` when it carries the
/// glyph, otherwise the bundled symbols font so Private-Use-Area icons resolve
/// to it ahead of cosmic-text's system fallback.
pub(super) fn glyph_family<'a>(
    font_system: &mut FontSystem,
    ch: char,
    primary: Family<'a>,
    covered: &mut Option<CoveredSet>,
) -> (Family<'a>, bool) {
    if family_covers(font_system, primary, ch) {
        return (primary, true);
    }

    let symbols = Family::Name(SYMBOLS_FAMILY);
    if family_covers(font_system, symbols, ch) {
        return (symbols, true);
    }

    // Both bundled charmaps missed, which is the only case worth the set. A
    // launch whose text stays inside them never builds it.
    let set = covered.get_or_insert_with(|| CoveredSet::of(font_system.db()));
    (symbols, set.covers(ch))
}

/// Every codepoint some face in a font database maps.
///
/// A character no face covers is what this exists for. Shaping one with fallback
/// walks the whole database, loading each face and shaping the character with it,
/// and leaves every face resident for the session. One lookup here decides
/// instead whether the walk can find anything.
///
/// Built from the faces' character maps rather than by loading them, so the cost
/// is one pass over the database and the pages its maps sit on.
pub(super) struct CoveredSet(Box<[u64; UNICODE_WORDS]>);

impl CoveredSet {
    /// Build the union of every face's character map in `db`.
    pub(super) fn of(db: &Database) -> CoveredSet {
        let mut bits = Box::new([0u64; UNICODE_WORDS]);
        for face in db.faces() {
            db.with_face_data(face.id, |data, index| {
                let Ok(parsed) = TtfFace::parse(data, index) else {
                    return;
                };
                let Some(cmap) = parsed.tables().cmap else {
                    return;
                };
                for subtable in cmap.subtables {
                    subtable.codepoints(|codepoint| {
                        if let Some(word) = bits.get_mut(codepoint as usize / 64) {
                            *word |= 1 << (codepoint % 64);
                        }
                    });
                }
            });
        }
        CoveredSet(bits)
    }

    /// Whether some face maps `ch`.
    ///
    /// A face may still map it to glyph zero, which `codepoints` reports as
    /// defined. Answering `true` there costs a fallback walk that finds nothing,
    /// which is what the character would have cost anyway.
    pub(super) fn covers(&self, ch: char) -> bool {
        let codepoint = ch as usize;
        self.0
            .get(codepoint / 64)
            .is_some_and(|word| word & (1 << (codepoint % 64)) != 0)
    }
}

/// Whether the face that `family` resolves to in `font_system` has a glyph for
/// `ch`.
///
/// Checks the resolved face's character map directly, so the answer reflects the
/// face that would actually shape `ch` rather than cosmic-text's fallback chain.
fn family_covers(font_system: &mut FontSystem, family: Family<'_>, ch: char) -> bool {
    let Some(id) = font_system.db().query(&Query {
        families: &[family],
        ..Default::default()
    }) else {
        return false;
    };

    font_system
        .get_font(id, Weight::NORMAL)
        .is_some_and(|font| font_covers(&font, ch))
}

/// Whether `font` has a glyph for `ch`, read from its character map.
pub(super) fn font_covers(font: &Font, ch: char) -> bool {
    font.as_swash().charmap().map(ch) != 0
}

/// Resolve the primary shaping `family` to its face, for the per-cell coverage
/// test. `None` when no family resolves, so coverage falls through to the
/// fallback font. Looked up once when the family is set, since it is fixed for
/// the pass's lifetime.
pub(super) fn resolve_primary_font(
    font_system: &mut FontSystem,
    family: Option<&str>,
) -> Option<Arc<Font>> {
    let id = font_system.db().query(&Query {
        families: &[shape_family(family)],
        ..Default::default()
    })?;
    font_system.get_font(id, Weight::NORMAL)
}

/// Build the [`FontSystem`] a [`super::TextPass`] shapes with: the bundled faces
/// plus every font installed on the system.
///
/// Enumerating the system fonts dominates renderer startup, and this needs no
/// window or GPU, so it is run on a background thread (see
/// [`GpuContext::new`](crate::gpu::GpuContext::new)) concurrently with the
/// main-thread surface and device setup.
///
/// See also:
/// - [`bundled_font_system`] for the half that reads no file, which a first frame shapes with while
///   the scan runs.
pub fn build_font_system() -> FontSystem {
    FontSystem::new_with_locale_and_db(locale(), scan_system_fonts(bundled_database()))
}

/// Build a [`FontSystem`] over the bundled faces alone, opening no file.
///
/// The shipped config shapes with the bundled JetBrains Mono, so a launch that
/// keeps to it can draw its first frame from this and let the system scan finish
/// behind the frame. [`scan_system_fonts`] takes this system's database and
/// returns the same faces at the same ids plus everything installed, which is
/// what lets the two be swapped under a running pass without reshaping a cell
/// differently.
///
/// The generic monospace points at the bundled family rather than at
/// cosmic-text's `Noto Sans Mono`, which the bundled database does not hold. A
/// scanned system points it at the same family, so text that shaped through the
/// generic keeps its metrics across the swap.
pub fn bundled_font_system() -> FontSystem {
    bundled_font_system_with_locale(locale())
}

/// Build a [`FontSystem`] over the bundled faces alone, matching against
/// `locale`.
///
/// Split from [`bundled_font_system`] so the database build stays clear of the
/// environment the locale is read from.
pub(crate) fn bundled_font_system_with_locale(locale: String) -> FontSystem {
    FontSystem::new_with_locale_and_db(locale, bundled_database())
}

/// A font database holding the bundled faces alone.
///
/// The seed [`scan_system_fonts`] adds the installed fonts to, and what
/// [`bundled_font_system`] shapes against. Both start here, so the bundled faces
/// carry the same ids whichever of the two a pass holds.
pub(crate) fn bundled_database() -> Database {
    let mut db = Database::new();
    load_bundled_fonts(&mut db);
    db.set_monospace_family(BUNDLED_FAMILY);
    db
}

/// Add every font installed on the system to `db`.
///
/// Opens and parses each font file in each configured directory, which is the
/// dominant cost of a cold launch. The faces already in `db` keep their ids, so
/// a database seeded by [`bundled_font_system`] comes back a superset of itself.
///
/// The scan reads the platform's own generic-family aliases, which name a family
/// that need not be installed, so the generic monospace is pointed back at
/// [`BUNDLED_FAMILY`] afterward. A bundled database and a scanned one must agree
/// on it, or text that shaped through the generic changes metrics when one
/// replaces the other.
pub fn scan_system_fonts(mut db: Database) -> Database {
    db.load_system_fonts();
    db.set_monospace_family(BUNDLED_FAMILY);
    db
}

/// The locale font matching resolves against, the way cosmic-text reads it.
fn locale() -> String {
    sys_locale::get_locale().unwrap_or_else(|| {
        tracing::warn!("no system locale reported, falling back to en-US");
        String::from("en-US")
    })
}

/// Shape every space-delimited word of `text` at `font_size`, returning the
/// total glyph count.
///
/// The measurement entry point for what shaping costs, with no cache and no
/// GPU between the caller and the work. A grid row shapes one run per word, so
/// this is the same shaping a screenful of never-seen text drives, minus
/// everything a frame does around it.
///
/// Every word is shaped afresh. The run cache the renderer keeps would turn a
/// second pass over the same words into hits, and the miss is the cost worth
/// knowing. The glyph count comes back so a caller cannot have the work
/// optimized out from under it.
///
/// See also:
/// - [`build_font_system`] for the font system to hand in, which is where the bundled faces this
///   resolves against are registered.
pub fn shape_words(font_system: &mut FontSystem, font_size: u32, text: &str) -> usize {
    let primary = resolve_primary_family(font_system, &["JetBrains Mono".to_owned()]);
    let family = shape_family(primary.as_deref());
    let metrics = CellMetrics::from_font_size(font_size, 1.0);
    let mut scratch = ShapeScratch::default();

    text.split(' ')
        .filter(|word| !word.is_empty())
        .map(|word| shape_run(&mut scratch, font_system, word, metrics, family).len())
        .sum()
}

/// Shape every space-separated word of `text` through `cache`, and answer how
/// many glyphs came back.
///
/// What a frame of scrolling prose actually pays. Terminal rows repeat their
/// words, so the renderer shapes the few a line introduces and takes hits for
/// the rest, and a caller holding one cache across frames measures that rather
/// than the all-novel bound [`shape_words`] reports.
///
/// The glyph count comes back so a caller cannot have the work optimized out
/// from under it.
///
/// See also:
/// - [`shape_words`] for the same walk with every word shaped afresh.
/// - [`build_font_system`] for the font system to hand in.
pub fn shape_words_cached(
    cache: &mut RunShapeCache,
    font_system: &mut FontSystem,
    font_size: u32,
    text: &str,
) -> usize {
    let primary = resolve_primary_family(font_system, &["JetBrains Mono".to_owned()]);
    let family = shape_family(primary.as_deref());
    let metrics = CellMetrics::from_font_size(font_size, 1.0);

    text.split(' ')
        .filter(|word| !word.is_empty())
        .map(|word| {
            shape_run_cached(cache, word, |scratch| {
                shape_run(scratch, font_system, word, metrics, family)
            })
            .len()
        })
        .sum()
}

/// Register the bundled faces into `db` so they resolve regardless of which
/// fonts are installed system-wide: the JetBrains Mono variable faces (the
/// [`BUNDLED_FAMILY`] family) and the Symbols Nerd Font Mono symbol face
/// ([`SYMBOLS_FAMILY`]) that backs the Private-Use-Area fallback.
///
/// Called before any system font is added, so the bundled faces take the lowest
/// ids and a database seeded from this one keeps them at those ids.
pub(super) fn load_bundled_fonts(db: &mut Database) {
    const REGULAR: &[u8] =
        include_bytes!("../../../assets/fonts/JetBrainsMono/JetBrainsMono[wght].ttf");
    const ITALIC: &[u8] =
        include_bytes!("../../../assets/fonts/JetBrainsMono/JetBrainsMono-Italic[wght].ttf");
    const SYMBOLS: &[u8] =
        include_bytes!("../../../assets/fonts/SymbolsNerdFont/SymbolsNerdFontMono-Regular.ttf");

    db.load_font_data(REGULAR.to_vec());
    db.load_font_data(ITALIC.to_vec());
    db.load_font_data(SYMBOLS.to_vec());
}

/// The first family in `cascade` present in `font_system`'s db, or `None` when
/// none are installed so shaping falls back to the generic monospace.
///
/// Shared rather than owned, since the shaping paths need the name alongside a
/// mutable borrow of the pass and so take a copy on every frame.
pub(crate) fn resolve_primary_family(
    font_system: &FontSystem,
    cascade: &[String],
) -> Option<Arc<str>> {
    let db = font_system.db();
    cascade
        .iter()
        .find(|name| {
            db.query(&Query {
                families: &[Family::Name(name.as_str())],
                ..Default::default()
            })
            .is_some()
        })
        .map(|name| Arc::from(name.as_str()))
}

/// The cosmic-text family to shape with, being the resolved primary by name, or the
/// generic monospace when no configured family was present.
///
/// Takes the name rather than the option the caller stores it in, so how the pass
/// holds it is free to change.
pub(super) fn shape_family(family: Option<&str>) -> Family<'_> {
    family.map_or(Family::Monospace, Family::Name)
}

/// Baseline offset from a cell's top, in physical pixels, measured once from the
/// font so glyphs sit on a consistent baseline within their cell.
pub(super) fn probe_baseline(
    font_system: &mut FontSystem,
    metrics: CellMetrics,
    family: Family<'_>,
) -> f32 {
    let mut buffer =
        CosmicBuffer::new(font_system, Metrics::new(metrics.font_size, metrics.height));
    buffer.set_text(
        font_system,
        "M",
        &Attrs::new().family(family),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);
    buffer
        .layout_runs()
        .next()
        .map(|run| run.line_y)
        .unwrap_or(metrics.height * 0.8)
}

/// Cap height in physical pixels, measured off the face so chrome sized to the
/// capitals matches the text beside it.
///
/// The face carries the number, so this needs no shaping pass. A face that
/// reports none, and a family that resolves to no face at all, both fall back to
/// seven tenths of the rasterization size, which is about where a latin capital
/// lands.
pub(super) fn probe_cap_height(font: Option<&Font>, metrics: CellMetrics) -> f32 {
    let reported = font.map_or(0.0, |font| {
        font.as_swash()
            .metrics(&[])
            .scale(metrics.font_size)
            .cap_height
    });
    if reported > 0.0 {
        reported
    } else {
        0.7 * metrics.font_size
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_font_system, bundled_database, bundled_font_system_with_locale, font_covers,
        glyph_family, resolve_primary_family, resolve_primary_font, run_text_and_columns_into,
        shape_char, shape_family, shape_run, shape_run_cached, shape_words, substitution_rules,
        CoveredSet, RunShapeCache, ShapeScratch, BUNDLED_FAMILY, RUN_SHAPE_CACHE_CAP,
        SYMBOLS_FAMILY,
    };
    use crate::render::CellMetrics;
    use cosmic_text::{
        fontdb::{Database, Query, Weight, ID},
        Attrs, Buffer as CosmicBuffer, Family, FontSystem, Metrics, Shaping,
    };

    /// A bundled-only system at a fixed locale, so a test never reads the
    /// environment's.
    fn bundled() -> FontSystem {
        bundled_font_system_with_locale("en-US".into())
    }

    fn face_of(font_system: &FontSystem, family: Family<'_>) -> Option<ID> {
        font_system.db().query(&Query {
            families: &[family],
            ..Default::default()
        })
    }

    #[test]
    fn shape_char_bold_resolves_a_distinct_face() {
        let mut font_system = build_font_system();
        let metrics = CellMetrics::from_font_size(30, 1.0);
        let family = Family::Name("JetBrains Mono");

        let normal = shape_char(
            &mut font_system,
            'A',
            1.0,
            metrics,
            family,
            Weight::NORMAL,
            &mut None,
        )
        .expect("normal glyph shapes");
        let bold = shape_char(
            &mut font_system,
            'A',
            1.0,
            metrics,
            family,
            Weight::BOLD,
            &mut None,
        )
        .expect("bold glyph shapes");

        assert_ne!(
            normal, bold,
            "the bold weight resolves a distinct face, diverging the glyph cache key",
        );
    }

    #[test]
    fn a_bundled_system_resolves_every_family_the_renderer_shapes_with() {
        let font_system = bundled();
        let bundled = face_of(&font_system, Family::Name(BUNDLED_FAMILY));

        assert!(bundled.is_some(), "the bundled text family resolves");
        assert!(
            face_of(&font_system, Family::Name(SYMBOLS_FAMILY)).is_some(),
            "the bundled symbol family resolves",
        );
        assert_eq!(
            face_of(&font_system, Family::Monospace),
            bundled,
            "the generic monospace resolves to the bundled family",
        );
    }

    #[test]
    fn a_scanned_system_keeps_the_bundled_faces_at_their_ids() {
        let bundled = bundled();
        let scanned = build_font_system();

        assert_eq!(
            face_of(&scanned, Family::Name(BUNDLED_FAMILY)),
            face_of(&bundled, Family::Name(BUNDLED_FAMILY)),
            "the scan adds faces without moving the bundled text family",
        );
        assert_eq!(
            face_of(&scanned, Family::Name(SYMBOLS_FAMILY)),
            face_of(&bundled, Family::Name(SYMBOLS_FAMILY)),
            "the scan adds faces without moving the bundled symbol family",
        );
        assert_eq!(
            face_of(&scanned, Family::Monospace),
            face_of(&bundled, Family::Monospace),
            "the generic monospace resolves the same before and after the scan",
        );
    }

    #[test]
    fn glyph_family_falls_back_to_symbols_font_for_uncovered_glyphs() {
        let mut font_system = bundled();
        let primary = Family::Name(BUNDLED_FAMILY);

        assert_eq!(
            glyph_family(&mut font_system, 'A', primary, &mut None),
            (primary, true),
            "a glyph the primary family carries shapes with the primary"
        );
        assert_eq!(
            glyph_family(&mut font_system, '\u{e0b6}', primary, &mut None),
            (Family::Name(SYMBOLS_FAMILY), true),
            "a Private-Use-Area powerline glyph the primary lacks routes to the symbols font"
        );
    }

    /// U+0378 is unassigned, so no face maps it and no fallback walk can find
    /// one. Reporting that is what spares the walk.
    #[test]
    fn a_codepoint_no_face_maps_reports_uncovered_and_still_shapes() {
        let mut font_system = bundled();
        let primary = Family::Name(BUNDLED_FAMILY);
        let mut covered = None;

        assert_eq!(
            glyph_family(&mut font_system, '\u{378}', primary, &mut covered),
            (Family::Name(SYMBOLS_FAMILY), false),
            "a codepoint no face maps routes to the symbols font and reports uncovered",
        );
        assert!(covered.is_some(), "and the miss is what builds the set");

        let metrics = CellMetrics::from_font_size(30, 1.0);
        assert!(
            shape_char(
                &mut font_system,
                '\u{378}',
                1.0,
                metrics,
                primary,
                Weight::NORMAL,
                &mut covered,
            )
            .is_some(),
            "and it still shapes, to the symbols face's own glyph for it",
        );
    }

    /// Skipping the fallback walk has to reach the glyph the walk reaches.
    ///
    /// The walk ends on the symbols face's own glyph for a character nothing
    /// maps, which is what the basic path takes directly. Were the two to
    /// differ, the saving would be a rendering change rather than a saving.
    #[test]
    fn skipping_the_walk_keys_the_glyph_the_walk_would() {
        let mut font_system = bundled();
        let metrics = CellMetrics::from_font_size(30, 1.0);
        let primary = Family::Name(BUNDLED_FAMILY);
        let uncovered = '\u{378}';

        let skipped = shape_char(
            &mut font_system,
            uncovered,
            1.0,
            metrics,
            primary,
            Weight::NORMAL,
            &mut None,
        );

        let walked = {
            let mut buffer = CosmicBuffer::new(
                &mut font_system,
                Metrics::new(metrics.font_size, metrics.height),
            );
            let mut encoded = [0u8; 4];
            buffer.set_text(
                &mut font_system,
                uncovered.encode_utf8(&mut encoded),
                &Attrs::new()
                    .family(Family::Name(SYMBOLS_FAMILY))
                    .weight(Weight::NORMAL),
                Shaping::Advanced,
                None,
            );
            buffer.shape_until_scroll(&mut font_system, false);
            let run = buffer.layout_runs().next().expect("one run");
            let glyph = run.glyphs.first().expect("one glyph");
            Some(glyph.physical((0.0, 0.0), 1.0).cache_key)
        };

        assert_eq!(
            skipped, walked,
            "the basic path keys what the fallback walk keys",
        );
    }

    /// The set reads the database it is built from, which is what decides
    /// whether a fallback walk has anything to find.
    #[test]
    fn the_covered_set_reports_what_its_database_maps() {
        let empty = CoveredSet::of(&Database::new());
        let bundled = CoveredSet::of(&bundled_database());

        assert!(
            !empty.covers('A'),
            "a database holding no face maps nothing",
        );
        assert!(bundled.covers('A'), "the bundled faces map a letter",);
        assert!(
            bundled.covers('\u{e0b6}'),
            "and the Private-Use-Area powerline glyph the symbols face carries",
        );
        assert!(
            !bundled.covers('\u{378}'),
            "and not an unassigned codepoint no face maps",
        );
    }

    /// A rule fires only where the run holds every glyph the rule names, so an
    /// ordinary code run skips the shaper and a ligature site does not.
    ///
    /// A coverage alone put both on the shaping path. Every glyph of a `calt`
    /// sequence carries its own rule with its partners in backtrack or
    /// lookahead, so one bracket was enough to send a run to the shaper.
    ///
    /// Each run is classified against the shaper itself rather than against a
    /// list somebody wrote down. A run the shaper leaves alone must take the
    /// fast path and a run it changes must not, which is the whole contract.
    #[test]
    fn a_run_reaches_the_shaper_only_for_a_rule_it_could_match() {
        let mut font_system = bundled();
        let font = resolve_primary_font(&mut font_system, Some("JetBrains Mono"))
            .expect("the bundled face resolves");
        let rules = substitution_rules(&font);
        let charmap = font.as_swash().charmap();
        let metrics = CellMetrics::from_font_size(16, 1.0);
        let jbm = Family::Name("JetBrains Mono");

        let mut ligated = 0;
        for text in [
            "fn(x)", "foo_bar", "self.x", "list[i]", "_|", "#(", "=>", "..", "::", "//",
        ] {
            // What the caller lays down when it skips: each character shaped on
            // its own, at the byte offset the run puts it at.
            let mut covered = None;
            let mut alone = Vec::new();
            let mut offset = 0;
            for ch in text.chars() {
                if let Some(key) = shape_char(
                    &mut font_system,
                    ch,
                    1.0,
                    metrics,
                    jbm,
                    Weight::NORMAL,
                    &mut covered,
                ) {
                    alone.push((offset, key));
                }
                offset += ch.len_utf8();
            }

            let shaped = shape_run(
                &mut ShapeScratch::default(),
                &mut font_system,
                text,
                metrics,
                jbm,
            );
            let changes = shaped != alone;
            ligated += usize::from(changes);

            let glyphs: Vec<u16> = text.chars().map(|ch| charmap.map(ch)).collect();
            assert_eq!(
                rules.reshapes(&glyphs),
                changes,
                "{text:?}: the table and the shaper disagree about whether it needs shaping",
            );
        }

        assert!(
            (1..10).contains(&ligated),
            "{ligated} of ten runs ligate, so the fixture has to hold both kinds",
        );
    }

    #[test]
    fn shape_run_forms_ligatures_and_maps_clusters() {
        let mut font_system = bundled();
        let metrics = CellMetrics::from_font_size(16, 1.0);
        let jbm = Family::Name("JetBrains Mono");

        let offsets: Vec<usize> = shape_run(
            &mut ShapeScratch::default(),
            &mut font_system,
            "ab",
            metrics,
            jbm,
        )
        .iter()
        .map(|(offset, _)| *offset)
        .collect();
        assert_eq!(
            offsets,
            [0, 1],
            "non-ligating characters map to their source byte offsets"
        );

        let alone = shape_run(
            &mut ShapeScratch::default(),
            &mut font_system,
            "=",
            metrics,
            jbm,
        );
        let ligated = shape_run(
            &mut ShapeScratch::default(),
            &mut font_system,
            "=>",
            metrics,
            jbm,
        );
        assert_eq!(alone.len(), 1, "a lone = shapes to one glyph");
        assert_ne!(
            alone[0].1.glyph_id, ligated[0].1.glyph_id,
            "shaping => as a run substitutes the = via calt, so the ligature forms across cells"
        );
        assert_eq!(
            ligated[0].0, 0,
            "the ligature's first glyph maps back to the run's first column"
        );
    }

    /// A character keys the same however far into the run it sits, because the
    /// grid draws every glyph at an integer cell origin.
    ///
    /// Keying by the glyph's own position instead would sort each column into
    /// its own subpixel bin, and one character would take as many atlas entries
    /// as the row has columns.
    #[test]
    fn shape_run_keys_a_repeated_character_once() {
        let mut font_system = bundled();
        let metrics = CellMetrics::from_font_size(16, 1.0);

        let shaped = shape_run(
            &mut ShapeScratch::default(),
            &mut font_system,
            "aaaa",
            metrics,
            Family::Name("JetBrains Mono"),
        );
        assert_eq!(shaped.len(), 4, "one glyph per character");
        assert!(
            shaped.iter().all(|(_, key)| *key == shaped[0].1),
            "every column keys the same entry: {:?}",
            shaped.iter().map(|(_, key)| *key).collect::<Vec<_>>()
        );
    }

    /// A glyph rasterizes at the font size the metrics name, not at the cell
    /// height around it. The two differ by the line-height ratio, so passing
    /// the wrong one renders every glyph a fifth too large with nothing else
    /// out of place to notice.
    #[test]
    fn shape_run_keys_the_font_size_the_metrics_name() {
        let mut font_system = bundled();

        for size in [11u32, 16, 30] {
            let metrics = CellMetrics::from_font_size(size, 1.0);
            let shaped = shape_run(
                &mut ShapeScratch::default(),
                &mut font_system,
                "a",
                metrics,
                Family::Name("JetBrains Mono"),
            );
            assert_eq!(
                shaped[0].1.font_size_bits,
                metrics.font_size.to_bits(),
                "size {size} keys at the font size, not the cell height \
                 {}",
                metrics.height
            );
        }
    }

    /// A run of characters the primary family lacks shapes through the fallback
    /// the font system finds, and each glyph still names the byte it came from.
    ///
    /// The cluster mapping is what puts a glyph in its column, so it has to
    /// survive the fallback rather than collapsing to the run's start.
    #[test]
    fn shape_run_maps_clusters_through_a_fallback_font() {
        let mut font_system = bundled();
        let metrics = CellMetrics::from_font_size(16, 1.0);

        // Two Private-Use-Area powerline separators, which only the bundled
        // symbols font carries. Three bytes each in UTF-8.
        let shaped = shape_run(
            &mut ShapeScratch::default(),
            &mut font_system,
            "\u{e0b6}\u{e0b4}",
            metrics,
            Family::Name(SYMBOLS_FAMILY),
        );
        let offsets: Vec<usize> = shaped.iter().map(|(offset, _)| *offset).collect();
        assert_eq!(
            offsets,
            [0, 3],
            "each separator maps to its own source byte"
        );
        assert_ne!(
            shaped[0].1.glyph_id, shaped[1].1.glyph_id,
            "and the two separators are distinct glyphs rather than one repeated"
        );
    }

    /// A charmap held across lookups answers coverage the way a fresh one does.
    ///
    /// The shaping paths build one per pass and map every cell through it, since
    /// constructing one parses the font's cmap table directory. That is only sound
    /// if a held charmap is not order-dependent or otherwise single-use.
    #[test]
    fn a_held_charmap_answers_coverage_like_a_fresh_one() {
        let mut font_system = bundled();
        let font = resolve_primary_font(&mut font_system, Some("JetBrains Mono"))
            .expect("the bundled primary family resolves");

        let charmap = font.as_swash().charmap();
        let held: Vec<bool> = "a=→\u{1F600} z"
            .chars()
            .map(|ch| charmap.map(ch) != 0)
            .collect();
        let fresh: Vec<bool> = "a=→\u{1F600} z"
            .chars()
            .map(|ch| font_covers(&font, ch))
            .collect();

        assert_eq!(
            held, fresh,
            "one charmap answers every lookup as a per-call one would"
        );
        assert!(
            held.iter().any(|&covered| covered) && held.iter().any(|&covered| !covered),
            "the sample must span covered and uncovered characters to mean anything: {held:?}"
        );
    }

    /// Words shape apart, and a ligature still forms inside one.
    ///
    /// Three words shape to four glyphs. The arrow is one glyph rather than two,
    /// which is what says the split at spaces did not cost the contextual
    /// alternate that makes it.
    #[test]
    fn shape_words_shapes_each_word_and_keeps_its_ligature() {
        let mut font_system = bundled();

        assert_eq!(shape_words(&mut font_system, 15, "a => b"), 3 + 1);
    }

    #[test]
    fn shape_run_cached_returns_the_cached_run_without_reshaping() {
        let mut font_system = bundled();
        let metrics = CellMetrics::from_font_size(16, 1.0);
        let jbm = Family::Name("JetBrains Mono");

        let mut cache = RunShapeCache::default();
        let fresh = shape_run_cached(&mut cache, "==", |scratch| {
            shape_run(scratch, &mut font_system, "==", metrics, jbm)
        })
        .to_vec();
        assert_eq!(
            fresh,
            shape_run(
                &mut ShapeScratch::default(),
                &mut font_system,
                "==",
                metrics,
                jbm
            ),
            "the miss stores the same glyphs a direct shape produces"
        );
        assert_eq!(
            (cache.at.len(), cache.runs.len()),
            (1, 1),
            "the miss shaped and stored one run"
        );

        // Poison the stored glyphs with another run's. A reshape would overwrite
        // them, so getting the poisoned glyphs back proves the hit read the cache.
        let poison = shape_run(
            &mut ShapeScratch::default(),
            &mut font_system,
            "ab",
            metrics,
            jbm,
        );
        cache.runs[cache.at["=="]].glyphs = poison.clone();
        let hit = shape_run_cached(&mut cache, "==", |scratch| {
            shape_run(scratch, &mut font_system, "==", metrics, jbm)
        });
        assert_eq!(
            hit,
            poison.as_slice(),
            "a hit returns the stored run, not a reshape"
        );
        assert_eq!(
            (cache.at.len(), cache.runs.len()),
            (1, 1),
            "a hit adds no entry"
        );
    }

    /// A full cache evicts what nothing has asked for and keeps what something
    /// has, which is the whole point of bounding it this way rather than
    /// flushing it. A repainted row's runs are exactly the hit ones, so a bound
    /// that drops them hands the next frame a re-shape storm.
    ///
    /// The sweep runs against a filled cache built by hand, since driving
    /// [`RUN_SHAPE_CACHE_CAP`] real runs through the shaper costs seconds to
    /// prove a rule that is about bookkeeping.
    #[test]
    fn a_full_cache_evicts_the_runs_nothing_asked_for() {
        let mut font_system = bundled();
        let metrics = CellMetrics::from_font_size(16, 1.0);
        let jbm = Family::Name("JetBrains Mono");

        let mut cache = RunShapeCache::default();
        for index in 0..RUN_SHAPE_CACHE_CAP {
            cache.store(&format!("run{index}"), Vec::new());
        }
        assert_eq!(cache.runs.len(), RUN_SHAPE_CACHE_CAP, "the cache filled");

        // Ask for one run near the hand's start and leave its neighbour alone,
        // so the next sweep meets both and has to choose between them.
        shape_run_cached(&mut cache, "run0", |scratch| {
            shape_run(scratch, &mut font_system, "run0", metrics, jbm)
        });

        cache.store("first new run", Vec::new());
        assert!(
            cache.at.contains_key("run0"),
            "the run asked for since the hand last passed survives its sweep"
        );
        assert!(
            !cache.at.contains_key("run1"),
            "the untouched run behind it is the one evicted"
        );
        assert_eq!(
            (cache.at.len(), cache.runs.len()),
            (RUN_SHAPE_CACHE_CAP, RUN_SHAPE_CACHE_CAP),
            "an insert past the cap replaces rather than grows"
        );

        // The survivor's second chance is spent, so the next sweep reaching it
        // takes it. Every later insert keeps the cache at its bound.
        for index in 0..RUN_SHAPE_CACHE_CAP {
            cache.store(&format!("later{index}"), Vec::new());
        }
        assert_eq!(
            (cache.at.len(), cache.runs.len()),
            (RUN_SHAPE_CACHE_CAP, RUN_SHAPE_CACHE_CAP),
            "a stream of unique runs settles at the bound instead of growing"
        );
        assert!(
            !cache.at.contains_key("run0"),
            "a run nobody asks for again is evicted on a later lap"
        );
    }

    /// `clear` has to reset the hand with the two halves. A hand left past the
    /// end of a refilled ring indexes a slot that is no longer there.
    #[test]
    fn clearing_resets_the_eviction_hand() {
        let mut cache = RunShapeCache::default();
        for index in 0..RUN_SHAPE_CACHE_CAP + 1 {
            cache.store(&format!("run{index}"), Vec::new());
        }
        assert_ne!(cache.hand, 0, "the overflowing insert moved the hand");

        cache.clear();
        assert_eq!(
            (cache.at.len(), cache.runs.len(), cache.hand),
            (0, 0, 0),
            "clear empties both halves and takes the hand back to the start"
        );
    }

    #[test]
    fn run_text_and_columns_maps_each_byte_to_its_cell() {
        let mut text = String::new();
        let mut col_of_byte = Vec::new();
        run_text_and_columns_into(
            &[(3, 'a'), (4, '世'), (5, 'b')],
            &mut text,
            &mut col_of_byte,
        );

        assert_eq!(text, "a世b");
        assert_eq!(
            col_of_byte,
            [3, 4, 4, 4, 5],
            "the three bytes of 世 all map to its column, so a glyph's start byte resolves correctly"
        );
    }

    #[test]
    fn resolve_primary_family_picks_first_present_then_falls_back() {
        let font_system = bundled();

        assert_eq!(
            resolve_primary_family(
                &font_system,
                &["Nonexistent Face".to_owned(), "JetBrains Mono".to_owned()],
            )
            .as_deref(),
            Some("JetBrains Mono"),
            "skips the missing family and resolves the first present one"
        );
        assert_eq!(
            resolve_primary_family(&font_system, &["Nonexistent Face".to_owned()]),
            None,
            "a cascade with no present family resolves to None"
        );
        assert_eq!(
            resolve_primary_family(&font_system, &[]),
            None,
            "an empty cascade resolves to None"
        );
    }

    #[test]
    fn shape_family_maps_resolved_name_else_monospace() {
        assert_eq!(
            shape_family(Some("JetBrains Mono")),
            Family::Name("JetBrains Mono")
        );
        assert_eq!(shape_family(None), Family::Monospace);
    }
}
