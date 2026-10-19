use crate::{
    input_view::InputView,
    picker::{Preview, PreviewSource, TargetPicker},
    walkthrough::{Stop, Walkthrough},
    workspace::Workspace,
};
use std::{fmt::Write, sync::Arc};
use stoat_scheduler::Executor;
use tokio::sync::Notify;

/// Modal listing the workspace's stored walkthroughs, so the reader plays one.
///
/// A reader walks several tours in a row, so the list opens on the tour after
/// the one playing, and Enter alone plays the next. Each row carries a markdown
/// preview of its tour, so the reader sees what a tour covers before playing
/// it.
///
/// The picker owns its rows rather than reading the store while it is open, so
/// a frame never touches the filesystem.
pub(crate) struct WalkthroughPicker {
    /// The filter, selection, and preview every target list shares.
    pub(crate) picker: TargetPicker<WalkthroughRow>,
    /// The row of the tour that was open when the picker opened, which the list
    /// marks.
    playing: Option<usize>,
}

/// One stored walkthrough as the picker lists it.
pub(crate) struct WalkthroughRow {
    pub(crate) slug: String,
    pub(crate) title: String,
    pub(crate) stops: usize,
    /// The markdown the preview pane shows for this tour. It holds the title,
    /// the description, and every stop in order.
    pub(crate) preview: Arc<str>,
}

impl WalkthroughPicker {
    /// Wrap one row per tour in `tours` with the prompt and preview.
    ///
    /// The selection starts on the row after the `playing` slug and wraps to
    /// the first, so Enter alone advances to the next tour. With no tour
    /// playing, it starts on the first row.
    pub(crate) fn from_tours(
        tours: Vec<Walkthrough>,
        playing: Option<&str>,
        input: InputView,
        preview: Preview,
        executor: Executor,
        redraw: Arc<Notify>,
    ) -> Self {
        let rows: Vec<WalkthroughRow> = tours.into_iter().map(row).collect();
        let haystacks = rows.iter().map(walkthrough_haystack).collect();
        let playing = playing.and_then(|slug| rows.iter().position(|row| row.slug == slug));
        let selected = playing.map_or(0, |idx| (idx + 1) % rows.len());

        let mut picker = TargetPicker::new(rows, haystacks, input, preview, executor, redraw);
        picker.move_selection(selected as i32);
        Self { picker, playing }
    }

    pub(crate) fn entries(&self) -> &[WalkthroughRow] {
        self.picker.entries()
    }

    pub(crate) fn filtered(&self) -> &[usize] {
        self.picker.filtered()
    }

    /// Cursor into the filtered rows, not into the entries.
    pub(crate) fn selected(&self) -> usize {
        self.picker.selected()
    }

    /// The slug of the tour under the selection, or [`None`] when the filter
    /// matched nothing.
    pub(crate) fn selected_slug(&self) -> Option<&str> {
        self.picker
            .selected_entry()
            .map(|entry| entry.slug.as_str())
    }

    /// Index into [`Self::entries`] of the tour that was open when the picker
    /// opened.
    pub(crate) fn playing_idx(&self) -> Option<usize> {
        self.playing
    }

    pub(crate) fn page(&mut self, dir: i32) {
        self.picker.page(dir);
    }

    pub(crate) fn dispose(self, ws: &mut Workspace) {
        self.picker.dispose(ws);
    }
}

/// Where a tour's preview reads from and the 0-based line it opens on.
///
/// The preview is composed when the picker opens, so it starts at its top
/// line.
pub(crate) fn walkthrough_target(entry: &WalkthroughRow) -> (PreviewSource, u32) {
    (PreviewSource::Markdown(entry.preview.clone()), 0)
}

/// Characters of [`walkthrough_haystack`] that come before the title.
///
/// The renderer highlights only the matched characters that fall in the title
/// column, so it subtracts this from every match offset.
pub(crate) fn haystack_prefix_len(entry: &WalkthroughRow) -> u32 {
    (entry.slug.chars().count() + 1) as u32
}

/// What a query matches a tour against. A reader knows a tour by its slug or
/// by its title, which are what the row shows.
fn walkthrough_haystack(entry: &WalkthroughRow) -> String {
    format!("{} {}", entry.slug, entry.title)
}

fn row(tour: Walkthrough) -> WalkthroughRow {
    let preview = preview_text(&tour).into();
    WalkthroughRow {
        stops: tour.stops.len(),
        preview,
        slug: tour.slug,
        title: tour.title,
    }
}

/// The markdown preview of `tour`.
///
/// It holds the title as a heading, the description, the stop count, and one
/// numbered line per stop.
fn preview_text(tour: &Walkthrough) -> String {
    let mut text = format!("# {}\n\n", tour.title);
    let description = tour.description.trim();
    if !description.is_empty() {
        let _ = write!(text, "{description}\n\n");
    }

    let count = tour.stops.len();
    let plural = if count == 1 { "" } else { "s" };
    let _ = write!(text, "{count} stop{plural}\n\n");
    for (number, stop) in (1..).zip(&tour.stops) {
        let _ = writeln!(text, "{number}. {}", stop_line(stop));
    }
    text
}

/// One stop as the preview lists it.
///
/// The line starts with the stop's title, or its focus path when it has none.
/// The path and the range follow, in the form the CLI reads.
fn stop_line(stop: &Stop) -> String {
    let path = stop.focus.path.display().to_string();
    let range = stop.focus.range;
    let name = stop.title.as_deref().unwrap_or(&path);
    format!(
        "{name}: {path} {}:{}-{}:{}",
        range.start.line, range.start.col, range.end.line, range.end.col
    )
}
