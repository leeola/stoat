//! Font resolution and text shaping, independent of any render pass.
//!
//! Turning a character or a run of them into glyph cache keys needs a font
//! database, a coverage test, and a cosmic-text shaping buffer, none of which
//! depend on GPU state. This module holds that half so the pass above it deals
//! only in the keys it gets back.

use crate::render::CellMetrics;
use cosmic_text::{
    fontdb::{Query, Weight},
    Attrs, AttrsList, Buffer as CosmicBuffer, CacheKey, Family, Font, FontSystem, Hinting, Metrics,
    ShapeLine, Shaping, Wrap,
};
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// Family name of the bundled Nerd Font, registered by [`load_bundled_fonts`].
///
/// Carries the Private-Use-Area powerline separators and icon glyphs that
/// programming fonts omit, so it serves as the symbol fallback ahead of any
/// system font (see [`glyph_family`]).
pub(super) const SYMBOLS_FAMILY: &str = "Symbols Nerd Font Mono";

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
) -> Option<CacheKey> {
    let family = glyph_family(font_system, ch, primary);
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
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);

    let run = buffer.layout_runs().next()?;
    let glyph = run.glyphs.first()?;
    Some(glyph.physical((0.0, 0.0), 1.0).cache_key)
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
    font_system: &mut FontSystem,
    text: &str,
    metrics: CellMetrics,
    family: Family<'_>,
) -> Vec<(usize, CacheKey)> {
    let attrs = AttrsList::new(&Attrs::new().family(family));
    let line = ShapeLine::new(font_system, text, &attrs, Shaping::Advanced, TAB_WIDTH);
    // No width bound, so the run lays out as one line and nothing wraps. The
    // layout is cosmic-text's rather than ours because a fallback font whose
    // monospace em width differs from the primary's has its size adjusted here,
    // and that adjusted size is part of the cache key below.
    let layout = line.layout(
        metrics.font_size,
        None,
        Wrap::None,
        None,
        None,
        Hinting::Disabled,
    );

    let Some(first) = layout.first() else {
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

/// Shape `text` as one run, reusing an identical run's glyphs from `cache`.
///
/// [`shape_run`] rebuilds a cosmic-text buffer and reshapes from scratch, the
/// dominant per-frame cost when a ligature row is repainted. The run text alone
/// keys the result, since runs group only same-scale primary-covered cells in
/// the constant `family`. On a miss the run is shaped and stored, evicting a
/// run nothing has asked for lately once the cache is full.
pub(super) fn shape_run_cached<'a>(
    cache: &'a mut RunShapeCache,
    font_system: &mut FontSystem,
    text: &str,
    metrics: CellMetrics,
    family: Family<'_>,
) -> &'a [(usize, CacheKey)] {
    // The slot copies out, so the lookup's borrow ends before the miss path
    // needs the cache mutably. A map holding the glyphs themselves cannot do that,
    // and pays a second hash to read what the first already found.
    if let Some(&slot) = cache.at.get(text) {
        cache.runs[slot].asked_for = true;
        return &cache.runs[slot].glyphs;
    }

    let shaped = shape_run(font_system, text, metrics, family);
    let slot = cache.store(text, shaped);
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
pub(super) struct RunShapeCache {
    at: FxHashMap<Arc<str>, usize>,
    runs: Vec<CachedRun>,
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

    /// Characters shaped to fill this cache, which is the work a screen of text
    /// costs: every distinct run is shaped exactly once.
    pub(super) fn shaped_chars(&self) -> usize {
        self.at.keys().map(|key| key.chars().count()).sum()
    }
}

impl RunShapeCache {
    /// Drop every shaped run, so the two halves cannot disagree about what is here.
    pub(super) fn clear(&mut self) {
        self.at.clear();
        self.runs.clear();
        self.hand = 0;
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
) -> Family<'a> {
    if family_covers(font_system, primary, ch) {
        primary
    } else {
        Family::Name(SYMBOLS_FAMILY)
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

/// Build the [`FontSystem`] a [`super::TextPass`] shapes with: cosmic-text's system
/// font enumeration plus the bundled fonts.
///
/// Enumerating the system fonts dominates renderer startup, and this needs no
/// window or GPU, so it is run on a background thread (see
/// [`GpuContext::new`](crate::gpu::GpuContext::new)) concurrently with the
/// main-thread surface and device setup.
pub fn build_font_system() -> FontSystem {
    let mut font_system = FontSystem::new();
    load_bundled_fonts(&mut font_system);
    font_system
}

/// Register the bundled faces into `font_system`'s font database so they resolve
/// regardless of which fonts are installed system-wide: the JetBrains Mono
/// variable faces (the `JetBrains Mono` family) and the Symbols Nerd Font Mono
/// symbol face ([`SYMBOLS_FAMILY`]) that backs the Private-Use-Area fallback.
pub(super) fn load_bundled_fonts(font_system: &mut FontSystem) {
    const REGULAR: &[u8] =
        include_bytes!("../../../assets/fonts/JetBrainsMono/JetBrainsMono[wght].ttf");
    const ITALIC: &[u8] =
        include_bytes!("../../../assets/fonts/JetBrainsMono/JetBrainsMono-Italic[wght].ttf");
    const SYMBOLS: &[u8] =
        include_bytes!("../../../assets/fonts/SymbolsNerdFont/SymbolsNerdFontMono-Regular.ttf");

    let db = font_system.db_mut();
    db.load_font_data(REGULAR.to_vec());
    db.load_font_data(ITALIC.to_vec());
    db.load_font_data(SYMBOLS.to_vec());
}

/// The first family in `cascade` present in `font_system`'s db, or `None` when
/// none are installed so shaping falls back to the generic monospace.
///
/// Shared rather than owned, since the shaping paths need the name alongside a
/// mutable borrow of the pass and so take a copy on every frame.
pub(super) fn resolve_primary_family(
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

#[cfg(test)]
mod tests {
    use super::{
        build_font_system, font_covers, glyph_family, load_bundled_fonts, resolve_primary_family,
        resolve_primary_font, run_text_and_columns_into, shape_char, shape_family, shape_run,
        shape_run_cached, RunShapeCache, RUN_SHAPE_CACHE_CAP, SYMBOLS_FAMILY,
    };
    use crate::render::CellMetrics;
    use cosmic_text::{
        fontdb::{Database, Query, Weight},
        Family, FontSystem,
    };

    #[test]
    fn shape_char_bold_resolves_a_distinct_face() {
        let mut font_system = build_font_system();
        let metrics = CellMetrics::from_font_size(30, 1.0);
        let family = Family::Name("JetBrains Mono");

        let normal = shape_char(&mut font_system, 'A', 1.0, metrics, family, Weight::NORMAL)
            .expect("normal glyph shapes");
        let bold = shape_char(&mut font_system, 'A', 1.0, metrics, family, Weight::BOLD)
            .expect("bold glyph shapes");

        assert_ne!(
            normal, bold,
            "the bold weight resolves a distinct face, diverging the glyph cache key",
        );
    }

    #[test]
    fn bundled_fonts_make_jetbrains_mono_resolvable() {
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);

        assert!(
            font_system
                .db()
                .query(&Query {
                    families: &[Family::Name("JetBrains Mono")],
                    ..Default::default()
                })
                .is_some(),
            "bundled faces resolve JetBrains Mono in an otherwise empty font db"
        );
    }

    #[test]
    fn glyph_family_falls_back_to_symbols_font_for_uncovered_glyphs() {
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);
        let primary = Family::Name("JetBrains Mono");

        assert_eq!(
            glyph_family(&mut font_system, 'A', primary),
            primary,
            "a glyph the primary family carries shapes with the primary"
        );
        assert_eq!(
            glyph_family(&mut font_system, '\u{e0b6}', primary),
            Family::Name(SYMBOLS_FAMILY),
            "a Private-Use-Area powerline glyph the primary lacks routes to the symbols font"
        );
    }

    #[test]
    fn shape_run_forms_ligatures_and_maps_clusters() {
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);
        let metrics = CellMetrics::from_font_size(16, 1.0);
        let jbm = Family::Name("JetBrains Mono");

        let offsets: Vec<usize> = shape_run(&mut font_system, "ab", metrics, jbm)
            .iter()
            .map(|(offset, _)| *offset)
            .collect();
        assert_eq!(
            offsets,
            [0, 1],
            "non-ligating characters map to their source byte offsets"
        );

        let alone = shape_run(&mut font_system, "=", metrics, jbm);
        let ligated = shape_run(&mut font_system, "=>", metrics, jbm);
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
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);
        let metrics = CellMetrics::from_font_size(16, 1.0);

        let shaped = shape_run(
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
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);

        for size in [11u32, 16, 30] {
            let metrics = CellMetrics::from_font_size(size, 1.0);
            let shaped = shape_run(
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
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);
        let metrics = CellMetrics::from_font_size(16, 1.0);

        // Two Private-Use-Area powerline separators, which only the bundled
        // symbols font carries. Three bytes each in UTF-8.
        let shaped = shape_run(
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
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);
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

    #[test]
    fn shape_run_cached_returns_the_cached_run_without_reshaping() {
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);
        let metrics = CellMetrics::from_font_size(16, 1.0);
        let jbm = Family::Name("JetBrains Mono");

        let mut cache = RunShapeCache::default();
        let fresh = shape_run_cached(&mut cache, &mut font_system, "==", metrics, jbm).to_vec();
        assert_eq!(
            fresh,
            shape_run(&mut font_system, "==", metrics, jbm),
            "the miss stores the same glyphs a direct shape produces"
        );
        assert_eq!(
            (cache.at.len(), cache.runs.len()),
            (1, 1),
            "the miss shaped and stored one run"
        );

        // Poison the stored glyphs with another run's. A reshape would overwrite
        // them, so getting the poisoned glyphs back proves the hit read the cache.
        let poison = shape_run(&mut font_system, "ab", metrics, jbm);
        cache.runs[cache.at["=="]].glyphs = poison.clone();
        let hit = shape_run_cached(&mut cache, &mut font_system, "==", metrics, jbm);
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
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);
        let metrics = CellMetrics::from_font_size(16, 1.0);
        let jbm = Family::Name("JetBrains Mono");

        let mut cache = RunShapeCache::default();
        for index in 0..RUN_SHAPE_CACHE_CAP {
            cache.store(&format!("run{index}"), Vec::new());
        }
        assert_eq!(cache.runs.len(), RUN_SHAPE_CACHE_CAP, "the cache filled");

        // Ask for one run near the hand's start and leave its neighbour alone,
        // so the next sweep meets both and has to choose between them.
        shape_run_cached(&mut cache, &mut font_system, "run0", metrics, jbm);

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
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);

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
