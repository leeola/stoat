use crate::{
    action_handlers,
    buffer::BufferId,
    input_view::InputView,
    pane::{Pane, PaneTree, View},
    picker::{Preview, PreviewSource, TargetPicker},
    render::table,
    workspace::Workspace,
};
use std::{path::PathBuf, sync::Arc};
use stoat_scheduler::Executor;
use tokio::sync::Notify;

/// A modal that lists every view the workspace holds, so the reader brings one
/// back on screen by name.
///
/// A view drops out of sight in a parked tab, in a dock, or behind the pane
/// that covers it. Each row says what the view is and where it shows, and
/// Enter takes the reader there.
///
/// The finder owns its rows rather than reading the workspace while it is
/// open. A view that closes after the open keeps its row, and Enter on that
/// row reports it.
pub(crate) struct PaneFinder {
    /// The filter, selection, and preview every target list shares.
    pub(crate) picker: TargetPicker<PaneRow>,
}

impl PaneFinder {
    /// Wrap `rows` with the prompt and preview, with the selection on the
    /// first row.
    pub(crate) fn from_rows(
        rows: Vec<PaneRow>,
        input: InputView,
        preview: Preview,
        executor: Executor,
        redraw: Arc<Notify>,
    ) -> Self {
        let haystacks = rows.iter().map(pane_haystack).collect();
        Self {
            picker: TargetPicker::new(rows, haystacks, input, preview, executor, redraw),
        }
    }

    pub(crate) fn entries(&self) -> &[PaneRow] {
        self.picker.entries()
    }

    pub(crate) fn filtered(&self) -> &[usize] {
        self.picker.filtered()
    }

    /// Cursor into the filtered rows, not into the entries.
    pub(crate) fn selected(&self) -> usize {
        self.picker.selected()
    }

    /// The row under the selection, or [`None`] when the filter matched
    /// nothing.
    pub(crate) fn selected_row(&self) -> Option<&PaneRow> {
        self.picker.selected_entry()
    }

    pub(crate) fn page(&mut self, dir: i32) {
        self.picker.page(dir);
    }

    pub(crate) fn dispose(self, ws: &mut Workspace) {
        self.picker.dispose(ws);
    }
}

/// One held view as the finder lists it.
pub(crate) struct PaneRow {
    /// The word the kind column shows, such as `buffer`, `run`, or `term`.
    pub(crate) kind: &'static str,
    pub(crate) title: String,
    pub(crate) place: PanePlace,
    pub(crate) target: PaneTarget,
}

/// Where a listed view shows.
pub(crate) enum PanePlace {
    /// A split or detached pane of the tab at this 0-based index.
    Tab(usize),
    Dock,
    /// A buffer that no pane shows, or a terminal behind the pane that covers
    /// it.
    Hidden,
}

impl PanePlace {
    /// The place as the place column shows it. Tabs count from 1, as
    /// `GotoTab` counts them.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Tab(index) => format!("tab {}", index + 1),
            Self::Dock => "dock".to_string(),
            Self::Hidden => "hidden".to_string(),
        }
    }
}

/// What Enter brings back on screen for a row.
#[derive(Clone)]
pub(crate) enum PaneTarget {
    /// A buffer the buffer picker lists. Enter opens its path in the focused
    /// pane, and the preview shows its live text.
    Buffer { id: BufferId, path: PathBuf },
    /// A view that a pane or a dock shows, or a terminal that a pane covers.
    View(View),
}

/// The rows of every view `ws` holds, in the order the finder lists them.
///
/// The buffers come first, in path order, under the buffer picker's rule for
/// which buffers to list, so a buffer that no pane shows still has a row. The
/// views follow in tab order from the first tab, with each tab's split panes
/// before its detached panes. The docks come next, and the terminals that
/// panes cover come last.
///
/// An editor lists through its buffer, so a scratch buffer with no path has
/// no row. A label has no row either. A view that shows in two places lists
/// once, at the first, so a covered copy of a terminal never hides the pane
/// that shows it.
pub(crate) fn rows(ws: &Workspace) -> Vec<PaneRow> {
    let trees: Vec<(usize, &PaneTree)> = ws
        .tabs
        .iter()
        .enumerate()
        .filter_map(|(index, tab)| match index == ws.active_tab {
            true => Some((index, &ws.panes)),
            false => tab.parked.as_ref().map(|tree| (index, tree)),
        })
        .collect();

    let mut rows: Vec<PaneRow> = ws
        .buffers
        .listed_paths(&ws.held_buffers())
        .into_iter()
        .filter_map(|path| {
            let id = ws.buffers.id_for_path(&path)?;
            let place = trees
                .iter()
                .find(|(_, tree)| shows_buffer(ws, tree, id))
                .map_or(PanePlace::Hidden, |(index, _)| PanePlace::Tab(*index));
            Some(PaneRow {
                kind: "buffer",
                title: table::display_path(&path, &ws.git_root, usize::MAX),
                place,
                target: PaneTarget::Buffer { id, path },
            })
        })
        .collect();

    let shown = trees.iter().flat_map(|&(index, tree)| {
        tree_panes(tree).map(move |pane| (PanePlace::Tab(index), &pane.view))
    });
    let docked = ws.docks.values().map(|dock| (PanePlace::Dock, &dock.view));
    let covered = trees
        .iter()
        .flat_map(|&(_, tree)| tree_panes(tree))
        .filter_map(|pane| pane.prev_view.as_ref())
        .filter(|view| matches!(view, View::Terminal(_) | View::Agent(_)))
        .map(|view| (PanePlace::Hidden, view));

    for (place, view) in shown.chain(docked).chain(covered) {
        let Some(kind) = view_kind(view) else {
            continue;
        };
        let listed = rows
            .iter()
            .any(|row| matches!(&row.target, PaneTarget::View(seen) if seen == view));
        if listed || !action_handlers::pane::view_is_live(ws, Some(view)) {
            continue;
        }

        rows.push(PaneRow {
            kind,
            title: view_title(ws, view),
            place,
            target: PaneTarget::View(view.clone()),
        });
    }
    rows
}

/// Where a row's preview reads from and the 0-based line it opens on, or
/// [`None`] for a row with nothing to show.
///
/// A buffer shows its live text, and a terminal or an agent shows its screen.
/// Every other view blanks the pane.
pub(crate) fn pane_target(row: &PaneRow) -> Option<(PreviewSource, u32)> {
    match &row.target {
        PaneTarget::Buffer { id, .. } => Some((PreviewSource::Buffer(*id), 0)),
        PaneTarget::View(View::Terminal(id) | View::Agent(id)) => {
            Some((PreviewSource::Terminal(*id), 0))
        },
        PaneTarget::View(_) => None,
    }
}

/// Characters of [`pane_haystack`] that come before the title.
///
/// The renderer highlights only the matched characters that fall in the title
/// column, so it subtracts this from every match offset.
pub(crate) fn haystack_prefix_len(row: &PaneRow) -> u32 {
    (row.kind.chars().count() + 1) as u32
}

/// What a query matches a row against. A reader knows a view by its kind or
/// by its title, which are what the row shows.
fn pane_haystack(row: &PaneRow) -> String {
    format!("{} {}", row.kind, row.title)
}

/// The panes of `tree`, its split panes in layout order and then its detached
/// panes.
fn tree_panes(tree: &PaneTree) -> impl Iterator<Item = &Pane> {
    tree.split_panes().map(|(_, pane)| pane).chain(
        tree.windowed_panes()
            .into_iter()
            .map(move |(id, _)| tree.pane(id)),
    )
}

/// Whether a split pane of `tree` shows an editor of buffer `id`.
fn shows_buffer(ws: &Workspace, tree: &PaneTree, id: BufferId) -> bool {
    tree.split_panes().any(|(_, pane)| {
        matches!(pane.view, View::Editor(editor)
            if ws.editors.get(editor).is_some_and(|editor| editor.buffer_id == id))
    })
}

/// The kind column's word for `view`, or [`None`] for a view with no row of
/// its own.
///
/// An editor lists through its buffer, and a label names nothing to bring
/// back.
fn view_kind(view: &View) -> Option<&'static str> {
    match view {
        View::Run(_) => Some("run"),
        View::Terminal(_) => Some("term"),
        View::Agent(_) => Some("agent"),
        View::Commits(_) => Some("commits"),
        View::DiffChanges(_) => Some("changes"),
        View::Image { .. } => Some("image"),
        View::Editor(_) | View::Label(_) => None,
    }
}

/// The title column's text for `view`.
///
/// A run names the last command it ran, or reads `run` before its first. Any
/// other view reads as [`Workspace::view_title`] names it.
fn view_title(ws: &Workspace, view: &View) -> String {
    match view {
        View::Run(id) => ws
            .runs
            .get(*id)
            .and_then(|run| run.title.clone())
            .unwrap_or_else(|| "run".to_string()),
        _ => ws.view_title(view),
    }
}

#[cfg(test)]
mod tests {
    use super::rows;
    use crate::{
        action_handlers::dispatch,
        pane::{Axis, View},
        test_fixture::{fake_term_session, open_scratch_at},
        test_harness::TestHarness,
        workspace::Workspace,
    };
    use stoat_action::{NewTab, OpenRun, SplitNewRight, Terminal};

    /// Each row of `ws` as its kind, title, and place, joined by spaces.
    fn listed(ws: &Workspace) -> Vec<String> {
        rows(ws)
            .iter()
            .map(|row| format!("{} {} {}", row.kind, row.title, row.place.label()))
            .collect()
    }

    #[test]
    fn rows_list_buffers_then_views_in_tab_order() {
        let mut h = TestHarness::with_size(80, 24);
        open_scratch_at(&mut h, "/ws/a.rs", "fn a() {}");
        dispatch(&mut h.stoat, &SplitNewRight);
        dispatch(&mut h.stoat, &OpenRun);
        dispatch(&mut h.stoat, &NewTab);
        dispatch(&mut h.stoat, &Terminal);

        assert_eq!(
            listed(h.stoat.active_workspace()),
            ["buffer a.rs tab 1", "run run tab 1", "term term tab 2"],
        );
    }

    /// A shell that a pane covers lists as hidden, after every view. A shell
    /// that one pane shows and another covers lists once, at the pane that
    /// shows it.
    #[test]
    fn a_hidden_terminal_lists_once_behind_its_shown_copy() {
        let mut h = TestHarness::with_size(80, 24);
        let ws = h.stoat.active_workspace_mut();
        let [shown, covered] = ["shown", "covered"].map(|title| {
            let (mut session, _) = fake_term_session(4, 10);
            session.term.feed(format!("\x1b]0;{title}\x07").as_bytes());
            ws.terms.insert(session)
        });

        let focus = ws.panes.focus();
        let pane = ws.panes.pane_mut(focus);
        pane.view = View::Terminal(shown);
        pane.prev_view = Some(View::Terminal(covered));
        let side = ws.panes.split(Axis::Vertical);
        let pane = ws.panes.pane_mut(side);
        pane.view = View::Label("side".into());
        pane.prev_view = Some(View::Terminal(shown));

        assert_eq!(listed(ws), ["term shown tab 1", "term covered hidden"]);
    }
}
