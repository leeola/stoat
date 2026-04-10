use super::{
    fold_map::FoldOffset,
    highlights::{Chunk, HighlightEndpoint},
    tab_map::{TabChunks, TabPoint, TabSnapshot},
};
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::VecDeque,
    future::Future,
    mem,
    ops::{Deref, Range},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// Yields control back to the executor once, allowing other tasks to run.
fn yield_now() -> impl Future<Output = ()> {
    struct YieldNow(bool);
    impl Future for YieldNow {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
    YieldNow(false)
}
use stoat_scheduler::{Executor, Task};
use stoat_text::{
    patch::{Edit, Patch},
    Bias, ContextLessSummary, Cursor, Dimension, Dimensions, Item, SeekTarget, SumTree,
};

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WrapPoint(pub TabPoint);

impl WrapPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self(TabPoint::new(row, column))
    }

    pub fn row(&self) -> u32 {
        self.0.row()
    }

    pub fn column(&self) -> u32 {
        self.0.column()
    }
}

impl From<TabPoint> for WrapPoint {
    fn from(point: TabPoint) -> Self {
        Self(point)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WrapRowKind {
    Primary,
    Continuation,
}

#[derive(Clone, Debug, Default)]
struct TransformSummary {
    input_rows: u32,
    output_rows: u32,
    longest_row: u32,
    longest_row_chars: u32,
}

impl ContextLessSummary for TransformSummary {
    fn add_summary(&mut self, other: &Self) {
        if other.longest_row_chars > self.longest_row_chars {
            self.longest_row = self.output_rows + other.longest_row;
            self.longest_row_chars = other.longest_row_chars;
        }
        self.input_rows += other.input_rows;
        self.output_rows += other.output_rows;
    }
}

#[derive(Clone, Debug)]
struct Transform {
    summary: TransformSummary,
    wrap_columns: Vec<u32>,
    tab_line_len: u32,
}

impl Item for Transform {
    type Summary = TransformSummary;

    fn summary(&self, _cx: ()) -> TransformSummary {
        self.summary.clone()
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct InputRow(u32);

impl<'a> Dimension<'a, TransformSummary> for InputRow {
    fn zero(_cx: ()) -> Self {
        InputRow(0)
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _cx: ()) {
        self.0 += summary.input_rows;
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct OutputRow(u32);

impl<'a> Dimension<'a, TransformSummary> for OutputRow {
    fn zero(_cx: ()) -> Self {
        OutputRow(0)
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _cx: ()) {
        self.0 += summary.output_rows;
    }
}

impl<'a> SeekTarget<'a, TransformSummary, Dimensions<InputRow, OutputRow>> for OutputRow {
    fn cmp(&self, cursor_location: &Dimensions<InputRow, OutputRow>, _cx: ()) -> Ordering {
        Ord::cmp(&self.0, &cursor_location.1 .0)
    }
}

#[derive(Clone, Default)]
struct LongestInRange {
    output_rows: u32,
    longest_row: u32,
    longest_row_chars: u32,
}

impl<'a> Dimension<'a, TransformSummary> for LongestInRange {
    fn zero(_cx: ()) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _cx: ()) {
        if summary.longest_row_chars > self.longest_row_chars {
            self.longest_row = self.output_rows + summary.longest_row;
            self.longest_row_chars = summary.longest_row_chars;
        }
        self.output_rows += summary.output_rows;
    }
}

const WRAP_SYNC_THRESHOLD: u32 = 100;

pub struct WrapMap {
    snapshot: WrapSnapshot,
    pending_edits: VecDeque<(TabSnapshot, Patch<u32>)>,
    interpolated_edits: Patch<u32>,
    edits_since_sync: Patch<u32>,
    wrap_width: Option<u32>,
    background_task: Option<Task<(WrapSnapshot, Patch<u32>)>>,
    executor: Executor,
}

#[derive(Clone)]
pub struct WrapSnapshot {
    tab_snapshot: TabSnapshot,
    transforms: SumTree<Transform>,
    wrap_width: Option<u32>,
    total_rows: u32,
    longest_row: u32,
    longest_row_chars: u32,
    pub interpolated: bool,
}

impl Deref for WrapSnapshot {
    type Target = TabSnapshot;
    fn deref(&self) -> &TabSnapshot {
        &self.tab_snapshot
    }
}

impl WrapMap {
    pub fn new(
        tab_snapshot: TabSnapshot,
        wrap_width: Option<u32>,
        executor: Executor,
    ) -> (Self, Arc<WrapSnapshot>) {
        let snapshot = build_snapshot(tab_snapshot, wrap_width);
        let snapshot_arc = Arc::new(snapshot.clone());
        let map = WrapMap {
            snapshot,
            pending_edits: VecDeque::new(),
            interpolated_edits: Patch::empty(),
            edits_since_sync: Patch::empty(),
            wrap_width,
            background_task: None,
            executor,
        };
        (map, snapshot_arc)
    }

    pub fn sync(
        &mut self,
        tab_snapshot: TabSnapshot,
        tab_edits: &Patch<u32>,
    ) -> (Arc<WrapSnapshot>, Patch<u32>) {
        let wrap_width_changed = self.wrap_width != self.snapshot.wrap_width;
        let new_fold_ver = tab_snapshot.fold_snapshot().version();
        let new_buf_ver = tab_snapshot.fold_snapshot().inlay_snapshot().version();
        let new_inlay_ver = tab_snapshot.fold_snapshot().inlay_snapshot().inlay_version;
        let old_fold_ver = self.snapshot.tab_snapshot.fold_snapshot().version();
        let old_buf_ver = self
            .snapshot
            .tab_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .version();
        let old_inlay_ver = self
            .snapshot
            .tab_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .inlay_version;
        let version_changed = new_fold_ver != old_fold_ver
            || new_buf_ver != old_buf_ver
            || new_inlay_ver != old_inlay_ver;

        let needs_full_rebuild = wrap_width_changed || (version_changed && tab_edits.is_empty());

        if needs_full_rebuild {
            let old_line_count = self.snapshot.line_count();
            self.snapshot = build_snapshot(tab_snapshot, self.wrap_width);
            let new_line_count = self.snapshot.line_count();
            self.edits_since_sync = self.edits_since_sync.compose([Edit {
                old: 0..old_line_count,
                new: 0..new_line_count,
            }]);
        } else if !tab_edits.is_empty() && self.wrap_width.is_some() {
            self.pending_edits
                .push_back((tab_snapshot, tab_edits.clone()));
            self.flush_edits();
        } else if !tab_edits.is_empty() {
            // No wrapping: apply identity
            let interpolated = self.snapshot.interpolate(tab_snapshot, tab_edits);
            self.edits_since_sync = self
                .edits_since_sync
                .compose(interpolated.edits().iter().cloned());
            self.snapshot.interpolated = false;
        }

        (
            Arc::new(self.snapshot.clone()),
            mem::take(&mut self.edits_since_sync),
        )
    }

    fn flush_edits(&mut self) {
        self.poll_background_task();

        if !self.snapshot.interpolated {
            let snap_version = self.snapshot.tab_snapshot.fold_snapshot().version();
            let mut to_remove = 0;
            for (tab_snapshot, _) in &self.pending_edits {
                if tab_snapshot.fold_snapshot().version() <= snap_version {
                    to_remove += 1;
                } else {
                    break;
                }
            }
            self.pending_edits.drain(..to_remove);
        }

        if self.pending_edits.is_empty() {
            return;
        }

        if let Some(wrap_width) = self.wrap_width {
            if self.background_task.is_none() {
                let is_small = self.pending_edits.len() == 1
                    && self
                        .pending_edits
                        .back()
                        .map(|(_, edits)| {
                            edits.edits().iter().all(|e| {
                                e.new.end.saturating_sub(e.new.start) < WRAP_SYNC_THRESHOLD
                            })
                        })
                        .unwrap_or(false);

                if is_small {
                    let (tab_snapshot, tab_edits) = self
                        .pending_edits
                        .pop_back()
                        .expect("pending_edits.len() == 1");
                    let wrap_edits = sync_incremental(
                        &self.snapshot,
                        tab_snapshot,
                        &tab_edits,
                        Some(wrap_width),
                    );
                    self.snapshot = wrap_edits.0;
                    self.edits_since_sync = self
                        .edits_since_sync
                        .compose(wrap_edits.1.edits().iter().cloned());
                    return;
                }

                let mut snapshot = self.snapshot.clone();
                let pending = self.pending_edits.clone();
                self.background_task = Some(self.executor.spawn(async move {
                    let mut edits = Patch::empty();
                    for (tab_snapshot, tab_edits) in pending {
                        let (new_snap, wrap_edits) =
                            sync_incremental(&snapshot, tab_snapshot, &tab_edits, Some(wrap_width));
                        snapshot = new_snap;
                        edits = edits.compose(wrap_edits.edits().iter().cloned());
                        yield_now().await;
                    }
                    (snapshot, edits)
                }));
            }

            // Apply interpolated edits for any remaining pending
            let was_interpolated = self.snapshot.interpolated;
            let snap_version = self.snapshot.tab_snapshot.fold_snapshot().version();
            let mut to_remove = 0;
            for (tab_snapshot, edits) in &self.pending_edits {
                if tab_snapshot.fold_snapshot().version() <= snap_version {
                    to_remove += 1;
                } else {
                    let interpolated = self.snapshot.interpolate(tab_snapshot.clone(), edits);
                    self.edits_since_sync = self
                        .edits_since_sync
                        .compose(interpolated.edits().iter().cloned());
                    self.interpolated_edits = self
                        .interpolated_edits
                        .compose(interpolated.edits().iter().cloned());
                }
            }
            if !was_interpolated {
                self.pending_edits.drain(..to_remove);
            }
        }
    }

    fn poll_background_task(&mut self) {
        if let Some(ref mut task) = self.background_task {
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            if let Poll::Ready((snapshot, edits)) = Pin::new(task).poll(&mut cx) {
                let mut inverted = mem::take(&mut self.interpolated_edits);
                inverted.invert();
                self.edits_since_sync = self
                    .edits_since_sync
                    .compose(inverted.edits().iter().cloned())
                    .compose(edits.edits().iter().cloned());
                self.snapshot = snapshot;
                self.background_task = None;
                self.pending_edits.clear();
                self.flush_edits();
            }
        }
    }

    pub fn set_wrap_width(&mut self, width: Option<u32>) {
        self.wrap_width = width;
    }

    pub fn wrap_width(&self) -> Option<u32> {
        self.wrap_width
    }
}

fn build_snapshot(tab_snapshot: TabSnapshot, wrap_width: Option<u32>) -> WrapSnapshot {
    let tab_line_count = tab_snapshot.line_count();
    let mut transforms = SumTree::new(());

    for tab_row in 0..tab_line_count {
        let tab_line_len = tab_snapshot.line_len(tab_row);

        let wrap_columns = match wrap_width {
            None => vec![0],
            Some(width) => {
                let chars = tab_snapshot.fold_snapshot().fold_line_chars(tab_row);
                compute_wrap_columns(
                    chars,
                    tab_line_len,
                    width,
                    tab_snapshot.tab_size(),
                    tab_snapshot.max_expansion_column(),
                )
            },
        };

        let output_rows = wrap_columns.len() as u32;
        let (local_longest_row, local_longest_chars) =
            compute_transform_longest(&wrap_columns, tab_line_len);

        transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows: 1,
                    output_rows,
                    longest_row: local_longest_row,
                    longest_row_chars: local_longest_chars,
                },
                wrap_columns,
                tab_line_len,
            },
            (),
        );
    }

    let s = transforms.summary();
    let total_rows = s.output_rows;
    let longest_row = s.longest_row;
    let longest_row_chars = s.longest_row_chars;

    WrapSnapshot {
        tab_snapshot,
        transforms,
        wrap_width,
        total_rows,
        longest_row,
        longest_row_chars,
        interpolated: false,
    }
}

fn sync_incremental(
    old: &WrapSnapshot,
    tab_snapshot: TabSnapshot,
    tab_edits: &Patch<u32>,
    wrap_width: Option<u32>,
) -> (WrapSnapshot, Patch<u32>) {
    let mut new_transforms = SumTree::new(());
    let mut cursor = old.transforms.cursor::<Dimensions<InputRow, OutputRow>>(());
    let mut wrap_edits = Patch::empty();

    for edit in tab_edits {
        new_transforms.append(cursor.slice(&InputRow(edit.old.start), Bias::Left), ());
        let old_output_start = cursor.start().1 .0;

        cursor.seek_forward(&InputRow(edit.old.end), Bias::Right);
        let old_output_end = cursor.start().1 .0;

        let new_output_start: u32 = new_transforms.summary().output_rows;

        for tab_row in edit.new.start..edit.new.end {
            let tab_line_len = tab_snapshot.line_len(tab_row);
            let wrap_columns = match wrap_width {
                None => vec![0],
                Some(width) => {
                    let chars = tab_snapshot.fold_snapshot().fold_line_chars(tab_row);
                    compute_wrap_columns(
                        chars,
                        tab_line_len,
                        width,
                        tab_snapshot.tab_size(),
                        tab_snapshot.max_expansion_column(),
                    )
                },
            };
            let output_rows = wrap_columns.len() as u32;
            let (local_longest_row, local_longest_chars) =
                compute_transform_longest(&wrap_columns, tab_line_len);
            new_transforms.push(
                Transform {
                    summary: TransformSummary {
                        input_rows: 1,
                        output_rows,
                        longest_row: local_longest_row,
                        longest_row_chars: local_longest_chars,
                    },
                    wrap_columns,
                    tab_line_len,
                },
                (),
            );
        }

        let new_output_end: u32 = new_transforms.summary().output_rows;

        wrap_edits.push(Edit {
            old: old_output_start..old_output_end,
            new: new_output_start..new_output_end,
        });
    }

    new_transforms.append(cursor.suffix(), ());

    let s = new_transforms.summary();
    let total_rows = s.output_rows;
    let longest_row = s.longest_row;
    let longest_row_chars = s.longest_row_chars;

    let snapshot = WrapSnapshot {
        tab_snapshot,
        transforms: new_transforms,
        wrap_width,
        total_rows,
        longest_row,
        longest_row_chars,
        interpolated: false,
    };

    (snapshot, wrap_edits)
}

fn compute_transform_longest(wrap_columns: &[u32], tab_line_len: u32) -> (u32, u32) {
    let mut best_row = 0u32;
    let mut best_chars = 0u32;
    for sub_idx in 0..wrap_columns.len() {
        let sub_len = if sub_idx + 1 < wrap_columns.len() {
            wrap_columns[sub_idx + 1] - wrap_columns[sub_idx]
        } else {
            tab_line_len - wrap_columns[sub_idx]
        };
        if sub_len > best_chars {
            best_row = sub_idx as u32;
            best_chars = sub_len;
        }
    }
    (best_row, best_chars)
}

fn compute_wrap_columns(
    chars: impl Iterator<Item = char>,
    tab_line_len: u32,
    width: u32,
    tab_size: u32,
    max_expansion_column: u32,
) -> Vec<u32> {
    if width == 0 || tab_line_len <= width {
        return vec![0];
    }

    let mut breaks = vec![0u32];
    let mut expanded_col = 0u32;
    let mut last_break_candidate: Option<u32> = None;

    for ch in chars {
        let char_width = if ch == '\t' {
            if expanded_col >= max_expansion_column {
                1
            } else {
                tab_size - (expanded_col % tab_size)
            }
        } else {
            super::display_width(ch)
        };

        if ch == ' ' || ch == '\t' || ch == '-' {
            last_break_candidate = Some(expanded_col + char_width);
        } else if char_width >= 2 {
            // CJK and other wide characters can break at any boundary.
            last_break_candidate = Some(expanded_col);
        }

        expanded_col += char_width;

        let segment_start = *breaks.last().expect("breaks starts with [0]");
        if expanded_col - segment_start >= width {
            let break_at = match last_break_candidate {
                Some(b) if b > segment_start => b,
                _ => expanded_col,
            };
            breaks.push(break_at);
            last_break_candidate = None;
        }
    }

    if breaks.len() > 1 && *breaks.last().expect("breaks starts with [0]") >= tab_line_len {
        breaks.pop();
    }

    breaks
}

impl WrapSnapshot {
    /// Cheap approximation: replaces edited regions with 1:1 identity transforms
    /// (no wrapping). Fast but inaccurate -- sets `interpolated = true`.
    fn interpolate(&mut self, new_tab_snapshot: TabSnapshot, tab_edits: &Patch<u32>) -> Patch<u32> {
        let mut new_transforms = SumTree::new(());
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        let mut wrap_edits = Patch::empty();

        for edit in tab_edits {
            new_transforms.append(cursor.slice(&InputRow(edit.old.start), Bias::Left), ());
            let old_output_start = cursor.start().1 .0;

            cursor.seek_forward(&InputRow(edit.old.end), Bias::Right);
            let old_output_end = cursor.start().1 .0;

            let new_output_start: u32 = new_transforms.summary().output_rows;

            for tab_row in edit.new.start..edit.new.end {
                let tab_line_len = new_tab_snapshot.line_len(tab_row);
                new_transforms.push(
                    Transform {
                        summary: TransformSummary {
                            input_rows: 1,
                            output_rows: 1,
                            longest_row: 0,
                            longest_row_chars: tab_line_len,
                        },
                        wrap_columns: vec![0],
                        tab_line_len,
                    },
                    (),
                );
            }

            let new_output_end: u32 = new_transforms.summary().output_rows;
            wrap_edits.push(Edit {
                old: old_output_start..old_output_end,
                new: new_output_start..new_output_end,
            });
        }

        new_transforms.append(cursor.suffix(), ());
        drop(cursor);

        let s = new_transforms.summary().clone();
        self.tab_snapshot = new_tab_snapshot;
        self.transforms = new_transforms;
        self.total_rows = s.output_rows;
        self.longest_row = s.longest_row;
        self.longest_row_chars = s.longest_row_chars;
        self.interpolated = true;

        wrap_edits
    }

    pub fn tab_snapshot(&self) -> &TabSnapshot {
        &self.tab_snapshot
    }

    pub fn wrap_width(&self) -> Option<u32> {
        self.wrap_width
    }

    pub fn to_tab_point(&self, wrap_point: WrapPoint) -> TabPoint {
        if self.wrap_width.is_none() {
            return TabPoint::new(wrap_point.row(), wrap_point.column());
        }

        let target = OutputRow(wrap_point.row() + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let sub_row = wrap_point.row() - output_start.0;

        if let Some(transform) = cursor.item() {
            let tab_col = transform.wrap_columns[sub_row as usize] + wrap_point.column();
            TabPoint::new(input_start.0, tab_col)
        } else {
            let last_tab_row = input_start.0.saturating_sub(1);
            TabPoint::new(last_tab_row, wrap_point.column())
        }
    }

    pub fn to_wrap_point(&self, tab_point: TabPoint) -> WrapPoint {
        if self.wrap_width.is_none() {
            return WrapPoint::new(tab_point.row(), tab_point.column());
        }

        let target = InputRow(tab_point.row() + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(_input_start, output_start, _) = cursor.start();

        if let Some(transform) = cursor.item() {
            let tab_col = tab_point.column();
            let sub_row = transform
                .wrap_columns
                .partition_point(|&c| c <= tab_col)
                .saturating_sub(1);
            let wrap_col = tab_col - transform.wrap_columns[sub_row];
            WrapPoint::new(output_start.0 + sub_row as u32, wrap_col)
        } else {
            WrapPoint::new(output_start.0, tab_point.column())
        }
    }

    pub fn classify_row(&self, wrap_row: u32) -> WrapRowKind {
        if self.wrap_width.is_none() {
            return WrapRowKind::Primary;
        }

        let target = OutputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let sub_row = wrap_row - cursor.start().1 .0;
        if sub_row == 0 {
            WrapRowKind::Primary
        } else {
            WrapRowKind::Continuation
        }
    }

    pub fn clip_point(&self, point: WrapPoint, _bias: Bias) -> WrapPoint {
        let max_row = self.total_rows.saturating_sub(1);
        let row = point.row().min(max_row);
        let max_col = self.line_len(row);
        let col = point.column().min(max_col);
        WrapPoint::new(row, col)
    }

    pub fn line_len(&self, wrap_row: u32) -> u32 {
        if self.wrap_width.is_none() {
            return self.tab_snapshot.line_len(wrap_row);
        }

        let target = OutputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(_input_start, output_start, _) = cursor.start();
        let sub_row = wrap_row - output_start.0;

        if let Some(transform) = cursor.item() {
            let next_idx = sub_row as usize + 1;
            if next_idx < transform.wrap_columns.len() {
                transform.wrap_columns[next_idx] - transform.wrap_columns[sub_row as usize]
            } else {
                transform.tab_line_len - transform.wrap_columns[sub_row as usize]
            }
        } else {
            0
        }
    }

    pub fn soft_wrap_indent(&self, wrap_row: u32) -> u32 {
        if self.wrap_width.is_none() {
            return 0;
        }

        let target = OutputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let sub_row = wrap_row - cursor.start().1 .0;
        if sub_row == 0 {
            return 0;
        }

        let tab_row = cursor.start().0 .0;
        self.tab_snapshot
            .fold_snapshot()
            .fold_line_chars(tab_row)
            .take_while(|c| c.is_whitespace())
            .count() as u32
    }

    pub fn write_display_line(&self, buf: &mut String, wrap_row: u32) {
        if self.wrap_width.is_none() {
            self.tab_snapshot.write_expand_line(buf, wrap_row);
            return;
        }

        let target = OutputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let sub_row = (wrap_row - output_start.0) as usize;
        let tab_row = input_start.0;

        if let Some(transform) = cursor.item() {
            let start_col = transform.wrap_columns[sub_row];
            let end_col = if sub_row + 1 < transform.wrap_columns.len() {
                Some(transform.wrap_columns[sub_row + 1])
            } else {
                None
            };
            self.tab_snapshot
                .write_expand_line_range(buf, tab_row, start_col, end_col);
        } else {
            self.tab_snapshot.write_expand_line(buf, tab_row);
        }
    }

    pub fn display_line(&self, wrap_row: u32) -> String {
        let mut result = String::new();
        self.write_display_line(&mut result, wrap_row);
        result
    }

    /// Stream [`Chunk`]s covering the given wrap-row range.
    ///
    /// Fast path: when `wrap_width` is `None`, delegates directly to
    /// [`TabSnapshot::chunks`] over the matching fold-offset range.
    ///
    /// Wrapped path: re-streams each tab row's chunks via
    /// [`TabSnapshot::chunks`], slicing the per-chunk text to the wrap row's
    /// display-column window so highlights and tab expansion are preserved
    /// across the wrap boundary.
    pub fn chunks<'a>(
        &'a self,
        rows: Range<u32>,
        endpoints: Arc<[HighlightEndpoint]>,
    ) -> WrapChunks<'a> {
        if self.wrap_width.is_none() {
            // wrap row = tab row = fold row in this mode. Compute the matching
            // fold-offset range. The range spans from the start of the first
            // row to the end of the last row's content (excluding the trailing
            // newline). Intermediate newlines between rows are included
            // naturally from the rope.
            let fold = self.tab_snapshot.fold_snapshot();
            let (start, end) = if rows.start >= rows.end {
                (FoldOffset(0), FoldOffset(0))
            } else {
                let start = fold.row_start_offset(rows.start);
                let last_row = rows.end - 1;
                let line_count = fold.line_count();
                let end = if last_row >= line_count {
                    fold.len()
                } else {
                    let last_start = fold.row_start_offset(last_row);
                    FoldOffset(last_start.0 + fold.line_len(last_row) as usize)
                };
                (start, end)
            };
            return WrapChunks::Passthrough {
                tab_chunks: Box::new(self.tab_snapshot.chunks(start..end, 0, endpoints)),
            };
        }

        WrapChunks::Wrapped(Box::new(WrappedChunksInner {
            snapshot: self,
            endpoints,
            rows: rows.clone(),
            current_row: rows.start,
            row_state: None,
            pending_newline: false,
        }))
    }

    pub fn longest_line(&self) -> (u32, u32) {
        (self.longest_row, self.longest_row_chars)
    }

    pub fn longest_in_output_range(&self, start: u32, count: u32) -> (u32, u32) {
        if count == 0 {
            return (0, 0);
        }
        let end = start + count;

        if self.wrap_width.is_none() {
            let mut cursor = self
                .transforms
                .cursor::<Dimensions<InputRow, OutputRow>>(());
            cursor.seek(&OutputRow(start + 1), Bias::Left);
            let result: LongestInRange = cursor.summary(&OutputRow(end), Bias::Right);
            return (result.longest_row, result.longest_row_chars);
        }

        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&OutputRow(start + 1), Bias::Left);

        let mut best_row = 0u32;
        let mut best_chars = 0u32;

        let output_start = cursor.start().1 .0;
        let Some(transform) = cursor.item() else {
            return (0, 0);
        };

        let transform_end = output_start + transform.summary.output_rows;
        let sub_start = (start - output_start) as usize;
        let sub_end = (end.min(transform_end) - output_start) as usize;

        for sub_idx in sub_start..sub_end {
            let len = transform_sub_row_len(transform, sub_idx);
            if len > best_chars {
                best_row = (output_start + sub_idx as u32) - start;
                best_chars = len;
            }
        }

        if transform_end >= end {
            return (best_row, best_chars);
        }

        cursor.next();
        let middle_start = cursor.start().1 .0;

        let middle: LongestInRange = cursor.summary(&OutputRow(end), Bias::Right);
        if middle.longest_row_chars > best_chars {
            best_row = (middle_start - start) + middle.longest_row;
            best_chars = middle.longest_row_chars;
        }

        if let Some(transform) = cursor.item() {
            let t_start = cursor.start().1 .0;
            if t_start < end {
                let sub_end = (end - t_start) as usize;
                for sub_idx in 0..sub_end {
                    let len = transform_sub_row_len(transform, sub_idx);
                    if len > best_chars {
                        best_row = (t_start + sub_idx as u32) - start;
                        best_chars = len;
                    }
                }
            }
        }

        (best_row, best_chars)
    }

    pub fn line_count(&self) -> u32 {
        self.total_rows
    }

    pub fn wrap_point_cursor(&self) -> WrapPointCursor<'_> {
        WrapPointCursor {
            cursor: self
                .transforms
                .cursor::<Dimensions<InputRow, OutputRow>>(()),
            wrap_width: self.wrap_width,
        }
    }
}

/// Iterator returned by [`WrapSnapshot::chunks`].
pub enum WrapChunks<'a> {
    /// No wrapping: a thin wrapper around [`TabChunks`].
    Passthrough { tab_chunks: Box<TabChunks<'a>> },
    /// Wrapping active: emits row-by-row using the expanded display line
    /// text. Unstyled; see the FIXME on [`WrapSnapshot::chunks`].
    Wrapped(Box<WrappedChunksInner<'a>>),
}

#[doc(hidden)]
pub struct WrappedChunksInner<'a> {
    snapshot: &'a WrapSnapshot,
    endpoints: Arc<[HighlightEndpoint]>,
    rows: Range<u32>,
    current_row: u32,
    row_state: Option<RowChunksState<'a>>,
    pending_newline: bool,
}

/// Per-wrap-row streaming state. Holds an open [`TabChunks`] over the full
/// underlying tab row plus the running display column. Chunks pulled from
/// `tab_chunks` are sliced to the `[target_start, target_end)` display
/// column window before being yielded.
struct RowChunksState<'a> {
    tab_chunks: TabChunks<'a>,
    column: u32,
    target_start: u32,
    target_end: Option<u32>,
    done: bool,
}

impl<'a> Iterator for WrapChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        match self {
            WrapChunks::Passthrough { tab_chunks } => tab_chunks.next(),
            WrapChunks::Wrapped(inner) => inner.next(),
        }
    }
}

impl<'a> WrappedChunksInner<'a> {
    fn next(&mut self) -> Option<Chunk<'a>> {
        loop {
            if self.pending_newline {
                self.pending_newline = false;
                return Some(Chunk {
                    text: Cow::Borrowed("\n"),
                    ..Default::default()
                });
            }

            if self.row_state.is_none() {
                if self.current_row >= self.rows.end {
                    return None;
                }
                self.row_state = self.start_row(self.current_row);
                if self.row_state.is_none() {
                    self.advance_row();
                    continue;
                }
            }

            let state = self.row_state.as_mut().expect("checked Some above");
            if state.done {
                self.advance_row();
                continue;
            }

            let Some(chunk) = state.tab_chunks.next() else {
                self.advance_row();
                continue;
            };

            if let Some(sliced) = slice_chunk_to_window(chunk, state) {
                return Some(sliced);
            }
            // chunk fell entirely below or above the window; pull next.
        }
    }

    fn advance_row(&mut self) {
        self.row_state = None;
        self.current_row += 1;
        if self.current_row < self.rows.end {
            self.pending_newline = true;
        }
    }

    fn start_row(&self, wrap_row: u32) -> Option<RowChunksState<'a>> {
        let target = OutputRow(wrap_row + 1);
        let mut cursor = self
            .snapshot
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let sub_row = (wrap_row - output_start.0) as usize;
        let tab_row = input_start.0;

        let transform = cursor.item()?;
        let (target_start, target_end) = if sub_row < transform.wrap_columns.len() {
            let start = transform.wrap_columns[sub_row];
            let end = if sub_row + 1 < transform.wrap_columns.len() {
                Some(transform.wrap_columns[sub_row + 1])
            } else {
                None
            };
            (start, end)
        } else {
            (0, None)
        };

        let fold = self.snapshot.tab_snapshot.fold_snapshot();
        if tab_row >= fold.line_count() {
            return None;
        }
        let row_start = fold.row_start_offset(tab_row);
        let row_end = FoldOffset(row_start.0 + fold.line_len(tab_row) as usize);

        let tab_chunks =
            self.snapshot
                .tab_snapshot
                .chunks(row_start..row_end, 0, self.endpoints.clone());

        Some(RowChunksState {
            tab_chunks,
            column: 0,
            target_start,
            target_end,
            done: false,
        })
    }
}

/// Slice `chunk` to the display-column window described by `state`. Returns
/// `None` if the chunk falls entirely outside the window. Updates
/// `state.column` to reflect the column past the consumed input and sets
/// `state.done` once the window has been exceeded.
fn slice_chunk_to_window<'a>(
    chunk: Chunk<'a>,
    state: &mut RowChunksState<'a>,
) -> Option<Chunk<'a>> {
    let text: &str = chunk.text.as_ref();
    let chunk_start_col = state.column;
    let mut col = chunk_start_col;
    let mut byte_start: Option<usize> = None;
    let mut byte_end = text.len();

    for (byte_idx, ch) in text.char_indices() {
        // Tab chunks are pre-expanded to ASCII spaces, so each char is one
        // display column wide. Non-tab chunks use the global width table.
        let char_width = if chunk.is_tab {
            1
        } else {
            super::display_width(ch)
        };
        let next_col = col + char_width;

        if next_col <= state.target_start {
            col = next_col;
            continue;
        }
        if let Some(end) = state.target_end {
            if col >= end {
                byte_end = byte_idx;
                state.done = true;
                break;
            }
        }
        if byte_start.is_none() {
            byte_start = Some(byte_idx);
        }
        col = next_col;
    }
    state.column = col;

    let bs = byte_start?;
    let mut sliced = chunk;
    sliced.text = match sliced.text {
        Cow::Borrowed(s) => Cow::Borrowed(&s[bs..byte_end]),
        Cow::Owned(s) => Cow::Owned(s[bs..byte_end].to_string()),
    };
    Some(sliced)
}

pub struct WrapPointCursor<'a> {
    cursor: Cursor<'a, 'static, Transform, Dimensions<InputRow, OutputRow>>,
    wrap_width: Option<u32>,
}

impl WrapPointCursor<'_> {
    pub fn map(&mut self, tab_point: TabPoint) -> WrapPoint {
        if self.wrap_width.is_none() {
            return WrapPoint::new(tab_point.row(), tab_point.column());
        }

        let target = InputRow(tab_point.row() + 1);
        if self.cursor.did_seek() {
            self.cursor.seek_forward(&target, Bias::Left);
        } else {
            self.cursor.seek(&target, Bias::Left);
        }

        let Dimensions(_input_start, output_start, _) = self.cursor.start();
        if let Some(transform) = self.cursor.item() {
            let tab_col = tab_point.column();
            let sub_row = transform
                .wrap_columns
                .partition_point(|&c| c <= tab_col)
                .saturating_sub(1);
            let wrap_col = tab_col - transform.wrap_columns[sub_row];
            WrapPoint::new(output_start.0 + sub_row as u32, wrap_col)
        } else {
            WrapPoint::new(output_start.0, tab_point.column())
        }
    }
}

fn transform_sub_row_len(transform: &Transform, sub_idx: usize) -> u32 {
    if sub_idx + 1 < transform.wrap_columns.len() {
        transform.wrap_columns[sub_idx + 1] - transform.wrap_columns[sub_idx]
    } else {
        transform.tab_line_len - transform.wrap_columns[sub_idx]
    }
}

#[cfg(test)]
mod tests {
    use super::{WrapMap, WrapPoint, WrapRowKind};
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::{
            fold_map::FoldMap,
            inlay_map::InlayMap,
            tab_map::{TabMap, TabPoint},
        },
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::patch::Patch;

    fn test_executor() -> Executor {
        Executor::new(Arc::new(TestScheduler::new()))
    }

    fn make_snapshot(content: &str, wrap_width: Option<u32>) -> Arc<super::WrapSnapshot> {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (_, wrap_snapshot) = WrapMap::new(tab_snapshot, wrap_width, test_executor());
        wrap_snapshot
    }

    #[test]
    fn no_wrap_passthrough() {
        let snap = make_snapshot("hello\nworld", None);
        assert_eq!(snap.line_count(), 2);
        let tp = TabPoint::new(1, 3);
        let wp = snap.to_wrap_point(tp);
        assert_eq!(wp, WrapPoint::new(1, 3));
        let back = snap.to_tab_point(wp);
        assert_eq!(back, tp);
    }

    #[test]
    fn short_lines_no_wrap() {
        let snap = make_snapshot("ab\ncd\nef", Some(10));
        assert_eq!(snap.line_count(), 3);
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(2, 1)),
            WrapPoint::new(2, 1)
        );
    }

    #[test]
    fn single_long_line_wraps() {
        let snap = make_snapshot("abcdefghij", Some(5));
        assert_eq!(snap.line_count(), 2);

        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 0)),
            WrapPoint::new(0, 0)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 3)),
            WrapPoint::new(0, 3)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 5)),
            WrapPoint::new(1, 0)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 8)),
            WrapPoint::new(1, 3)
        );

        assert_eq!(snap.to_tab_point(WrapPoint::new(0, 3)), TabPoint::new(0, 3));
        assert_eq!(snap.to_tab_point(WrapPoint::new(1, 0)), TabPoint::new(0, 5));
        assert_eq!(snap.to_tab_point(WrapPoint::new(1, 3)), TabPoint::new(0, 8));
    }

    #[test]
    fn multiple_wraps_one_line() {
        let snap = make_snapshot("abcdefghijklmno", Some(5));
        assert_eq!(snap.line_count(), 3);
    }

    #[test]
    fn mixed_lines() {
        let snap = make_snapshot("ab\nabcdefghij\ncd", Some(5));
        assert_eq!(snap.line_count(), 4);

        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 1)),
            WrapPoint::new(0, 1)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(1, 7)),
            WrapPoint::new(2, 2)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(2, 1)),
            WrapPoint::new(3, 1)
        );
    }

    #[test]
    fn classify_primary_and_continuation() {
        let snap = make_snapshot("abcdefghij", Some(5));
        assert_eq!(snap.classify_row(0), WrapRowKind::Primary);
        assert_eq!(snap.classify_row(1), WrapRowKind::Continuation);
    }

    #[test]
    fn line_len_no_wrap() {
        let snap = make_snapshot("hello\nhi", None);
        assert_eq!(snap.line_len(0), 5);
        assert_eq!(snap.line_len(1), 2);
    }

    #[test]
    fn line_len_wrapped() {
        let snap = make_snapshot("abcdefghij", Some(5));
        assert_eq!(snap.line_len(0), 5);
        assert_eq!(snap.line_len(1), 5);
    }

    #[test]
    fn line_len_wrapped_remainder() {
        let snap = make_snapshot("abcdefgh", Some(5));
        assert_eq!(snap.line_len(0), 5);
        assert_eq!(snap.line_len(1), 3);
    }

    #[test]
    fn word_boundary_wrap() {
        let snap = make_snapshot("hello world foo", Some(8));
        assert_eq!(snap.line_count(), 3);
        assert_eq!(snap.line_len(0), 6);
        assert_eq!(snap.line_len(1), 6);
        assert_eq!(snap.line_len(2), 3);
    }

    #[test]
    fn word_boundary_roundtrip() {
        let snap = make_snapshot("hello world foo", Some(8));

        let wp = snap.to_wrap_point(TabPoint::new(0, 0));
        assert_eq!(wp, WrapPoint::new(0, 0));
        assert_eq!(snap.to_tab_point(wp), TabPoint::new(0, 0));

        let wp = snap.to_wrap_point(TabPoint::new(0, 5));
        assert_eq!(wp, WrapPoint::new(0, 5));

        let wp = snap.to_wrap_point(TabPoint::new(0, 6));
        assert_eq!(wp, WrapPoint::new(1, 0));
        assert_eq!(snap.to_tab_point(wp), TabPoint::new(0, 6));

        let wp = snap.to_wrap_point(TabPoint::new(0, 12));
        assert_eq!(wp, WrapPoint::new(2, 0));
        assert_eq!(snap.to_tab_point(wp), TabPoint::new(0, 12));
    }

    #[test]
    fn long_word_hard_wraps() {
        let snap = make_snapshot("abcdefghijklmno", Some(8));
        assert_eq!(snap.line_count(), 2);
        assert_eq!(snap.line_len(0), 8);
        assert_eq!(snap.line_len(1), 7);
    }

    #[test]
    fn soft_wrap_indent_primary() {
        let snap = make_snapshot("    hello world foo", Some(8));
        assert_eq!(snap.soft_wrap_indent(0), 0);
    }

    #[test]
    fn soft_wrap_indent_continuation() {
        let snap = make_snapshot("    hello world foo", Some(8));
        assert!(snap.line_count() > 1);
        assert_eq!(snap.soft_wrap_indent(1), 4);
    }

    fn make_wrap_map(
        content: &str,
        wrap_width: Option<u32>,
    ) -> (WrapMap, Arc<super::WrapSnapshot>, MultiBuffer) {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (wrap_map, wrap_snapshot) = WrapMap::new(tab_snapshot, wrap_width, test_executor());
        (wrap_map, wrap_snapshot, multi_buffer)
    }

    fn resync(multi_buffer: &MultiBuffer, wrap_map: &mut WrapMap) -> Arc<super::WrapSnapshot> {
        let snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (wrap_snapshot, _) = wrap_map.sync(tab_snapshot, &Patch::empty());
        wrap_snapshot
    }

    #[test]
    fn incremental_sync_matches_full_rebuild() {
        let (mut wrap_map, _, multi_buffer) = make_wrap_map("abcdefghij\nshort\nxy", Some(5));

        multi_buffer
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(0..1, "ZZ");

        let incremental = resync(&multi_buffer, &mut wrap_map);

        let full_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(full_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let full = super::build_snapshot(tab_snapshot, Some(5));

        assert_eq!(incremental.line_count(), full.line_count());
        assert_eq!(incremental.longest_row, full.longest_row);
        assert_eq!(incremental.longest_row_chars, full.longest_row_chars);
        for row in 0..full.line_count() {
            assert_eq!(
                incremental.line_len(row),
                full.line_len(row),
                "line_len mismatch at row {row}"
            );
        }
    }

    #[test]
    fn incremental_sync_after_line_count_change() {
        let (mut wrap_map, _, multi_buffer) = make_wrap_map("abcdefghij\nshort", Some(5));

        multi_buffer
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(5..5, "\nnewline");

        let result = resync(&multi_buffer, &mut wrap_map);

        let full_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(full_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let full = super::build_snapshot(tab_snapshot, Some(5));

        assert_eq!(result.line_count(), full.line_count());
        for row in 0..full.line_count() {
            assert_eq!(
                result.line_len(row),
                full.line_len(row),
                "line_len mismatch at row {row}"
            );
        }
    }

    #[test]
    fn write_display_line_matches_display_line() {
        let snap = make_snapshot("abcdefghij\nshort\nxy", Some(5));
        for row in 0..snap.line_count() {
            let expected = snap.display_line(row);
            let mut buf = String::new();
            snap.write_display_line(&mut buf, row);
            assert_eq!(buf, expected, "mismatch at row {row}");
        }
    }

    #[test]
    fn incremental_sync_content_change_same_length() {
        let (mut wrap_map, _, multi_buffer) = make_wrap_map("ab cd ef gh", Some(6));

        multi_buffer
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(2..3, "c");

        let incremental = resync(&multi_buffer, &mut wrap_map);

        let full_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(full_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let full = super::build_snapshot(tab_snapshot, Some(6));

        assert_eq!(incremental.line_count(), full.line_count());
        for row in 0..full.line_count() {
            assert_eq!(
                incremental.display_line(row),
                full.display_line(row),
                "display_line mismatch at row {row}"
            );
        }
    }

    fn assert_incremental_matches_full(content: &str, old_width: u32, new_width: u32) {
        let (mut wrap_map, _, multi_buffer) = make_wrap_map(content, Some(old_width));
        wrap_map.set_wrap_width(Some(new_width));
        let incremental = resync(&multi_buffer, &mut wrap_map);

        let full_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(full_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let mut tab_map = TabMap::new(std::num::NonZeroU32::new(4).unwrap());
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let full = super::build_snapshot(tab_snapshot, Some(new_width));

        assert_eq!(incremental.line_count(), full.line_count());
        assert_eq!(incremental.longest_row, full.longest_row);
        assert_eq!(incremental.longest_row_chars, full.longest_row_chars);
        for row in 0..full.line_count() {
            assert_eq!(
                incremental.line_len(row),
                full.line_len(row),
                "line_len mismatch at row {row}"
            );
            assert_eq!(
                incremental.display_line(row),
                full.display_line(row),
                "display_line mismatch at row {row}"
            );
        }
    }

    #[test]
    fn incremental_sync_on_width_increase() {
        assert_incremental_matches_full("abcdefghij\nshort\nxy", 5, 20);
    }

    #[test]
    fn incremental_sync_on_width_decrease() {
        assert_incremental_matches_full("abcdefghij\nshort\nxy", 20, 5);
    }

    #[test]
    fn wrap_respects_max_expansion_column() {
        let mut content = "x".repeat(260);
        content.push('\t');
        content.push_str("abcdef");
        // Tab at col 260 is past MAX_EXPANSION_COLUMN (256), so width = 1.
        // Total expanded length = 260 + 1 + 6 = 267, which fits in 270.
        let snap = make_snapshot(&content, Some(270));
        assert_eq!(snap.line_count(), 1);
    }

    fn assert_longest_in_range_matches_linear(content: &str, wrap_width: Option<u32>) {
        let snap = make_snapshot(content, wrap_width);
        for start in 0..snap.line_count() {
            for count in 0..=(snap.line_count() - start) {
                let (row, chars) = snap.longest_in_output_range(start, count);

                let mut expected_row = 0u32;
                let mut expected_chars = 0u32;
                for i in 0..count {
                    let len = snap.line_len(start + i);
                    if len > expected_chars {
                        expected_row = i;
                        expected_chars = len;
                    }
                }

                assert_eq!(
                    (row, chars),
                    (expected_row, expected_chars),
                    "start={start}, count={count}"
                );
            }
        }
    }

    #[test]
    fn longest_in_output_range_no_wrap() {
        assert_longest_in_range_matches_linear("short\nlonger line here\nab\nmedium", None);
    }

    #[test]
    fn longest_in_output_range_with_wrap() {
        assert_longest_in_range_matches_linear("abcdefghij\nshort\nxy\nmedium text", Some(5));
    }

    #[test]
    fn longest_in_output_range_single_line() {
        assert_longest_in_range_matches_linear("hello", None);
    }

    #[test]
    fn longest_in_output_range_empty_count() {
        let snap = make_snapshot("hello\nworld", None);
        assert_eq!(snap.longest_in_output_range(0, 0), (0, 0));
        assert_eq!(snap.longest_in_output_range(1, 0), (0, 0));
    }

    #[test]
    fn wrapped_chunks_preserve_highlights() {
        use crate::display_map::highlights::{
            create_highlight_endpoints, HighlightKey, HighlightLayer, HighlightStyle,
        };
        use ratatui::style::Color;
        use std::collections::HashMap;
        use stoat_text::Anchor;

        // Wrap "abcdefghij" at width 5 -> two wrap rows: "abcde", "fghij".
        // A highlight on bytes 2..8 (Red) should appear:
        //   row 0: bytes 2..5 styled (chars c, d, e)
        //   row 1: bytes 0..3 styled (chars f, g, h)
        let snap = make_snapshot("abcdefghij", Some(5));
        assert_eq!(snap.line_count(), 2);

        let red = HighlightStyle {
            foreground: Some(Color::Red),
            ..Default::default()
        };

        let mut highlights_map = HashMap::new();
        let key = HighlightKey::new(HighlightLayer::SyntaxToken, 0);
        let mk_anchor = |offset: usize| Anchor {
            timestamp: 0,
            offset: offset as u32,
            bias: stoat_text::Bias::Left,
            buffer_id: None,
        };
        highlights_map.insert(
            key,
            Arc::new((red.clone(), vec![mk_anchor(2)..mk_anchor(8)])),
        );
        let highlights = Arc::new(highlights_map);
        let resolve = |a: &Anchor| a.offset as usize;
        let endpoints: Arc<[_]> = Arc::from(create_highlight_endpoints(
            &(0..10),
            &highlights,
            None,
            &resolve,
        ));

        let chunks: Vec<_> = snap.chunks(0..2, endpoints).collect();
        let mut chars_with_style: Vec<(char, Option<Color>)> = Vec::new();
        for chunk in &chunks {
            let color = chunk.highlight_style.as_ref().and_then(|s| s.foreground);
            for ch in chunk.text.chars() {
                chars_with_style.push((ch, color));
            }
        }
        let recovered: String = chars_with_style.iter().map(|(c, _)| c).collect();
        assert_eq!(recovered, "abcde\nfghij", "wrap reconstruction must match");

        // 'c','d','e' are red (bytes 2,3,4 of source) and 'f','g','h'
        // are red (bytes 5,6,7 of source, on wrap row 1 at cols 0,1,2).
        let expected_colors = [
            ('a', None),
            ('b', None),
            ('c', Some(Color::Red)),
            ('d', Some(Color::Red)),
            ('e', Some(Color::Red)),
            ('\n', None),
            ('f', Some(Color::Red)),
            ('g', Some(Color::Red)),
            ('h', Some(Color::Red)),
            ('i', None),
            ('j', None),
        ];
        assert_eq!(
            chars_with_style.len(),
            expected_colors.len(),
            "char count mismatch"
        );
        for (idx, (got, expected)) in chars_with_style
            .iter()
            .zip(expected_colors.iter())
            .enumerate()
        {
            assert_eq!(
                got, expected,
                "char {idx}: got {got:?}, expected {expected:?}"
            );
        }
    }
}
