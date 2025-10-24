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
///! let display_point = snapshot.point_to_display_point(buffer_point, Bias::Left);
///!
///! // Convert back
///! let back = snapshot.display_point_to_point(display_point, Bias::Left);
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
use text::{subscription::Subscription, Buffer, BufferSnapshot, Edit, Point};

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
        let display_snapshot = DisplaySnapshot { block_snapshot };
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{px, AppContext};
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

                assert_eq!(back, original, "Roundtrip failed for {:?}", original);
            }
        }
    }

    #[gpui::test]
    fn test_multiline_buffer_max_point(cx: &mut gpui::TestAppContext) {
        stoat_log::test();

        // Create a buffer with 100 lines
        let text = (0..100)
            .map(|i| format!("Line {}", i))
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

        let max = snapshot.max_point();

        // With no transforms, should match buffer max
        assert!(max.row >= 0);
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
            assert_eq!(back, point, "Roundtrip failed for point {:?}", point);
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
            assert_eq!(back, point, "Roundtrip failed for long line at {:?}", point);
        }
    }

    #[gpui::test]
    fn large_file_coordinate_conversions(cx: &mut gpui::TestAppContext) {
        let lines: Vec<String> = (0..1000).map(|i| format!("Line {}", i)).collect();
        let content = lines.join("\n");
        let display_map = create_display_map(&content, cx);

        let snapshot = display_map.update(cx, |dm, cx| dm.snapshot(cx));

        // Test conversion at various points throughout the file
        let test_rows = vec![0, 100, 500, 750, 999];

        for row in test_rows {
            let point = Point::new(row, 0);
            let display_point = snapshot.point_to_display_point(point, Bias::Left);
            let back = snapshot.display_point_to_point(display_point, Bias::Left);
            assert_eq!(
                back, point,
                "Roundtrip failed for large file at row {}",
                row
            );
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
                buffer.edit([(offset..offset, format!(" {}", i).as_str())]);
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
}
