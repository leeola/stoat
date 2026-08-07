//! Font resolution and text shaping, independent of any render pass.
//!
//! Turning a character or a run of them into glyph cache keys needs a font
//! database, a coverage test, and a cosmic-text shaping buffer, none of which
//! depend on GPU state. This module holds that half so the pass above it deals
//! only in the keys it gets back.

use crate::render::CellMetrics;
use cosmic_text::{
    fontdb::{Query, Weight},
    Attrs, Buffer as CosmicBuffer, CacheKey, Family, Font, FontSystem, Metrics, Shaping,
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
    let mut buffer =
        CosmicBuffer::new(font_system, Metrics::new(metrics.font_size, metrics.height));
    buffer.set_text(
        font_system,
        text,
        &Attrs::new().family(family),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);

    let Some(run) = buffer.layout_runs().next() else {
        return Vec::new();
    };
    run.glyphs
        .iter()
        .map(|glyph| {
            let pixel_aligned = (-(glyph.x + glyph.font_size * glyph.x_offset), 0.0);
            (glyph.start, glyph.physical(pixel_aligned, 1.0).cache_key)
        })
        .collect()
}

/// The largest the run-shape cache grows before it is flushed whole.
///
/// Terminal content repeats, so a flushed cache repopulates within a frame. The
/// bound keeps a pathological stream of unique runs from growing it unbounded.
const RUN_SHAPE_CACHE_CAP: usize = 65_536;

/// Shape `text` as one run, reusing an identical run's glyphs from `cache`.
///
/// [`shape_run`] rebuilds a cosmic-text buffer and reshapes from scratch, the
/// dominant per-frame cost when a ligature row is repainted. The run text alone
/// keys the result, since runs group only same-scale primary-covered cells in
/// the constant `family`. On a miss the run is shaped and stored. The cache is
/// flushed whole once it reaches [`RUN_SHAPE_CACHE_CAP`], before the new entry
/// lands so the current run survives.
pub(super) fn shape_run_cached<'a>(
    cache: &'a mut RunShapeCache,
    font_system: &mut FontSystem,
    text: &str,
    metrics: CellMetrics,
    family: Family<'_>,
) -> &'a [(usize, CacheKey)] {
    // The position copies out, so the lookup's borrow ends before the miss path
    // needs the cache mutably. A map holding the glyphs themselves cannot do that,
    // and pays a second hash to read what the first already found.
    if let Some(&at) = cache.at.get(text) {
        return &cache.runs[at];
    }

    let shaped = shape_run(font_system, text, metrics, family);
    if cache.at.len() >= RUN_SHAPE_CACHE_CAP {
        cache.clear();
    }
    cache.at.insert(text.to_string(), cache.runs.len());
    cache.runs.push(shaped);
    cache.runs.last().expect("just pushed")
}

/// Shaped glyphs of each ligature run, found by the run's text.
///
/// The glyphs live in a Vec and the map holds positions into it, so a hit costs one
/// hash and an index. Holding the glyphs in the map instead would cost a second hash
/// on every hit, to read what the first lookup already proved was there.
#[derive(Default)]
pub(super) struct RunShapeCache {
    at: FxHashMap<String, usize>,
    runs: Vec<Vec<(usize, CacheKey)>>,
}

impl RunShapeCache {
    /// Drop every shaped run, so the two halves cannot disagree about what is here.
    pub(super) fn clear(&mut self) {
        self.at.clear();
        self.runs.clear();
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
        shape_run_cached, RunShapeCache, SYMBOLS_FAMILY,
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
        cache.runs[cache.at["=="]] = poison.clone();
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
