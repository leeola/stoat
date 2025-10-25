//! WrapMap v2: Soft-wrapping transformation layer with async background processing.
//!
//! This implementation uses the Transform pattern with async wrapping for optimal
//! performance:
//! - **Interpolation**: Instant response by assuming wraps stay unchanged
//! - **Background wrapping**: Real wrapping using `LineWrapper` in async task
//! - **5ms timeout**: Fast completions update immediately, slow ones continue in background
//!
//! # Transform Architecture
//!
//! Unlike InlayMap (enum) and FoldMap (struct with Option), WrapMap uses a struct with
//! optional `display_text` field:
//! - `display_text == None`: Isomorphic transform (no wrap)
//! - `display_text == Some(...)`: Wrap transform (soft wrap with indent)
//!
//! # Coordinate Transformation
//!
//! Wraps **add rows** to display:
//! ```text
//! TabPoint (input):         WrapPoint (output):
//! Row 0, Col 0-100:         Row 0: "This is a very long..."
//!                           Row 1: "  line that wraps"
//! Row 1: "Next line"        Row 2: "Next line"
//! ```
//!
//! # Async Wrapping Flow
//!
//! 1. Edit arrives - queue in `pending_edits`
//! 2. Call `interpolate()` - fast estimate (assume wraps stay same)
//! 3. Return snapshot immediately
//! 4. Spawn background task with `LineWrapper`
//! 5. If completes <5ms: update snapshot with real wraps
//! 6. Else: continue in background, notify when done
//! 7. Compose `interpolated_edits.invert()` with real edits
//!
//! # Performance
//!
//! - Interpolation: O(log n) - instant (<1ms)
//! - Real wrapping: O(file_size) - background (20-50ms for large files)
//! - UI never blocks: Always returns interpolated snapshot first
//!
//! # Point vs Anchor
//!
//! WrapMap correctly uses [`Point`] coordinates (not [`text::Anchor`]) because:
//! - **Ephemeral transformations**: Wraps are recalculated on width/font changes
//! - **No persistence needed**: Transform tree rebuilt on each sync
//! - **Pixel-dependent**: Wrapping depends on font metrics, not buffer positions
//! - **Derived from stable sources**: Input comes from TabSnapshot which derives from
//!   FoldSnapshot's `Range<Anchor>` storage
//!
//! Wraps don't need to "survive" buffer edits - they're recalculated from the
//! updated tab coordinates which are themselves derived from stable fold anchors.
//!
//! # Related
//!
//! - Input: [`TabPoint`](crate::TabPoint) from [`TabSnapshot`](crate::tab_map::TabSnapshot)
//! - Output: [`WrapPoint`](crate::WrapPoint)
//! - Uses [`gpui::LineWrapper`] for pixel-based wrapping

use crate::{
    coords::{TabPoint, WrapPoint},
    dimensions::TabOffset,
    tab_map::TabSnapshot,
};
use gpui::{App, AppContext, Context, Entity, Font, LineWrapper, Pixels, Task};
use smol::future::yield_now;
use std::{collections::VecDeque, mem, ops::Range, sync::LazyLock, time::Duration};
use sum_tree::{Bias, Item, SumTree};
use text::{Edit, OffsetUtf16, Patch, Point, TextSummary};

/// Edit in TabPoint space (input to WrapMap from TabMap).
pub type TabEdit = Edit<TabPoint>;

/// Edit in WrapOffset space (output from WrapMap).
pub type WrapEdit = Edit<u32>;

/// Transform representing either an isomorphic region or a wrap point.
///
/// This is a **struct** where `display_text` determines the type:
/// - `display_text == None`: Isomorphic transform (no wrap, 1:1 mapping)
/// - `display_text == Some(...)`: Wrap transform (soft wrap with indent)
///
/// The wrap's display text is a newline followed by spaces for indentation,
/// stored as a static string for efficiency.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Transform {
    /// Aggregated summary of input/output coordinates.
    summary: TransformSummary,

    /// If Some, this is a wrap point. The string contains newline + indent spaces.
    /// None indicates isomorphic region.
    display_text: Option<&'static str>,
}

impl Transform {
    /// Create an isomorphic transform with 1:1 mapping.
    ///
    /// Input and output summaries are identical since no wrapping occurs.
    fn isomorphic(summary: TextSummary) -> Self {
        #[cfg(test)]
        assert!(
            !summary.lines.is_zero(),
            "Isomorphic transform must have content"
        );

        Self {
            summary: TransformSummary {
                input: summary,
                output: summary,
            },
            display_text: None,
        }
    }

    /// Create a wrap transform with the given indent amount.
    ///
    /// The wrap has:
    /// - Zero input (doesn't consume tab space)
    /// - One output line with `indent` columns
    /// - Static display text: newline + indent spaces
    fn wrap(indent: u32) -> Self {
        // Preallocated wrap text with max indent
        static WRAP_TEXT: LazyLock<String> = LazyLock::new(|| {
            let mut text = String::new();
            text.push('\n');
            text.extend((0..LineWrapper::MAX_INDENT as usize).map(|_| ' '));
            text
        });

        Self {
            summary: TransformSummary {
                input: TextSummary::default(),
                output: TextSummary {
                    lines: Point::new(1, indent),
                    first_line_chars: 0,
                    last_line_chars: indent,
                    longest_row: 1,
                    longest_row_chars: indent,
                    len: 1 + indent as usize,
                    chars: indent as usize,
                    last_line_len_utf16: indent,
                    len_utf16: OffsetUtf16(1 + indent as usize),
                },
            },
            display_text: Some(&WRAP_TEXT[..1 + indent as usize]),
        }
    }

    /// Check if this transform is isomorphic (no wrap).
    fn is_isomorphic(&self) -> bool {
        self.display_text.is_none()
    }
}

/// Summary aggregating coordinate information for a Transform subtree.
///
/// Tracks both input (TabPoint) and output (WrapPoint) coordinate spaces.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TransformSummary {
    /// Input summary (TabPoint space before wrapping).
    input: TextSummary,

    /// Output summary (WrapPoint space after wrapping).
    output: TextSummary,
}

impl sum_tree::ContextLessSummary for TransformSummary {
    fn zero() -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
    }
}

impl Item for Transform {
    type Summary = TransformSummary;

    fn summary(&self, _cx: ()) -> Self::Summary {
        self.summary.clone()
    }
}

/// Push an isomorphic transform, merging with the last transform if it's also isomorphic.
///
/// This optimization reduces transform tree size by combining adjacent unchanged regions.
fn push_isomorphic(transforms: &mut Vec<Transform>, summary: TextSummary) {
    if let Some(last) = transforms.last_mut() {
        if last.is_isomorphic() {
            last.summary.input += &summary;
            last.summary.output += &summary;
            return;
        }
    }
    transforms.push(Transform::isomorphic(summary));
}

/// Extension trait for SumTree<Transform> to handle push-or-merge logic.
trait SumTreeExt {
    /// Push a transform, or extend the last one if they're both isomorphic.
    fn push_or_extend(&mut self, transform: Transform);
}

impl SumTreeExt for SumTree<Transform> {
    fn push_or_extend(&mut self, transform: Transform) {
        if transform.is_isomorphic() {
            let mut did_extend = false;
            self.update_last(
                |last| {
                    if last.is_isomorphic() {
                        last.summary.input += &transform.summary.input;
                        last.summary.output += &transform.summary.output;
                        did_extend = true;
                    }
                },
                (),
            );

            if !did_extend {
                self.push(transform, ());
            }
        } else {
            self.push(transform, ());
        }
    }
}

// Dimension trait implementations for coordinate seeking

impl<'a> sum_tree::Dimension<'a, TransformSummary> for TabPoint {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        let lines = &summary.input.lines;
        if lines.row > 0 {
            self.row += lines.row;
            self.column = lines.column;
        } else {
            self.column += lines.column;
        }
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for WrapPoint {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        let lines = &summary.output.lines;
        if lines.row > 0 {
            self.row += lines.row;
            self.column = lines.column;
        } else {
            self.column += lines.column;
        }
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for TabOffset {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        self.0 += summary.input.len;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for u32 {
    fn zero(_cx: ()) -> Self {
        0
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: ()) {
        *self += summary.output.len as u32;
    }
}

#[cfg(test)]
mod tests_transform {
    use super::*;

    #[test]
    fn transform_isomorphic() {
        let summary = TextSummary::from("hello world");
        let transform = Transform::isomorphic(summary.clone());

        assert!(transform.is_isomorphic());
        assert_eq!(transform.summary.input, summary);
        assert_eq!(transform.summary.output, summary);
        assert_eq!(transform.display_text, None);
    }

    #[test]
    fn transform_wrap() {
        let transform = Transform::wrap(4);

        assert!(!transform.is_isomorphic());
        assert_eq!(transform.summary.input, TextSummary::default());
        assert_eq!(transform.summary.output.lines, Point::new(1, 4));
        assert_eq!(transform.summary.output.last_line_chars, 4);
        assert!(transform.display_text.is_some());
        assert_eq!(transform.display_text.unwrap(), "\n    ");
    }

    #[test]
    fn push_isomorphic_merges() {
        let mut transforms = Vec::new();

        let summary1 = TextSummary::from("hello ");
        let summary2 = TextSummary::from("world");

        push_isomorphic(&mut transforms, summary1.clone());
        push_isomorphic(&mut transforms, summary2.clone());

        // Should have merged into one transform
        assert_eq!(transforms.len(), 1);
        assert_eq!(
            transforms[0].summary.input,
            summary1.clone() + summary2.clone()
        );
    }

    #[test]
    fn push_isomorphic_doesnt_merge_with_wrap() {
        let mut transforms = Vec::new();

        transforms.push(Transform::wrap(4));
        push_isomorphic(&mut transforms, TextSummary::from("hello"));

        // Should have 2 transforms (wrap doesn't merge with isomorphic)
        assert_eq!(transforms.len(), 2);
    }
}

/// Mutable WrapMap managing async wrapping state.
///
/// Uses GPUI Entity pattern for background task management and state updates.
/// Provides instant UI response via interpolation while real wrapping runs in background.
pub struct WrapMap {
    /// Current snapshot (may be interpolated or real).
    snapshot: WrapSnapshot,

    /// Queue of pending edits waiting to be processed.
    ///
    /// Edits accumulate while background tasks run, then get batch-processed.
    pending_edits: VecDeque<(TabSnapshot, Vec<TabEdit>)>,

    /// Edits from interpolation that haven't been replaced by real wraps yet.
    ///
    /// When background task completes, these are inverted and composed with real edits.
    interpolated_edits: Patch<u32>,

    /// Accumulated edits since last sync, returned to caller.
    edits_since_sync: Patch<u32>,

    /// Current wrap width in pixels.
    ///
    /// None means no wrapping (infinite width).
    wrap_width: Option<Pixels>,

    /// Background task computing real wraps.
    ///
    /// When present, indicates wrapping is in progress.
    background_task: Option<Task<()>>,

    /// Font and size for LineWrapper.
    font_with_size: (Font, Pixels),
}

impl WrapMap {
    /// Create a new WrapMap Entity with the given configuration.
    ///
    /// Returns an Entity handle and the initial snapshot. The Entity manages async
    /// background tasks and can be updated through the handle.
    pub fn new(
        tab_snapshot: TabSnapshot,
        font: Font,
        font_size: Pixels,
        wrap_width: Option<Pixels>,
        cx: &mut App,
    ) -> (Entity<Self>, WrapSnapshot) {
        let handle = cx.new(|cx| {
            let mut this = Self {
                font_with_size: (font, font_size),
                wrap_width: None,
                pending_edits: VecDeque::new(),
                interpolated_edits: Patch::default(),
                edits_since_sync: Patch::default(),
                snapshot: WrapSnapshot::new(tab_snapshot),
                background_task: None,
            };
            this.set_wrap_width(wrap_width, cx);
            mem::take(&mut this.edits_since_sync);
            this
        });
        let snapshot = handle.read(cx).snapshot.clone();
        (handle, snapshot)
    }

    /// Get the current wrap snapshot.
    pub fn snapshot(&self) -> &WrapSnapshot {
        &self.snapshot
    }

    pub fn is_rewrapping(&self) -> bool {
        self.background_task.is_some()
    }

    /// Sync with new tab snapshot and handle incoming edits.
    ///
    /// If wrapping is enabled, queues edits and processes them asynchronously.
    /// Returns the current snapshot and accumulated edits since last sync.
    pub fn sync(
        &mut self,
        tab_snapshot: TabSnapshot,
        edits: Vec<TabEdit>,
        cx: &mut Context<Self>,
    ) -> (WrapSnapshot, Patch<u32>) {
        let tab_max = tab_snapshot.max_point();

        if self.wrap_width.is_some() {
            // If transforms are empty (initial sync), use interpolate to get a valid
            // snapshot immediately. Background rewrapping will refine it later.
            if self.snapshot.transforms.is_empty() {
                self.edits_since_sync = self
                    .edits_since_sync
                    .compose(self.snapshot.interpolate(tab_snapshot.clone(), &edits));
            }

            self.pending_edits.push_back((tab_snapshot, edits));
            self.flush_edits(cx);
        } else {
            self.edits_since_sync = self
                .edits_since_sync
                .compose(self.snapshot.interpolate(tab_snapshot, &edits));
            self.snapshot.interpolated = false;
        }

        let wrap_max = self.snapshot.max_point();
        tracing::trace!(
            "WrapMap.sync: tab_max=({}, {}) -> wrap_max=({}, {})",
            tab_max.row,
            tab_max.column,
            wrap_max.row,
            wrap_max.column
        );

        (self.snapshot.clone(), mem::take(&mut self.edits_since_sync))
    }

    /// Update font and font size, triggering rewrap if changed.
    pub fn set_font_with_size(
        &mut self,
        font: Font,
        font_size: Pixels,
        cx: &mut Context<Self>,
    ) -> bool {
        let font_with_size = (font, font_size);

        if font_with_size == self.font_with_size {
            false
        } else {
            self.font_with_size = font_with_size;
            self.rewrap(cx);
            true
        }
    }

    /// Update wrap width, triggering rewrap if changed.
    pub fn set_wrap_width(&mut self, wrap_width: Option<Pixels>, cx: &mut Context<Self>) -> bool {
        if wrap_width == self.wrap_width {
            return false;
        }

        self.wrap_width = wrap_width;
        self.rewrap(cx);
        true
    }

    /// Rewrap entire file with new configuration.
    ///
    /// Clears pending state and spawns background task with 5ms timeout.
    fn rewrap(&mut self, cx: &mut Context<Self>) {
        self.background_task.take();
        self.interpolated_edits.clear();
        self.pending_edits.clear();

        if let Some(wrap_width) = self.wrap_width {
            let mut new_snapshot = self.snapshot.clone();

            let text_system = cx.text_system().clone();
            let (font, font_size) = self.font_with_size.clone();
            let task = cx.background_spawn(async move {
                let mut line_wrapper = text_system.line_wrapper(font, font_size);
                let tab_snapshot = new_snapshot.tab_snapshot.clone();
                let range = TabPoint::new(0, 0)..tab_snapshot.max_point();
                let edits = new_snapshot
                    .update(
                        tab_snapshot,
                        &[TabEdit {
                            old: range.clone(),
                            new: range.clone(),
                        }],
                        wrap_width,
                        &mut line_wrapper,
                    )
                    .await;
                (new_snapshot, edits)
            });

            match cx
                .background_executor()
                .block_with_timeout(Duration::from_millis(5), task)
            {
                Ok((snapshot, edits)) => {
                    self.snapshot = snapshot;
                    self.edits_since_sync = self.edits_since_sync.compose(&edits);
                },
                Err(wrap_task) => {
                    self.background_task = Some(cx.spawn(async move |this, cx| {
                        let (snapshot, edits) = wrap_task.await;
                        this.update(cx, |this, cx| {
                            this.snapshot = snapshot;
                            this.edits_since_sync = this.edits_since_sync.compose(&edits);
                            this.background_task = None;
                            this.flush_edits(cx);
                            cx.notify();
                        })
                        .ok();
                    }));
                },
            }
        } else {
            let old_rows = self.snapshot.transforms.summary().output.lines.row + 1;
            self.snapshot.transforms = SumTree::default();
            let max_point = self.snapshot.tab_snapshot.max_point();
            let summary = self
                .snapshot
                .tab_snapshot
                .text_summary_for_range(TabPoint::new(0, 0)..max_point);
            if !summary.lines.is_zero() {
                self.snapshot
                    .transforms
                    .push(Transform::isomorphic(summary), ());
            }
            let new_rows = self.snapshot.transforms.summary().output.lines.row + 1;
            self.snapshot.interpolated = false;
            self.edits_since_sync = self.edits_since_sync.compose(Patch::new(vec![WrapEdit {
                old: 0..old_rows,
                new: 0..new_rows,
            }]));
        }
    }

    /// Process pending edits with async wrapping.
    ///
    /// Uses 1ms timeout for fast response, spawning continuation tasks for slow wrapping.
    fn flush_edits(&mut self, cx: &mut Context<Self>) {
        if !self.snapshot.interpolated {
            let mut to_remove_len = 0;
            for (tab_snapshot, _) in &self.pending_edits {
                if tab_snapshot.version <= self.snapshot.tab_snapshot.version {
                    to_remove_len += 1;
                } else {
                    break;
                }
            }
            self.pending_edits.drain(..to_remove_len);
        }

        if self.pending_edits.is_empty() {
            return;
        }

        if let Some(wrap_width) = self.wrap_width {
            if self.background_task.is_none() {
                let pending_edits = self.pending_edits.clone();
                let mut snapshot = self.snapshot.clone();
                let text_system = cx.text_system().clone();
                let (font, font_size) = self.font_with_size.clone();
                let update_task = cx.background_spawn(async move {
                    let mut edits = Patch::default();
                    let mut line_wrapper = text_system.line_wrapper(font, font_size);
                    for (tab_snapshot, tab_edits) in pending_edits {
                        let wrap_edits = snapshot
                            .update(tab_snapshot, &tab_edits, wrap_width, &mut line_wrapper)
                            .await;
                        edits = edits.compose(&wrap_edits);
                    }
                    (snapshot, edits)
                });

                match cx
                    .background_executor()
                    .block_with_timeout(Duration::from_millis(1), update_task)
                {
                    Ok((snapshot, output_edits)) => {
                        self.snapshot = snapshot;
                        self.edits_since_sync = self.edits_since_sync.compose(&output_edits);
                    },
                    Err(update_task) => {
                        self.background_task = Some(cx.spawn(async move |this, cx| {
                            let (snapshot, edits) = update_task.await;
                            this.update(cx, |this, cx| {
                                this.snapshot = snapshot;
                                this.edits_since_sync = this
                                    .edits_since_sync
                                    .compose(mem::take(&mut this.interpolated_edits).invert())
                                    .compose(&edits);
                                this.background_task = None;
                                this.flush_edits(cx);
                                cx.notify();
                            })
                            .ok();
                        }));
                    },
                }
            }
        }

        let was_interpolated = self.snapshot.interpolated;
        let mut to_remove_len = 0;
        for (tab_snapshot, edits) in &self.pending_edits {
            if tab_snapshot.version <= self.snapshot.tab_snapshot.version {
                to_remove_len += 1;
            } else {
                let interpolated_edits = self.snapshot.interpolate(tab_snapshot.clone(), edits);
                self.edits_since_sync = self.edits_since_sync.compose(&interpolated_edits);
                self.interpolated_edits = self.interpolated_edits.compose(&interpolated_edits);
            }
        }

        if !was_interpolated {
            self.pending_edits.drain(..to_remove_len);
        }
    }
}

/// Immutable snapshot of wrap state.
///
/// Cheap to clone (Arc-based TabSnapshot). Provides fast coordinate conversions
/// and can be interpolated for instant UI updates.
///
/// # Interpolation vs Real Wrapping
///
/// - **Interpolated**: Fast estimate assuming wraps stay unchanged (< 1ms)
/// - **Real wrapping**: Actual wrapping using LineWrapper (20-50ms for large files)
///
/// The `interpolated` flag tracks which mode was used to create this snapshot.
#[derive(Clone)]
pub struct WrapSnapshot {
    /// Tab snapshot providing input coordinates.
    pub tab_snapshot: TabSnapshot,

    /// Transform tree representing wraps and isomorphic regions.
    transforms: SumTree<Transform>,

    /// True if this snapshot was created via interpolation (fast estimate).
    /// False if created via real wrapping with LineWrapper.
    interpolated: bool,
}

impl WrapSnapshot {
    /// Create a new wrap snapshot with no wrapping.
    ///
    /// The entire file is represented as a single isomorphic transform.
    pub fn new(tab_snapshot: TabSnapshot) -> Self {
        // Start with empty transforms - wrapping added later via update()
        // This matches Zed's pattern where wrapping is done asynchronously
        Self {
            tab_snapshot,
            transforms: SumTree::new(()),
            interpolated: false,
        }
    }

    /// Get the maximum WrapPoint in this snapshot.
    pub fn max_point(&self) -> WrapPoint {
        let lines = &self.transforms.summary().output.lines;
        WrapPoint::new(lines.row, lines.column)
    }

    /// Get the longest row in display coordinates.
    pub fn longest_row(&self) -> u32 {
        self.transforms.summary().output.longest_row
    }

    /// Convert TabPoint to WrapPoint.
    ///
    /// Seeks to the tab point in the transform tree and calculates the
    /// corresponding wrap point based on accumulated transforms.
    pub fn tab_point_to_wrap_point(&self, tab_point: TabPoint) -> WrapPoint {
        if self.transforms.is_empty() {
            return WrapPoint::new(tab_point.row, tab_point.column);
        }

        let mut cursor = self
            .transforms
            .cursor::<sum_tree::Dimensions<TabPoint, WrapPoint>>(());
        cursor.seek(&tab_point, Bias::Right);

        let tab_start = cursor.start().0;
        let wrap_start = cursor.start().1;

        // Calculate row/column offset separately
        if tab_point.row > tab_start.row {
            // Different rows - use tab_point's column directly
            WrapPoint::new(
                wrap_start.row + (tab_point.row - tab_start.row),
                tab_point.column,
            )
        } else {
            // Same row - add column offset
            WrapPoint::new(
                wrap_start.row,
                wrap_start.column + (tab_point.column - tab_start.column),
            )
        }
    }

    /// Convert WrapPoint to TabPoint.
    ///
    /// Seeks to the wrap point and converts back to tab coordinates.
    /// If the wrap point is inside a wrap transform (soft wrap), clamps to
    /// the position before the wrap.
    pub fn to_tab_point(&self, wrap_point: WrapPoint) -> TabPoint {
        if self.transforms.is_empty() {
            return TabPoint::new(wrap_point.row, wrap_point.column);
        }

        let mut cursor = self
            .transforms
            .cursor::<sum_tree::Dimensions<WrapPoint, TabPoint>>(());
        cursor.seek(&wrap_point, Bias::Right);

        let wrap_start = cursor.start().0;
        let tab_start = cursor.start().1;

        if cursor.item().is_some_and(|t| t.is_isomorphic()) {
            // Isomorphic - calculate offset
            if wrap_point.row > wrap_start.row {
                TabPoint::new(
                    tab_start.row + (wrap_point.row - wrap_start.row),
                    wrap_point.column,
                )
            } else {
                TabPoint::new(
                    tab_start.row,
                    tab_start.column + (wrap_point.column - wrap_start.column),
                )
            }
        } else {
            // Wrap transform - return start position
            tab_start
        }
    }

    /// Get text summary for a range of wrap rows.
    pub fn text_summary_for_range(&self, rows: Range<u32>) -> TextSummary {
        let mut summary = TextSummary::default();

        let start = WrapPoint {
            row: rows.start,
            column: 0,
        };
        let end = WrapPoint {
            row: rows.end,
            column: 0,
        };

        let mut cursor = self
            .transforms
            .cursor::<sum_tree::Dimensions<WrapPoint, TabPoint>>(());
        cursor.seek(&start, Bias::Right);

        // Accumulate transforms in range
        while let Some(transform) = cursor.item() {
            let wrap_pos = cursor.end().0;

            if wrap_pos.row >= end.row {
                break;
            }

            summary += &transform.summary.output;
            cursor.next();
        }

        summary
    }

    /// Fast interpolation: assume wraps stay unchanged, update transforms via slicing.
    ///
    /// Returns instantly (<1ms) by copying unchanged transforms and splicing in edits.
    /// Sets `interpolated = true` flag since this is an estimate.
    fn interpolate(&mut self, new_tab_snapshot: TabSnapshot, tab_edits: &[TabEdit]) -> Patch<u32> {
        let mut new_transforms;
        if tab_edits.is_empty() {
            if self.transforms.is_empty() {
                // Initial sync: build transforms from full tab_snapshot
                let max_point = new_tab_snapshot.max_point();
                if max_point.row > 0 || max_point.column > 0 {
                    let summary =
                        new_tab_snapshot.text_summary_for_range(TabPoint::new(0, 0)..max_point);
                    new_transforms = SumTree::from_item(Transform::isomorphic(summary), ());
                } else {
                    // Empty buffer - keep transforms empty
                    new_transforms = SumTree::new(());
                }
            } else {
                new_transforms = self.transforms.clone();
            }
        } else {
            let mut old_cursor = self.transforms.cursor::<TabPoint>(());

            let mut tab_edits_iter = tab_edits.iter().peekable();
            new_transforms = old_cursor.slice(
                &tab_edits_iter
                    .peek()
                    .expect("tab_edits is not empty (checked above)")
                    .old
                    .start,
                Bias::Right,
            );

            while let Some(edit) = tab_edits_iter.next() {
                let input_lines = new_transforms.summary().input.lines;
                let current_point = TabPoint::new(input_lines.row, input_lines.column);
                if edit.new.start > current_point {
                    let summary =
                        new_tab_snapshot.text_summary_for_range(current_point..edit.new.start);
                    new_transforms.push_or_extend(Transform::isomorphic(summary));
                }

                if !edit.new.is_empty() {
                    new_transforms.push_or_extend(Transform::isomorphic(
                        new_tab_snapshot.text_summary_for_range(edit.new.clone()),
                    ));
                }

                old_cursor.seek_forward(&edit.old.end, Bias::Right);
                if let Some(next_edit) = tab_edits_iter.peek() {
                    if next_edit.old.start > old_cursor.end() {
                        if old_cursor.end() > edit.old.end {
                            let summary = self
                                .tab_snapshot
                                .text_summary_for_range(edit.old.end..old_cursor.end());
                            new_transforms.push_or_extend(Transform::isomorphic(summary));
                        }

                        old_cursor.next();
                        new_transforms
                            .append(old_cursor.slice(&next_edit.old.start, Bias::Right), ());
                    }
                } else {
                    if old_cursor.end() > edit.old.end {
                        let summary = self
                            .tab_snapshot
                            .text_summary_for_range(edit.old.end..old_cursor.end());
                        new_transforms.push_or_extend(Transform::isomorphic(summary));
                    }
                    old_cursor.next();
                    new_transforms.append(old_cursor.suffix(), ());
                }
            }
        }

        let old_snapshot = mem::replace(
            self,
            WrapSnapshot {
                tab_snapshot: new_tab_snapshot,
                transforms: new_transforms,
                interpolated: true,
            },
        );
        self.check_invariants();
        old_snapshot.compute_edits(tab_edits, self)
    }

    /// Real async wrapping using LineWrapper to compute actual wrap boundaries.
    ///
    /// Processes edited rows, builds line fragments, and calls LineWrapper to determine
    /// wrap points. Yields periodically to avoid blocking. Returns computed edits.
    async fn update(
        &mut self,
        new_tab_snapshot: TabSnapshot,
        tab_edits: &[TabEdit],
        wrap_width: Pixels,
        line_wrapper: &mut LineWrapper,
    ) -> Patch<u32> {
        #[derive(Debug)]
        struct RowEdit {
            old_rows: Range<u32>,
            new_rows: Range<u32>,
        }

        let mut tab_edits_iter = tab_edits.iter().peekable();
        let mut row_edits = Vec::with_capacity(tab_edits.len());
        while let Some(edit) = tab_edits_iter.next() {
            let mut row_edit = RowEdit {
                old_rows: edit.old.start.row..edit.old.end.row + 1,
                new_rows: edit.new.start.row..edit.new.end.row + 1,
            };

            while let Some(next_edit) = tab_edits_iter.peek() {
                if next_edit.old.start.row <= row_edit.old_rows.end {
                    row_edit.old_rows.end = next_edit.old.end.row + 1;
                    row_edit.new_rows.end = next_edit.new.end.row + 1;
                    tab_edits_iter.next();
                } else {
                    break;
                }
            }

            row_edits.push(row_edit);
        }

        let mut new_transforms;
        if row_edits.is_empty() {
            new_transforms = self.transforms.clone();
        } else {
            let mut row_edits = row_edits.into_iter().peekable();
            let mut old_cursor = self.transforms.cursor::<TabPoint>(());

            new_transforms = old_cursor.slice(
                &TabPoint::new(
                    row_edits
                        .peek()
                        .expect("row_edits is not empty (checked above)")
                        .old_rows
                        .start,
                    0,
                ),
                Bias::Right,
            );

            while let Some(edit) = row_edits.next() {
                if edit.new_rows.start > new_transforms.summary().input.lines.row {
                    let input_lines = new_transforms.summary().input.lines;
                    let summary = new_tab_snapshot.text_summary_for_range(
                        TabPoint::new(input_lines.row, input_lines.column)
                            ..TabPoint::new(edit.new_rows.start, 0),
                    );
                    new_transforms.push_or_extend(Transform::isomorphic(summary));
                }

                // Process each line in the edited range with LineWrapper
                let mut edit_transforms = Vec::<Transform>::new();
                for row in edit.new_rows.start..edit.new_rows.end {
                    let line_start = TabPoint::new(row, 0);
                    let line_end = if row + 1 < new_tab_snapshot.max_point().row {
                        TabPoint::new(row + 1, 0)
                    } else {
                        new_tab_snapshot.max_point()
                    };

                    let line_summary =
                        new_tab_snapshot.text_summary_for_range(line_start..line_end);

                    if line_summary.lines.row > 0 {
                        // Line exists - wrap it using actual line text from buffer
                        let line_text =
                            crate::buffer_utils::get_line_text(new_tab_snapshot.buffer(), row);
                        let fragments = vec![gpui::LineFragment::text(&line_text)];

                        let mut prev_boundary_ix = 0;
                        for boundary in line_wrapper.wrap_line(&fragments, wrap_width) {
                            let wrapped_len = boundary.ix - prev_boundary_ix;
                            push_isomorphic(
                                &mut edit_transforms,
                                TextSummary {
                                    lines: Point::new(0, wrapped_len as u32),
                                    len: wrapped_len,
                                    ..Default::default()
                                },
                            );
                            edit_transforms.push(Transform::wrap(boundary.next_indent));
                            prev_boundary_ix = boundary.ix;
                        }

                        // Add remaining line content
                        if prev_boundary_ix < line_summary.len {
                            let remaining_len = line_summary.len - prev_boundary_ix;
                            push_isomorphic(
                                &mut edit_transforms,
                                TextSummary {
                                    lines: Point::new(1, 0),
                                    len: remaining_len,
                                    ..Default::default()
                                },
                            );
                        } else {
                            push_isomorphic(&mut edit_transforms, line_summary);
                        }
                    }

                    yield_now().await;
                }

                let mut edit_transforms = edit_transforms.into_iter();
                if let Some(transform) = edit_transforms.next() {
                    new_transforms.push_or_extend(transform);
                }
                new_transforms.extend(edit_transforms, ());

                old_cursor.seek_forward(&TabPoint::new(edit.old_rows.end, 0), Bias::Right);
                if let Some(next_edit) = row_edits.peek() {
                    if next_edit.old_rows.start > old_cursor.end().row {
                        if old_cursor.end() > TabPoint::new(edit.old_rows.end, 0) {
                            let summary = self.tab_snapshot.text_summary_for_range(
                                TabPoint::new(edit.old_rows.end, 0)..old_cursor.end(),
                            );
                            new_transforms.push_or_extend(Transform::isomorphic(summary));
                        }
                        old_cursor.next();
                        new_transforms.append(
                            old_cursor
                                .slice(&TabPoint::new(next_edit.old_rows.start, 0), Bias::Right),
                            (),
                        );
                    }
                } else {
                    if old_cursor.end() > TabPoint::new(edit.old_rows.end, 0) {
                        let summary = self.tab_snapshot.text_summary_for_range(
                            TabPoint::new(edit.old_rows.end, 0)..old_cursor.end(),
                        );
                        new_transforms.push_or_extend(Transform::isomorphic(summary));
                    }
                    old_cursor.next();
                    new_transforms.append(old_cursor.suffix(), ());
                }
            }
        }

        let old_snapshot = mem::replace(
            self,
            WrapSnapshot {
                tab_snapshot: new_tab_snapshot,
                transforms: new_transforms,
                interpolated: false,
            },
        );
        self.check_invariants();
        old_snapshot.compute_edits(tab_edits, self)
    }

    /// Compute wrap edits by comparing old and new snapshot outputs.
    ///
    /// Simplified implementation that returns overall change.
    /// FIXME: Implement precise row-level edit tracking using cursor seeking.
    fn compute_edits(&self, _tab_edits: &[TabEdit], new_snapshot: &WrapSnapshot) -> Patch<u32> {
        let old_rows = self.transforms.summary().output.lines.row + 1;
        let new_rows = new_snapshot.transforms.summary().output.lines.row + 1;

        if old_rows != new_rows {
            Patch::new(vec![WrapEdit {
                old: 0..old_rows,
                new: 0..new_rows,
            }])
        } else {
            Patch::default()
        }
    }

    /// Debug-only invariant checking.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // Verify transform tree is consistent
        let summary = self.transforms.summary();

        // Basic invariant: output should have at least as many rows as input (due to wrapping)
        debug_assert!(
            summary.output.lines.row >= summary.input.lines.row,
            "Wrapping should not decrease row count: input={}, output={}",
            summary.input.lines.row,
            summary.output.lines.row
        );

        // Verify tab snapshot matches expected input
        let tab_max = self.tab_snapshot.max_point();
        debug_assert!(
            summary.input.lines.row <= tab_max.row + 1,
            "Input row count should match tab snapshot: input={}, tab_max={}",
            summary.input.lines.row,
            tab_max.row
        );

        // TODO: Additional checks:
        // - Verify no adjacent isomorphic transforms (should be merged)
        // - Verify wrap indents are within reasonable bounds
        // - Verify interpolated flag consistency with pending edits
    }

    #[cfg(not(debug_assertions))]
    fn check_invariants(&self) {}
}

#[cfg(test)]
mod tests_wrap_snapshot {
    use super::*;
    use crate::{fold_map::FoldSnapshot, inlay_map::InlaySnapshot};
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> text::BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    fn build_wrap_snapshot(text: &str, tab_width: u32) -> WrapSnapshot {
        let buffer = create_buffer(text);
        let inlay_snapshot = InlaySnapshot::new(buffer);
        let fold_snapshot = FoldSnapshot::new(inlay_snapshot);
        let tab_snapshot = TabSnapshot::new(fold_snapshot, tab_width);
        WrapSnapshot::new(tab_snapshot)
    }

    #[test]
    fn empty_wrap_snapshot() {
        let snapshot = build_wrap_snapshot("", 4);

        assert_eq!(snapshot.transforms.summary().input, TextSummary::default());
        assert_eq!(snapshot.transforms.summary().output, TextSummary::default());
        assert!(!snapshot.interpolated);
    }

    #[test]
    fn wrap_snapshot_no_transforms() {
        let snapshot = build_wrap_snapshot("hello world", 4);

        // Empty transforms means isomorphic (1:1 mapping)
        let tab_point = TabPoint::new(0, 5);
        let wrap_point = snapshot.tab_point_to_wrap_point(tab_point);

        assert_eq!(wrap_point, WrapPoint::new(0, 5));
    }

    #[test]
    fn wrap_snapshot_roundtrip() {
        let snapshot = build_wrap_snapshot("hello\nworld", 4);

        // Test roundtrip: TabPoint -> WrapPoint -> TabPoint
        let original = TabPoint::new(1, 3);
        let wrap_point = snapshot.tab_point_to_wrap_point(original);
        let roundtrip = snapshot.to_tab_point(wrap_point);

        assert_eq!(roundtrip, original);
    }

    #[test]
    fn max_point_empty() {
        let snapshot = build_wrap_snapshot("", 4);
        let max = snapshot.max_point();

        assert_eq!(max, WrapPoint::new(0, 0));
    }

    #[test]
    fn text_summary_for_range_empty() {
        let snapshot = build_wrap_snapshot("line 1\nline 2\nline 3", 4);
        let summary = snapshot.text_summary_for_range(0..0);

        assert_eq!(summary, TextSummary::default());
    }
}
