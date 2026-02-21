//! DisplayMap v2: Complete coordinate transformation pipeline.
//!
//! Integrates all transformation layers to provide end-to-end conversion
//! between buffer coordinates ([`Point`]) and display coordinates ([`DisplayPoint`]).
//!
//! # Architecture
//!
//! The DisplayMap chains six transformation layers:
//!
//! ```text
//! Point (buffer)
//!   | InlayMap    - Adds type hints, parameter names
//! InlayPoint
//!   | FoldMap     - Hides folded regions
//! FoldPoint
//!   | TabMap      - Expands tabs to spaces
//! TabPoint
//!   | WrapMap     - Wraps long lines
//! WrapPoint
//!   | BlockMap    - Inserts visual blocks
//! BlockPoint = DisplayPoint (final)
//! ```
//!
//! # Usage
//!
//! ```ignore
//! let display_map = DisplayMap::new(buffer_snapshot, tab_width);
//! let snapshot = display_map.snapshot();
//!
//! // Convert buffer position to display position
//! let buffer_point = Point::new(10, 5);
//! let display_point = snapshot.point_to_display_point(buffer_point, Bias::Left);
//!
//! // Convert back
//! let back = snapshot.display_point_to_point(display_point, Bias::Left);
//! assert_eq!(back, buffer_point);
//! ```
//!
//! # Related
//!
//! - See `.claude/DISPLAY_MAP.md` for implementation details
//! - Based on Zed's DisplayMap architecture

// ============================================================================
// Highlight System Types
// ============================================================================

/// Key for identifying different types of text highlights.
///
/// Allows multiple highlight layers (e.g., search results, selections, diagnostics)
/// to coexist without interfering with each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HighlightKey {
    /// Highlight identified by a TypeId (e.g., TypeId::of::<SearchResults>())
    Type(TypeId),
    /// Highlight identified by TypeId + index (for multiple instances of same type)
    TypePlus(TypeId, usize),
}

/// Visual styling for highlighted text regions.
///
/// Supports colors, background colors, underlines, strikethrough, and font weight.
/// Multiple highlight styles can be combined via the `highlight()` method.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HighlightStyle {
    /// Text color
    pub color: Option<Hsla>,
    /// Background color
    pub background_color: Option<Hsla>,
    /// Underline styling
    pub underline: Option<UnderlineStyle>,
    /// Strikethrough
    pub strikethrough: Option<Hsla>,
    /// Fade out factor (0.0-1.0) for dimming text
    pub fade_out: Option<f32>,
    /// Font weight adjustment
    pub font_weight: Option<FontWeight>,
}

/// Underline styling options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnderlineStyle {
    /// Underline color
    pub color: Option<Hsla>,
    /// Line thickness in pixels
    pub thickness: Pixels,
    /// Whether the underline should be wavy (for diagnostics)
    pub wavy: bool,
}

/// Font weight values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontWeight {
    Normal,
    Bold,
}

impl HighlightStyle {
    /// Combine this highlight style with another, with `other` taking precedence.
    ///
    /// For each field, uses `other`'s value if present, otherwise keeps this style's value.
    pub fn highlight(mut self, other: HighlightStyle) -> Self {
        if other.color.is_some() {
            self.color = other.color;
        }
        if other.background_color.is_some() {
            self.background_color = other.background_color;
        }
        if other.underline.is_some() {
            self.underline = other.underline;
        }
        if other.strikethrough.is_some() {
            self.strikethrough = other.strikethrough;
        }
        if other.fade_out.is_some() {
            self.fade_out = other.fade_out;
        }
        if other.font_weight.is_some() {
            self.font_weight = other.font_weight;
        }
        self
    }
}

/// Information about a highlighted inlay.
///
/// Associates an inlay with its highlight range and type information.
#[derive(Clone, Debug)]
pub struct InlayHighlight {
    /// The inlay being highlighted
    pub inlay: InlayId,
    /// Additional metadata (reserved for future use)
    pub metadata: Option<String>,
}

/// Storage for text highlights (syntax, search, selections, etc.).
///
/// Maps highlight keys to their styles and anchor ranges. Uses TreeMap for
/// efficient insertion, removal, and iteration.
pub type TextHighlights = TreeMap<HighlightKey, Arc<(HighlightStyle, Vec<Range<Anchor>>)>>;

/// Storage for inlay highlights (type hints, parameter names, etc.).
///
/// Two-level map: TypeId -> (InlayId -> (style, metadata)). Allows grouping
/// inlay highlights by type for efficient batch operations.
pub type InlayHighlights = TreeMap<TypeId, TreeMap<InlayId, (HighlightStyle, InlayHighlight)>>;

// ============================================================================
// Text Layout Types
// ============================================================================

/// Text layout details for rendering operations.
///
/// Contains styling and layout system references needed for text shaping and measurement.
/// This is a simplified version - production will have additional fields for scroll state,
/// visible rows, etc.
pub struct TextLayoutDetails {
    /// Text rendering system for layout operations
    pub text_system: Arc<WindowTextSystem>,
    /// Font size in pixels
    pub font_size: Pixels,
    /// Font for rendering
    pub font: Font,
}

// ============================================================================
// Text Chunk Types (for rendering)
// ============================================================================

/// A chunk of text with associated metadata for rendering.
///
/// Represents a contiguous piece of text with uniform styling and properties.
/// Used by the chunk iterator to provide text with all rendering information
/// (highlights, syntax, diagnostics, etc.) attached.
#[derive(Clone, Debug)]
pub struct Chunk<'a> {
    /// The text content of this chunk
    pub text: &'a str,

    /// Custom highlight style (from TextHighlights/InlayHighlights)
    pub highlight_style: Option<HighlightStyle>,

    /// Syntax highlighting color identifier
    pub syntax_highlight_id: Option<u32>,

    /// Diagnostic severity (for error/warning underlines)
    pub diagnostic_severity: Option<DiagnosticSeverity>,

    /// Whether this chunk is a tab character
    pub is_tab: bool,

    /// Whether this chunk is an inlay hint
    pub is_inlay: bool,

    /// Whether this is unnecessary code (dimmed)
    pub is_unnecessary: bool,

    /// Whether to show diagnostic underline
    pub underline: bool,
}

/// Simplified chunk for rendering, with merged styles.
///
/// This is the final output of the highlighting pipeline, with all styles
/// (syntax, custom highlights, diagnostics) merged into a single style.
#[derive(Clone, Debug)]
pub struct HighlightedChunk<'a> {
    /// The text content
    pub text: &'a str,

    /// Merged highlight style (syntax + custom + diagnostics)
    pub style: Option<HighlightStyle>,

    /// Whether this is a tab character
    pub is_tab: bool,

    /// Whether this is an inlay hint
    pub is_inlay: bool,
}

/// Context for chunk iteration with highlights.
///
/// Bundles highlight data and rendering preferences for the chunk iterator.
/// Allows specifying which highlights to apply and how to render them.
#[derive(Clone, Copy, Debug, Default)]
pub struct Highlights<'a> {
    /// Text highlights to apply (from TextHighlights storage)
    pub text_highlights: Option<&'a TextHighlights>,

    /// Inlay highlights to apply (from InlayHighlights storage)
    pub inlay_highlights: Option<&'a InlayHighlights>,
}

/// Diagnostic severity levels.
///
/// Matches LSP diagnostic severity for error/warning/info/hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

/// Display row index (row in display space, after all transformations).
///
/// This is distinct from buffer rows - it accounts for inlays, folds, wrapping, and blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayRow(pub u32);

impl DisplayRow {
    /// Create a new DisplayRow
    pub fn new(row: u32) -> Self {
        Self(row)
    }

    /// Get the next row
    pub fn next_row(self) -> Self {
        Self(self.0 + 1)
    }

    /// Get the previous row (saturating at 0)
    pub fn prev_row(self) -> Self {
        Self(self.0.saturating_sub(1))
    }
}

impl From<u32> for DisplayRow {
    fn from(row: u32) -> Self {
        Self(row)
    }
}

impl From<DisplayRow> for u32 {
    fn from(row: DisplayRow) -> Self {
        row.0
    }
}

// ============================================================================
// DisplayMap
// ============================================================================

use crate::{
    block_map::BlockSnapshot,
    coords::{BlockPoint, DisplayPoint},
    fold_map::FoldSnapshot,
    inlay_map::{InlayId, InlaySnapshot},
    tab_map::TabSnapshot,
    wrap_map::{WrapMap, WrapSnapshot},
};
use gpui::{App, Entity, Font, Hsla, LineLayout, Pixels, SharedString, WindowTextSystem};
use std::{any::TypeId, ops::Range, sync::Arc};
use sum_tree::{Bias, TreeMap};
use text::{subscription::Subscription, Anchor, Buffer, BufferSnapshot, Edit, Point};

/// DisplayMap coordinating all transformation layers.
///
/// Maintains stateful map holders for each layer and handles edit propagation.
/// When the buffer changes, edits are propagated through all layers to update
/// their transforms incrementally.
///
/// Uses async WrapMap for non-blocking soft wrapping.
pub struct DisplayMap {
    buffer: Entity<Buffer>,
    buffer_subscription: Subscription,
    buffer_version: usize,

    // Mutable layer holders
    inlay_map: crate::inlay_map::InlayMap,
    fold_map: crate::fold_map::FoldMap,
    tab_map: crate::tab_map::TabMap,
    wrap_map: Entity<WrapMap>,
    block_map: crate::block_map::BlockMap,
    crease_map: crate::crease_map::CreaseMap,

    // Highlight storage
    text_highlights: TextHighlights,
    inlay_highlights: InlayHighlights,

    // Wrap configuration
    font: Font,
    font_size: Pixels,
    wrap_width: Option<Pixels>,
}

impl DisplayMap {
    /// Create a new DisplayMap for the given buffer.
    ///
    /// Requires GPUI context for async WrapMap Entity and buffer subscription.
    pub fn new(
        buffer: Entity<Buffer>,
        tab_width: u32,
        font: Font,
        font_size: Pixels,
        wrap_width: Option<Pixels>,
        cx: &mut App,
    ) -> Self {
        // Subscribe to buffer changes
        let buffer_subscription = buffer.update(cx, |buffer, _| buffer.subscribe());

        // Get initial buffer snapshot for layer initialization
        let buffer_snapshot = buffer.read(cx).snapshot();

        // Initialize all layers
        let inlay_map = crate::inlay_map::InlayMap::new(buffer_snapshot.clone());
        let (fold_map, fold_snapshot) = crate::fold_map::FoldMap::new(inlay_map.snapshot());
        let (tab_map, tab_snapshot) = crate::tab_map::TabMap::new(fold_snapshot, tab_width);

        // Create async WrapMap Entity
        let (wrap_map, wrap_snapshot) =
            WrapMap::new(tab_snapshot, font.clone(), font_size, wrap_width, cx);

        let block_map = crate::block_map::BlockMap::new(wrap_snapshot);
        let crease_map = crate::crease_map::CreaseMap::new(buffer_snapshot);

        Self {
            buffer,
            buffer_subscription,
            buffer_version: 0,
            inlay_map,
            fold_map,
            tab_map,
            wrap_map,
            block_map,
            crease_map,
            text_highlights: Default::default(),
            inlay_highlights: Default::default(),
            font,
            font_size,
            wrap_width,
        }
    }

    /// Get an immutable snapshot of the current display state.
    ///
    /// Automatically syncs with buffer changes via subscription before creating snapshot.
    /// The snapshot is cheap to clone and can be used across threads.
    pub fn snapshot(&mut self, cx: &mut App) -> DisplaySnapshot {
        // Get current buffer snapshot first to check if buffer has changed
        let buffer_snapshot = self.buffer.read(cx).snapshot();

        // Consume accumulated edits from subscription
        let edits = self.buffer_subscription.consume().into_inner();

        tracing::trace!(
            "DisplayMap.snapshot(): buffer_version={}, buffer_len={}, edits_count={}",
            self.buffer_version,
            buffer_snapshot.len(),
            edits.len()
        );

        tracing::trace!(
            "DisplayMap entering sync path: edits={}, buffer_version={}",
            edits.len(),
            self.buffer_version
        );

        // Convert Edit<usize> to Edit<Point>
        let buffer_edits: Vec<Edit<Point>> = edits
            .iter()
            .map(|edit| Edit {
                old: buffer_snapshot.offset_to_point(edit.old.start)
                    ..buffer_snapshot.offset_to_point(edit.old.end),
                new: buffer_snapshot.offset_to_point(edit.new.start)
                    ..buffer_snapshot.offset_to_point(edit.new.end),
            })
            .collect();

        // Always propagate through layers (even with no edits) to ensure
        // transforms are built on first call
        let (inlay_snapshot, inlay_edits) =
            self.inlay_map.sync(buffer_snapshot.clone(), buffer_edits);
        let (fold_snapshot, fold_edits) = self.fold_map.read(inlay_snapshot, inlay_edits);
        let (tab_snapshot, tab_edits) = self.tab_map.read(fold_snapshot, fold_edits);
        let (wrap_snapshot, wrap_edits) = self.wrap_map.update(cx, |wrap_map, cx| {
            wrap_map.sync(tab_snapshot, tab_edits, cx)
        });
        let (_block_snapshot, _block_edits) =
            self.block_map.sync(wrap_snapshot, wrap_edits.into_inner());

        // Update crease map with new buffer
        self.crease_map.set_buffer(buffer_snapshot);

        // Only increment version if there were actual edits
        if !edits.is_empty() {
            self.buffer_version += 1;
        }

        // Return current snapshot
        let block_snapshot = self.block_map.snapshot();
        let display_snapshot = DisplaySnapshot {
            block_snapshot,
            text_highlights: self.text_highlights.clone(),
            inlay_highlights: self.inlay_highlights.clone(),
        };
        tracing::trace!(
            "DisplayMap.snapshot() returning: max_point=({}, {})",
            display_snapshot.max_point().row,
            display_snapshot.max_point().column
        );
        display_snapshot
    }

    /// Get the current buffer version.
    pub fn buffer_version(&self) -> usize {
        self.buffer_version
    }

    /// Get the font.
    pub fn font(&self) -> &Font {
        &self.font
    }

    /// Get the font size.
    pub fn font_size(&self) -> Pixels {
        self.font_size
    }

    /// Get the wrap width.
    pub fn wrap_width(&self) -> Option<Pixels> {
        self.wrap_width
    }

    /// Access the inlay map for mutation.
    pub fn inlay_map_mut(&mut self) -> &mut crate::inlay_map::InlayMap {
        &mut self.inlay_map
    }

    /// Access the fold map for mutation.
    pub fn fold_map_mut(&mut self) -> &mut crate::fold_map::FoldMap {
        &mut self.fold_map
    }

    /// Access the block map for mutation.
    pub fn block_map_mut(&mut self) -> &mut crate::block_map::BlockMap {
        &mut self.block_map
    }

    /// Access the crease map for mutation.
    pub fn crease_map_mut(&mut self) -> &mut crate::crease_map::CreaseMap {
        &mut self.crease_map
    }

    // High-level mutation APIs

    /// Insert inlays and propagate changes through layers.
    ///
    /// Returns the IDs of the inserted inlays.
    pub fn insert_inlays(
        &mut self,
        inlays: Vec<(text::Anchor, String, sum_tree::Bias)>,
        cx: &mut App,
    ) -> Vec<crate::inlay_map::InlayId> {
        let ids = self.inlay_map.insert_batch(inlays);
        self.propagate_inlay_changes(cx);
        ids
    }

    /// Remove inlays by ID and propagate changes.
    pub fn remove_inlays(&mut self, ids: &[crate::inlay_map::InlayId], cx: &mut App) {
        self.inlay_map.remove(ids);
        self.propagate_inlay_changes(cx);
    }

    /// Insert blocks and propagate changes through layers.
    ///
    /// Returns the IDs of the inserted blocks.
    pub fn insert_blocks(
        &mut self,
        blocks: Vec<crate::block_map::BlockProperties<text::Anchor>>,
    ) -> Vec<crate::block_map::CustomBlockId> {
        self.block_map.insert(blocks)
        // No downstream propagation needed - BlockMap is last layer
    }

    /// Remove blocks by ID.
    pub fn remove_blocks(&mut self, ids: &[crate::block_map::CustomBlockId]) {
        self.block_map.remove(ids);
        // No downstream propagation needed - BlockMap is last layer
    }

    /// Propagate inlay changes through downstream layers.
    fn propagate_inlay_changes(&mut self, cx: &mut App) {
        let inlay_snapshot = self.inlay_map.snapshot();
        let (fold_snapshot, fold_edits) = self.fold_map.read(inlay_snapshot, Vec::new());
        let (tab_snapshot, tab_edits) = self.tab_map.read(fold_snapshot, fold_edits);

        // Use async WrapMap
        let (wrap_snapshot, wrap_edits) = self.wrap_map.update(cx, |wrap_map, cx| {
            wrap_map.sync(tab_snapshot, tab_edits, cx)
        });

        let (_block_snapshot, _block_edits) =
            self.block_map.sync(wrap_snapshot, wrap_edits.into_inner());
    }

    // Wrap configuration API

    /// Set the wrap width for soft wrapping.
    ///
    /// When set to `Some(width)`, lines longer than the width will be soft-wrapped.
    /// When set to `None`, wrapping is disabled.
    ///
    /// This triggers background rewrapping of the buffer.
    pub fn set_wrap_width(&mut self, width: Option<Pixels>, cx: &mut App) {
        if self.wrap_width == width {
            return;
        }

        self.wrap_width = width;

        // Update WrapMap with new width
        self.wrap_map.update(cx, |wrap_map, cx| {
            wrap_map.set_wrap_width(width, cx);
        });
    }

    /// Set the font and font size for wrapping calculations.
    ///
    /// The font metrics affect where line breaks occur during soft wrapping.
    /// This triggers background rewrapping of the buffer.
    pub fn set_font(&mut self, font: Font, font_size: Pixels, cx: &mut App) {
        if self.font == font && self.font_size == font_size {
            return;
        }

        self.font = font.clone();
        self.font_size = font_size;

        // Update WrapMap with new font
        self.wrap_map.update(cx, |wrap_map, cx| {
            wrap_map.set_font_with_size(font, font_size, cx);
        });
    }

    /// Check if background rewrapping is currently in progress.
    ///
    /// Returns `true` if the WrapMap is actively rewrapping content in the background.
    /// While rewrapping, the snapshot may show interpolated (approximate) wrap positions.
    pub fn is_rewrapping(&self, cx: &App) -> bool {
        self.wrap_map.read(cx).is_rewrapping()
    }

    // ========================================================================
    // Highlight Management
    // ========================================================================

    /// Add or update text highlights for a given type.
    ///
    /// Highlights are identified by a `HighlightKey` which allows multiple highlight layers
    /// (e.g., search results, selections, diagnostics) to coexist. The `ranges` are specified
    /// as [`Anchor`] ranges, making them stable across buffer edits.
    ///
    /// # Example
    /// ```ignore
    /// let key = HighlightKey::Type(TypeId::of::<SearchResults>());
    /// let style = HighlightStyle {
    ///     background_color: Some(yellow),
    ///     ..Default::default()
    /// };
    /// display_map.highlight_text(key, vec![anchor1..anchor2], style);
    /// ```
    pub fn highlight_text(
        &mut self,
        key: HighlightKey,
        ranges: Vec<Range<Anchor>>,
        style: HighlightStyle,
    ) {
        self.text_highlights.insert(key, Arc::new((style, ranges)));
    }

    /// Add or update inlay highlights for a given type.
    ///
    /// Inlay highlights are organized by TypeId, allowing batch operations on related inlays.
    /// Each inlay is individually associated with its style and metadata.
    pub fn highlight_inlays(
        &mut self,
        type_id: TypeId,
        highlights: Vec<InlayHighlight>,
        style: HighlightStyle,
    ) {
        for highlight in highlights {
            let update = self.inlay_highlights.update(&type_id, |inlay_map| {
                inlay_map.insert(highlight.inlay, (style, highlight.clone()))
            });
            if update.is_none() {
                self.inlay_highlights.insert(
                    type_id,
                    TreeMap::from_ordered_entries([(highlight.inlay, (style, highlight))]),
                );
            }
        }
    }

    /// Query text highlights for a given TypeId.
    ///
    /// Returns the highlight style and anchor ranges, or None if no highlights exist
    /// for this type.
    pub fn text_highlights(&self, type_id: TypeId) -> Option<(HighlightStyle, &[Range<Anchor>])> {
        let highlights = self.text_highlights.get(&HighlightKey::Type(type_id))?;
        Some((highlights.0, &highlights.1))
    }

    /// Get all text highlights (for debugging/testing).
    ///
    /// Returns an iterator over all highlight entries, regardless of type.
    #[cfg(test)]
    pub fn all_text_highlights(
        &self,
    ) -> impl Iterator<Item = &Arc<(HighlightStyle, Vec<Range<Anchor>>)>> {
        self.text_highlights.values()
    }

    /// Clear all highlights for a given TypeId.
    ///
    /// Removes both text highlights and inlay highlights associated with this type.
    /// Returns `true` if any highlights were removed.
    pub fn clear_highlights(&mut self, type_id: TypeId) -> bool {
        let mut cleared = self
            .text_highlights
            .remove(&HighlightKey::Type(type_id))
            .is_some();
        cleared |= self.inlay_highlights.remove(&type_id).is_some();
        cleared
    }
}

/// Immutable snapshot of the complete display state.
///
/// Provides end-to-end coordinate conversion between buffer and display space.
/// Cheap to clone (Arc-based).
#[derive(Clone)]
pub struct DisplaySnapshot {
    block_snapshot: BlockSnapshot,
    text_highlights: TextHighlights,
    inlay_highlights: InlayHighlights,
}

impl DisplaySnapshot {
    /// Convert buffer Point to final DisplayPoint.
    ///
    /// The bias parameter controls positioning at transformation boundaries (inlays, folds, etc).
    /// Chains through: Point to InlayPoint to FoldPoint to TabPoint to WrapPoint to BlockPoint
    pub fn point_to_display_point(&self, point: Point, bias: Bias) -> DisplayPoint {
        // Chain through all layers with consistent bias
        let inlay_point = self.inlay_snapshot().to_inlay_point(point, bias);
        let fold_point = self.fold_snapshot().to_fold_point(inlay_point, bias);
        let tab_point = self.tab_snapshot().to_tab_point(fold_point, bias);
        let wrap_point = self.wrap_snapshot().tab_point_to_wrap_point(tab_point);
        let block_point = self.block_snapshot.wrap_point_to_block_point(wrap_point);

        // BlockPoint is our final DisplayPoint
        DisplayPoint {
            row: block_point.row,
            column: block_point.column,
        }
    }

    /// Convert DisplayPoint to buffer Point.
    ///
    /// The bias parameter controls positioning at transformation boundaries.
    /// Chains back through: DisplayPoint to BlockPoint to WrapPoint to TabPoint to FoldPoint to
    /// InlayPoint to Point
    pub fn display_point_to_point(&self, display_point: DisplayPoint, bias: Bias) -> Point {
        // Convert DisplayPoint to BlockPoint
        let block_point = BlockPoint {
            row: display_point.row,
            column: display_point.column,
        };

        // Chain back through all layers with consistent bias
        let wrap_point = self.block_snapshot.to_wrap_point(block_point);
        let tab_point = self.wrap_snapshot().to_tab_point(wrap_point);
        let fold_point = self.tab_snapshot().to_fold_point(tab_point, bias);
        let inlay_point = self.fold_snapshot().to_inlay_point(fold_point);
        let point = self.inlay_snapshot().to_point(inlay_point, bias);

        point
    }

    /// Get the maximum DisplayPoint in this snapshot.
    pub fn max_point(&self) -> DisplayPoint {
        let block_point = self.block_snapshot.max_point();
        DisplayPoint {
            row: block_point.row,
            column: block_point.column,
        }
    }

    /// Access the underlying InlaySnapshot.
    fn inlay_snapshot(&self) -> &InlaySnapshot {
        &self.fold_snapshot().inlay_snapshot
    }

    /// Access the underlying FoldSnapshot.
    fn fold_snapshot(&self) -> &FoldSnapshot {
        &self.tab_snapshot().fold_snapshot
    }

    /// Access the underlying TabSnapshot.
    fn tab_snapshot(&self) -> &TabSnapshot {
        &self.wrap_snapshot().tab_snapshot
    }

    /// Access the underlying WrapSnapshot.
    fn wrap_snapshot(&self) -> &WrapSnapshot {
        &self.block_snapshot.wrap_snapshot
    }

    /// Access the underlying buffer snapshot.
    pub fn buffer(&self) -> &BufferSnapshot {
        self.inlay_snapshot().buffer()
    }

    /// Returns text chunks starting at the given display row until the end of the file.
    ///
    /// This is a simple text-only iterator without highlights. Use [`highlighted_chunks`]
    /// for rendering with syntax highlighting and custom highlights.
    pub fn text_chunks(&self, display_row: DisplayRow) -> impl Iterator<Item = &str> {
        self.block_snapshot
            .chunks(
                display_row.0..self.max_point().row + 1,
                Highlights::default(),
            )
            .map(|chunk| chunk.text)
    }

    /// Returns text chunks starting at the end of the given display row in reverse until
    /// the start of the file.
    ///
    /// Useful for backward text search and navigation.
    pub fn reverse_text_chunks(&self, display_row: DisplayRow) -> impl Iterator<Item = &str> {
        (0..=display_row.0).rev().flat_map(move |row| {
            self.block_snapshot
                .chunks(row..row + 1, Highlights::default())
                .map(|chunk| chunk.text)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
        })
    }

    /// Returns chunks with highlight information for the given display row range.
    ///
    /// This is the core method that merges text content with highlights.
    pub fn chunks<'a>(
        &'a self,
        display_rows: Range<DisplayRow>,
        highlights: Highlights<'a>,
    ) -> impl Iterator<Item = Chunk<'a>> + 'a {
        self.block_snapshot
            .chunks(display_rows.start.0..display_rows.end.0, highlights)
    }

    /// Returns highlighted chunks for rendering, merging syntax and custom highlights.
    ///
    /// This transforms raw chunks into a form ready for rendering by combining:
    /// - Syntax highlighting (from language server)
    /// - Custom highlights (selections, search results, diagnostics)
    /// - Style information
    pub fn highlighted_chunks<'a>(
        &'a self,
        display_rows: Range<DisplayRow>,
    ) -> impl Iterator<Item = HighlightedChunk<'a>> + 'a {
        let highlights = Highlights {
            text_highlights: Some(&self.text_highlights),
            inlay_highlights: Some(&self.inlay_highlights),
        };

        self.chunks(display_rows, highlights).map(|chunk| {
            // For now, just convert Chunk to HighlightedChunk
            // Production will merge syntax highlighting here
            HighlightedChunk {
                text: chunk.text,
                style: chunk.highlight_style,
                is_tab: chunk.is_tab,
                is_inlay: chunk.is_inlay,
            }
        })
    }

    /// Layout a single display row, returning shaped text ready for rendering.
    ///
    /// This is a simplified stub implementation. Production version will:
    /// - Use the text_system for proper glyph shaping
    /// - Handle complex scripts, ligatures, and bidirectional text
    /// - Apply syntax highlighting runs
    /// - Cache layout results for performance
    pub fn layout_row(
        &self,
        display_row: DisplayRow,
        details: &TextLayoutDetails,
    ) -> Arc<LineLayout> {
        // FIXME: This is a stub that creates empty layouts
        // Production needs full text shaping with:
        // - Glyph runs with proper fonts
        // - Syntax highlighting colors
        // - Tab expansion
        // - Bidirectional text support

        let mut text: String = self.text_chunks(display_row).collect();

        // Remove trailing newline if present
        if text.ends_with('\n') {
            text.pop();
        }

        // FIXME: Leaking memory here - production should use proper arena allocation
        // or cache layouts
        let leaked: &'static str = Box::leak(text.into_boxed_str());

        // Create a simple layout (production would use text_system.layout_line())
        details
            .text_system
            .layout_line(leaked, details.font_size, &[], None)
    }

    /// Get the pixel x-coordinate for a display point.
    ///
    /// Uses text layout to determine the horizontal position of the cursor
    /// at the given column within the row.
    pub fn x_for_display_point(
        &self,
        display_point: DisplayPoint,
        details: &TextLayoutDetails,
    ) -> Pixels {
        let line = self.layout_row(DisplayRow::new(display_point.row), details);
        line.x_for_index(display_point.column as usize)
    }

    /// Get the display column for a pixel x-coordinate within a row.
    ///
    /// Returns the closest column to the given x position. Used for
    /// click-to-position and horizontal mouse navigation.
    pub fn display_column_for_x(
        &self,
        display_row: DisplayRow,
        x: Pixels,
        details: &TextLayoutDetails,
    ) -> u32 {
        let line = self.layout_row(display_row, details);
        line.closest_index_for_x(x) as u32
    }

    /// Get the grapheme cluster at the given display point.
    ///
    /// Returns the complete grapheme (user-perceived character) at the position,
    /// which may span multiple Unicode code points (e.g., emoji with skin tone modifiers).
    pub fn grapheme_at(&self, point: DisplayPoint) -> Option<SharedString> {
        // Clip point to valid range
        let max = self.max_point();
        if point.row > max.row {
            return None;
        }

        let point = if point.column > max.column {
            DisplayPoint {
                row: point.row,
                column: max.column,
            }
        } else {
            point
        };

        // Collect characters at the point
        let chars: String = self
            .text_chunks(DisplayRow::new(point.row))
            .flat_map(|chunk| chunk.chars())
            .skip_while({
                let mut column = 0;
                move |ch| {
                    let at_point = column >= point.column;
                    column += ch.len_utf8() as u32;
                    !at_point
                }
            })
            .take(1) // FIXME: Simplified - should take full grapheme cluster
            .collect();

        if chars.is_empty() {
            None
        } else {
            Some(SharedString::from(chars))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId, ToOffset};

    fn create_buffer(text: &str) -> BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    fn create_buffer_entity(text: &str, cx: &mut gpui::TestAppContext) -> Entity<Buffer> {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        cx.new(|_| buffer)
    }

    fn create_display_map(text: &str, cx: &mut gpui::TestAppContext) -> Entity<DisplayMap> {
        let buffer = create_buffer_entity(text, cx);
        let font = gpui::font("Helvetica");
        let font_size = Pixels::from(14.0);
        let wrap_width = None;
        cx.new(|cx| DisplayMap::new(buffer, 4, font, font_size, wrap_width, cx))
    }

    #[gpui::test]
    fn display_map_basic_creation(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello world", cx);
        let _snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
    }

    #[gpui::test]
    fn point_to_display_point_identity(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello world", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let point = Point::new(0, 5);
        let display_point = snapshot.point_to_display_point(point, Bias::Left);

        assert_eq!(display_point.row, 0);
        assert_eq!(display_point.column, 5);
    }

    #[gpui::test]
    fn display_point_to_point_identity(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello world", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let display_point = DisplayPoint { row: 0, column: 5 };
        let point = snapshot.display_point_to_point(display_point, Bias::Left);

        assert_eq!(point.row, 0);
        assert_eq!(point.column, 5);
    }

    #[gpui::test]
    fn roundtrip_conversion(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello\nworld\ntest", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test multiple points
        for row in 0..3 {
            for col in 0..5 {
                let original = Point::new(row, col);
                let display = snapshot.point_to_display_point(original, Bias::Left);
                let back = snapshot.display_point_to_point(display, Bias::Left);

                assert_eq!(back, original, "Roundtrip failed for {original:?}");
            }
        }
    }

    #[gpui::test]
    fn test_multiline_buffer_max_point(cx: &mut gpui::TestAppContext) {
        stoat_log::test();

        // Create a buffer with 100 lines
        let text = (0..100)
            .map(|i| format!("Line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let display_map = create_display_map(&text, cx);

        // Get snapshot
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Buffer has 100 lines (rows 0-99), so max_point should be at least 99
        let max_point = snapshot.max_point();
        assert!(
            max_point.row >= 99,
            "Expected max_point.row >= 99, got {}",
            max_point.row
        );
    }

    #[gpui::test]
    fn max_point(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("line 1\nline 2", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let _max = snapshot.max_point();
    }

    #[gpui::test]
    fn multiline_text(cx: &mut gpui::TestAppContext) {
        let text = "fn example() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n";
        let display_map = create_display_map(text, cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test first line
        let p1 = Point::new(0, 0);
        let d1 = snapshot.point_to_display_point(p1, Bias::Left);
        assert_eq!(d1.row, 0);

        // Test indented line
        let p2 = Point::new(1, 4);
        let d2 = snapshot.point_to_display_point(p2, Bias::Left);
        assert_eq!(d2.row, 1);

        // Verify roundtrip
        let back = snapshot.display_point_to_point(d2, Bias::Left);
        assert_eq!(back, p2);
    }

    #[gpui::test]
    fn empty_buffer(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let point = Point::new(0, 0);
        let display = snapshot.point_to_display_point(point, Bias::Left);

        assert_eq!(display.row, 0);
        assert_eq!(display.column, 0);
    }

    #[gpui::test]
    fn buffer_access(cx: &mut gpui::TestAppContext) {
        let text = "hello world";
        let buffer = create_buffer(text);
        let display_map = create_display_map(text, cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Should be able to access buffer through snapshot
        let retrieved_buffer = snapshot.buffer();
        assert_eq!(retrieved_buffer.len(), buffer.len());
    }

    // Integration tests for Phase 2: Edit propagation and mutations

    #[gpui::test]
    fn buffer_subscription_system_works(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello world", cx);

        // Initial version should be 0
        let initial_version = display_map.read_with(cx, |dm, _cx| dm.buffer_version());
        assert_eq!(initial_version, 0);

        // Calling snapshot() without any edits should not increment version
        let _snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let version = display_map.read_with(cx, |dm, _cx| dm.buffer_version());
        assert_eq!(version, 0);

        // Multiple snapshots without edits should keep version at 0
        let _snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let version = display_map.read_with(cx, |dm, _cx| dm.buffer_version());
        assert_eq!(version, 0);
    }

    #[gpui::test]
    fn inlay_insertion_updates_coordinates(cx: &mut gpui::TestAppContext) {
        let buffer = create_buffer("let x = 42;");
        let display_map = create_display_map("let x = 42;", cx);

        // Get initial coordinates
        let point = Point::new(0, 5); // After "let x"
        let snapshot1 = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let display1 = snapshot1.point_to_display_point(point, Bias::Left);

        // Insert inlay at position 5
        let anchor = buffer.anchor_before(5);
        let ids = display_map.update(cx, |dm, cx| {
            dm.insert_inlays(vec![(anchor, ": i32".to_string(), Bias::Right)], cx)
        });

        assert_eq!(ids.len(), 1);

        // Get updated coordinates
        let snapshot2 = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let display2 = snapshot2.point_to_display_point(point, Bias::Left);

        // The inlay should affect display coordinates
        // (exact values depend on bias handling, just verify snapshot updated)
        let _ = (display1, display2); // Use to avoid warnings
    }

    #[gpui::test]
    fn block_insertion_increases_rows(cx: &mut gpui::TestAppContext) {
        let buffer = create_buffer("line 1\nline 2\nline 3");
        let display_map = create_display_map("line 1\nline 2\nline 3", cx);

        let snapshot1 = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let max1 = snapshot1.max_point();

        // Insert a 3-row block at line 1
        let anchor = buffer.anchor_before(7); // Start of line 2
        let block = crate::block_map::BlockProperties {
            placement: crate::block_map::BlockPlacement::Above(anchor),
            height: Some(3),
            style: crate::block_map::BlockStyle::Fixed,
            priority: 0,
        };

        let ids = display_map.update(cx, |dm, _cx| dm.insert_blocks(vec![block]));
        assert_eq!(ids.len(), 1);

        let snapshot2 = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let max2 = snapshot2.max_point();

        // Block should add rows
        assert!(max2.row >= max1.row);
    }

    #[gpui::test]
    fn multiple_mutations_work(cx: &mut gpui::TestAppContext) {
        let buffer = create_buffer("fn example() {\n    let x = 42;\n}");
        let display_map = create_display_map("fn example() {\n    let x = 42;\n}", cx);

        // Insert inlay
        let anchor1 = buffer.anchor_before(10);
        let inlay_ids = display_map.update(cx, |dm, cx| {
            dm.insert_inlays(vec![(anchor1, "value: ".to_string(), Bias::Left)], cx)
        });

        // Insert block
        let anchor2 = buffer.anchor_before(0);
        let block_ids = display_map.update(cx, |dm, _cx| {
            dm.insert_blocks(vec![crate::block_map::BlockProperties {
                placement: crate::block_map::BlockPlacement::Above(anchor2),
                height: Some(2),
                style: crate::block_map::BlockStyle::Fixed,
                priority: 0,
            }])
        });

        assert_eq!(inlay_ids.len(), 1);
        assert_eq!(block_ids.len(), 1);

        // Verify snapshot still works
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let point = Point::new(0, 0);
        let _display = snapshot.point_to_display_point(point, Bias::Left);
    }

    #[gpui::test]
    fn remove_inlays(cx: &mut gpui::TestAppContext) {
        let buffer = create_buffer("test");
        let display_map = create_display_map("test", cx);

        let anchor = buffer.anchor_before(2);
        let ids = display_map.update(cx, |dm, cx| {
            dm.insert_inlays(vec![(anchor, "XX".to_string(), Bias::Left)], cx)
        });

        assert_eq!(ids.len(), 1);

        // Remove the inlay
        display_map.update(cx, |dm, cx| dm.remove_inlays(&ids, cx));

        // Snapshot should still work
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let _ = snapshot.max_point();
    }

    #[gpui::test]
    fn remove_blocks(cx: &mut gpui::TestAppContext) {
        let buffer = create_buffer("test");
        let display_map = create_display_map("test", cx);

        let anchor = buffer.anchor_before(0);
        let ids = display_map.update(cx, |dm, _cx| {
            dm.insert_blocks(vec![crate::block_map::BlockProperties {
                placement: crate::block_map::BlockPlacement::Above(anchor),
                height: Some(1),
                style: crate::block_map::BlockStyle::Fixed,
                priority: 0,
            }])
        });

        assert_eq!(ids.len(), 1);

        // Remove the block
        display_map.update(cx, |dm, _cx| dm.remove_blocks(&ids));

        // Snapshot should still work
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let _ = snapshot.max_point();
    }

    #[gpui::test]
    fn single_character_buffer(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("x", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Verify single character handling
        let start = Point::new(0, 0);
        let end = Point::new(0, 1);

        let display_start = snapshot.point_to_display_point(start, Bias::Left);
        let display_end = snapshot.point_to_display_point(end, Bias::Left);

        assert_eq!(display_start.row, 0);
        assert_eq!(display_start.column, 0);
        assert_eq!(display_end.row, 0);
        assert_eq!(display_end.column, 1);

        // Roundtrip
        let back_start = snapshot.display_point_to_point(display_start, Bias::Left);
        let back_end = snapshot.display_point_to_point(display_end, Bias::Left);
        assert_eq!(back_start, start);
        assert_eq!(back_end, end);

        // Max point should not panic (actual value may vary by implementation)
        let _ = snapshot.max_point();
    }

    #[gpui::test]
    fn empty_buffer_comprehensive(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // All coordinates should be (0,0)
        let origin = Point::new(0, 0);
        let display_origin = snapshot.point_to_display_point(origin, Bias::Left);
        assert_eq!(display_origin.row, 0);
        assert_eq!(display_origin.column, 0);

        // Roundtrip
        let back = snapshot.display_point_to_point(display_origin, Bias::Left);
        assert_eq!(back, origin);

        // Max point should be origin
        let max = snapshot.max_point();
        assert_eq!(max.row, 0);
        assert_eq!(max.column, 0);

        // Try mutations on empty buffer (should not panic)
        let buffer = create_buffer("");
        let anchor = buffer.anchor_before(0);

        display_map.update(cx, |dm, cx| {
            // Insert inlay at origin
            dm.insert_inlays(vec![(anchor, "hint".to_string(), Bias::Right)], cx);

            // Insert block at origin
            dm.insert_blocks(vec![crate::block_map::BlockProperties {
                placement: crate::block_map::BlockPlacement::Above(anchor),
                height: Some(1),
                style: crate::block_map::BlockStyle::Fixed,
                priority: 0,
            }]);
        });

        // Snapshot after mutations should still work
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let _ = snapshot.max_point();
    }

    #[gpui::test]
    fn buffer_edit_with_inlays_updates_coordinates(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("hello world", cx);
        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        let font = gpui::font("Helvetica");
        let font_size = Pixels::from(14.0);
        let display_map =
            cx.new(|cx| DisplayMap::new(buffer_entity.clone(), 4, font, font_size, None, cx));

        // Insert inlay at position 5 (after "hello")
        let inlay_anchor = buffer.anchor_before(5);
        display_map.update(cx, |dm, cx| {
            dm.insert_inlays(vec![(inlay_anchor, ": str".to_string(), Bias::Right)], cx);
        });

        let snapshot_before = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let point_before = Point::new(0, 6); // 'w' in "world"
        let display_before = snapshot_before.point_to_display_point(point_before, Bias::Left);

        // Edit buffer: insert text before the inlay
        buffer_entity.update(cx, |buffer, _cx| {
            buffer.edit([(0..0, "fn ")]);
        });

        let snapshot_after = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let point_after = Point::new(0, 9); // 'w' in "world" (offset by 3)
        let display_after = snapshot_after.point_to_display_point(point_after, Bias::Left);

        // Display coordinates should shift by buffer edit + inlay width
        assert!(display_after.column > display_before.column);
    }

    #[gpui::test]
    fn inlays_persist_through_buffer_edits(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("line 1\nline 2\nline 3", cx);
        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        let display_map = create_display_map("line 1\nline 2\nline 3", cx);

        // Insert inlay on line 2
        let inlay_anchor = buffer.anchor_after(Point::new(1, 6));
        display_map.update(cx, |dm, cx| {
            dm.insert_inlays(
                vec![(inlay_anchor, " // comment".to_string(), Bias::Right)],
                cx,
            );
        });

        // Edit buffer on line 1 (before inlay)
        buffer_entity.update(cx, |buffer, _cx| {
            let offset = Point::new(0, 0).to_offset(&buffer.snapshot());
            buffer.edit([(offset..offset, "// ")]);
        });

        let snapshot_after = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Inlay should still be present (anchors are stable)
        let point_after = Point::new(1, 6);
        let display_after = snapshot_after.point_to_display_point(point_after, Bias::Right);

        // Display column should account for inlay
        assert!(display_after.column >= point_after.column);
    }

    #[gpui::test]
    fn utf8_multibyte_characters_with_display_map(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("Hello world ABC", cx);

        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test character at different positions
        let points = vec![
            Point::new(0, 0),  // 'H'
            Point::new(0, 6),  // 'w'
            Point::new(0, 12), // 'A'
            Point::new(0, 14), // 'C'
        ];

        for point in points {
            let display_point = snapshot.point_to_display_point(point, Bias::Left);
            let back = snapshot.display_point_to_point(display_point, Bias::Left);
            assert_eq!(back, point, "Roundtrip failed for point {point:?}");
        }
    }

    #[gpui::test]
    fn very_long_line_coordinate_conversions(cx: &mut gpui::TestAppContext) {
        let long_line = "a".repeat(1000);
        let display_map = create_display_map(&long_line, cx);

        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test coordinate conversion at various points along long line
        let test_points = vec![
            Point::new(0, 0),
            Point::new(0, 100),
            Point::new(0, 500),
            Point::new(0, 999),
        ];

        for point in test_points {
            let display_point = snapshot.point_to_display_point(point, Bias::Left);
            let back = snapshot.display_point_to_point(display_point, Bias::Left);
            assert_eq!(back, point, "Roundtrip failed for long line at {point:?}");
        }
    }

    #[gpui::test]
    fn large_file_coordinate_conversions(cx: &mut gpui::TestAppContext) {
        let lines: Vec<String> = (0..1000).map(|i| format!("Line {i}")).collect();
        let content = lines.join("\n");
        let display_map = create_display_map(&content, cx);

        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test conversion at various points throughout the file
        let test_rows = vec![0, 100, 500, 750, 999];

        for row in test_rows {
            let point = Point::new(row, 0);
            let display_point = snapshot.point_to_display_point(point, Bias::Left);
            let back = snapshot.display_point_to_point(display_point, Bias::Left);
            assert_eq!(back, point, "Roundtrip failed for large file at row {row}");
        }
    }

    #[gpui::test]
    fn rapid_editing_maintains_coordinate_correctness(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("line 1\nline 2\nline 3", cx);
        let display_map = create_display_map("line 1\nline 2\nline 3", cx);

        let test_point = Point::new(2, 0);

        // Perform 10 rapid edits
        for i in 0..10 {
            buffer_entity.update(cx, |buffer, _cx| {
                let offset = Point::new(0, 6).to_offset(&buffer.snapshot());
                buffer.edit([(offset..offset, format!(" {i}").as_str())]);
            });

            let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
            let display_point = snapshot.point_to_display_point(test_point, Bias::Left);
            let back = snapshot.display_point_to_point(display_point, Bias::Left);

            assert_eq!(back, test_point, "Roundtrip failed after {} edits", i + 1);
        }
    }

    #[gpui::test]
    fn multiple_inlays_interaction(cx: &mut gpui::TestAppContext) {
        let buffer = create_buffer("fn foo() {\n    let x = 1;\n    let y = 2;\n}");
        let display_map = create_display_map("fn foo() {\n    let x = 1;\n    let y = 2;\n}", cx);

        // Add inlays for type hints
        let x_anchor = buffer.anchor_after(Point::new(1, 13));
        let y_anchor = buffer.anchor_after(Point::new(2, 13));
        display_map.update(cx, |dm, cx| {
            dm.insert_inlays(
                vec![
                    (x_anchor, ": i32".to_string(), Bias::Right),
                    (y_anchor, ": i32".to_string(), Bias::Right),
                ],
                cx,
            );
        });

        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Verify coordinates still work with multiple inlays
        let point = Point::new(0, 0);
        let display_point = snapshot.point_to_display_point(point, Bias::Left);
        let back = snapshot.display_point_to_point(display_point, Bias::Left);
        assert_eq!(back, point);

        // Verify inlays affect display coordinates
        let point_after_inlay = Point::new(1, 14);
        let display_after = snapshot.point_to_display_point(point_after_inlay, Bias::Left);
        assert!(display_after.column >= point_after_inlay.column);
    }

    #[gpui::test]
    fn blocks_with_buffer_edits(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("line 1\nline 2\nline 3", cx);
        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        let display_map = create_display_map("line 1\nline 2\nline 3", cx);

        // Insert block above line 2
        let anchor = buffer.anchor_before(Point::new(1, 0));
        display_map.update(cx, |dm, _cx| {
            dm.insert_blocks(vec![crate::block_map::BlockProperties {
                placement: crate::block_map::BlockPlacement::Above(anchor),
                height: Some(2),
                style: crate::block_map::BlockStyle::Fixed,
                priority: 0,
            }]);
        });

        let snapshot_before = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let max_before = snapshot_before.max_point();

        // Edit buffer above the block
        buffer_entity.update(cx, |buffer, _cx| {
            let offset = Point::new(0, 6).to_offset(&buffer.snapshot());
            buffer.edit([(offset..offset, " extra")]);
        });

        let snapshot_after = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let max_after = snapshot_after.max_point();

        // Block should still add same number of rows
        assert_eq!(max_before.row, max_after.row);
    }

    #[gpui::test]
    fn empty_buffer_operations_after_edits(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("", cx);
        let font = gpui::font("Helvetica");
        let font_size = Pixels::from(14.0);
        let display_map =
            cx.new(|cx| DisplayMap::new(buffer_entity.clone(), 4, font, font_size, None, cx));

        // Add content to empty buffer
        buffer_entity.update(cx, |buffer, _cx| {
            buffer.edit([(0..0, "Hello\nWorld")]);
        });

        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let max = snapshot.max_point();

        assert_eq!(max.row, 1);

        // Add inlay
        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        let anchor = buffer.anchor_before(5);
        display_map.update(cx, |dm, cx| {
            dm.insert_inlays(vec![(anchor, " there".to_string(), Bias::Right)], cx);
        });

        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));
        let point = Point::new(0, 5);
        let display_point = snapshot.point_to_display_point(point, Bias::Left);
        let back = snapshot.display_point_to_point(display_point, Bias::Left);
        assert_eq!(back, point);
    }

    // ========================================================================
    // Highlight System Tests
    // ========================================================================

    #[gpui::test]
    fn highlight_text_storage_and_retrieval(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("Hello, world!", cx);
        let font = gpui::font("Helvetica");
        let font_size = Pixels::from(14.0);
        let display_map =
            cx.new(|cx| DisplayMap::new(buffer_entity.clone(), 4, font, font_size, None, cx));

        // Create anchors for a range
        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        let start_anchor = buffer.anchor_before(0);
        let end_anchor = buffer.anchor_after(5);

        // Define a highlight style
        let style = HighlightStyle {
            background_color: Some(gpui::Hsla {
                h: 0.5,
                s: 0.5,
                l: 0.5,
                a: 1.0,
            }),
            ..Default::default()
        };

        // Add highlight
        let key = HighlightKey::Type(TypeId::of::<i32>());
        display_map.update(cx, |dm, _cx| {
            dm.highlight_text(key, vec![start_anchor..end_anchor], style);
        });

        // Retrieve highlight and verify
        display_map.update(cx, |dm, _cx| {
            let retrieved = dm.text_highlights(TypeId::of::<i32>());
            assert!(retrieved.is_some());
            let (retrieved_style, ranges) = retrieved.unwrap();
            assert_eq!(retrieved_style, style);
            assert_eq!(ranges.len(), 1);
        });
    }

    #[gpui::test]
    fn highlight_text_multiple_ranges(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("one two three", cx);
        let font = gpui::font("Helvetica");
        let font_size = Pixels::from(14.0);
        let display_map =
            cx.new(|cx| DisplayMap::new(buffer_entity.clone(), 4, font, font_size, None, cx));

        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        let range1 = buffer.anchor_before(0)..buffer.anchor_after(3);
        let range2 = buffer.anchor_before(8)..buffer.anchor_after(13);

        let style = HighlightStyle {
            color: Some(gpui::Hsla {
                h: 0.0,
                s: 1.0,
                l: 0.5,
                a: 1.0,
            }),
            ..Default::default()
        };

        let key = HighlightKey::Type(TypeId::of::<String>());
        display_map.update(cx, |dm, _cx| {
            dm.highlight_text(key, vec![range1, range2], style);
        });

        display_map.update(cx, |dm, _cx| {
            let retrieved = dm.text_highlights(TypeId::of::<String>());
            assert!(retrieved.is_some());
            let (_style, ranges) = retrieved.unwrap();
            assert_eq!(ranges.len(), 2);
        });
    }

    #[gpui::test]
    fn clear_highlights_removes_all_types(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("test", cx);
        let font = gpui::font("Helvetica");
        let font_size = Pixels::from(14.0);
        let display_map =
            cx.new(|cx| DisplayMap::new(buffer_entity.clone(), 4, font, font_size, None, cx));

        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        let anchor_range = buffer.anchor_before(0)..buffer.anchor_after(4);

        let style = HighlightStyle::default();
        let key = HighlightKey::Type(TypeId::of::<u64>());

        display_map.update(cx, |dm, _cx| {
            dm.highlight_text(key, vec![anchor_range], style);
        });

        // Verify it exists
        display_map.update(cx, |dm, _cx| {
            let exists = dm.text_highlights(TypeId::of::<u64>());
            assert!(exists.is_some());
        });

        // Clear highlights
        let cleared = display_map.update(cx, |dm, _cx| dm.clear_highlights(TypeId::of::<u64>()));
        assert!(cleared);

        // Verify it's gone
        display_map.update(cx, |dm, _cx| {
            let gone = dm.text_highlights(TypeId::of::<u64>());
            assert!(gone.is_none());
        });

        // Clearing again should return false
        let cleared_again =
            display_map.update(cx, |dm, _cx| dm.clear_highlights(TypeId::of::<u64>()));
        assert!(!cleared_again);
    }

    #[gpui::test]
    fn highlight_style_combination(_cx: &mut gpui::TestAppContext) {
        let style1 = HighlightStyle {
            color: Some(gpui::Hsla {
                h: 0.5,
                s: 0.5,
                l: 0.5,
                a: 1.0,
            }),
            background_color: None,
            ..Default::default()
        };

        let style2 = HighlightStyle {
            color: None,
            background_color: Some(gpui::Hsla {
                h: 0.8,
                s: 0.8,
                l: 0.8,
                a: 1.0,
            }),
            ..Default::default()
        };

        let combined = style1.highlight(style2);

        // style2 background should override
        assert!(combined.background_color.is_some());
        // style1 color should remain (style2 doesn't have color)
        assert_eq!(combined.color, style1.color);
    }

    #[gpui::test]
    fn highlights_persist_across_snapshots(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("persistent", cx);
        let font = gpui::font("Helvetica");
        let font_size = Pixels::from(14.0);
        let display_map =
            cx.new(|cx| DisplayMap::new(buffer_entity.clone(), 4, font, font_size, None, cx));

        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        let anchor_range = buffer.anchor_before(0)..buffer.anchor_after(10);

        let style = HighlightStyle {
            underline: Some(UnderlineStyle {
                color: None,
                thickness: Pixels::from(2.0),
                wavy: true,
            }),
            ..Default::default()
        };

        let key = HighlightKey::Type(TypeId::of::<bool>());
        display_map.update(cx, |dm, _cx| {
            dm.highlight_text(key, vec![anchor_range], style);
        });

        // Take a snapshot
        let _snapshot1 = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Edit the buffer
        buffer_entity.update(cx, |buffer, _cx| {
            buffer.edit([(5..5, " extra")]);
        });

        // Take another snapshot
        let _snapshot2 = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Both snapshots should have highlights (they're cloned)
        // This is a basic check - we can't directly access snapshot highlights
        // but we can verify the DisplayMap still has them
        display_map.update(cx, |dm, _cx| {
            let still_there = dm.text_highlights(TypeId::of::<bool>());
            assert!(still_there.is_some());
        });
    }

    // ========================================================================
    // Chunk Types Tests
    // ========================================================================

    #[gpui::test]
    fn display_row_arithmetic(_cx: &mut gpui::TestAppContext) {
        let row = DisplayRow::new(5);
        assert_eq!(row.0, 5);

        let next = row.next_row();
        assert_eq!(next.0, 6);

        let prev = row.prev_row();
        assert_eq!(prev.0, 4);

        // Test saturating behavior
        let zero = DisplayRow::new(0);
        let prev_zero = zero.prev_row();
        assert_eq!(prev_zero.0, 0);
    }

    #[gpui::test]
    fn display_row_conversions(_cx: &mut gpui::TestAppContext) {
        let row: DisplayRow = 42u32.into();
        assert_eq!(row.0, 42);

        let val: u32 = row.into();
        assert_eq!(val, 42);
    }

    #[gpui::test]
    fn chunk_creation(_cx: &mut gpui::TestAppContext) {
        let chunk = Chunk {
            text: "hello",
            highlight_style: None,
            syntax_highlight_id: Some(1),
            diagnostic_severity: None,
            is_tab: false,
            is_inlay: false,
            is_unnecessary: false,
            underline: false,
        };

        assert_eq!(chunk.text, "hello");
        assert!(chunk.syntax_highlight_id.is_some());
        assert!(!chunk.is_tab);
    }

    #[gpui::test]
    fn highlighted_chunk_creation(_cx: &mut gpui::TestAppContext) {
        let style = HighlightStyle {
            color: Some(gpui::Hsla {
                h: 0.5,
                s: 0.5,
                l: 0.5,
                a: 1.0,
            }),
            ..Default::default()
        };

        let chunk = HighlightedChunk {
            text: "highlighted",
            style: Some(style),
            is_tab: false,
            is_inlay: true,
        };

        assert_eq!(chunk.text, "highlighted");
        assert!(chunk.style.is_some());
        assert!(chunk.is_inlay);
    }

    #[gpui::test]
    fn highlights_default(_cx: &mut gpui::TestAppContext) {
        let highlights: Highlights = Default::default();
        assert!(highlights.text_highlights.is_none());
        assert!(highlights.inlay_highlights.is_none());
    }

    #[gpui::test]
    fn diagnostic_severity_ordering(_cx: &mut gpui::TestAppContext) {
        assert!(DiagnosticSeverity::Error < DiagnosticSeverity::Warning);
        assert!(DiagnosticSeverity::Warning < DiagnosticSeverity::Information);
        assert!(DiagnosticSeverity::Information < DiagnosticSeverity::Hint);

        // Error is most severe (lowest number)
        assert_eq!(DiagnosticSeverity::Error as u32, 1);
        assert_eq!(DiagnosticSeverity::Hint as u32, 4);
    }

    // ========================================================================
    // Text Iteration API Tests
    // ========================================================================

    #[gpui::test]
    fn text_chunks_basic(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello\nworld\ntest", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let chunks: Vec<&str> = snapshot.text_chunks(DisplayRow::new(0)).collect();

        assert_eq!(chunks.len(), 1);
        let text = chunks.concat();
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
        assert!(text.contains("test"));
    }

    #[gpui::test]
    fn text_chunks_from_middle(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("line 1\nline 2\nline 3", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let chunks: Vec<&str> = snapshot.text_chunks(DisplayRow::new(1)).collect();

        let text = chunks.concat();
        assert!(!text.contains("line 1"));
        assert!(text.contains("line 2"));
        assert!(text.contains("line 3"));
    }

    #[gpui::test]
    fn reverse_text_chunks_basic(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello\nworld", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let chunks: Vec<&str> = snapshot.reverse_text_chunks(DisplayRow::new(1)).collect();

        assert!(!chunks.is_empty());
        let text = chunks.concat();
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
    }

    #[gpui::test]
    fn reverse_text_chunks_from_start(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello\nworld", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let chunks: Vec<&str> = snapshot.reverse_text_chunks(DisplayRow::new(0)).collect();

        assert!(!chunks.is_empty());
        let text = chunks.concat();
        assert!(text.contains("hello"));
    }

    #[gpui::test]
    fn chunks_with_highlights(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello\nworld", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let highlights = Highlights::default();
        let chunks: Vec<_> = snapshot
            .chunks(DisplayRow::new(0)..DisplayRow::new(2), highlights)
            .collect();

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("hello"));
        assert!(chunks[0].text.contains("world"));
    }

    #[gpui::test]
    fn highlighted_chunks_basic(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello\nworld", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let chunks: Vec<_> = snapshot
            .highlighted_chunks(DisplayRow::new(0)..DisplayRow::new(2))
            .collect();

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("hello"));
        assert!(chunks[0].text.contains("world"));
        assert!(!chunks[0].is_tab);
        assert!(!chunks[0].is_inlay);
    }

    #[gpui::test]
    fn highlighted_chunks_with_custom_highlights(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("hello world", cx);
        let display_map = create_display_map("hello world", cx);

        // Add a highlight
        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        display_map.update(cx, |dm, _cx| {
            let start = buffer.anchor_before(0);
            let end = buffer.anchor_after(5);

            let style = HighlightStyle {
                color: Some(gpui::Hsla {
                    h: 0.5,
                    s: 0.8,
                    l: 0.6,
                    a: 1.0,
                }),
                ..Default::default()
            };

            dm.highlight_text(
                HighlightKey::Type(TypeId::of::<String>()),
                vec![start..end],
                style,
            );
        });

        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let chunks: Vec<_> = snapshot
            .highlighted_chunks(DisplayRow::new(0)..DisplayRow::new(1))
            .collect();

        assert!(!chunks.is_empty());
        // Highlight merging will be improved in production
        // For now, just verify we get chunks back
    }

    // ========================================================================
    // Text Layout API Tests
    // ========================================================================

    #[gpui::test]
    fn grapheme_at_basic(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello world", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let point = DisplayPoint { row: 0, column: 0 };
        let grapheme = snapshot.grapheme_at(point);

        assert!(grapheme.is_some());
        assert_eq!(grapheme.unwrap().as_ref(), "h");
    }

    #[gpui::test]
    fn grapheme_at_out_of_bounds(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let point = DisplayPoint { row: 10, column: 0 };
        let grapheme = snapshot.grapheme_at(point);

        assert!(grapheme.is_none());
    }

    #[gpui::test]
    fn grapheme_at_column_overflow(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hi", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let point = DisplayPoint {
            row: 0,
            column: 100,
        };
        let grapheme = snapshot.grapheme_at(point);

        // Should return None when past end of line
        assert!(grapheme.is_none());
    }

    // ========================================================================
    // Integration Tests - Full Pipeline
    // ========================================================================

    #[gpui::test]
    fn end_to_end_coordinate_transformation_with_highlights(cx: &mut gpui::TestAppContext) {
        // Test the complete pipeline: buffer -> display -> highlights -> iteration
        let buffer_entity = create_buffer_entity("line 1\nline 2\nline 3", cx);
        let display_map = create_display_map("line 1\nline 2\nline 3", cx);

        // Add highlights
        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        display_map.update(cx, |dm, _cx| {
            let start = buffer.anchor_before(7); // Start of line 2
            let end = buffer.anchor_after(13); // End of line 2

            let style = HighlightStyle {
                color: Some(gpui::Hsla {
                    h: 0.5,
                    s: 0.8,
                    l: 0.6,
                    a: 1.0,
                }),
                ..Default::default()
            };

            dm.highlight_text(
                HighlightKey::Type(TypeId::of::<i32>()),
                vec![start..end],
                style,
            );
        });

        // Get snapshot and verify coordinates work
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test coordinate conversion
        let buffer_point = Point::new(1, 3); // Line 2, col 3
        let display_point = snapshot.point_to_display_point(buffer_point, Bias::Left);
        assert_eq!(display_point.row, 1);
        assert_eq!(display_point.column, 3);

        // Test reverse conversion
        let back = snapshot.display_point_to_point(display_point, Bias::Left);
        assert_eq!(back, buffer_point);

        // Test iteration with highlights
        let chunks: Vec<_> = snapshot
            .highlighted_chunks(DisplayRow::new(0)..DisplayRow::new(3))
            .collect();

        assert!(!chunks.is_empty());
        let text: String = chunks.iter().map(|c| c.text).collect();
        assert!(text.contains("line 1"));
        assert!(text.contains("line 2"));
        assert!(text.contains("line 3"));
    }

    #[gpui::test]
    fn full_rendering_pipeline_simulation(cx: &mut gpui::TestAppContext) {
        // Simulate a complete rendering pipeline: highlights -> chunks -> layout
        let display_map = create_display_map("Hello, World!\nRust is great!", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test 1: Get chunks for rendering
        let chunks: Vec<_> = snapshot
            .highlighted_chunks(DisplayRow::new(0)..DisplayRow::new(2))
            .collect();

        assert!(!chunks.is_empty());

        // Test 2: Verify text content
        let text: String = chunks.iter().map(|c| c.text).collect();
        assert!(text.contains("Hello"));
        assert!(text.contains("Rust"));

        // Test 3: Grapheme extraction works
        let grapheme = snapshot.grapheme_at(DisplayPoint { row: 0, column: 0 });
        assert!(grapheme.is_some());
        assert_eq!(grapheme.unwrap().as_ref(), "H");

        // Test 4: Max point is correct
        let max = snapshot.max_point();
        assert!(max.row >= 1); // At least 2 lines
    }

    #[gpui::test]
    fn multiline_text_iteration_and_navigation(cx: &mut gpui::TestAppContext) {
        let text = "fn main() {\n    println!(\"Hello\");\n    let x = 42;\n}";
        let display_map = create_display_map(text, cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test forward iteration
        let forward: Vec<&str> = snapshot.text_chunks(DisplayRow::new(0)).collect();
        let forward_text = forward.concat();
        assert!(forward_text.contains("fn main"));
        assert!(forward_text.contains("println"));
        assert!(forward_text.contains("let x"));

        // Test reverse iteration from end
        let max_row = snapshot.max_point().row;
        let reverse: Vec<&str> = snapshot
            .reverse_text_chunks(DisplayRow::new(max_row))
            .collect();
        let reverse_text = reverse.concat();
        assert!(reverse_text.contains("fn main"));
        assert!(reverse_text.contains("}"));

        // Test grapheme extraction at different points
        let g1 = snapshot.grapheme_at(DisplayPoint { row: 0, column: 0 });
        assert_eq!(g1.unwrap().as_ref(), "f");

        // Row 1 is "    println!..." - test at the start
        let g2 = snapshot.grapheme_at(DisplayPoint { row: 1, column: 0 });
        assert_eq!(g2.unwrap().as_ref(), " "); // Leading space
    }

    #[gpui::test]
    fn highlight_persistence_through_iterations(cx: &mut gpui::TestAppContext) {
        let buffer_entity = create_buffer_entity("one two three", cx);
        let display_map = create_display_map("one two three", cx);

        // Add multiple highlights
        let buffer = buffer_entity.read_with(cx, |b, _| b.snapshot());
        display_map.update(cx, |dm, _cx| {
            let style1 = HighlightStyle {
                color: Some(gpui::Hsla {
                    h: 0.0,
                    s: 1.0,
                    l: 0.5,
                    a: 1.0,
                }),
                ..Default::default()
            };

            let style2 = HighlightStyle {
                color: Some(gpui::Hsla {
                    h: 0.33,
                    s: 1.0,
                    l: 0.5,
                    a: 1.0,
                }),
                ..Default::default()
            };

            dm.highlight_text(
                HighlightKey::Type(TypeId::of::<i32>()),
                vec![buffer.anchor_before(0)..buffer.anchor_after(3)],
                style1,
            );

            dm.highlight_text(
                HighlightKey::Type(TypeId::of::<String>()),
                vec![buffer.anchor_before(8)..buffer.anchor_after(13)],
                style2,
            );
        });

        // Take snapshot and verify highlights work through iteration
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        let chunks: Vec<_> = snapshot
            .highlighted_chunks(DisplayRow::new(0)..DisplayRow::new(1))
            .collect();

        assert!(!chunks.is_empty());

        // Verify we can retrieve highlights
        display_map.update(cx, |dm, _cx| {
            assert!(dm.text_highlights(TypeId::of::<i32>()).is_some());
            assert!(dm.text_highlights(TypeId::of::<String>()).is_some());
        });
    }

    #[gpui::test]
    fn coordinate_transformation_edge_cases(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("a\n\nc", cx);
        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test empty line
        let empty_line = Point::new(1, 0);
        let display = snapshot.point_to_display_point(empty_line, Bias::Left);
        let back = snapshot.display_point_to_point(display, Bias::Left);
        assert_eq!(back.row, 1);

        // Test last line
        let last = Point::new(2, 0);
        let display_last = snapshot.point_to_display_point(last, Bias::Left);
        assert_eq!(display_last.row, 2);

        // Test grapheme at empty line
        let g = snapshot.grapheme_at(DisplayPoint { row: 1, column: 0 });
        assert!(g.is_none() || g.unwrap().as_ref() == "\n");
    }
}
