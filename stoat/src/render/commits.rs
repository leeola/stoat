use crate::{
    app::{SeparatorAxis, SplitSeparator},
    commit_list::{CommitListState, CommitRefs, Preview},
    commit_picker::{age_label, age_words},
    diff_map::{BaseHighlights, ChangeKind, ProseChange},
    display_map::DEFAULT_TAB_SIZE,
    host::{CommitFileChange, CommitFileChangeKind, CommitInfo},
    pane::Pane,
    render::{
        commit_picker::{graph_width, paint_commit_graph, tip_colors, GraphPaint},
        paint::{blend, render_empty_num, render_side_num, render_side_text, style_rgb},
        pane::ScreenSubject,
        picker::line_strong,
        review::{
            paint_base_row, resolve_diff_tints, DiffColumns, DiffDials, DiffLayout, DiffTints,
        },
        text::{
            clip_to_width, truncate_to_cols, wrap_text, write_cell, write_str, write_str_clipped,
        },
        FrameCtx,
    },
    review::{ReviewRow, ReviewSide},
    review_session::DiffDocument,
    theme::TypeRole,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
use std::{collections::BTreeMap, fmt::Write, path::Path, sync::Arc};
use stoat_widgets::{
    bar::Bar,
    text_run::{self, TextRun, TextRunBatch},
};

pub(crate) fn render_commits(
    pane: &Pane,
    state: &mut CommitListState,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let theme = frame.theme;
    let workspace_root = frame.workspace_root;
    let inner = pane.area;

    let Some(left_area) = commits_list_rect(pane.area, frame.commits_split) else {
        return;
    };
    let sep_x = left_area.x + left_area.width;

    let sep_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    crate::render::chrome::vline(buf, sep_x, inner.y, inner.height, sep_style, &mut *scene);

    state.viewport_rows = left_area.height as usize;
    state.ensure_selected_visible(state.viewport_rows);
    if let Some(header) = commits_header_rect(pane.area, frame.commits_split) {
        render_graph_header(state, theme, header, buf, &mut *scene);
    }
    render_commit_list_pane(state, theme, left_area, buf, &mut *scene);

    if let Some(right_area) = commits_detail_rect(pane.area, frame.commits_split) {
        render_commit_detail_pane(
            state,
            workspace_root,
            theme,
            right_area,
            frame.preview_dials,
            buf,
            scene,
        );
    }
}

/// The selected commit, as the window's status row names it after the commits
/// screen, with a merge's count of parents as a remark.
pub(crate) fn status_subject(state: &CommitListState) -> Option<ScreenSubject<'_>> {
    state
        .commits
        .get(state.selected)
        .map(|commit| ScreenSubject {
            name: &commit.short_sha,
            note: (commit.parents.len() >= 2)
                .then(|| format!("merge \u{b7} {} parents", commit.parents.len())),
        })
}

/// Narrowest the commit list gets, however far the separator is dragged toward
/// it. A sha, a relative date, and a few words of summary is the least that
/// still identifies a row.
pub(crate) const MIN_LIST_COLUMNS: u16 = 22;

/// Narrowest the detail pane gets, the counterpart floor on the other side of
/// the one-column separator.
pub(crate) const MIN_DETAIL_COLUMNS: u16 = 11;

/// Cells a row's summary keeps beside its badges and its author and age
/// columns, enough for a few words of it.
const MIN_SUMMARY_COLUMNS: u16 = 8;

/// Cells the author column takes, for two initials.
const INITIALS_COLUMNS: u16 = 2;

/// Cells the age column takes, the longest label [`age_label`] gives.
const AGE_COLUMNS: u16 = 4;

/// Cells the author and age columns take at a row's right end: a gap before
/// each and one cell after the age.
const META_COLUMNS: u16 = 1 + INITIALS_COLUMNS + 1 + AGE_COLUMNS + 1;

/// Cells between two badges, and between the last badge and the summary.
const BADGE_GAP: u16 = 1;

/// Share of a local branch's lane color mixed into its badge's fill, in
/// percent.
const BADGE_TINT_PERCENT: u16 = 12;

/// Share of a remote or tag badge's color mixed into its ring, in percent.
///
/// Light enough to frame the name without competing with it.
const RING_PERCENT: u16 = 40;

/// Thickness of a badge's ring, in sixteenths of a cell.
const RING_STROKE: u16 = 1;

/// Cells the commits view's text rows stand in from each side of their pane,
/// such as the graph header and the detail header.
const TEXT_INSET: u16 = 1;

/// Rows the graph island's header takes above the commit rows.
const GRAPH_HEADER_ROWS: u16 = 1;

/// Branch tips the graph header's legend names at most.
const LEGEND_TIPS: usize = 4;

/// Width of a legend swatch in sixteenths, set in from both sides of its cell.
const SWATCH_WIDTH: u16 = 14;

/// Height of a legend swatch in sixteenths, centered in its row.
const SWATCH_HEIGHT: u16 = 2;

/// Thickness of the graph header's rule, in sixteenths of a cell.
const RULE_STROKE: u16 = 1;

/// Cells between the pieces of the detail header's meta row.
const META_GAP: u16 = 1;

/// Rows the detail header gives a commit's message body at most.
///
/// Enough for a short paragraph, while a long body leaves the files table and
/// the diff their room.
const BODY_ROWS: usize = 4;

/// Cells a file row's stat bar takes inside stoatty.
const STAT_BAR_CELLS: u16 = 7;

/// Height of a stat bar in sixteenths of a cell, centered in its row.
const STAT_BAR_HEIGHT: u16 = 4;

/// The keys the commits view's preview answers, each a bound key and the word
/// for what it does.
///
/// Written out rather than read from the keymap, as a modal's footer hints
/// are, so the label names the default bindings.
const PREVIEW_HINTS: [(&str, &str); 2] = [("C-d/C-u", "scroll"), ("Enter", "review")];

/// The text between two of the preview's hints.
const HINT_SEPARATOR: &str = " \u{b7} ";

/// Width of a preview row's lane bar in sixteenths of a cell, the Commits
/// board's 3 px.
const LANE_BAR_WIDTH: u16 = 6;

/// The commit list's rows within an overlay pane, or `None` when the pane is
/// too small. Shared by the renderer, the smooth-scroll emit, and the pointer,
/// so the pooled region and a pressed row match the painted rows.
///
/// The rows start under the graph island's header row, which
/// [`commits_header_rect`] places. So a glide leaves the header out of the
/// pooled region, and a press maps to the row under the pointer.
///
/// `split` is the dragged share of the body as a percentage, `None` until the
/// reader drags the separator.
pub(crate) fn commits_list_rect(pane_area: Rect, split: Option<u16>) -> Option<Rect> {
    let inner = pane_area;
    if inner.width < 10 || inner.height <= GRAPH_HEADER_ROWS {
        return None;
    }
    let left_w = commit_list_width(inner.width, split);
    Some(Rect::new(
        inner.x,
        inner.y + GRAPH_HEADER_ROWS,
        left_w,
        inner.height - GRAPH_HEADER_ROWS,
    ))
}

/// The graph island's header row above the commit rows, or `None` when the
/// pane has no room for the list.
fn commits_header_rect(pane_area: Rect, split: Option<u16>) -> Option<Rect> {
    let list = commits_list_rect(pane_area, split)?;
    Some(Rect::new(
        list.x,
        list.y - GRAPH_HEADER_ROWS,
        list.width,
        GRAPH_HEADER_ROWS,
    ))
}

/// The detail pane's rectangle within an overlay pane, right of the
/// separator, or `None` when the pane is too small to carry one.
///
/// The counterpart to [`commits_list_rect`], so the paint and the hit test
/// read one geometry and a wheel lands on the pane the reader sees.
pub(crate) fn commits_detail_rect(pane_area: Rect, split: Option<u16>) -> Option<Rect> {
    let inner = pane_area;
    let list = commits_list_rect(pane_area, split)?;
    let width = inner.width.saturating_sub(list.width + 1);
    (width > 0).then(|| Rect::new(list.x + list.width + 1, inner.y, width, inner.height))
}

/// The commits list/detail separator, or `None` when the pane is too small to
/// carry two panes.
///
/// Built from the same inputs the renderer reads, so a hit-test lands on the
/// line the reader sees.
pub(crate) fn commits_separator(pane_area: Rect, split: Option<u16>) -> Option<SplitSeparator> {
    let inner = pane_area;
    let list = commits_list_rect(pane_area, split)?;
    Some(SplitSeparator {
        axis: SeparatorAxis::Columns,
        line: list.x + list.width,
        span: inner.y..inner.y + inner.height,
        body: inner.x..inner.x + inner.width,
        min_list: MIN_LIST_COLUMNS,
        min_preview: MIN_DETAIL_COLUMNS,
    })
}

/// Columns the list takes out of `total`, floored and capped so both panes
/// keep their minimum whatever `split` asks for.
///
/// With no dragged share the list takes four sevenths, the share the Commits
/// board gives its graph island, so a summary reads whole beside its graph.
///
/// The percentage divides truncating, which is what [`SplitSeparator::share_at`]
/// rounds up against, so a dragged separator lands on the cell the pointer left
/// it on rather than a column short.
fn commit_list_width(total: u16, split: Option<u16>) -> u16 {
    let widest = total.saturating_sub(MIN_DETAIL_COLUMNS + 1);
    let target = match split {
        Some(pct) => (total as u32 * pct as u32 / 100) as u16,
        None => (total as u32 * 4 / 7) as u16,
    };
    target.max(MIN_LIST_COLUMNS).min(widest)
}

fn render_commit_list_pane(
    state: &CommitListState,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let top = state.scroll_top.min(state.commits.len().saturating_sub(1));
    paint_commit_rows(state, area, top, theme, true, buf, scene);
}

/// Paint the graph island's header into `row`.
///
/// The row reads `graph` in bold, then a legend that pairs lanes with the
/// branch tips in them, each a swatch in the lane's tip color and the tip's
/// name in `ui.text.muted`. [`legend_tips`] picks the entries. An entry that
/// does not fit stays off the row with every entry after it.
///
/// Inside stoatty each swatch is a sub-cell bar, and a hairline in
/// `ui.surface.raised` underlines the row. A plain terminal draws the swatch
/// as a line glyph and no rule.
fn render_graph_header(
    state: &CommitListState,
    theme: &crate::theme::Theme,
    row: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    use crate::theme::scope as s;
    let end_x = row.x + row.width;
    let bold = theme.get(s::UI_TEXT).add_modifier(Modifier::BOLD);
    let x0 = row.x + TEXT_INSET;
    write_str_clipped(buf, x0, row.y, "graph", bold, end_x);

    let muted = theme.get(s::UI_TEXT_MUTED);
    let tip_color = tip_colors(theme, state.graph.1);
    let live = scene.live();
    let mut x = x0 + "graph".len() as u16 + 1;
    for (lane, name) in legend_tips(state) {
        let width = 1 + name.chars().count() as u16;
        if x.saturating_add(width) > end_x {
            break;
        }

        let color = tip_color(lane);
        match live {
            true => Bar {
                x: ((16 - SWATCH_WIDTH) / 2) as i16,
                y: ((16 - SWATCH_HEIGHT) / 2) as i16,
                width: SWATCH_WIDTH,
                height: SWATCH_HEIGHT,
                color,
            }
            .render(Rect::new(x, row.y, 1, 1), buf, &mut *scene),
            false => {
                let [r, g, b] = color;
                let swatch = Style::new().fg(Color::Rgb(r, g, b));
                write_cell(buf, x, row.y, '\u{2501}', swatch);
            },
        }
        write_str(buf, x + 1, row.y, name, muted);
        x += width + 1;
    }

    if let Some(color) = style_rgb(theme.get(s::UI_SURFACE_RAISED).bg) {
        Bar {
            x: 0,
            y: (16 - RULE_STROKE) as i16,
            width: row.width * 16,
            height: RULE_STROKE,
            color,
        }
        .render(row, buf, scene);
    }
}

/// The legend entries of the graph header, as `(lane, name)` pairs.
///
/// One entry stands for each lane that holds a local branch tip among the
/// loaded rows, lanes in order, at most [`LEGEND_TIPS`] of them. Each names
/// the first branch at the topmost tip in its lane.
fn legend_tips(state: &CommitListState) -> Vec<(u16, &str)> {
    let mut by_lane: BTreeMap<u16, &str> = BTreeMap::new();
    for (commit, row) in state.commits.iter().zip(&state.graph.0) {
        if let Some(name) = state
            .refs
            .local
            .get(&commit.sha)
            .and_then(|names| names.first())
        {
            by_lane.entry(row.node_lane).or_insert(name);
        }
    }
    by_lane.into_iter().take(LEGEND_TIPS).collect()
}

/// Paint the commit list into `area` starting at row `start_row`, with the
/// selected row highlighted and a trailing load/end marker.
///
/// The lane graph takes a column at the left and the rows take the rest, as the
/// commit picker's table sits right of its graph. HEAD, the commit the list's
/// walk starts from, is the first row.
///
/// A row reads its sha, the badges of the refs that point at it, and its
/// summary, which a merge's row paints in the metadata color. The author's
/// initials and the commit's age close the row where the list has room for
/// them beside a few words of summary.
///
/// Shared by the live list, which derives `start_row` from `scroll_top`, and the
/// smooth-scroll pool, which paints absolute pages, so both render identical
/// rows. The selected row's accent bar and the graph's strokes go into `scene`.
///
/// With `panels`, a filled badge's box goes through
/// [`chip_cells`](crate::render::chrome::chip_cells). A pool page captures no
/// panel, so a page passes `false` and paints the box into the badge's cells.
pub(crate) fn paint_commit_rows(
    state: &CommitListState,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    panels: bool,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    use crate::theme::scope as s;
    let dim = theme.get(s::VCS_COMMIT_METADATA);
    if state.commits.is_empty() {
        let msg = if state.pending_load.is_some() {
            "loading commits..."
        } else {
            "no commits"
        };
        write_str(buf, area.x, area.y, msg, dim);
        return;
    }

    let (rows, lanes) = (&state.graph.0, state.graph.1);
    let graph_cells = match lanes {
        0 => 0,
        lanes => graph_width(lanes).min(area.width),
    };
    if graph_cells > 0 {
        let graph = GraphPaint {
            rows,
            lanes,
            commits: &state.commits,
            tips: &state.refs.local,
            background: style_rgb(theme.get(s::UI_BACKGROUND).bg),
            head: state.commits.first().map(|commit| commit.sha.as_str()),
        };
        let column = Rect::new(area.x, area.y, graph_cells, area.height);
        paint_commit_graph(&graph, start_row, column, theme, buf, &mut *scene);
    }
    let area = Rect::new(
        area.x + graph_cells,
        area.y,
        area.width - graph_cells,
        area.height,
    );

    let sel_style = theme.list_selected(crate::theme::scope::UI_SELECTION_REVERSED);
    let sha_style = theme.get(s::VCS_COMMIT_SHA);
    let summary_style = theme.get(s::VCS_COMMIT_SUMMARY);
    let badge_styles = BadgeStyles::new(theme);
    let tip_color = tip_colors(theme, lanes);

    let top = start_row.min(state.commits.len());
    let rows_visible = area.height as usize;
    let end = (top + rows_visible).min(state.commits.len());
    let area_end = area.x + area.width;

    for (i, commit) in state.commits[top..end].iter().enumerate() {
        let index = top + i;
        let y = area.y + i as u16;
        let is_selected = index == state.selected;
        // The selected row's own fill carries it, so its text drops the column
        // colors, as the commit picker's does. The badges keep theirs.
        let pick = |style: Style| if is_selected { sel_style } else { style };

        if is_selected && area.width > 0 {
            for x in area.x..area_end {
                buf[(x, y)].set_style(sel_style);
            }
            crate::render::chrome::selection_bar(area.x, y, theme, buf, scene);
        }

        let sha = &commit.short_sha;
        let sha_len = sha.len().min(area.width as usize);
        write_str(buf, area.x, y, &sha[..sha_len], pick(sha_style));

        let text_x = area.x + sha_len as u16 + 1;
        let summary_end =
            match area_end.saturating_sub(text_x) >= MIN_SUMMARY_COLUMNS + META_COLUMNS {
                true => {
                    let age_end = area_end - 1;
                    let age = age_label(state.now_epoch, commit.time);
                    write_right(buf, age_end, y, AGE_COLUMNS, &age, pick(dim));
                    let author = initials(&commit.author_name);
                    let author_end = age_end - AGE_COLUMNS - 1;
                    write_right(buf, author_end, y, INITIALS_COLUMNS, &author, pick(dim));
                    area_end - META_COLUMNS
                },
                false => area_end,
            };

        let badges = BadgeRow {
            x: text_x,
            end: summary_end,
            y,
            lane_color: tip_color(rows.get(index).map_or(0, |row| row.node_lane)),
            panels,
        };
        let summary_x = paint_badges(
            row_badges(&state.refs, &commit.sha),
            badges,
            &badge_styles,
            buf,
            &mut *scene,
        );

        let room = summary_end.saturating_sub(summary_x);
        if room > 0 {
            let summary = truncate_to_cols(&commit.summary, room as usize);
            let style = match commit.parents.len() >= 2 {
                true => dim,
                false => summary_style,
            };
            write_str(buf, summary_x, y, &summary, pick(style));
        }
    }

    if state.pending_load.is_some() && end == state.commits.len() && end - top < rows_visible {
        let y = area.y + (end - top) as u16;
        write_str(buf, area.x, y, "loading more...", dim);
    } else if state.reached_end && end == state.commits.len() && end - top < rows_visible {
        let y = area.y + (end - top) as u16;
        write_str(buf, area.x, y, "(end of history)", dim);
    }
}

/// One ref badge beside a commit row.
#[derive(Clone, Copy)]
struct RefBadge<'a> {
    kind: RefKind,
    /// The text before the name, such as a tag's mark.
    prefix: &'static str,
    name: &'a str,
}

impl RefBadge<'_> {
    fn cols(&self) -> usize {
        self.prefix.chars().count() + self.name.chars().count()
    }
}

#[derive(Clone, Copy)]
enum RefKind {
    Head,
    Local,
    Remote,
    Tag,
}

/// The badge colors one paint of the list shares, read from the theme once.
struct BadgeStyles {
    head: Style,
    remote: Style,
    tag: Style,
    /// The list's background, which a local badge's fill and a ring mix their
    /// color into.
    ground: Option<[u8; 3]>,
}

impl BadgeStyles {
    fn new(theme: &crate::theme::Theme) -> Self {
        use crate::theme::scope as s;
        Self {
            head: theme.get(s::VCS_REF_HEAD),
            remote: theme.get(s::VCS_REF_REMOTE),
            tag: theme.get(s::VCS_REF_TAG),
            ground: style_rgb(theme.get(s::UI_BACKGROUND).bg),
        }
    }

    /// The style a `kind` badge paints with, a local branch's in `lane_color`.
    fn style(&self, kind: RefKind, lane_color: [u8; 3]) -> Style {
        match kind {
            RefKind::Head => self.head,
            RefKind::Remote => self.remote,
            RefKind::Tag => self.tag,
            RefKind::Local => {
                let [r, g, b] = lane_color;
                let style = Style::new()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::BOLD);
                match self.ground {
                    Some(ground) => {
                        let [r, g, b] = blend(ground, lane_color, BADGE_TINT_PERCENT);
                        style.bg(Color::Rgb(r, g, b))
                    },
                    None => style,
                }
            },
        }
    }

    /// The color of the hairline ring around a `kind` badge.
    ///
    /// Only a remote or a tag badge, which has no fill of its own, takes a
    /// ring. `None` for the rest, and where the theme gives no RGB to mix.
    fn ring(&self, kind: RefKind) -> Option<[u8; 3]> {
        let fg = match kind {
            RefKind::Remote => self.remote.fg,
            RefKind::Tag => self.tag.fg,
            RefKind::Head | RefKind::Local => return None,
        };
        Some(blend(self.ground?, style_rgb(fg)?, RING_PERCENT))
    }
}

/// Where one row's badges go.
#[derive(Clone, Copy)]
struct BadgeRow {
    x: u16,
    /// The column where the summary's room ends.
    end: u16,
    y: u16,
    /// The color of the lane the row's node sits in.
    lane_color: [u8; 3],
    /// Whether a filled badge's box draws as a chip panel.
    panels: bool,
}

/// The badges of the row that shows `sha`, in the order the row paints them:
/// HEAD, then local branches, remote-tracking branches, and tags.
///
/// The head badge names the branch HEAD is on, so that branch takes no local
/// badge of its own.
fn row_badges<'a>(refs: &'a CommitRefs, sha: &str) -> impl Iterator<Item = RefBadge<'a>> {
    let at_head = refs.head.as_deref() == Some(sha);
    let checked_out = refs.head_branch.as_deref().filter(|_| at_head);
    let head = at_head.then_some(match checked_out {
        Some(branch) => RefBadge {
            kind: RefKind::Head,
            prefix: "HEAD \u{2192} ",
            name: branch,
        },
        None => RefBadge {
            kind: RefKind::Head,
            prefix: "HEAD",
            name: "",
        },
    });

    let named = |names: Option<&'a Vec<String>>, kind: RefKind, prefix: &'static str| {
        names.into_iter().flatten().map(move |name| RefBadge {
            kind,
            prefix,
            name: name.as_str(),
        })
    };
    let local = named(refs.local.get(sha), RefKind::Local, "")
        .filter(move |badge| Some(badge.name) != checked_out);
    let remote = named(refs.remote.get(sha), RefKind::Remote, "");
    let tags = named(refs.tags.get(sha), RefKind::Tag, "\u{25c6} ");
    head.into_iter().chain(local).chain(remote).chain(tags)
}

/// Paint one row's badges from `row.x`, and return the column the summary
/// starts at.
///
/// A badge is its text with one cell of padding at each end. A badge that
/// leaves the summary fewer than [`MIN_SUMMARY_COLUMNS`] cells before
/// `row.end` stays off the row, with every badge after it, so the badges that
/// show keep their order.
///
/// With [`BadgeRow::panels`], a filled badge's box goes through
/// [`chip_cells`](crate::render::chrome::chip_cells).
fn paint_badges<'a>(
    badges: impl Iterator<Item = RefBadge<'a>>,
    row: BadgeRow,
    styles: &BadgeStyles,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> u16 {
    let mut x = row.x;
    for badge in badges {
        let width = badge.cols() + 2;
        let summary_x = x as usize + width + BADGE_GAP as usize;
        if summary_x + MIN_SUMMARY_COLUMNS as usize > row.end as usize {
            break;
        }

        let cells = Rect::new(x, row.y, width as u16, 1);
        let style = match row.panels {
            true => crate::render::chrome::chip_cells(
                cells,
                styles.style(badge.kind, row.lane_color),
                scene,
            ),
            false => styles.style(badge.kind, row.lane_color),
        };
        for col in cells.x..cells.right() {
            buf[(col, row.y)].set_char(' ').set_style(style);
        }
        write_str(buf, x + 1, row.y, badge.prefix, style);
        let name_x = x + 1 + badge.prefix.chars().count() as u16;
        write_str(buf, name_x, row.y, badge.name, style);

        if let Some(color) = styles.ring(badge.kind) {
            ring(cells, color, buf, scene);
        }
        x = summary_x as u16;
    }
    x
}

/// Frame `cells` with a hairline in `color`.
///
/// The ring stands one stroke in from the row's top and bottom, so the rings
/// of badges on neighbouring rows stay apart.
fn ring(cells: Rect, color: [u8; 3], buf: &mut Buffer, scene: &mut stoat_widgets::ApcScene) {
    let span = cells.width * 16;
    let tall = 16 - 2 * RING_STROKE;
    let (top, bottom) = (RING_STROKE as i16, tall as i16);
    let sides = [
        (0, top, span, RING_STROKE),
        (0, bottom, span, RING_STROKE),
        (0, top, RING_STROKE, tall),
        ((span - RING_STROKE) as i16, top, RING_STROKE, tall),
    ];
    for (x, y, width, height) in sides {
        Bar {
            x,
            y,
            width,
            height,
            color,
        }
        .render(cells, buf, scene);
    }
}

/// The initials the author column shows for `name`, at most two letters.
///
/// A name of two or more words gives the first letters of its first and last
/// words, and a one-word name its first two letters, so "Ada Lovelace" reads
/// `AL` and "Lee" reads `LE`.
fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().unwrap_or_default();
    let letters = match words.next_back() {
        Some(last) => [first.chars().next(), last.chars().next()],
        None => {
            let mut chars = first.chars();
            [chars.next(), chars.next()]
        },
    };
    letters
        .into_iter()
        .flatten()
        .flat_map(char::to_uppercase)
        .take(INITIALS_COLUMNS as usize)
        .collect()
}

/// Write `text` so its last char sits in the column before `end`, cut to its
/// first `width` chars.
fn write_right(buf: &mut Buffer, end: u16, y: u16, width: u16, text: &str, style: Style) {
    let cut = text
        .char_indices()
        .nth(width as usize)
        .map_or(text.len(), |(byte, _)| byte);
    let text = &text[..cut];
    let cols = text.chars().count() as u16;
    write_str(buf, end.saturating_sub(cols), y, text, style);
}

/// Paint the selected commit's header, files table, and diff preview into
/// `area`.
///
/// A label row names the file the preview stands on. The preview takes the
/// pane's full width, and the rows above it stand one cell in from each side.
fn render_commit_detail_pane(
    state: &mut CommitListState,
    workspace_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    dials: PreviewDials,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let dim = theme.get(crate::theme::scope::VCS_COMMIT_METADATA);
    let Some(commit) = state.commits.get(state.selected) else {
        write_str(buf, area.x, area.y, "no selection", dim);
        return;
    };
    let sha = commit.sha.clone();
    let text_area = Rect::new(
        area.x + TEXT_INSET,
        area.y,
        area.width.saturating_sub(2 * TEXT_INSET),
        area.height,
    );
    let header_rows = render_commit_header(state, commit, theme, text_area, buf, scene);

    let end_y = area.y + area.height;
    let files_y = area.y + header_rows + 1;
    if files_y >= end_y {
        return;
    }
    let changes = state.summaries.get(&sha);
    let files_rows = changes
        .map_or(1, |changes| 1 + changes.len())
        .min(usize::from(end_y - files_y)) as u16;
    let label_y = files_y + files_rows + 1;
    let preview_area = Rect::new(
        area.x,
        label_y + 1,
        area.width,
        end_y.saturating_sub(label_y + 1),
    );

    let preview = match state.preview_sessions.get(&sha) {
        Preview::Built(document) => Some(Some(Arc::clone(document))),
        Preview::Empty => Some(None),
        Preview::Unbuilt => None,
    };
    if let Some(Some(document)) = &preview {
        // Clamped here, where the diff's line count and the pane's height
        // are both known. Written back so the wheel and the keys step from
        // the offset actually painted rather than past the last page.
        state.preview_scroll =
            clamped_line_scroll(document, state.preview_scroll, preview_area.height);
    }
    let shown = match &preview {
        Some(Some(document)) => file_at_line(document, state.preview_scroll),
        _ => None,
    };
    let current = match (&preview, shown) {
        (Some(Some(document)), Some((file, _))) => Some(document.files[file].rel_path.as_str()),
        _ => None,
    };

    let files_area = Rect::new(area.x, files_y, area.width, files_rows);
    match changes {
        Some(changes) => render_commit_files(
            changes,
            current,
            workspace_root,
            theme,
            files_area,
            buf,
            &mut *scene,
        ),
        None => write_str(buf, text_area.x, files_y, "loading summary...", dim),
    }

    if label_y >= end_y {
        return;
    }
    let label = Rect::new(text_area.x, label_y, text_area.width, 1);
    render_preview_label(current, theme, label, buf);
    if preview_area.height == 0 {
        return;
    }
    match preview {
        Some(Some(document)) => {
            if let Some((file, skip)) = shown {
                render_file_preview(
                    &document,
                    file,
                    skip,
                    theme,
                    preview_area,
                    dials,
                    buf,
                    scene,
                );
            }
        },
        Some(None) => write_str(buf, text_area.x, preview_area.y, "no changes", dim),
        None => write_str(buf, text_area.x, preview_area.y, "loading preview...", dim),
    }
}

/// Paint the header of `commit`, a commit of `state`'s list, into `area`, and
/// return the rows it took.
///
/// The first row is the summary in bold, and the second its meta row, which
/// [`paint_commit_meta`] lays out. The message body that `state` holds for the
/// commit follows in `ui.text.dim`, word-wrapped into at most [`BODY_ROWS`]
/// rows.
fn render_commit_header(
    state: &CommitListState,
    commit: &CommitInfo,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> u16 {
    use crate::theme::scope as s;
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    let end_x = area.x + area.width;
    let bold = theme.get(s::UI_TEXT).add_modifier(Modifier::BOLD);
    write_str_clipped(buf, area.x, area.y, &commit.summary, bold, end_x);
    if area.height == 1 {
        return 1;
    }

    let meta = Rect::new(area.x, area.y + 1, area.width, 1);
    paint_commit_meta(
        commit,
        &state.refs,
        state.now_epoch,
        theme,
        meta,
        buf,
        scene,
    );

    let room = usize::from(area.height - 2);
    let lines = state
        .bodies
        .get(&commit.sha)
        .map_or_else(Vec::new, |body| body_rows(body, usize::from(area.width)));
    let dim = theme.get(s::UI_TEXT_DIM);
    for (row, line) in lines.iter().take(room).enumerate() {
        write_str_clipped(buf, area.x, area.y + 2 + row as u16, line, dim, end_x);
    }
    2 + lines.len().min(room) as u16
}

/// Paint the meta row of `commit` into `row`.
///
/// The row reads the short sha, the author, and the age in words, then a
/// divider and a chip per parent. A chip reads the parent's number, its short
/// sha, and the first local branch that points at it. The first parent's chip
/// takes `vcs.commit.parent` and the rest `vcs.commit.parent.merged`, and its
/// box goes through [`chip_cells`](crate::render::chrome::chip_cells).
///
/// A chip that does not fit stays off the row with every chip after it. A root
/// commit has no parent, so its row ends at the age.
fn paint_commit_meta(
    commit: &CommitInfo,
    refs: &CommitRefs,
    now_epoch: i64,
    theme: &crate::theme::Theme,
    row: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    use crate::theme::scope as s;
    let end_x = row.x + row.width;
    let meta = theme.get(s::VCS_COMMIT_METADATA);
    let age = age_words(now_epoch, commit.time);

    let mut x = row.x;
    for (text, style) in [
        (commit.short_sha.as_str(), theme.get(s::VCS_COMMIT_SHA)),
        (commit.author_name.as_str(), meta),
        (age.as_str(), meta),
    ] {
        write_str_clipped(buf, x, row.y, text, style, end_x);
        x = x.saturating_add(text.chars().count() as u16 + META_GAP);
    }
    if commit.parents.is_empty() {
        return;
    }

    let divider = line_strong(theme).map_or(meta, |[r, g, b]| Style::new().fg(Color::Rgb(r, g, b)));
    write_str_clipped(buf, x, row.y, "\u{2502}", divider, end_x);
    x = x.saturating_add(1 + META_GAP);
    let label = match commit.parents.len() {
        1 => "parent",
        _ => "parents",
    };
    write_str_clipped(buf, x, row.y, label, meta, end_x);
    x = x.saturating_add(label.len() as u16 + META_GAP);

    let short_len = commit.short_sha.len();
    for (index, parent) in commit.parents.iter().enumerate() {
        let short = &parent[..short_len.min(parent.len())];
        let chip = match refs.local.get(parent).and_then(|names| names.first()) {
            Some(branch) => format!("{} \u{b7} {short} {branch}", index + 1),
            None => format!("{} \u{b7} {short}", index + 1),
        };
        let width = chip.chars().count() as u16 + 2;
        if x.saturating_add(width) > end_x {
            break;
        }

        let style = match index {
            0 => theme.get(s::VCS_COMMIT_PARENT),
            _ => theme.get(s::VCS_COMMIT_PARENT_MERGED),
        };
        let style = crate::render::chrome::chip_cells(Rect::new(x, row.y, width, 1), style, scene);
        for col in x..x + width {
            buf[(col, row.y)].set_char(' ').set_style(style);
        }
        write_str(buf, x + 1, row.y, &chip, style);
        x += width + META_GAP;
    }
}

/// The message `body` word-wrapped to `width` cells, in at most [`BODY_ROWS`]
/// rows.
///
/// Its line breaks reflow, since its author wrapped it for some other width. A
/// body that needs more rows ends its last row in an ellipsis.
fn body_rows(body: &str, width: usize) -> Vec<String> {
    let mut rows = wrap_text(body, width);
    if rows.len() <= BODY_ROWS {
        return rows;
    }

    rows.truncate(BODY_ROWS);
    if let Some(last) = rows.last_mut() {
        let kept = clip_to_width(last, width.saturating_sub(1)).trim_end();
        *last = format!("{kept}\u{2026}");
    }
    rows
}

/// Paint `changes` into `area` as the commit's files table.
///
/// The header row reads `files` and `vs parent`, with the commit's added and
/// deleted counts at the right. A row per file follows while rows remain.
///
/// A file row reads its kind letter in the diff
/// color of its change, its directory in `ui.text.muted` and its name, then its
/// counts, in columns as wide as the widest count. Inside stoatty a stat bar
/// closes the row. The row of `current`, the file the preview stands on, fills
/// `ui.list.current` from edge to edge.
fn render_commit_files(
    changes: &[CommitFileChange],
    current: Option<&str>,
    workspace_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    use crate::theme::scope as s;
    if area.height == 0 || area.width <= 2 * TEXT_INSET {
        return;
    }
    let x0 = area.x + TEXT_INSET;
    let end_x = area.x + area.width - TEXT_INSET;
    let muted = theme.get(s::UI_TEXT_MUTED);
    let added = theme.get(s::DIFF_ADDED);
    let deleted = theme.get(s::DIFF_DELETED);

    let bold = theme.get(s::UI_TEXT).add_modifier(Modifier::BOLD);
    write_str_clipped(buf, x0, area.y, "files", bold, end_x);
    write_str_clipped(buf, x0 + 6, area.y, "vs parent", muted, end_x);
    let total_add = format!("+{}", changes.iter().map(|c| c.additions).sum::<u32>());
    let total_del = format!(
        "\u{2212}{}",
        changes.iter().map(|c| c.deletions).sum::<u32>()
    );
    let del_x = end_x.saturating_sub(total_del.chars().count() as u16);
    let add_x = del_x.saturating_sub(total_add.chars().count() as u16 + 1);
    if add_x > x0 + 6 + "vs parent".len() as u16 {
        write_str(buf, add_x, area.y, &total_add, added);
        write_str(buf, del_x, area.y, &total_del, deleted);
    }

    let count_cols = |count: fn(&CommitFileChange) -> u32| {
        changes
            .iter()
            .map(|change| {
                count(change)
                    .checked_ilog10()
                    .map_or(1, |log| log as u16 + 1)
                    + 1
            })
            .max()
            .unwrap_or(2)
    };
    let (add_cols, del_cols) = (count_cols(|c| c.additions), count_cols(|c| c.deletions));
    let bar_cols = match scene.live() {
        true => STAT_BAR_CELLS + 1,
        false => 0,
    };
    let del_end = end_x.saturating_sub(bar_cols);
    let add_end = del_end.saturating_sub(del_cols + 1);
    let path_end = add_end.saturating_sub(add_cols + 1);
    let largest = changes
        .iter()
        .map(|change| change.additions + change.deletions)
        .max()
        .unwrap_or(0);
    let current_style = theme.get(s::UI_LIST_CURRENT);
    let text = theme.get(s::UI_TEXT);

    for (index, change) in changes
        .iter()
        .take(usize::from(area.height - 1))
        .enumerate()
    {
        let y = area.y + 1 + index as u16;
        if current.is_some_and(|path| Path::new(path) == change.rel_path) {
            for col in area.x..area.x + area.width {
                buf[(col, y)].set_style(current_style);
            }
        }

        let (kind, kind_scope) = kind_mark(change.kind);
        write_cell(buf, x0, y, kind, theme.get(kind_scope));
        let path = crate::paths::display_relative(&change.rel_path, workspace_root);
        paint_file_path(buf, x0 + 2, path_end, y, &path, muted, text);

        let additions = format!("+{}", change.additions);
        write_right(buf, add_end, y, add_cols, &additions, added);
        let deletions = format!("\u{2212}{}", change.deletions);
        write_right(buf, del_end, y, del_cols, &deletions, deleted);
        if bar_cols > 0 {
            let cells = Rect::new(end_x - STAT_BAR_CELLS, y, STAT_BAR_CELLS, 1);
            stat_bar(cells, change, largest, theme, buf, &mut *scene);
        }
    }
}

/// The letter that marks a file's kind of change, and the diff scope it paints
/// in.
fn kind_mark(kind: CommitFileChangeKind) -> (char, &'static str) {
    use crate::theme::scope as s;
    match kind {
        CommitFileChangeKind::Added => ('A', s::DIFF_ADDED),
        CommitFileChangeKind::Modified => ('M', s::DIFF_MODIFIED),
        CommitFileChangeKind::Deleted => ('D', s::DIFF_DELETED),
        CommitFileChangeKind::Renamed => ('R', s::DIFF_MOVED),
        CommitFileChangeKind::TypeChange => ('T', s::DIFF_MODIFIED),
    }
}

/// Paint `path` in row `y` from column `x` up to `end`, its directory in
/// `dir_style` and its file name in `name_style`.
///
/// A path wider than the room gives up the front of its directory to an
/// ellipsis, so the file name stays whole. A name wider than the room alone is
/// cut at its end.
fn paint_file_path(
    buf: &mut Buffer,
    x: u16,
    end: u16,
    y: u16,
    path: &str,
    dir_style: Style,
    name_style: Style,
) {
    let room = usize::from(end.saturating_sub(x));
    if room == 0 {
        return;
    }
    let (dir, name) = match path.rfind('/') {
        Some(slash) => path.split_at(slash + 1),
        None => ("", path),
    };
    let (dir_cols, name_cols) = (dir.chars().count(), name.chars().count());

    if dir_cols + name_cols <= room {
        write_str(buf, x, y, dir, dir_style);
        write_str(buf, x + dir_cols as u16, y, name, name_style);
        return;
    }
    if name_cols >= room {
        write_str_clipped(buf, x, y, name, name_style, end);
        return;
    }

    let kept = room - 1 - name_cols;
    let tail = dir
        .char_indices()
        .nth(dir_cols - kept)
        .map_or("", |(byte, _)| &dir[byte..]);
    write_cell(buf, x, y, '\u{2026}', dir_style);
    write_str(buf, x + 1, y, tail, dir_style);
    write_str(buf, x + 1 + kept as u16, y, name, name_style);
}

/// Draw a file row's stat bar over `cells`.
///
/// A track fills `ui.divider`, and the added and deleted shares fill over it
/// from the left, each the [`stat_share`] of its count against `largest`, the
/// largest change in the commit.
fn stat_bar(
    cells: Rect,
    change: &CommitFileChange,
    largest: u32,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    use crate::theme::scope as s;
    let span = cells.width * 16;
    let added = stat_share(change.additions, largest, span);
    let deleted = stat_share(change.deletions, largest, span).min(span - added);

    let y = ((16 - STAT_BAR_HEIGHT) / 2) as i16;
    for (x, width, scope) in [
        (0, span, s::UI_DIVIDER),
        (0, added, s::DIFF_ADDED),
        (added, deleted, s::DIFF_DELETED),
    ] {
        let Some(color) = style_rgb(theme.get(scope).fg).filter(|_| width > 0) else {
            continue;
        };
        Bar {
            x: x as i16,
            y,
            width,
            height: STAT_BAR_HEIGHT,
            color,
        }
        .render(cells, buf, scene);
    }
}

/// The sixteenths of a `span` wide stat bar that `count` changed lines fill,
/// rounded, in proportion to `largest`.
///
/// The commit's largest change fills the whole span. A nonzero count keeps at
/// least a sixteenth, so a one-line change still shows beside a large one.
fn stat_share(count: u32, largest: u32, span: u16) -> u16 {
    if count == 0 {
        return 0;
    }
    let scaled =
        (u64::from(count) * u64::from(span) + u64::from(largest) / 2) / u64::from(largest.max(1));
    (scaled.min(u64::from(span)) as u16).max(1)
}

/// Paint the preview's label row into `row`.
///
/// `preview` leads in bold, then `path`, the file the preview stands on, in
/// `ui.text.muted`. The preview's keys close the row at the right, each key in
/// `ui.key_label` and its word in `ui.text.muted`, where they clear the path.
fn render_preview_label(
    path: Option<&str>,
    theme: &crate::theme::Theme,
    row: Rect,
    buf: &mut Buffer,
) {
    use crate::theme::scope as s;
    let end_x = row.x + row.width;
    let muted = theme.get(s::UI_TEXT_MUTED);
    let bold = theme.get(s::UI_TEXT).add_modifier(Modifier::BOLD);
    write_str_clipped(buf, row.x, row.y, "preview", bold, end_x);
    let path = path.unwrap_or_default();
    let path_x = row.x + "preview".len() as u16 + META_GAP;
    write_str_clipped(buf, path_x, row.y, path, muted, end_x);

    let hints_cols: usize = PREVIEW_HINTS
        .iter()
        .map(|(key, word)| key.len() + 1 + word.len())
        .sum::<usize>()
        + HINT_SEPARATOR.chars().count() * (PREVIEW_HINTS.len() - 1);
    let hints_x = end_x.saturating_sub(hints_cols as u16);
    if hints_x <= path_x + path.chars().count() as u16 {
        return;
    }

    let key_style = theme.get(s::UI_KEY_LABEL);
    let mut x = hints_x;
    for (index, (key, word)) in PREVIEW_HINTS.iter().enumerate() {
        if index > 0 {
            write_str(buf, x, row.y, HINT_SEPARATOR, muted);
            x += HINT_SEPARATOR.chars().count() as u16;
        }
        write_str(buf, x, row.y, key, key_style);
        write_str(buf, x + key.len() as u16 + 1, row.y, word, muted);
        x += (key.len() + 1 + word.len()) as u16;
    }
}

/// Paint file `file` of `doc` into `area` from its line `skip` on, one line per
/// row, as the Commits board draws its preview.
///
/// A row holds a lane mark at its left edge in the line's change color, the
/// line's number, and its text. An added line's row takes
/// `vcs.commit.row.added` and a removed line's row `vcs.commit.row.removed`.
///
/// A removed line's text takes `ui.text.dim` with no token colors, since the
/// commit leaves it behind. An added line keeps its token colors at full
/// strength, and the tint behind it marks the change. A context line recedes
/// as the commit picker's context does, so both answer the same dials.
#[allow(clippy::too_many_arguments)]
fn render_file_preview(
    doc: &DiffDocument,
    file: usize,
    skip: usize,
    theme: &crate::theme::Theme,
    area: Rect,
    dials: PreviewDials,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    use crate::theme::scope as s;
    if area.height == 0 || area.width == 0 {
        return;
    }

    // The widest number in the whole file sets the field, so the text column
    // holds still while the preview scrolls.
    let digits = file_lines(doc, file)
        .map(|(_, side)| side.line_num)
        .max()
        .and_then(u32::checked_ilog10)
        .map_or(1, |log| log as u16 + 1);
    let num_x = area.x + 1;
    let text_x = num_x + digits + 1;
    let end_x = area.x + area.width;
    let text_width = usize::from(end_x.saturating_sub(text_x));

    let muted = theme.get(s::UI_TEXT_MUTED);
    let removed_text = theme.get(s::UI_TEXT_DIM);
    let added_bg = theme.get(s::VCS_COMMIT_ROW_ADDED).bg;
    let removed_bg = theme.get(s::VCS_COMMIT_ROW_REMOVED).bg;
    let tints = resolve_diff_tints(theme);
    // line_num is 1-based, and the highlight index is 0-based per line. An off
    // syntax toggle withholds the spans, as it does in the picker's preview.
    let highlights = doc.files[file]
        .buffer_highlights
        .as_deref()
        .filter(|_| dials.syntax);
    let tokens = |side: &ReviewSide| {
        highlights
            .and_then(|lines| lines.get(side.line_num.saturating_sub(1) as usize))
            .map_or(&[][..], Vec::as_slice)
    };
    let number_scale = theme.type_scale(TypeRole::Small);
    let number_color = style_rgb(muted.fg).filter(|_| scene.live());

    let mut numbers = TextRunBatch::default();
    let mut num_text = String::new();

    let lines = file_lines(doc, file)
        .skip(skip)
        .take(usize::from(area.height));
    for (y, (kind, side)) in (area.y..).zip(lines) {
        let row_bg = match kind {
            LineKind::Context => None,
            LineKind::Added => added_bg,
            LineKind::Removed => removed_bg,
        };
        if let Some(bg) = row_bg {
            for x in area.x..end_x {
                buf[(x, y)].set_bg(bg);
            }
        }
        paint_lane(kind, side, theme, Rect::new(area.x, y, 1, 1), buf, scene);

        match number_color {
            Some(color) => {
                num_text.clear();
                write!(num_text, "{}", side.line_num).expect("writing to a String is infallible");
                let advance = text_run::advance_sixteenths(num_text.len(), number_scale);
                numbers.push(
                    Rect::new(num_x, y, 1, 1),
                    TextRun {
                        col: (digits * 16).saturating_sub(advance) as i16,
                        row: 0,
                        scale: number_scale,
                        color,
                        bg: None,
                        text: &num_text,
                        follow: 0,
                        anchor: None,
                    },
                );
            },
            None => render_side_num(
                buf,
                &mut num_text,
                num_x,
                y,
                side.line_num,
                muted,
                usize::from(digits),
            ),
        }

        let (token_spans, fallback, soften_row) = match kind {
            LineKind::Removed => (&[][..], removed_text, None),
            LineKind::Added => (tokens(side), Style::default(), None),
            LineKind::Context => (
                tokens(side),
                Style::default(),
                tints.as_ref().map(|tints| tints.bg),
            ),
        };
        paint_base_row(
            buf,
            text_x,
            y,
            &side.text,
            text_width,
            DEFAULT_TAB_SIZE,
            token_spans,
            fallback,
            &[],
            tints.as_ref(),
            soften_row,
            None,
            dials.paint,
            None,
        );
    }
    numbers.flush(scene);
}

/// Mark the lane cell `cell` of a preview row that shows `side` as a line of
/// `kind`.
///
/// A live scene draws a bar at the cell's left edge, and a plain terminal a
/// left quarter block in the same color. A context line takes no mark. A moved
/// line takes the moved color on either side, as the split diff's gutter does.
fn paint_lane(
    kind: LineKind,
    side: &ReviewSide,
    theme: &crate::theme::Theme,
    cell: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    use crate::theme::scope as s;
    let scope = match kind {
        LineKind::Context => return,
        _ if !side.moved_spans.is_empty() => s::DIFF_MOVED,
        LineKind::Added => s::DIFF_ADDED,
        LineKind::Removed => s::DIFF_DELETED,
    };
    let fg = theme.get(scope).fg;
    match style_rgb(fg).filter(|_| scene.live()) {
        Some(color) => Bar {
            x: 0,
            y: 0,
            width: LANE_BAR_WIDTH,
            height: 16,
            color,
        }
        .render(cell, buf, scene),
        None => {
            let mark = Style {
                fg,
                ..Style::default()
            };
            buf[(cell.x, cell.y)].set_char('\u{258e}').set_style(mark);
        },
    }
}

/// What one row of the commits view's preview shows, which decides its lane
/// mark, its background, and how its text paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineKind {
    /// A line both sides hold, shown as its live line.
    Context,
    /// A base line the commit removed.
    Removed,
    /// A live line the commit added.
    Added,
}

/// The lines of file `file` of `doc`, in the order the commits view's preview
/// shows them.
///
/// Each hunk reads as a unified diff, so a run of changed rows shows all its
/// base lines and then all its live lines. A side-by-side row pairs the two,
/// and one column has room for only one line per row.
fn file_lines(doc: &DiffDocument, file: usize) -> impl Iterator<Item = (LineKind, &ReviewSide)> {
    doc.files[file]
        .chunks
        .iter()
        .filter_map(|chunk_id| doc.chunks.get(chunk_id))
        .flat_map(|chunk| {
            chunk.hunk.rows.chunk_by(|a, b| {
                matches!(a, ReviewRow::Changed { .. }) && matches!(b, ReviewRow::Changed { .. })
            })
        })
        .flat_map(|run| {
            let removed = run.iter().filter_map(|row| match row {
                ReviewRow::Changed { left, .. } => {
                    left.as_ref().map(|side| (LineKind::Removed, side))
                },
                ReviewRow::Context { .. } => None,
            });
            let shown = run.iter().filter_map(|row| match row {
                ReviewRow::Context { right, .. } => Some((LineKind::Context, right)),
                ReviewRow::Changed { right, .. } => {
                    right.as_ref().map(|side| (LineKind::Added, side))
                },
            });
            removed.chain(shown)
        })
}

/// The file of `doc` that holds line `line` of the commits view's preview,
/// and the line's place within that file, or `None` past the last line.
///
/// The preview counts every file's [`file_lines`] end to end, so its one
/// scroll offset names both the file the files table marks and the line the
/// preview starts on.
fn file_at_line(doc: &DiffDocument, line: usize) -> Option<(usize, usize)> {
    let mut start = 0;
    (0..doc.files.len()).find_map(|file| {
        let lines = file_lines(doc, file).count();
        let found = (line < start + lines).then(|| (file, line - start));
        start += lines;
        found
    })
}

/// The repo-relative path of the file the commits view's preview of `doc`
/// shows at scroll offset `scroll`, or `None` past its last line.
pub(crate) fn previewed_file(doc: &DiffDocument, scroll: usize) -> Option<&str> {
    file_at_line(doc, scroll).map(|(file, _)| doc.files[file].rel_path.as_str())
}

/// `scroll` held to the lines of `doc` that a commits view preview `height`
/// rows tall reaches.
///
/// The preview shows one file at a time, so the offset stops at the later of
/// two lines. The first line of the last file that holds any keeps a short
/// last file reachable. The line that fills the last page stops a long one on
/// a full page.
fn clamped_line_scroll(doc: &DiffDocument, scroll: usize, height: u16) -> usize {
    let mut counts = (0..doc.files.len()).map(|file| file_lines(doc, file).count());
    let total: usize = counts.clone().sum();
    let last = counts.rfind(|&lines| lines > 0).unwrap_or(0);
    scroll.min((total - last).max(total.saturating_sub(usize::from(height))))
}

/// Rows [`render_commit_preview`] would paint for `session` in full, counting
/// each chunk's header alongside its diff rows.
///
/// A caller's `skip_rows` is measured in these, so this is what bounds it.
pub(crate) fn preview_row_count(doc: &DiffDocument) -> usize {
    doc.files
        .iter()
        .flat_map(|file| file.chunks.iter())
        .filter_map(|chunk_id| doc.chunks.get(chunk_id))
        .map(|chunk| 1 + chunk.hunk.rows.len())
        .sum()
}

/// The diff view's session dials, resolved for one preview paint.
///
/// A commit preview paints through `:diff-files`'s own painter, so it answers
/// `:diff-files`'s dials rather than carrying a second set. One dial serves
/// both screens, which is what keeps a step on either of them meaning the same
/// thing.
#[derive(Copy, Clone)]
pub(crate) struct PreviewDials {
    paint: DiffDials,
    syntax: bool,
}

impl PreviewDials {
    pub(crate) fn from_stoat(stoat: &crate::app::Stoat) -> Self {
        Self {
            paint: DiffDials::from_stoat(stoat),
            syntax: stoat.diff_syntax,
        }
    }

    /// The values a session starts at with the tint off, for a test of a paint
    /// that the tint dial recolors.
    #[cfg(test)]
    fn untinted() -> Self {
        Self {
            paint: DiffDials::untinted(),
            syntax: true,
        }
    }
}

/// The status color a base-side changed row takes whole under the tint dial.
///
/// A base line is gone from the working tree either way, so moved wins over
/// deleted on the same test `:diff-files`'s own base column uses. The two agree
/// because they read the same rule, not because they were written alike.
fn base_row_tint(side: &ReviewSide, tints: &DiffTints) -> Color {
    match side.moved_spans.is_empty() {
        false => tints.moved,
        true => tints.deleted,
    }
}

/// Whether a changed row's side marks a change or a move on its text.
///
/// The row walk pairs lines by text, so a reformatted line sometimes lands
/// beside a line it shares no change with. Such a side marks nothing, and it
/// paints as context, as `:diff-files` paints the same line as plain text.
pub(crate) fn side_is_marked(side: &ReviewSide) -> bool {
    !side.change_spans.is_empty() || !side.moved_spans.is_empty()
}

/// The status color a live-side changed row takes whole under the tint dial.
///
/// `no_base` marks a row with nothing beside it on the base side, which is an
/// addition rather than a modification. Moved outranks both, matching the
/// status family `:diff-files`'s right column resolves.
fn live_row_tint(side: &ReviewSide, no_base: bool, tints: &DiffTints) -> Color {
    if !side.moved_spans.is_empty() {
        return tints.moved;
    }
    match no_base {
        true => tints.added,
        false => tints.modified,
    }
}

/// One side of a preview row, painted the way `:diff-files`'s base column
/// paints.
///
/// Parity with `:diff-files` comes from calling its painter, not from copying
/// its treatment, so the two surfaces cannot drift.
///
/// `tint_row` is the row's own status color, which the tint dial shifts every
/// char outside a change span toward. `None` leaves the row untinted, which is
/// what a context row takes.
///
/// `spans` is reused across rows rather than allocated per row, and is left
/// sorted for the painter's monotonic cursor.
#[allow(clippy::too_many_arguments)]
fn paint_preview_side(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    side: &ReviewSide,
    width: usize,
    highlights: Option<&BaseHighlights>,
    tints: &DiffTints,
    context: bool,
    dials: PreviewDials,
    tint_row: Option<Color>,
    spans: &mut Vec<(std::ops::Range<usize>, ChangeKind, ProseChange)>,
) {
    spans.clear();
    // A [`ReviewSide`] carries plain ranges, so no span here claims prose. The
    // session serializes its rows, and that format holds no prose state for the
    // preview to read.
    spans.extend(
        side.change_spans
            .iter()
            .map(|range| (range.clone(), ChangeKind::Replaced, ProseChange::None)),
    );
    spans.extend(
        side.moved_spans
            .iter()
            .map(|range| (range.clone(), ChangeKind::Moved, ProseChange::None)),
    );
    spans.sort_by_key(|(range, ..)| range.start);

    // line_num is 1-based, and the highlight index is 0-based per line.
    // An off syntax toggle withholds the spans rather than painting over them,
    // which is how a diff-view editor spends the same toggle.
    let token_spans = highlights
        .filter(|_| dials.syntax)
        .and_then(|lines| lines.get(side.line_num.saturating_sub(1) as usize))
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // A context row recedes whole. A changed row stays at full strength and
    // instead softens only the chars outside its spans, so the refinement
    // leads its own line.
    let (soften_row, soften_gaps) = match context {
        true => (Some(tints.bg), None),
        false => (None, Some(tints.bg)),
    };
    paint_base_row(
        buf,
        x,
        y,
        &side.text,
        width,
        DEFAULT_TAB_SIZE,
        token_spans,
        Style::default(),
        spans,
        Some(tints),
        soften_row,
        soften_gaps,
        dials.paint,
        tint_row,
    );
}
/// Render a compact preview of a [`DiffDocument`]: each chunk's rows
/// painted sequentially with a yellow file/chunk header, top-to-bottom
/// within `area`. Does not rely on editor machinery. The commit picker's
/// preview paints through it.
///
/// `skip_rows` drops that many rows off the top, counting the same rows
/// [`preview_row_count`] does, so a caller scrolls the diff by holding a row
/// offset rather than by owning any of the painting.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_commit_preview(
    doc: &DiffDocument,
    theme: &crate::theme::Theme,
    area: Rect,
    skip_rows: usize,
    dials: PreviewDials,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    use crate::theme::scope as s;
    let dim = theme.get(s::UI_TEXT_MUTED);
    let header_style = theme.get(s::VCS_COMMIT_SHA);
    let del_hl = theme.get(s::DIFF_DELETED);
    let add_hl = theme.get(s::DIFF_ADDED);
    let move_hl = theme.get(s::DIFF_MOVED).add_modifier(Modifier::ITALIC);
    let fallback_style = Style::default();
    // A non-RGB theme cannot blend, so it keeps the fg-span marking below.
    let tints = resolve_diff_tints(theme);

    // One buffer for every number this loop paints, rather than one per row.
    let mut num_text = String::new();
    // Reused across rows the way num_text is, since every row rebuilds it.
    let mut spans: Vec<(std::ops::Range<usize>, ChangeKind, ProseChange)> = Vec::new();

    let DiffColumns {
        left_num_x,
        left_text_x,
        left_content_w,
        right_num_x,
        right_text_x,
        right_content_w,
        sep_x,
        ..
    } = DiffColumns::compute(area, DiffLayout::REVIEW);

    let mut y = area.y;
    let end_y = area.y + area.height;
    // Counts every row the full preview would hold, painted or skipped, so the
    // caller's scroll lands on the same row it would have reached by counting
    // what it can see.
    let mut row = 0usize;

    for file in &doc.files {
        for chunk_id in &file.chunks {
            let Some(chunk) = doc.chunks.get(chunk_id) else {
                continue;
            };

            // A page deep in a long commit would otherwise step every row above
            // it and build a label for each chunk it never paints. The span is
            // the one `preview_row_count` sums, so the caller's row numbering
            // is unchanged, and the per-row check below still carries the one
            // chunk the offset falls inside.
            let chunk_rows = 1 + chunk.hunk.rows.len();
            if row + chunk_rows <= skip_rows {
                row += chunk_rows;
                continue;
            }

            let file_total = file.chunks.len();
            let lang_str = file
                .language
                .as_ref()
                .map(|l| l.name.to_string())
                .unwrap_or_default();
            let label = format!(
                "{} --- {}/{} --- {}",
                file.rel_path,
                chunk.chunk_index_in_file + 1,
                file_total,
                lang_str
            );
            let label_row = row;
            row += 1;
            if label_row >= skip_rows {
                if y >= end_y {
                    return;
                }
                let label_trunc = truncate_to_cols(&label, area.width as usize);
                write_str(buf, area.x, y, &label_trunc, header_style);
                y += 1;
            }

            for diff_row in &chunk.hunk.rows {
                let this_row = row;
                row += 1;
                if this_row < skip_rows {
                    continue;
                }
                if y >= end_y {
                    return;
                }
                if let Some(sep_x) = sep_x
                    && sep_x < area.x + area.width
                {
                    crate::render::chrome::vline(buf, sep_x, y, 1, dim, &mut *scene);
                }
                match diff_row {
                    ReviewRow::Context { left, right } => {
                        render_side_num(buf, &mut num_text, left_num_x, y, left.line_num, dim, 4);
                        render_side_num(buf, &mut num_text, right_num_x, y, right.line_num, dim, 4);
                        match &tints {
                            Some(tints) => {
                                paint_preview_side(
                                    buf,
                                    left_text_x,
                                    y,
                                    left,
                                    left_content_w,
                                    file.base_highlights.as_deref(),
                                    tints,
                                    true,
                                    dials,
                                    None,
                                    &mut spans,
                                );
                                paint_preview_side(
                                    buf,
                                    right_text_x,
                                    y,
                                    right,
                                    right_content_w,
                                    file.buffer_highlights.as_deref(),
                                    tints,
                                    true,
                                    dials,
                                    None,
                                    &mut spans,
                                );
                            },
                            None => {
                                render_side_text(
                                    buf,
                                    left_text_x,
                                    y,
                                    &left.text,
                                    left_content_w,
                                    fallback_style,
                                    &[],
                                    fallback_style,
                                    &[],
                                    move_hl,
                                );
                                render_side_text(
                                    buf,
                                    right_text_x,
                                    y,
                                    &right.text,
                                    right_content_w,
                                    fallback_style,
                                    &[],
                                    fallback_style,
                                    &[],
                                    move_hl,
                                );
                            },
                        }
                    },
                    ReviewRow::Changed { left, right } => {
                        match left {
                            Some(l) => {
                                render_side_num(
                                    buf,
                                    &mut num_text,
                                    left_num_x,
                                    y,
                                    l.line_num,
                                    dim,
                                    4,
                                );
                                match &tints {
                                    Some(tints) => paint_preview_side(
                                        buf,
                                        left_text_x,
                                        y,
                                        l,
                                        left_content_w,
                                        file.base_highlights.as_deref(),
                                        tints,
                                        !side_is_marked(l),
                                        dials,
                                        side_is_marked(l).then(|| base_row_tint(l, tints)),
                                        &mut spans,
                                    ),
                                    None => render_side_text(
                                        buf,
                                        left_text_x,
                                        y,
                                        &l.text,
                                        left_content_w,
                                        fallback_style,
                                        &l.change_spans,
                                        del_hl,
                                        &l.moved_spans,
                                        move_hl,
                                    ),
                                }
                            },
                            None => render_empty_num(buf, left_num_x, y, dim),
                        }
                        match right {
                            Some(r) => {
                                render_side_num(
                                    buf,
                                    &mut num_text,
                                    right_num_x,
                                    y,
                                    r.line_num,
                                    dim,
                                    4,
                                );
                                match &tints {
                                    Some(tints) => paint_preview_side(
                                        buf,
                                        right_text_x,
                                        y,
                                        r,
                                        right_content_w,
                                        file.buffer_highlights.as_deref(),
                                        tints,
                                        !side_is_marked(r),
                                        dials,
                                        side_is_marked(r)
                                            .then(|| live_row_tint(r, left.is_none(), tints)),
                                        &mut spans,
                                    ),
                                    None => render_side_text(
                                        buf,
                                        right_text_x,
                                        y,
                                        &r.text,
                                        right_content_w,
                                        fallback_style,
                                        &r.change_spans,
                                        add_hl,
                                        &r.moved_spans,
                                        move_hl,
                                    ),
                                }
                            },
                            None => render_empty_num(buf, right_num_x, y, dim),
                        }
                    },
                }
                y += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        body_rows, clamped_line_scroll, commit_list_width, commits_detail_rect,
        commits_header_rect, file_at_line, initials, legend_tips, paint_badges, paint_commit_meta,
        paint_file_path, render_commit_preview, render_file_preview, render_preview_label,
        row_badges, stat_share, BadgeRow, BadgeStyles, PreviewDials, RefBadge, RefKind,
        LANE_BAR_WIDTH, MIN_DETAIL_COLUMNS, MIN_LIST_COLUMNS, RING_PERCENT, RULE_STROKE,
        STAT_BAR_CELLS, STAT_BAR_HEIGHT, SWATCH_HEIGHT, SWATCH_WIDTH, TEXT_INSET,
    };
    use crate::{
        app::Stoat,
        commit_graph::GraphRow,
        commit_list::{CommitListState, CommitRefs},
        display_map::highlights::HighlightStyle,
        host::{CommitInfo, GitHost},
        render::{
            commit_picker::tip_colors,
            paint::{blend, dim_rgb, luma, style_rgb},
            review::{DiffDials, CONTEXT_SOFTEN, MODIFIED_ROW_SOFTEN},
        },
        review::{ReviewFileInput, ReviewHunk, ReviewRow, ReviewSide},
        review_session::DiffDocument,
        test_harness::TestHarness,
        theme::{scope, Theme},
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
    };
    use std::{
        collections::HashMap,
        iter,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_language::{Language, LanguageRegistry};
    use stoatty_protocol::command::{self, BorderStyle, Command, PanelCommand, PanelShadow};

    /// The same RGB theme the diff-view tests use, so the washes engage.
    fn rgb_theme() -> Theme {
        let src = r##"theme rgbtest {
            diff.context.fg  = "#808080";
            diff.added.fg    = "#00ff00";
            diff.modified.fg = "#ffff00";
            diff.deleted.fg  = "#ff0000";
            diff.moved.fg    = "#0000ff";
            diff.staged.fg   = "#00ffff";
            diff.unstaged.fg = "#ff00ff";
            diff.approved.fg = "#ff8000";
            diff.unapproved.fg = "#8000ff";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme config parses"), "rgbtest")
            .expect("rgb theme builds")
    }

    const TOKEN_FG: [u8; 3] = [0x11, 0x99, 0x44];
    const BG: [u8; 3] = [0x28, 0x2c, 0x34];

    /// A document over one file, with `highlights` covering the first `lines`
    /// lines of both sides at [`TOKEN_FG`] across the whole line.
    fn session(base: &str, buffer: &str, highlights: bool) -> DiffDocument {
        session_in(None, base, buffer, highlights)
    }

    /// [`session`] diffed under `language`, for a case that needs the tree
    /// differ's token pairs rather than a line diff.
    fn session_in(
        language: Option<Arc<Language>>,
        base: &str,
        buffer: &str,
        highlights: bool,
    ) -> DiffDocument {
        let mut doc = DiffDocument::default();
        doc.add_files(vec![ReviewFileInput {
            path: PathBuf::from("/w/a.rs"),
            rel_path: "a.rs".into(),
            language,
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        }]);
        if highlights {
            let style = HighlightStyle {
                foreground: Some(Color::Rgb(TOKEN_FG[0], TOKEN_FG[1], TOKEN_FG[2])),
                ..HighlightStyle::default()
            };
            let spans_for = |text: &str| -> Arc<crate::diff_map::BaseHighlights> {
                Arc::new(
                    text.lines()
                        .map(|line| vec![(0..line.len(), style.clone())])
                        .collect(),
                )
            };
            let file = &mut doc.files[0];
            file.base_highlights = Some(spans_for(base));
            file.buffer_highlights = Some(spans_for(buffer));
        }
        doc
    }

    /// Render into a wide buffer so both columns fit, and hand back the grid.
    fn rendered(doc: &DiffDocument, theme: &Theme) -> Buffer {
        rendered_with(doc, theme, PreviewDials::untinted())
    }

    /// [`rendered`] at a chosen dial setting, for the cases that step one.
    fn rendered_with(doc: &DiffDocument, theme: &Theme, dials: PreviewDials) -> Buffer {
        let area = Rect::new(0, 0, 120, 20);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        render_commit_preview(doc, theme, area, 0, dials, &mut buf, &mut scene);
        buf
    }

    /// [`rendered_with`] at a chosen `skip_rows`, tall enough that a skipped
    /// page and the full paint overlap for many rows.
    fn rendered_at(doc: &DiffDocument, theme: &Theme, skip_rows: usize) -> Buffer {
        let area = Rect::new(0, 0, 120, 60);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        render_commit_preview(
            doc,
            theme,
            area,
            skip_rows,
            PreviewDials::untinted(),
            &mut buf,
            &mut scene,
        );
        buf
    }

    /// A page deep in a commit paints what the full render holds at that row,
    /// which is what lets the painter seek past whole chunks rather than
    /// stepping every row above the offset.
    #[test]
    fn a_skipped_preview_paints_the_rows_the_full_one_does() {
        let theme = rgb_theme();
        let base: String = (0..12).map(|i| format!("line {i}\n")).collect();
        let buffer: String = (0..12)
            .map(|i| match i {
                0 | 11 => format!("changed {i}\n"),
                _ => format!("line {i}\n"),
            })
            .collect();
        let doc = session(&base, &buffer, false);
        assert!(
            doc.chunks.len() >= 2,
            "the edits at both ends have to land in separate chunks",
        );

        let first_chunk = doc
            .chunks
            .get(&doc.files[0].chunks[0])
            .expect("the first chunk");
        let skip_rows = 1 + first_chunk.hunk.rows.len();

        let full = rendered_at(&doc, &theme, 0);
        let skipped = rendered_at(&doc, &theme, skip_rows);

        let area = *full.area();
        for x in area.x..area.x + area.width {
            assert_eq!(
                skipped[(x, 0)],
                full[(x, skip_rows as u16)],
                "column {x} of the skipped page's first row",
            );
        }
    }

    /// The first cell in `row` whose symbol is `ch`.
    fn cell_with(buf: &Buffer, row: u16, ch: &str) -> ratatui::buffer::Cell {
        let area = *buf.area();
        cell_in(buf, row, ch, area.x..area.x + area.width)
    }

    /// [`cell_with`] bounded to a column range, for a symbol both sides carry.
    fn cell_in(
        buf: &Buffer,
        row: u16,
        ch: &str,
        columns: std::ops::Range<u16>,
    ) -> ratatui::buffer::Cell {
        columns
            .map(|x| buf[(x, row)].clone())
            .find(|cell| cell.symbol() == ch)
            .unwrap_or_else(|| panic!("no {ch:?} cell on row {row}"))
    }

    /// Context recedes so the changed rows around it lead, which is the whole
    /// reason the preview carries syntax colors rather than flat text.
    #[test]
    fn a_context_token_softens_toward_the_background() {
        let session = session("ctx\nold\n", "ctx\nnew\n", true);
        let buf = rendered(&session, &rgb_theme());

        // Row 0 is the file header, so the first content row is 1.
        let cell = cell_with(&buf, 1, "c");
        let softened = dim_rgb(TOKEN_FG, BG, CONTEXT_SOFTEN);
        assert_eq!(
            cell.style().fg,
            Some(Color::Rgb(softened[0], softened[1], softened[2])),
            "the context token carries its syntax color, softened"
        );
    }

    /// A changed span is the one thing on the row that recedes nowhere, and it
    /// carries no color behind it, which is what makes a commit diff read
    /// like `:diff-files`.
    ///
    /// This fixture's token color sits near the background, so the span takes
    /// the contrast lift as well. What it keeps is the theme's own color,
    /// standing further off the background than the theme wrote it rather than
    /// taking any color of the diff's.
    #[test]
    fn a_changed_span_keeps_its_token_color_and_takes_no_wash() {
        let session = session("ctx\nold\n", "ctx\nnew\n", true);
        let buf = rendered(&session, &rgb_theme());

        let cell = cell_with(&buf, 2, "n");
        assert_eq!(
            (cell.style().bg, cell.modifier),
            (Some(Color::Reset), Modifier::empty()),
            "the changed token takes no wash and no underline"
        );

        let Some(Color::Rgb(r, g, b)) = cell.style().fg else {
            panic!(
                "the changed token paints an rgb color, got {:?}",
                cell.style().fg
            );
        };
        let context = dim_rgb(TOKEN_FG, BG, CONTEXT_SOFTEN);
        assert!(
            luma([r, g, b]) > luma(TOKEN_FG) && luma(TOKEN_FG) > luma(context),
            "it stands above the color the theme wrote, which already stands above \
             the receded context: {:?} over {TOKEN_FG:?} over {context:?}",
            [r, g, b],
        );
    }

    /// A call reformatted across lines puts its old line beside the new first
    /// line by position, and the tree differ marks neither. Both sides of that
    /// row recede like context, so only the new argument leads.
    #[test]
    fn a_paired_side_without_spans_paints_as_context() {
        let rust = LanguageRegistry::standard().for_path(Path::new("/w/a.rs"));
        let base = "fn f() {\n    foo(bar, baz);\n}\n";
        let buffer = "fn f() {\n    foo(\n        bar,\n        bang,\n        baz,\n    );\n}\n";
        let buf = rendered(&session_in(rust, base, buffer, true), &rgb_theme());
        let half = |row: u16, columns: std::ops::Range<u16>| -> String {
            columns.map(|x| buf[(x, row)].symbol()).collect()
        };
        let row_with = |text: &str, columns: std::ops::Range<u16>| {
            (0..buf.area().height)
                .find(|&row| half(row, columns.clone()).contains(text))
                .unwrap_or_else(|| panic!("no row paints {text:?}"))
        };
        let paired = row_with("foo(bar, baz);", 0..60);
        let added = row_with("bang,", 60..120);

        let changed = {
            let canonical = session("ctx\nold\n", "ctx\nnew\n", true);
            cell_with(&rendered(&canonical, &rgb_theme()), 2, "n").style()
        };
        let context = dim_rgb(TOKEN_FG, BG, CONTEXT_SOFTEN);
        let context = Some(Color::Rgb(context[0], context[1], context[2]));
        assert_eq!(
            (
                half(paired, 60..120).trim_end().ends_with("foo("),
                cell_in(&buf, paired, "f", 0..60).style().fg,
                cell_in(&buf, paired, "f", 60..120).style().fg,
                cell_in(&buf, added, "g", 60..120).style(),
            ),
            (true, context, context, changed),
            "the old call sits beside foo( and both recede, while bang paints as a change",
        );
    }

    /// The preview marks a change by holding it at full strength while its
    /// neighbors recede, so a file the build never attached highlights to has
    /// nothing to mark with and paints flat.
    #[test]
    fn a_file_without_highlights_paints_no_marking_at_all() {
        let session = session("ctx\nold\n", "ctx\nnew\n", false);
        let buf = rendered(&session, &rgb_theme());

        let changed = cell_with(&buf, 2, "n");
        let gap = cell_with(&buf, 1, "c");
        assert_eq!(
            (changed.style().fg, gap.style().fg),
            (Some(Color::Reset), Some(Color::Reset)),
            "no color reaches the changed span or its receding neighbor"
        );
    }

    /// A theme whose diff colors are not RGB cannot blend, so the preview keeps
    /// marking changes by foreground exactly as it did before.
    #[test]
    fn a_non_rgb_theme_keeps_the_foreground_span_marking() {
        let session = session("ctx\nold\n", "ctx\nnew\n", true);
        let theme = Theme::empty();
        let buf = rendered(&session, &theme);

        let cell = cell_with(&buf, 2, "n");
        assert!(
            !matches!(cell.style().bg, Some(Color::Rgb(..))),
            "nothing washes behind the span: {:?}",
            cell.style().bg
        );
        assert_ne!(
            cell.style().fg,
            Some(Color::Rgb(TOKEN_FG[0], TOKEN_FG[1], TOKEN_FG[2])),
            "and the token color never reaches the cell, so the marking is the theme's"
        );
    }

    /// An unchanged char on a row the refinement split softens by the lighter
    /// amount, so the changed chars lead their own line without the row
    /// receding like context.
    #[test]
    fn an_unchanged_char_on_a_refined_row_softens_lightly() {
        let session = session("ctx\nfoo_a\n", "ctx\nfoo_b\n", true);
        let buf = rendered(&session, &rgb_theme());

        let cell = cell_with(&buf, 2, "f");
        let softened = dim_rgb(TOKEN_FG, BG, MODIFIED_ROW_SOFTEN);
        assert_eq!(
            cell.style().fg,
            Some(Color::Rgb(softened[0], softened[1], softened[2])),
            "the unchanged prefix softens lightly, not to context strength"
        );
    }

    /// The tint dial's other half. A context row keeps its syntax color at
    /// rest and drains toward its own luma gray as the dial rises, so the
    /// changed rows gaining status color lead against a graying field.
    #[test]
    fn a_stepped_tint_drains_the_color_out_of_a_context_token() {
        let session = session("ctx\nold\n", "ctx\nnew\n", true);
        let dials = PreviewDials {
            paint: DiffDials {
                tint_amount: 0.5,
                ..DiffDials::untinted()
            },
            ..PreviewDials::untinted()
        };
        let buf = rendered_with(&session, &rgb_theme(), dials);

        let softened = dim_rgb(TOKEN_FG, BG, CONTEXT_SOFTEN);
        let gray = [(luma(softened) * 255.0).round() as u8; 3];
        let drained = dim_rgb(softened, gray, 0.5);
        assert_eq!(
            cell_with(&buf, 1, "c").style().fg,
            Some(Color::Rgb(drained[0], drained[1], drained[2])),
            "the context token softens first, then drains half its color"
        );
    }

    /// The soften dial scales how far context recedes, so a step up puts the
    /// context further behind the changed rows than the shipped setting does.
    #[test]
    fn a_stepped_soften_recedes_a_context_token_further() {
        let session = session("ctx\nold\n", "ctx\nnew\n", true);
        let dials = PreviewDials {
            paint: DiffDials {
                soften_scale: 2.0,
                ..DiffDials::untinted()
            },
            ..PreviewDials::untinted()
        };
        let buf = rendered_with(&session, &rgb_theme(), dials);

        let deeper = dim_rgb(TOKEN_FG, BG, CONTEXT_SOFTEN * 2.0);
        assert_eq!(
            cell_with(&buf, 1, "c").style().fg,
            Some(Color::Rgb(deeper[0], deeper[1], deeper[2])),
            "the context token recedes by twice the shipped fraction"
        );
    }

    /// The syntax toggle withholds the token spans rather than painting over
    /// them, so a row falls back to the diff's own styling with no syntax
    /// color left to soften.
    #[test]
    fn the_syntax_toggle_off_paints_no_token_color() {
        let session = session("ctx\nold\n", "ctx\nnew\n", true);
        let theme = rgb_theme();
        let off = PreviewDials {
            syntax: false,
            ..PreviewDials::untinted()
        };

        let softened = dim_rgb(TOKEN_FG, BG, CONTEXT_SOFTEN);
        assert_eq!(
            (
                cell_with(&rendered(&session, &theme), 1, "c").style().fg,
                cell_with(&rendered_with(&session, &theme, off), 1, "c")
                    .style()
                    .fg,
            ),
            (
                Some(Color::Rgb(softened[0], softened[1], softened[2])),
                Some(Color::Reset),
            ),
            "the toggle trades the softened token color for the diff's own \
             fallback"
        );
    }

    /// The tint dial takes a changed row's chars outside its refinement whole.
    /// Each side reads its own status family, so the base side lands on
    /// deleted and a live side with a base beside it lands on modified.
    #[test]
    fn a_full_tint_takes_each_changed_side_to_its_own_status_color() {
        let session = session("ctx\nold tail\n", "ctx\nnew tail\n", true);
        let dials = PreviewDials {
            paint: DiffDials {
                tint_amount: 1.0,
                ..DiffDials::untinted()
            },
            ..PreviewDials::untinted()
        };
        let buf = rendered_with(&session, &rgb_theme(), dials);

        let gap = |status: [u8; 3]| {
            let softened = dim_rgb(status, BG, MODIFIED_ROW_SOFTEN);
            Some(Color::Rgb(softened[0], softened[1], softened[2]))
        };
        // The shared " tail" sits outside the refinement, so it is what the
        // row tint reaches. `i` appears nowhere else on the row, which keeps
        // the search off the refined "old"/"new". One column each side of the
        // 120-wide buffer.
        assert_eq!(
            (
                cell_in(&buf, 2, "i", 0..60).style().fg,
                cell_in(&buf, 2, "i", 60..120).style().fg,
            ),
            (gap([0xff, 0x00, 0x00]), gap([0xff, 0xff, 0x00])),
            "the base side takes deleted whole and the live side modified"
        );
    }

    #[test]
    fn a_body_reflows_and_a_cut_one_ends_in_an_ellipsis() {
        assert_eq!(
            [
                body_rows("one two\nthree", 9),
                body_rows("a b c d e f g h i j", 3),
            ],
            [
                vec!["one two", "three"],
                vec!["a b", "c d", "e f", "g\u{2026}"],
            ],
        );
    }

    /// The legend takes the topmost tip of each lane, lanes in order, and four
    /// lanes at most.
    #[test]
    fn the_legend_names_the_topmost_tip_of_each_lane_in_lane_order() {
        let h = Stoat::test();
        h.fake_git().add_repo("/repo");
        let repo = h.fake_git.discover(Path::new("/repo")).expect("the repo");
        let mut state = CommitListState::new(PathBuf::from("/repo"), repo, 0);
        let lanes = [3, 1, 4, 1, 0, 2, 0];
        let sha = |row: usize| format!("c{row}");
        state.commits = (0..lanes.len())
            .map(|row| CommitInfo {
                sha: sha(row),
                short_sha: sha(row),
                summary: String::new(),
                author_name: String::new(),
                author_email: String::new(),
                time: 0,
                parents: Vec::new(),
            })
            .collect();
        let rows = lanes.map(|node_lane| GraphRow {
            node_lane,
            edges: Vec::new(),
        });
        state.graph = (rows.to_vec(), 5);
        state.refs.local = (0..6)
            .map(|row| (sha(row), vec![format!("tip{row}")]))
            .collect();

        assert_eq!(
            legend_tips(&state),
            [(0, "tip4"), (1, "tip1"), (2, "tip5"), (3, "tip0")],
        );
    }

    /// A legend entry wider than the room left in the list stays off the header
    /// row, so the separator beside the list keeps its cell.
    #[test]
    fn a_legend_entry_past_the_list_stays_off_the_header() {
        let mut h = Stoat::test();
        h.resize(40, 8);
        h.stoat.stoatty = false;
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("a1000000", "feat: root", &[])
            .branch("a-branch-wider-than-the-list", "a1000000");
        h.open_commits("/repo");

        let text = h.rendered_text();
        let header: String = text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(23)
            .collect();
        assert_eq!(header, format!("{:<22}\u{2502}", " graph"));
    }

    /// Inside stoatty each legend swatch is a bar in its lane's tip color, set
    /// in its cell, and a hairline in surface.raised underlines the header.
    #[test]
    fn the_graph_header_draws_its_swatches_and_rule_as_bars() {
        let mut h = Stoat::test();
        h.resize(120, 12);
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("a1000000", "feat: root", &[])
            .commit_with_parent_message("b2000000", "a1000000", "feat: side", &[])
            .commit_with_parent_message("c3000000", "a1000000", "feat: main", &[])
            .merge_commit("d4000000", "b2000000", &[])
            .branch("side", "b2000000")
            .branch("main", "d4000000");
        h.open_commits("/repo");
        h.render_composited();

        let theme = &h.stoat.theme;
        let tip = tip_colors(theme, 2);
        let raised = style_rgb(theme.get(scope::UI_SURFACE_RAISED).bg).expect("an RGB surface");
        let bars: Vec<(i16, i16, u16, u16, [u8; 3])> =
            command::decode_stream(&h.stoat.apc_scene.all_bytes())
                .into_iter()
                .filter_map(|cmd| match cmd {
                    Command::Bar(bar)
                        if bar.height == SWATCH_HEIGHT
                            || (bar.height == RULE_STROKE && bar.color == raised) =>
                    {
                        Some((bar.x, bar.y, bar.width, bar.height, bar.color))
                    },
                    _ => None,
                })
                .collect();

        let pane = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let header = commits_header_rect(pane, None).expect("a header row");
        let text = h.rendered_text();
        let swatch = |name: &str, lane: u16| {
            let col = cell_of(&text, name).0 - 1;
            let x = (col * 16 + (16 - SWATCH_WIDTH) / 2) as i16;
            let y = (header.y * 16 + (16 - SWATCH_HEIGHT) / 2) as i16;
            (x, y, SWATCH_WIDTH, SWATCH_HEIGHT, tip(lane))
        };
        let rule = (
            (header.x * 16) as i16,
            (header.y * 16 + 16 - RULE_STROKE) as i16,
            header.width * 16,
            RULE_STROKE,
            raised,
        );
        assert_eq!(bars, [swatch("main", 0), swatch("side", 1), rule]);
    }

    #[test]
    fn a_stat_share_scales_to_the_largest_change_and_keeps_a_sixteenth() {
        assert_eq!(
            [(0, 4), (4, 4), (1, 4), (1, 300)]
                .map(|(count, largest)| stat_share(count, largest, 112)),
            [0, 112, 28, 1],
        );
    }

    #[test]
    fn a_long_path_gives_up_the_front_of_its_directory() {
        let paint = |width: u16| {
            let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
            let path = "stoat/src/render/commits.rs";
            paint_file_path(&mut buf, 0, width, 0, path, Style::new(), Style::new());
            row_text(&buf)
        };
        assert_eq!(
            [paint(30), paint(12), paint(8)],
            [
                "stoat/src/render/commits.rs",
                "\u{2026}/commits.rs",
                "commits."
            ],
        );
    }

    #[test]
    fn the_label_drops_its_keys_where_they_would_cover_the_path() {
        let label = |width: u16| {
            let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
            let row = Rect::new(0, 0, width, 1);
            render_preview_label(Some("src/a.rs"), &Theme::empty(), row, &mut buf);
            row_text(&buf)
        };
        assert_eq!(
            [label(50), label(30)],
            [
                format!(
                    "{:<21}C-d/C-u scroll \u{b7} Enter review",
                    "preview src/a.rs"
                ),
                "preview src/a.rs".to_string(),
            ],
        );
    }

    /// The first row of `buf` as text, its trailing blanks trimmed.
    fn row_text(buf: &Buffer) -> String {
        let row: String = (0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect();
        row.trim_end().to_string()
    }

    /// A document over one file per `(name, base, buffer)` triple, in order.
    fn doc_of(files: &[(&str, &str, &str)]) -> DiffDocument {
        let mut doc = DiffDocument::default();
        doc.add_files(
            files
                .iter()
                .map(|(name, base, buffer)| ReviewFileInput {
                    path: PathBuf::from(format!("/w/{name}")),
                    rel_path: (*name).into(),
                    language: None,
                    base_text: Arc::new(base.to_string()),
                    buffer_text: Arc::new(buffer.to_string()),
                })
                .collect(),
        );
        doc
    }

    /// `n` lines a file adds whole, each its own text.
    fn added_lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}\n")).collect()
    }

    /// Each file's lines continue the count after the file before it, so one
    /// offset names a file and the line the preview starts on within it.
    #[test]
    fn each_preview_line_belongs_to_the_file_that_holds_it() {
        let doc = doc_of(&[("a.rs", "x\n", "y\nz\n"), ("b.rs", "x\n", "y\nz\n")]);

        assert_eq!(
            [0, 2, 3, 5, 6].map(|line| file_at_line(&doc, line)),
            [Some((0, 0)), Some((0, 2)), Some((1, 0)), Some((1, 2)), None],
        );
    }

    /// A short last file stays reachable from its first line, and a long one
    /// stops on its last full page.
    #[test]
    fn the_preview_scroll_stops_on_the_last_file_s_top_or_its_last_page() {
        let (three, twenty) = (added_lines(3), added_lines(20));
        let short = doc_of(&[("a.rs", "", &three), ("b.rs", "", &three)]);
        let long = doc_of(&[("a.rs", "", &three), ("b.rs", "", &twenty)]);

        assert_eq!(
            [
                clamped_line_scroll(&short, 100, 10),
                clamped_line_scroll(&long, 100, 10),
                clamped_line_scroll(&long, 5, 10),
            ],
            [3, 13, 5],
        );
    }

    /// A file's preview reads as a unified diff in one column. A changed run
    /// shows its removed lines before its added ones, each row tinted whole,
    /// with a lane bar and a number right-aligned in a field as wide as the
    /// file's largest.
    ///
    /// A context line's token recedes, a removed line takes the dim text color,
    /// and an added line keeps its token color.
    #[test]
    fn the_preview_paints_one_file_as_a_unified_diff() {
        let base: String = (1..=12).map(|n| format!("l{n}\n")).collect();
        let buffer = base.replace("l10\nl11\n", "m10\nm11\n");
        let doc = session(&base, &buffer, true);
        let theme = row_theme();
        let rgb = |scope: &str, bg: bool| {
            let style = theme.get(scope);
            style_rgb(if bg { style.bg } else { style.fg }).expect("an RGB color")
        };
        let (removed, added) = (
            Some(rgb(scope::VCS_COMMIT_ROW_REMOVED, true)),
            Some(rgb(scope::VCS_COMMIT_ROW_ADDED, true)),
        );
        let (red, green, muted) = (
            rgb(scope::DIFF_DELETED, false),
            rgb(scope::DIFF_ADDED, false),
            rgb(scope::UI_TEXT_MUTED, false),
        );

        let (buf, commands) = painted_file(&doc, &theme, true);
        let bars: Vec<_> = commands
            .iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(bar) => Some((bar.x, bar.y, bar.width, bar.height, bar.color)),
                _ => None,
            })
            .collect();
        let numbers: Vec<_> = commands
            .iter()
            .filter_map(|cmd| match cmd {
                Command::TextRun(run) => Some((run.col, run.row, run.color, run.text.as_str())),
                _ => None,
            })
            .collect();
        let row_bg = |y: u16| [0, 19].map(|x| style_rgb(Some(buf[(x, y)].bg)));

        assert_eq!(
            (
                (0..8).map(|y| row_text_at(&buf, y)).collect::<Vec<_>>(),
                (0..8).map(row_bg).collect::<Vec<_>>(),
                [0, 3, 5].map(|y| style_rgb(Some(buf[(4, y)].fg))),
            ),
            (
                [
                    "    l7", "    l8", "    l9", "    l10", "    l11", "    m10", "    m11",
                    "    l12"
                ]
                .map(String::from)
                .to_vec(),
                [None, None, None, removed, removed, added, added, None]
                    .map(|bg| [bg; 2])
                    .to_vec(),
                [
                    Some(dim_rgb(TOKEN_FG, BG, CONTEXT_SOFTEN)),
                    Some(rgb(scope::UI_TEXT_DIM, false)),
                    Some(TOKEN_FG),
                ],
            ),
            "rows, their tints, and the text of a context, a removed, and an added line",
        );
        assert_eq!(
            bars,
            [3, 4, 5, 6].map(|row: i16| {
                let color = if row < 5 { red } else { green };
                (0, row * 16, LANE_BAR_WIDTH, 16, color)
            }),
        );
        assert_eq!(
            numbers,
            [
                (32, 0, muted, "7"),
                (32, 16, muted, "8"),
                (32, 32, muted, "9"),
                (16, 48, muted, "10"),
                (16, 64, muted, "11"),
                (16, 80, muted, "10"),
                (16, 96, muted, "11"),
                (16, 112, muted, "12"),
            ],
        );
    }

    /// A plain terminal marks the lane with a quarter block in the lane's color
    /// and writes the numbers into cells.
    #[test]
    fn a_plain_terminal_marks_the_lane_and_numbers_in_cells() {
        let base: String = (1..=12).map(|n| format!("l{n}\n")).collect();
        let buffer = base.replace("l11\n", "m11\n");
        let doc = doc_of(&[("a.rs", &base, &buffer)]);
        let theme = row_theme();

        let (buf, commands) = painted_file(&doc, &theme, false);
        let lane_fg = |y: u16| buf[(0, y)].fg;

        assert_eq!(
            (
                (0..6).map(|y| row_text_at(&buf, y)).collect::<Vec<_>>(),
                [3, 4].map(lane_fg),
                commands.len(),
            ),
            (
                [
                    "  8 l8",
                    "  9 l9",
                    " 10 l10",
                    "\u{258e}11 l11",
                    "\u{258e}11 m11",
                    " 12 l12",
                ]
                .map(String::from)
                .to_vec(),
                [
                    theme.get(scope::DIFF_DELETED).fg.expect("a deleted color"),
                    theme.get(scope::DIFF_ADDED).fg.expect("an added color"),
                ],
                0,
            ),
        );
    }

    /// A line that moved takes the moved lane where it left and where it
    /// arrived, rather than the removed or added lane.
    #[test]
    fn a_moved_line_takes_the_moved_lane_on_either_side() {
        let moved = |line_num| ReviewSide {
            text: "moved".into(),
            line_num,
            change_spans: Vec::new(),
            moved_spans: iter::once(0..5).collect(),
            move_provenance: None,
        };
        let mut doc = DiffDocument::default();
        doc.add_files_with_hunks(
            vec![ReviewFileInput {
                path: PathBuf::from("/w/a.rs"),
                rel_path: "a.rs".into(),
                language: None,
                base_text: Arc::new("moved\n".to_string()),
                buffer_text: Arc::new("moved\n".to_string()),
            }],
            vec![vec![ReviewHunk {
                rows: vec![
                    ReviewRow::Changed {
                        left: Some(moved(1)),
                        right: None,
                    },
                    ReviewRow::Changed {
                        left: None,
                        right: Some(moved(9)),
                    },
                ],
            }]],
        );
        let theme = row_theme();

        let (_, commands) = painted_file(&doc, &theme, true);
        let lanes: Vec<_> = commands
            .iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(bar) => Some((bar.y, bar.color)),
                _ => None,
            })
            .collect();

        let blue = style_rgb(theme.get(scope::DIFF_MOVED).fg).expect("an RGB moved color");
        assert_eq!(lanes, [(0, blue), (16, blue)]);
    }

    /// A theme with RGB lane, number, and row colors for the one-file preview.
    fn row_theme() -> Theme {
        let src = r##"theme rows {
            diff.added.fg    = "#00ff00";
            diff.deleted.fg  = "#ff0000";
            diff.moved.fg    = "#0000ff";
            ui.text.muted.fg = "#808080";
            ui.text.dim.fg   = "#606060";
            ui.background.bg = "#282c34";
            vcs.commit.row.added.bg   = "#18241b";
            vcs.commit.row.removed.bg = "#2a1b1d";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme config parses"), "rows").expect("row theme builds")
    }

    /// The first file of `doc` painted from its first line into a 20 by 8
    /// area, under a live or a dead scene, with the scene's commands.
    fn painted_file(doc: &DiffDocument, theme: &Theme, live: bool) -> (Buffer, Vec<Command>) {
        let area = Rect::new(0, 0, 20, 8);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_live(live);
        render_file_preview(
            doc,
            0,
            0,
            theme,
            area,
            PreviewDials::untinted(),
            &mut buf,
            &mut scene,
        );
        (buf, command::decode_stream(&scene.all_bytes()))
    }

    /// Row `y` of `buf` as text, its trailing blanks trimmed.
    fn row_text_at(buf: &Buffer, y: u16) -> String {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        row.trim_end().to_string()
    }

    /// The files table marks the file the preview stands on and the label names
    /// it, so a scroll into the next file moves both.
    #[test]
    fn the_previewed_file_follows_the_preview_scroll() {
        let mut h = Stoat::test();
        h.resize(120, 16);
        let lines = |tag: &str| -> String { (0..10).map(|n| format!("{tag} {n}\n")).collect() };
        let (a, b) = (lines("a"), lines("b"));
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message(
                "a1000000",
                "feat: root",
                &[("src/a.rs", ""), ("src/b.rs", "")],
            )
            .commit_with_parent_message(
                "b2000000",
                "a1000000",
                "feat: grow",
                &[("src/a.rs", &a), ("src/b.rs", &b)],
            );
        h.open_commits("/repo");

        let marks = |h: &mut TestHarness| {
            h.snapshot();
            let (buf, text) = (h.rendered_buffer(), h.rendered_text());
            let (col, row) = cell_of(&text, "src/a.rs");
            let raised = h.stoat.theme.get(scope::UI_LIST_CURRENT).bg;
            let current = [row, row + 1].map(|y| buf[(col, y)].bg == raised.unwrap_or_default());
            let label = text
                .lines()
                .find(|line| line.contains("C-d/C-u"))
                .and_then(|line| line.split('\u{2502}').nth(1))
                .and_then(|label| label.split_whitespace().nth(1))
                .unwrap_or_default()
                .to_string();
            (current, label)
        };
        let top = marks(&mut h);
        if let Some(list) = h.stoat.active_workspace_mut().focused_commits_mut() {
            list.preview_scroll = 10;
        }
        let scrolled = marks(&mut h);

        assert_eq!(
            [top, scrolled],
            [
                ([true, false], "src/a.rs".to_string()),
                ([false, true], "src/b.rs".to_string()),
            ],
        );
    }

    /// Inside stoatty a file row ends in a track, its added and deleted shares
    /// over it in proportion to the commit's largest change.
    #[test]
    fn a_file_rows_stat_bar_fills_its_shares_over_the_track() {
        let mut h = Stoat::test();
        h.resize(120, 16);
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message(
                "a1000000",
                "feat: root",
                &[("src/a.rs", ""), ("src/b.rs", "x\n")],
            )
            .commit_with_parent_message(
                "b2000000",
                "a1000000",
                "feat: grow",
                &[("src/a.rs", "1\n2\n3\n4\n"), ("src/b.rs", "y\n")],
            );
        h.open_commits("/repo");
        h.render_composited();

        let theme = &h.stoat.theme;
        let rgb = |name: &str| style_rgb(theme.get(name).fg).expect("an RGB scope");
        let kinds = [
            (rgb(scope::UI_DIVIDER), "track"),
            (rgb(scope::DIFF_ADDED), "added"),
            (rgb(scope::DIFF_DELETED), "deleted"),
        ];
        let bars: Vec<(&str, i16, i16, u16)> =
            command::decode_stream(&h.stoat.apc_scene.all_bytes())
                .into_iter()
                .filter_map(|cmd| match cmd {
                    Command::Bar(bar) if bar.height == STAT_BAR_HEIGHT => kinds
                        .iter()
                        .find(|(color, _)| *color == bar.color)
                        .map(|(_, kind)| (*kind, bar.x, bar.y, bar.width)),
                    _ => None,
                })
                .collect();

        let row = cell_of(&h.rendered_text(), "src/a.rs").1 as i16;
        let pane = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let detail = commits_detail_rect(pane, None).expect("a detail pane");
        let (x, a, b) = (
            ((detail.x + detail.width - TEXT_INSET - STAT_BAR_CELLS) * 16) as i16,
            row * 16 + 6,
            row * 16 + 22,
        );
        assert_eq!(
            bars,
            [
                ("track", x, a, 112),
                ("added", x, a, 112),
                ("track", x, b, 112),
                ("added", x, b, 28),
                ("deleted", x + 28, b, 28),
            ],
            "the largest change fills its track, and a smaller one fills its share",
        );
    }

    #[test]
    fn initials_take_the_outer_words_or_a_lone_words_first_two_letters() {
        assert_eq!(
            [
                "Ada Lovelace",
                "Grace Brewster Hopper",
                "Lee",
                "x",
                "  ",
                "\u{e9}mile zola"
            ]
            .map(initials),
            ["AL", "GH", "LE", "X", "", "\u{c9}Z"],
        );
    }

    /// HEAD's badge leads its row and names its branch, which then takes no
    /// badge of its own. Local branches, remote ones, and tags follow in that
    /// order, and a detached HEAD leaves every branch its badge.
    #[test]
    fn a_rows_badges_lead_with_head_and_skip_the_branch_it_names() {
        let names =
            |list: &[&str]| -> Vec<String> { list.iter().map(|name| name.to_string()).collect() };
        let refs = CommitRefs {
            head: Some("a".into()),
            head_branch: Some("main".into()),
            local: HashMap::from([
                ("a".into(), names(&["feature", "main"])),
                ("b".into(), names(&["side"])),
            ]),
            remote: HashMap::from([("a".into(), names(&["origin/main"]))]),
            tags: HashMap::from([("a".into(), names(&["v1"]))]),
        };
        let detached = CommitRefs {
            head_branch: None,
            ..refs.clone()
        };
        let labels = |refs: &CommitRefs, sha: &str| -> Vec<String> {
            row_badges(refs, sha)
                .map(|badge| format!("{}{}", badge.prefix, badge.name))
                .collect()
        };

        assert_eq!(
            [
                labels(&refs, "a"),
                labels(&refs, "b"),
                labels(&detached, "a")
            ],
            [
                vec![
                    "HEAD \u{2192} main",
                    "feature",
                    "origin/main",
                    "\u{25c6} v1"
                ],
                vec!["side"],
                vec!["HEAD", "feature", "main", "origin/main", "\u{25c6} v1"],
            ],
        );
    }

    /// Inside stoatty a remote or a tag badge frames its cells in hairline bars
    /// a sixteenth inside its row.
    #[test]
    fn remote_and_tag_badges_ring_themselves_in_hairline_bars() {
        let mut h = Stoat::test();
        h.resize(160, 12);
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("a1000000", "feat: root", &[])
            .commit_with_parent_message("b2000000", "a1000000", "feat: next", &[])
            .commit_with_parent_message("c3000000", "b2000000", "feat: last", &[])
            .tag("v1.0", "a1000000")
            .remote_branch("origin/main", "b2000000");
        h.open_commits("/repo");
        h.render_composited();

        let theme = &h.stoat.theme;
        let ground = style_rgb(theme.get(scope::UI_BACKGROUND).bg).expect("an RGB ground");
        let ring_of = |name: &str| {
            let fg = style_rgb(theme.get(name).fg).expect("an RGB badge");
            blend(ground, fg, RING_PERCENT)
        };
        let commands = command::decode_stream(&h.stoat.apc_scene.all_bytes());
        let bars_in = |color: [u8; 3]| -> Vec<(i16, i16, u16, u16)> {
            commands
                .iter()
                .filter_map(|cmd| match cmd {
                    Command::Bar(bar) if bar.color == color => {
                        Some((bar.x, bar.y, bar.width, bar.height))
                    },
                    _ => None,
                })
                .collect()
        };
        let frame_of = |name: &str, lead: u16| {
            let (col, row) = cell_of(&h.rendered_text(), name);
            let cells = name.chars().count() as u16 + lead + 2;
            let (x, y, span) = (
                ((col - lead - 1) * 16) as i16,
                (row * 16) as i16,
                cells * 16,
            );
            vec![
                (x, y + 1, span, 1),
                (x, y + 14, span, 1),
                (x, y + 1, 1, 14),
                (x + span as i16 - 1, y + 1, 1, 14),
            ]
        };
        assert_eq!(
            [
                bars_in(ring_of(scope::VCS_REF_REMOTE)),
                bars_in(ring_of(scope::VCS_REF_TAG)),
            ],
            [frame_of("origin/main", 0), frame_of("v1.0", 2)],
            "four hairlines around each ringed badge, inset from the row's top and bottom",
        );
    }

    /// A filled badge's box is a chip panel over its cells, which keep the
    /// row's own background. A pool page, which captures no panel, paints the
    /// box into the cells.
    #[test]
    fn a_filled_badge_box_is_a_chip_panel_or_its_cells() {
        let theme = chip_theme(r##"vcs.ref.head = { fg: "#010203", bg: "#040506" };"##);
        let styles = BadgeStyles::new(&theme);
        let head = RefBadge {
            kind: RefKind::Head,
            prefix: "HEAD",
            name: "",
        };
        let paint = |panels| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 1));
            let mut scene = stoat_widgets::ApcScene::new();
            let row = BadgeRow {
                x: 2,
                end: 40,
                y: 0,
                lane_color: [0, 0, 0],
                panels,
            };
            paint_badges([head].into_iter(), row, &styles, &mut buf, &mut scene);
            let cells: Vec<Color> = (2..8).map(|x| buf[(x, 0)].bg).collect();
            (cells, command::decode_stream(scene.bytes()))
        };

        assert_eq!(
            [paint(true), paint(false)],
            [
                (vec![Color::Reset; 6], vec![chip_panel(2, 6, [4, 5, 6])]),
                (vec![Color::Rgb(4, 5, 6); 6], Vec::new()),
            ],
        );
    }

    /// Each parent chip's box is a chip panel in its parent's color over its
    /// cells, which keep the row's own background.
    ///
    /// The row reads `m A just now │ parents`, so the chips `1 · p` and `2 · q`
    /// start at columns 23 and 31, seven cells each.
    #[test]
    fn each_parent_chip_draws_its_box_as_a_chip_panel() {
        let theme = chip_theme(
            r##"vcs.commit.parent = { fg: "#010203", bg: "#040506" };
                vcs.commit.parent.merged = { fg: "#070809", bg: "#0a0b0c" };"##,
        );
        let commit = CommitInfo {
            sha: "m".into(),
            short_sha: "m".into(),
            summary: String::new(),
            author_name: "A".into(),
            author_email: String::new(),
            time: 0,
            parents: vec!["p".into(), "q".into()],
        };
        let row = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(row);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_commit_meta(
            &commit,
            &CommitRefs::default(),
            0,
            &theme,
            row,
            &mut buf,
            &mut scene,
        );

        let cells: Vec<Color> = (23..30).chain(31..38).map(|x| buf[(x, 0)].bg).collect();
        assert_eq!(
            (cells, command::decode_stream(scene.bytes())),
            (
                vec![Color::Reset; 14],
                vec![
                    chip_panel(23, 7, [4, 5, 6]),
                    chip_panel(31, 7, [10, 11, 12]),
                ],
            ),
        );
    }

    /// The commits screen draws the mode chip's box and the HEAD badge's box as
    /// chip panels, each on its own row. A pool page of the list paints the
    /// badge's box into its cells and sends no panel.
    #[test]
    fn the_commits_screen_draws_its_chip_boxes_as_panels() {
        let mut h = Stoat::test();
        h.resize(160, 12);
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("a1000000", "feat: root", &[]);
        h.open_commits("/repo");
        h.render_composited();

        let theme = h.stoat.theme.clone();
        let text = h.rendered_text();
        let ((badge, badge_row), (mode, mode_row)) =
            (cell_of(&text, "HEAD"), cell_of(&text, "NOR"));
        let (_, mode_color) =
            crate::render::pane::mode_segment("normal", &theme, &Default::default());
        let mut panels: Vec<(u16, u16, Option<[u8; 3]>)> =
            command::decode_stream(&h.stoat.apc_scene.all_bytes())
                .into_iter()
                .filter_map(|command| match command {
                    Command::Panel(panel) if (panel.height, panel.inset_y) == (1, 1) => {
                        Some((panel.top, panel.left, panel.fill))
                    },
                    _ => None,
                })
                .collect();
        panels.sort();

        let list = h
            .stoat
            .active_workspace()
            .focused_commits()
            .expect("the commit list");
        let page = crate::smooth_scroll::render_commits_page(list, 0, &theme, 60, 4);
        let page_panels = command::decode_stream(&page)
            .into_iter()
            .filter(|command| matches!(command, Command::Panel(_)))
            .count();
        assert_eq!(
            (panels, page_panels),
            (
                vec![
                    (
                        badge_row,
                        badge - 1,
                        style_rgb(theme.get(scope::VCS_REF_HEAD).bg)
                    ),
                    (mode_row, mode - 1, style_rgb(Some(mode_color))),
                ],
                0,
            ),
        );
    }

    /// A theme of `lets`, the scopes a chip test colors.
    fn chip_theme(lets: &str) -> Theme {
        let (config, _) = stoat_config::parse(&format!("theme t {{ {lets} }}"));
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// The chip panel a chip's box draws as, `width` cells from column `left`
    /// of row 0, in `fill`.
    fn chip_panel(left: u16, width: u16, fill: [u8; 3]) -> Command {
        Command::Panel(PanelCommand {
            top: 0,
            left,
            width,
            height: 1,
            style: BorderStyle::None_,
            border: fill,
            corner_radius: 1,
            fill: Some(fill),
            shadow: PanelShadow::None_,
            inset_x: 0,
            inset_y: 1,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        })
    }

    /// The cell where `needle` first starts in `text`, as `(column, row)`.
    fn cell_of(text: &str, needle: &str) -> (u16, u16) {
        text.lines()
            .enumerate()
            .find_map(|(row, line)| {
                let byte = line.find(needle)?;
                Some((line[..byte].chars().count() as u16, row as u16))
            })
            .unwrap_or_else(|| panic!("{needle:?} is on screen"))
    }

    #[test]
    fn an_undragged_list_takes_four_sevenths_between_both_floors() {
        assert_eq!(
            [
                commit_list_width(60, None),
                commit_list_width(140, None),
                commit_list_width(40, None),
                commit_list_width(30, None),
            ],
            [34, 80, MIN_LIST_COLUMNS, 30 - (MIN_DETAIL_COLUMNS + 1)],
            "four sevenths, floored, then held off the detail floor",
        );
    }

    #[test]
    fn a_dragged_share_holds_between_both_floors() {
        assert_eq!(
            [
                commit_list_width(100, Some(50)),
                commit_list_width(100, Some(1)),
                commit_list_width(100, Some(99)),
            ],
            [50, MIN_LIST_COLUMNS, 100 - (MIN_DETAIL_COLUMNS + 1)],
            "the share is honored between the two floors, and clamped outside",
        );
    }
}
