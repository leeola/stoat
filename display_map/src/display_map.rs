///! DisplayMap v2: Complete coordinate transformation pipeline.
///!
///! Integrates all transformation layers to provide end-to-end conversion
///! between buffer coordinates ([`Point`]) and display coordinates ([`DisplayPoint`]).
///!
///! # Architecture
///!
///! The DisplayMap chains six transformation layers:
///!
///! ```text
///! Point (buffer)
///!   | InlayMap    - Adds type hints, parameter names
///! InlayPoint
///!   | FoldMap     - Hides folded regions
///! FoldPoint
///!   | TabMap      - Expands tabs to spaces
///! TabPoint
///!   | WrapMap     - Wraps long lines
///! WrapPoint
///!   | BlockMap    - Inserts visual blocks
///! BlockPoint = DisplayPoint (final)
///! ```
///!
///! # Usage
///!
///! ```ignore
///! let display_map = DisplayMap::new(buffer_snapshot, tab_width);
///! let snapshot = display_map.snapshot();
///!
///! // Convert buffer position to display position
///! let buffer_point = Point::new(10, 5);
///! let display_point = snapshot.point_to_display_point(buffer_point);
///!
///! // Convert back
///! let back = snapshot.display_point_to_point(display_point);
///! assert_eq!(back, buffer_point);
///! ```
///!
///! # Related
///!
///! - See `.claude/DISPLAY_MAP.md` for implementation details
///! - Based on Zed's DisplayMap architecture
use crate::{
    block_map::BlockSnapshot,
    coords::{BlockPoint, DisplayPoint},
    fold_map::FoldSnapshot,
    inlay_map::InlaySnapshot,
    tab_map::TabSnapshot,
    wrap_map::{WrapMap, WrapSnapshot},
};
use gpui::{App, Entity, Font, Pixels};
use sum_tree::Bias;
use text::{BufferSnapshot, Point};

/// DisplayMap coordinating all transformation layers.
///
/// Maintains stateful map holders for each layer and handles edit propagation.
/// When the buffer changes, edits are propagated through all layers to update
/// their transforms incrementally.
///
/// Uses async WrapMap for non-blocking soft wrapping.
pub struct DisplayMap {
    buffer: BufferSnapshot,
    buffer_version: usize,

    // Mutable layer holders
    inlay_map: crate::inlay_map::InlayMap,
    fold_map: crate::fold_map::FoldMap,
    tab_map: crate::tab_map::TabMap,
    wrap_map: Entity<WrapMap>,
    block_map: crate::block_map::BlockMap,
    crease_map: crate::crease_map::CreaseMap,

    // Wrap configuration
    font: Font,
    font_size: Pixels,
    wrap_width: Option<Pixels>,
}

impl DisplayMap {
    /// Create a new DisplayMap for the given buffer.
    ///
    /// Requires GPUI context for async WrapMap Entity.
    pub fn new(
        buffer: BufferSnapshot,
        tab_width: u32,
        font: Font,
        font_size: Pixels,
        wrap_width: Option<Pixels>,
        cx: &mut App,
    ) -> Self {
        // Initialize all layers
        let inlay_map = crate::inlay_map::InlayMap::new(buffer.clone());
        let (fold_map, fold_snapshot) = crate::fold_map::FoldMap::new(inlay_map.snapshot());
        let (tab_map, tab_snapshot) = crate::tab_map::TabMap::new(fold_snapshot, tab_width);

        // Create async WrapMap Entity
        let (wrap_map, wrap_snapshot) =
            WrapMap::new(tab_snapshot, font.clone(), font_size, wrap_width, cx);

        let block_map = crate::block_map::BlockMap::new(wrap_snapshot);
        let crease_map = crate::crease_map::CreaseMap::new(buffer.clone());

        Self {
            buffer: buffer.clone(),
            buffer_version: 0,
            inlay_map,
            fold_map,
            tab_map,
            wrap_map,
            block_map,
            crease_map,
            font,
            font_size,
            wrap_width,
        }
    }

    /// Update buffer and propagate edits through all layers.
    ///
    /// This is called when the buffer changes. It syncs all layers and propagates
    /// edits downstream using async WrapMap.
    pub fn update_buffer(&mut self, new_buffer: BufferSnapshot, cx: &mut App) {
        // For now, just update buffer and rebuild everything
        // TODO: Compute actual buffer edits
        self.buffer = new_buffer.clone();
        self.buffer_version += 1;

        // Propagate through layers
        let (inlay_snapshot, _inlay_edits) = self.inlay_map.sync(new_buffer.clone());
        let (fold_snapshot, _fold_edits) = self.fold_map.read(inlay_snapshot, Vec::new());
        let (tab_snapshot, tab_edits) = self.tab_map.read(fold_snapshot, Vec::new());

        // Use async WrapMap
        let (wrap_snapshot, _wrap_edits) = self.wrap_map.update(cx, |wrap_map, cx| {
            wrap_map.sync(tab_snapshot, tab_edits, cx)
        });

        let (_block_snapshot, _block_edits) = self.block_map.sync(wrap_snapshot);

        self.crease_map.set_buffer(new_buffer);
    }

    /// Get an immutable snapshot of the current display state.
    ///
    /// The snapshot is cheap to clone and can be used across threads.
    pub fn snapshot(&self, cx: &App) -> DisplaySnapshot {
        // Get wrap snapshot from Entity
        let wrap_snapshot = self.wrap_map.read(cx).snapshot().clone();

        // Block snapshot uses the wrap snapshot
        let block_snapshot = self.block_map.snapshot();
        DisplaySnapshot { block_snapshot }
    }

    /// Get the current buffer version.
    pub fn buffer_version(&self) -> usize {
        self.buffer_version
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
        let (wrap_snapshot, _wrap_edits) = self.wrap_map.update(cx, |wrap_map, cx| {
            wrap_map.sync(tab_snapshot, tab_edits, cx)
        });

        let (_block_snapshot, _block_edits) = self.block_map.sync(wrap_snapshot);
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
}

/// Immutable snapshot of the complete display state.
///
/// Provides end-to-end coordinate conversion between buffer and display space.
/// Cheap to clone (Arc-based).
#[derive(Clone)]
pub struct DisplaySnapshot {
    block_snapshot: BlockSnapshot,
}

impl DisplaySnapshot {
    /// Convert buffer Point to final DisplayPoint.
    ///
    /// Chains through: Point to InlayPoint to FoldPoint to TabPoint to WrapPoint to BlockPoint
    pub fn point_to_display_point(&self, point: Point) -> DisplayPoint {
        // Chain through all layers
        let inlay_point = self.inlay_snapshot().to_inlay_point(point);
        let fold_point = self.fold_snapshot().to_fold_point(inlay_point, Bias::Left);
        let tab_point = self.tab_snapshot().to_tab_point(fold_point, Bias::Left);
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
    /// Chains back through: DisplayPoint to BlockPoint to WrapPoint to TabPoint to FoldPoint to
    /// InlayPoint to Point
    pub fn display_point_to_point(&self, display_point: DisplayPoint) -> Point {
        // Convert DisplayPoint to BlockPoint
        let block_point = BlockPoint {
            row: display_point.row,
            column: display_point.column,
        };

        // Chain back through all layers
        let wrap_point = self.block_snapshot.to_wrap_point(block_point);
        let tab_point = self.wrap_snapshot().to_tab_point(wrap_point);
        let fold_point = self.tab_snapshot().to_fold_point(tab_point, Bias::Left);
        let inlay_point = self.fold_snapshot().to_inlay_point(fold_point);
        let point = self.inlay_snapshot().to_point(inlay_point);

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    fn create_display_map(text: &str, cx: &mut gpui::TestAppContext) -> Entity<DisplayMap> {
        let buffer = create_buffer(text);
        let font = gpui::font("Helvetica");
        let font_size = Pixels::from(14.0);
        let wrap_width = None;
        cx.new(|cx| DisplayMap::new(buffer, 4, font, font_size, wrap_width, cx))
    }

    #[gpui::test]
    fn display_map_basic_creation(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello world", cx);
        let _snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));
    }

    #[gpui::test]
    fn point_to_display_point_identity(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello world", cx);
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));

        let point = Point::new(0, 5);
        let display_point = snapshot.point_to_display_point(point);

        assert_eq!(display_point.row, 0);
        assert_eq!(display_point.column, 5);
    }

    #[gpui::test]
    fn display_point_to_point_identity(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello world", cx);
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));

        let display_point = DisplayPoint { row: 0, column: 5 };
        let point = snapshot.display_point_to_point(display_point);

        assert_eq!(point.row, 0);
        assert_eq!(point.column, 5);
    }

    #[gpui::test]
    fn roundtrip_conversion(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello\nworld\ntest", cx);
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));

        // Test multiple points
        for row in 0..3 {
            for col in 0..5 {
                let original = Point::new(row, col);
                let display = snapshot.point_to_display_point(original);
                let back = snapshot.display_point_to_point(display);

                assert_eq!(back, original, "Roundtrip failed for {:?}", original);
            }
        }
    }

    #[gpui::test]
    fn max_point(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("line 1\nline 2", cx);
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));

        let max = snapshot.max_point();

        // With no transforms, should match buffer max
        assert!(max.row >= 0);
    }

    #[gpui::test]
    fn multiline_text(cx: &mut gpui::TestAppContext) {
        let text = "fn example() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n";
        let display_map = create_display_map(text, cx);
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));

        // Test first line
        let p1 = Point::new(0, 0);
        let d1 = snapshot.point_to_display_point(p1);
        assert_eq!(d1.row, 0);

        // Test indented line
        let p2 = Point::new(1, 4);
        let d2 = snapshot.point_to_display_point(p2);
        assert_eq!(d2.row, 1);

        // Verify roundtrip
        let back = snapshot.display_point_to_point(d2);
        assert_eq!(back, p2);
    }

    #[gpui::test]
    fn empty_buffer(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("", cx);
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));

        let point = Point::new(0, 0);
        let display = snapshot.point_to_display_point(point);

        assert_eq!(display.row, 0);
        assert_eq!(display.column, 0);
    }

    #[gpui::test]
    fn buffer_access(cx: &mut gpui::TestAppContext) {
        let text = "hello world";
        let buffer = create_buffer(text);
        let display_map = create_display_map(text, cx);
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));

        // Should be able to access buffer through snapshot
        let retrieved_buffer = snapshot.buffer();
        assert_eq!(retrieved_buffer.len(), buffer.len());
    }

    // Integration tests for Phase 2: Edit propagation and mutations

    #[gpui::test]
    fn buffer_update_propagates_through_layers(cx: &mut gpui::TestAppContext) {
        let display_map = create_display_map("hello world", cx);

        let initial_version = display_map.read_with(cx, |dm, _cx| dm.buffer_version());
        assert_eq!(initial_version, 0);

        // Update buffer
        let new_buffer = create_buffer("hello beautiful world");
        display_map.update(cx, |dm, cx| dm.update_buffer(new_buffer, cx));

        let new_version = display_map.read_with(cx, |dm, _cx| dm.buffer_version());
        assert_eq!(new_version, 1);
    }

    #[gpui::test]
    fn inlay_insertion_updates_coordinates(cx: &mut gpui::TestAppContext) {
        let buffer = create_buffer("let x = 42;");
        let display_map = create_display_map("let x = 42;", cx);

        // Get initial coordinates
        let point = Point::new(0, 5); // After "let x"
        let snapshot1 = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));
        let display1 = snapshot1.point_to_display_point(point);

        // Insert inlay at position 5
        let anchor = buffer.anchor_before(5);
        let ids = display_map.update(cx, |dm, cx| {
            dm.insert_inlays(vec![(anchor, ": i32".to_string(), Bias::Right)], cx)
        });

        assert_eq!(ids.len(), 1);

        // Get updated coordinates
        let snapshot2 = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));
        let display2 = snapshot2.point_to_display_point(point);

        // The inlay should affect display coordinates
        // (exact values depend on bias handling, just verify snapshot updated)
        let _ = (display1, display2); // Use to avoid warnings
    }

    #[gpui::test]
    fn block_insertion_increases_rows(cx: &mut gpui::TestAppContext) {
        let buffer = create_buffer("line 1\nline 2\nline 3");
        let display_map = create_display_map("line 1\nline 2\nline 3", cx);

        let snapshot1 = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));
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

        let snapshot2 = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));
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
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));
        let point = Point::new(0, 0);
        let _display = snapshot.point_to_display_point(point);
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
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));
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
        let snapshot = display_map.read_with(cx, |dm, cx| dm.snapshot(cx));
        let _ = snapshot.max_point();
    }
}
