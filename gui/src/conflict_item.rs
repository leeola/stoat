use crate::{
    buffer::{Buffer, BufferEvent},
    diff_pane::{build_pane_editor, link_scroll_group},
    editor::Editor,
    globals::ExecutorGlobal,
    item::{DeserializeSnafu, ItemError, ItemView},
    theme::{ActiveTheme, Theme},
};
use gpui::{
    div, App, AppContext, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Window,
};
use ratatui::style::Color;
use serde_json::Value;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    buffer::BufferId,
    display_map::highlights::{DecorationHighlight, HighlightStyle},
    host::ConflictedFile,
};
use stoat_text::Bias;

const MISSING_SIDE_PLACEHOLDER: &str = "(file not present)\n";

/// Which side of a conflict block fills the resolution. Selected by
/// `ConflictTakeOurs` / `ConflictTakeTheirs` on the workspace dispatch
/// path; consumed by [`ConflictItem::take_side`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConflictSide {
    Ours,
    Theirs,
}

/// Pane-hosted 3-way merge view for one [`ConflictedFile`].
///
/// Layout: three side-by-side read-only editors (ancestor / ours /
/// theirs) on top, one writable "result" editor at the bottom. The
/// result editor's buffer starts populated with a standard
/// conflict-marker block (`<<<<<<< ours / ======= / >>>>>>> theirs`)
/// over `ours` and `theirs`; the user edits the result freely to
/// produce the final resolved content.
pub struct ConflictItem {
    rel_path: PathBuf,
    /// Original ancestor content from the [`ConflictedFile`]. Held
    /// separately from the ancestor [`SideView`] buffer because the
    /// buffer substitutes [`MISSING_SIDE_PLACEHOLDER`] when the
    /// ancestor was `None` (so the side pane renders), while
    /// resolutions that pull "ancestor" content need the real
    /// original value (empty string for `None`).
    ancestor_original: Option<String>,
    ancestor: SideView,
    ours: SideView,
    theirs: SideView,
    result: SideView,
    /// Keeps the result-buffer edit subscription alive. Dropped
    /// with the item; the subscription itself runs
    /// [`refresh_decoration_highlights`] after every edit so the
    /// `<<<<<<<`/`=======`/`>>>>>>>` marker styling tracks the
    /// current buffer content.
    _result_subscription: Subscription,
}

/// The handles backing one editor pane. The [`Buffer`] handle is
/// retained alongside the [`Editor`] so callers can read or rewrite
/// the buffer text without having to navigate through the editor's
/// `MultiBuffer`.
struct SideView {
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
}

impl ConflictItem {
    /// Build a [`ConflictItem`] from `file`. Missing sides (e.g.
    /// `ours = None` for a deletion) render with a placeholder so the
    /// editor pane stays visible.
    ///
    /// Reads [`ExecutorGlobal`] for the per-buffer `DisplayMap`; the
    /// caller must install it before constructing the entity.
    pub fn from_conflicted_file(file: ConflictedFile, cx: &mut Context<'_, Self>) -> Self {
        let ancestor_text = file.ancestor.as_deref().unwrap_or(MISSING_SIDE_PLACEHOLDER);
        let ours_text = file.ours.as_deref().unwrap_or(MISSING_SIDE_PLACEHOLDER);
        let theirs_text = file.theirs.as_deref().unwrap_or(MISSING_SIDE_PLACEHOLDER);
        let result_text = format_conflict_markers(file.ours.as_deref(), file.theirs.as_deref());

        let ancestor = build_side_editor(ancestor_text, cx);
        let ours = build_side_editor(ours_text, cx);
        let theirs = build_side_editor(theirs_text, cx);
        let result = build_side_editor(&result_text, cx);

        // Scroll the three read-only source panes as one group. The result pane
        // stays independent: its lines diverge from the sources as the merge is
        // resolved, so syncing its scroll would misalign the rows.
        link_scroll_group(&[&ancestor.editor, &ours.editor, &theirs.editor], cx);

        let result_subscription =
            cx.subscribe(&result.buffer, |this, _, event: &BufferEvent, cx| {
                if matches!(event, BufferEvent::Edited | BufferEvent::Reloaded) {
                    this.refresh_decoration_highlights(cx);
                }
            });

        let item = Self {
            rel_path: file.path,
            ancestor_original: file.ancestor,
            ancestor,
            ours,
            theirs,
            result,
            _result_subscription: result_subscription,
        };
        item.refresh_decoration_highlights(cx);
        item
    }

    /// Re-scan the result buffer for conflict-marker lines and feed
    /// the byte ranges into the result editor's display map as
    /// `vcs.conflict.header`-styled decoration highlights. Called
    /// once at construction and whenever the result buffer emits
    /// [`BufferEvent::Edited`] or [`BufferEvent::Reloaded`].
    fn refresh_decoration_highlights(&self, cx: &mut Context<'_, Self>) {
        let text = self.result_buffer_text(cx);
        let ranges = compute_conflict_marker_ranges(&text);
        let buffer_id = self.result.buffer.read(cx).read(|b| b.buffer_id());
        let display_map = self.result.editor.read(cx).display_map().clone();
        if ranges.is_empty() {
            display_map.update(cx, |dm, cx| dm.clear_decoration_highlights(buffer_id, cx));
            return;
        }
        let style = conflict_header_style(cx);
        let decorations: Vec<DecorationHighlight> = display_map.update(cx, |dm, _| {
            let snap = dm.snapshot();
            let buffer_snap = snap.buffer_snapshot();
            ranges
                .into_iter()
                .map(|range| {
                    let start = buffer_snap.anchor_at(range.start, Bias::Right);
                    let end = buffer_snap.anchor_at(range.end, Bias::Left);
                    DecorationHighlight {
                        range: start..end,
                        style: style.clone(),
                    }
                })
                .collect()
        });
        let decorations: Arc<[DecorationHighlight]> = Arc::from(decorations);
        display_map.update(cx, |dm, cx| {
            dm.set_decoration_highlights(buffer_id, decorations, cx)
        });
    }

    /// Replace the conflict block enclosing the result editor's
    /// primary cursor with `side`'s content. The block must be a
    /// well-formed `<<<<<<< / ======= / >>>>>>>` triple; mismatched
    /// or absent markers leave the buffer untouched.
    pub(crate) fn take_side(&self, side: ConflictSide, cx: &mut Context<'_, Self>) {
        let cursor = {
            let editor = self.result.editor.read(cx);
            let snapshot = editor.multi_buffer().read(cx).snapshot();
            let Some(primary) = editor
                .selections()
                .all_anchors()
                .iter()
                .max_by_key(|s| s.id)
            else {
                return;
            };
            snapshot.resolve_anchor(&primary.head())
        };
        let text = self.result_buffer_text(cx);
        let Some(block) = find_enclosing_conflict_block(&text, cursor) else {
            return;
        };
        let replacement_range = match side {
            ConflictSide::Ours => block.ours,
            ConflictSide::Theirs => block.theirs,
        };
        let replacement = text[replacement_range].to_owned();
        self.result.buffer.update(cx, |buf, cx| {
            buf.edit(block.range, &replacement, cx);
        });
    }

    /// Snapshot of the result buffer's text -- the resolved content
    /// the user has produced so far.
    pub(crate) fn result_buffer_text(&self, cx: &App) -> String {
        self.result.buffer.read(cx).text()
    }

    /// Repository-relative path of the conflicted file this view
    /// resolves. Used by the workspace's `ConflictApply` dispatcher to
    /// match an open view against the [`stoat::host::ConflictedFile`]
    /// list carried on [`stoat::rebase::RebasePause::Conflict`].
    pub(crate) fn path(&self) -> &Path {
        &self.rel_path
    }

    /// Resolve the file by replacing the entire result buffer with
    /// the ancestor's content. Yields an empty buffer when the file
    /// had no common ancestor (`ConflictedFile.ancestor = None`).
    pub(crate) fn skip_entry(&self, cx: &mut Context<'_, Self>) {
        let replacement = self.ancestor_original.clone().unwrap_or_default();
        let result_len = self.result_buffer_text(cx).len();
        self.result.buffer.update(cx, |buf, cx| {
            buf.edit(0..result_len, &replacement, cx);
        });
    }

    /// True while the result buffer still carries at least one
    /// complete `<<<<<<< / ======= / >>>>>>>` block.
    pub(crate) fn has_unresolved_conflicts(&self, cx: &App) -> bool {
        has_any_conflict_block(&self.result_buffer_text(cx))
    }

    /// Overwrite the result buffer with `text`. Used by tests that
    /// need deterministic resolved content without driving cursor
    /// motion plus [`ConflictItem::take_side`].
    #[cfg(test)]
    pub(crate) fn set_result_buffer_text_for_test(&self, text: &str, cx: &mut Context<'_, Self>) {
        let len = self.result_buffer_text(cx).len();
        self.result.buffer.update(cx, |buf, cx| {
            buf.edit(0..len, text, cx);
        });
    }
}

/// Byte offsets of a `<<<<<<< / ======= / >>>>>>>` block in a buffer.
/// `range` covers the entire marker block; `ours` and `theirs` cover
/// the content of each side, both bounded by `\n` line boundaries so
/// the slices stay UTF-8 valid.
struct ConflictBlock {
    range: Range<usize>,
    ours: Range<usize>,
    theirs: Range<usize>,
}

fn has_any_conflict_block(text: &str) -> bool {
    enum S {
        Outside,
        InOurs,
        InTheirs,
    }
    let mut state = S::Outside;
    for line in text.lines() {
        match state {
            S::Outside if line.starts_with("<<<<<<<") => state = S::InOurs,
            S::InOurs if line.starts_with("=======") => state = S::InTheirs,
            S::InTheirs if line.starts_with(">>>>>>>") => return true,
            _ => {},
        }
    }
    false
}

fn find_enclosing_conflict_block(text: &str, cursor: usize) -> Option<ConflictBlock> {
    enum State {
        Outside,
        InOurs {
            block_start: usize,
            ours_start: usize,
        },
        InTheirs {
            block_start: usize,
            ours: Range<usize>,
            theirs_start: usize,
        },
    }

    let mut state = State::Outside;
    let mut pos = 0usize;
    loop {
        let nl = text[pos..].find('\n');
        let line_end = nl.map(|p| pos + p).unwrap_or(text.len());
        let next_line_start = nl.map(|_| line_end + 1).unwrap_or(text.len());
        let line = &text[pos..line_end];

        match &state {
            State::Outside => {
                if line.starts_with("<<<<<<<") {
                    state = State::InOurs {
                        block_start: pos,
                        ours_start: next_line_start,
                    };
                }
            },
            State::InOurs {
                block_start,
                ours_start,
            } => {
                if line.starts_with("=======") {
                    state = State::InTheirs {
                        block_start: *block_start,
                        ours: *ours_start..pos,
                        theirs_start: next_line_start,
                    };
                }
            },
            State::InTheirs {
                block_start,
                ours,
                theirs_start,
            } => {
                if line.starts_with(">>>>>>>") {
                    let block_end = next_line_start;
                    if (*block_start..block_end).contains(&cursor) {
                        return Some(ConflictBlock {
                            range: *block_start..block_end,
                            ours: ours.clone(),
                            theirs: *theirs_start..pos,
                        });
                    }
                    state = State::Outside;
                }
            },
        }

        if nl.is_none() {
            break;
        }
        pos = next_line_start;
    }
    None
}

fn build_side_editor(text: &str, cx: &mut Context<'_, ConflictItem>) -> SideView {
    let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), text));
    let executor = cx.global::<ExecutorGlobal>().0.clone();
    let (_, editor) = build_pane_editor(buffer.clone(), Vec::new(), executor, cx);
    SideView { buffer, editor }
}

/// Scan `text` for lines starting with the three conflict-marker
/// prefixes (`<<<<<<<`, `=======`, `>>>>>>>`). Returns each match's
/// byte range, including the trailing `\n` when present so the
/// highlight covers the whole logical marker line.
fn compute_conflict_marker_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut pos = 0usize;
    while pos < text.len() {
        let nl = text[pos..].find('\n');
        let line_end = nl.map(|p| pos + p).unwrap_or(text.len());
        let next_line_start = nl.map(|_| line_end + 1).unwrap_or(text.len());
        let line = &text[pos..line_end];
        if line.starts_with("<<<<<<<") || line.starts_with("=======") || line.starts_with(">>>>>>>")
        {
            ranges.push(pos..next_line_start);
        }
        if nl.is_none() {
            break;
        }
        pos = next_line_start;
    }
    ranges
}

/// Build the [`HighlightStyle`] applied to conflict-marker lines.
/// Resolves the foreground from the active stoat-side `Theme`'s
/// `vcs.conflict.header` scope; falls back to red when the theme
/// has no entry, matching the gui side's `palette.danger`.
fn conflict_header_style(cx: &App) -> HighlightStyle {
    let foreground = cx
        .try_global::<Theme>()
        .and_then(|t| t.0.try_get(stoat::theme::scope::VCS_CONFLICT_HEADER))
        .and_then(|style| style.fg)
        .or(Some(Color::Red));
    HighlightStyle {
        foreground,
        bold: Some(true),
        ..Default::default()
    }
}

fn format_conflict_markers(ours: Option<&str>, theirs: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("<<<<<<< ours\n");
    out.push_str(ours.unwrap_or(""));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("=======\n");
    out.push_str(theirs.unwrap_or(""));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(">>>>>>> theirs\n");
    out
}

/// Reconstruct a [`ConflictedFile`] from the on-disk content of a
/// conflicted file, for restoring a conflict view across a restart
/// where the 3-way merge state is no longer in memory. Walks the
/// `<<<<<<< / ======= / >>>>>>>` blocks, accumulating full-file
/// versions: lines outside a block belong to both sides, `ours` /
/// `theirs` lines to their respective side, and marker lines are
/// dropped. The common ancestor is not recoverable from two-way
/// markers, so it is left absent.
///
/// Returns [`None`] when `content` carries no complete conflict block:
/// the conflict is resolved and the view should not be restored.
pub fn conflicted_file_from_markers(rel_path: PathBuf, content: &str) -> Option<ConflictedFile> {
    enum Side {
        Outside,
        Ours,
        Theirs,
    }
    let mut side = Side::Outside;
    let mut ours = String::new();
    let mut theirs = String::new();
    let mut saw_complete_block = false;
    for line in content.split_inclusive('\n') {
        let marker = line.strip_suffix('\n').unwrap_or(line);
        match side {
            Side::Outside => {
                if marker.starts_with("<<<<<<<") {
                    side = Side::Ours;
                } else {
                    ours.push_str(line);
                    theirs.push_str(line);
                }
            },
            Side::Ours => {
                if marker.starts_with("=======") {
                    side = Side::Theirs;
                } else {
                    ours.push_str(line);
                }
            },
            Side::Theirs => {
                if marker.starts_with(">>>>>>>") {
                    side = Side::Outside;
                    saw_complete_block = true;
                } else {
                    theirs.push_str(line);
                }
            },
        }
    }
    if !saw_complete_block {
        return None;
    }
    Some(ConflictedFile {
        path: rel_path,
        ancestor: None,
        ours: Some(ours),
        theirs: Some(theirs),
    })
}

impl Render for ConflictItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let top = div()
            .flex()
            .flex_row()
            .flex_1()
            .child(pane_with_header(
                "BASE",
                theme.muted_text,
                self.ancestor.editor.clone(),
            ))
            .child(pane_with_header(
                "OURS",
                theme.vcs_conflict_ours,
                self.ours.editor.clone(),
            ))
            .child(pane_with_header(
                "THEIRS",
                theme.vcs_conflict_theirs,
                self.theirs.editor.clone(),
            ));
        let bottom = pane_with_header(
            "RESULT",
            theme.vcs_conflict_header,
            self.result.editor.clone(),
        );
        div().flex().flex_col().size_full().child(top).child(bottom)
    }
}

/// Build a vertical pane: a small bold header label colored from
/// `color`, then the editor below it. Used by [`ConflictItem`] to
/// identify each side of the 3-way merge view.
fn pane_with_header(label: &str, color: Hsla, editor: Entity<Editor>) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .child(
            div()
                .px_2()
                .text_color(color)
                .font_weight(FontWeight::SEMIBOLD)
                .child(SharedString::from(label.to_string())),
        )
        .child(div().flex_1().child(editor))
}

impl ItemView for ConflictItem {
    fn tab_label(&self, _cx: &App) -> SharedString {
        let name = self
            .rel_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .unwrap_or_else(|| self.rel_path.display().to_string());
        format!("Conflict: {name}").into()
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "ConflictItem deserialize requires workspace-persistence wiring \
                     that has not yet landed",
        }
        .fail()
    }

    fn item_kind(&self) -> crate::item::ItemKind {
        crate::item::ItemKind::Conflict
    }

    fn serialize(&self, _cx: &App) -> Value {
        serde_json::json!({
            "rel_path": self.rel_path.to_string_lossy(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
        });
    }

    fn make_file(
        path: &str,
        ancestor: Option<&str>,
        ours: Option<&str>,
        theirs: Option<&str>,
    ) -> ConflictedFile {
        ConflictedFile {
            path: PathBuf::from(path),
            ancestor: ancestor.map(String::from),
            ours: ours.map(String::from),
            theirs: theirs.map(String::from),
        }
    }

    #[test]
    fn tab_label_uses_file_path() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("src/foo.rs", None, Some("a\n"), Some("b\n")),
                    cx,
                )
            })
        });
        item.read_with(&cx, |item, app| {
            assert_eq!(item.tab_label(app), SharedString::from("Conflict: foo.rs"));
        });
    }

    #[test]
    fn result_buffer_is_populated_with_conflict_markers() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("ours-line\n"), Some("theirs-line\n")),
                    cx,
                )
            })
        });
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(
            text,
            "<<<<<<< ours\nours-line\n=======\ntheirs-line\n>>>>>>> theirs\n",
        );
    }

    #[test]
    fn missing_ancestor_uses_placeholder() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("o"), Some("t")),
                    cx,
                )
            })
        });
        let ancestor_text = item.read_with(&cx, |item, cx| item.ancestor.buffer.read(cx).text());
        assert_eq!(ancestor_text, MISSING_SIDE_PLACEHOLDER);
    }

    #[test]
    fn is_dirty_is_false_initially() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("o"), Some("t")),
                    cx,
                )
            })
        });
        item.read_with(&cx, |item, app| {
            assert!(!item.is_dirty(app));
        });
    }

    #[test]
    fn deserialize_returns_error_until_persistence_wires_through() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("o"), Some("t")),
                    cx,
                )
            })
        });
        let err = item.update(&mut cx, |_, cx| {
            ConflictItem::deserialize(Value::Null, cx).err()
        });
        assert!(matches!(err, Some(ItemError::Deserialize { .. })));
    }

    #[test]
    fn missing_ours_yields_empty_ours_section_in_result() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, None, Some("theirs-line\n")),
                    cx,
                )
            })
        });
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "<<<<<<< ours\n=======\ntheirs-line\n>>>>>>> theirs\n",);
    }

    #[test]
    fn find_enclosing_conflict_block_single_block_cursor_in_ours() {
        let text = "<<<<<<< ours\nalpha\n=======\nbeta\n>>>>>>> theirs\n";
        let cursor = text.find("alpha").expect("ours line present");
        let block = find_enclosing_conflict_block(text, cursor).expect("cursor in block");
        assert_eq!(&text[block.range.clone()], text);
        assert_eq!(&text[block.ours], "alpha\n");
        assert_eq!(&text[block.theirs], "beta\n");
    }

    #[test]
    fn find_enclosing_conflict_block_no_marker_returns_none() {
        assert!(find_enclosing_conflict_block("just plain text\nno markers\n", 4).is_none());
    }

    #[test]
    fn find_enclosing_conflict_block_cursor_between_blocks_returns_none() {
        let text = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\nbetween\n<<<<<<< ours\nc\n=======\nd\n>>>>>>> theirs\n";
        let cursor = text.find("between").expect("between marker present");
        assert!(find_enclosing_conflict_block(text, cursor).is_none());
    }

    #[test]
    fn find_enclosing_conflict_block_returns_second_block_when_cursor_inside() {
        let text = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\nx\n<<<<<<< ours\nc\n=======\nd\n>>>>>>> theirs\n";
        let cursor = text.find('c').expect("second-block ours present");
        let block = find_enclosing_conflict_block(text, cursor).expect("cursor in second block");
        assert_eq!(&text[block.ours], "c\n");
        assert_eq!(&text[block.theirs], "d\n");
    }

    #[test]
    fn find_enclosing_conflict_block_handles_branch_name_suffix() {
        let text = "<<<<<<< feature-x\nfoo\n=======\nbar\n>>>>>>> main\n";
        let cursor = text.find("foo").expect("ours line present");
        let block = find_enclosing_conflict_block(text, cursor).expect("cursor in block");
        assert_eq!(&text[block.ours], "foo\n");
        assert_eq!(&text[block.theirs], "bar\n");
    }

    fn place_cursor_at(editor: &Entity<Editor>, offset: usize, cx: &mut TestAppContext) {
        use stoat_text::{Bias, Selection, SelectionGoal};
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let anchor = snapshot.anchor_at(offset, Bias::Left);
            ed.selections_mut().replace_with(
                vec![Selection {
                    id: 1,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &snapshot,
            );
            cx.notify();
        });
    }

    #[test]
    fn take_ours_replaces_block_with_ours_content() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        let editor = item.read_with(&cx, |item, _| item.result.editor.clone());
        place_cursor_at(&editor, 0, &mut cx);
        item.update(&mut cx, |item, cx| item.take_side(ConflictSide::Ours, cx));
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "alpha\n");
    }

    #[test]
    fn decoration_highlights_track_conflict_markers_through_take_side() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        cx.run_until_parked();

        let (display_map, buffer_id) = item.read_with(&cx, |item, cx| {
            let display_map = item.result.editor.read(cx).display_map().clone();
            let buffer_id = item.result.buffer.read(cx).read(|b| b.buffer_id());
            (display_map, buffer_id)
        });
        let initial = display_map.update(&mut cx, |dm, _| {
            dm.snapshot()
                .decoration_highlights()
                .get(&buffer_id)
                .map(|d| d.len())
                .unwrap_or(0)
        });
        assert_eq!(
            initial, 3,
            "the initial result text has three marker lines (<<<<<<<, =======, >>>>>>>)"
        );

        let editor = item.read_with(&cx, |item, _| item.result.editor.clone());
        place_cursor_at(&editor, 0, &mut cx);
        item.update(&mut cx, |item, cx| item.take_side(ConflictSide::Ours, cx));
        cx.run_until_parked();

        let after = display_map.update(&mut cx, |dm, _| {
            dm.snapshot()
                .decoration_highlights()
                .get(&buffer_id)
                .map(|d| d.len())
                .unwrap_or(0)
        });
        assert_eq!(
            after, 0,
            "taking a side removes the conflict block so no marker decorations remain"
        );
    }

    #[test]
    fn take_theirs_replaces_block_with_theirs_content() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        let editor = item.read_with(&cx, |item, _| item.result.editor.clone());
        place_cursor_at(&editor, 0, &mut cx);
        item.update(&mut cx, |item, cx| item.take_side(ConflictSide::Theirs, cx));
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "beta\n");
    }

    #[test]
    fn has_any_conflict_block_returns_true_for_complete_block() {
        assert!(has_any_conflict_block(
            "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n"
        ));
    }

    #[test]
    fn has_any_conflict_block_returns_false_without_markers() {
        assert!(!has_any_conflict_block("just plain text\n"));
    }

    #[test]
    fn has_any_conflict_block_returns_false_for_partial_markers() {
        assert!(!has_any_conflict_block(
            "<<<<<<< ours\na\n=======\nb\n(no end marker)\n"
        ));
        assert!(!has_any_conflict_block(
            "<<<<<<< ours\na\n(no separator)\n>>>>>>> theirs\n"
        ));
        assert!(!has_any_conflict_block(
            "(only end marker)\n>>>>>>> theirs\n"
        ));
    }

    #[test]
    fn has_unresolved_conflicts_is_true_for_fresh_item() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        item.read_with(&cx, |item, cx| {
            assert!(item.has_unresolved_conflicts(cx));
        });
    }

    #[test]
    fn has_unresolved_conflicts_is_false_after_take_side() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        let editor = item.read_with(&cx, |item, _| item.result.editor.clone());
        place_cursor_at(&editor, 0, &mut cx);
        item.update(&mut cx, |item, cx| item.take_side(ConflictSide::Ours, cx));
        item.read_with(&cx, |item, cx| {
            assert!(!item.has_unresolved_conflicts(cx));
        });
    }

    #[test]
    fn skip_entry_replaces_result_with_ancestor_content() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", Some("base\n"), Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        item.update(&mut cx, |item, cx| item.skip_entry(cx));
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "base\n");
    }

    #[test]
    fn skip_entry_yields_empty_buffer_when_ancestor_is_none() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        item.update(&mut cx, |item, cx| item.skip_entry(cx));
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "");
    }

    #[test]
    fn skip_entry_clears_unresolved_state() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", Some("base\n"), Some("alpha\n"), Some("beta\n")),
                    cx,
                )
            })
        });
        item.update(&mut cx, |item, cx| item.skip_entry(cx));
        item.read_with(&cx, |item, cx| {
            assert!(!item.has_unresolved_conflicts(cx));
        });
    }

    #[test]
    fn take_side_is_noop_when_cursor_outside_any_block() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let item = cx.update(|cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file("a.txt", None, Some("a\n"), Some("b\n")),
                    cx,
                )
            })
        });
        item.update(&mut cx, |item, cx| {
            item.result.buffer.update(cx, |buf, cx| {
                let text = buf.text();
                buf.edit(0..text.len(), "no markers here\n", cx);
            });
        });
        let editor = item.read_with(&cx, |item, _| item.result.editor.clone());
        place_cursor_at(&editor, 0, &mut cx);
        item.update(&mut cx, |item, cx| item.take_side(ConflictSide::Ours, cx));
        let text = item.read_with(&cx, |item, cx| item.result.buffer.read(cx).text());
        assert_eq!(text, "no markers here\n");
    }

    #[test]
    fn source_panes_scroll_together_result_stays_independent() {
        use gpui::{px, size, Modifiers, Point, ScrollDelta, ScrollWheelEvent, TouchPhase};
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let tall: String = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let vcx = cx.add_empty_window();
        let item = vcx.update(|_, cx| {
            cx.new(|cx| {
                ConflictItem::from_conflicted_file(
                    make_file(
                        "a.txt",
                        Some(tall.as_str()),
                        Some(tall.as_str()),
                        Some(tall.as_str()),
                    ),
                    cx,
                )
            })
        });
        let (ancestor, ours, theirs, result) = item.read_with(vcx, |item, _| {
            (
                item.ancestor.editor.clone(),
                item.ours.editor.clone(),
                item.theirs.editor.clone(),
                item.result.editor.clone(),
            )
        });
        let cell = size(px(8.0), px(16.0));
        for editor in [&ancestor, &ours, &theirs, &result] {
            editor.update_in(vcx, |ed, _, cx| ed.set_cell_size(cell, cx));
        }
        vcx.run_until_parked();

        ours.update_in(vcx, |ed, window, cx| {
            ed.handle_scroll_wheel(
                &ScrollWheelEvent {
                    position: Point::new(px(0.), px(0.)),
                    delta: ScrollDelta::Lines(Point::new(0., -4.)),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Moved,
                },
                window,
                cx,
            );
        });
        vcx.run_until_parked();

        assert_eq!(
            ours.read_with(vcx, |ed, _| ed.scroll_row()),
            4,
            "the scrolled source pane moved",
        );
        assert_eq!(
            ancestor.read_with(vcx, |ed, _| ed.scroll_row()),
            4,
            "the base pane mirrors the scrolled source pane",
        );
        assert_eq!(
            theirs.read_with(vcx, |ed, _| ed.scroll_row()),
            4,
            "the theirs pane mirrors the scrolled source pane",
        );
        assert_eq!(
            result.read_with(vcx, |ed, _| ed.scroll_row()),
            0,
            "the result pane is not part of the source scroll group",
        );
    }
}
