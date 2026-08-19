//! Cursor-to-symbol resolution and symbol jumps over the code graph.
//!
//! These are the shared entry points the graph-navigation actions build on.
//! They resolve the cursor to a [`SymbolKey`] and jump to a symbol's
//! definition.

use crate::{
    action_handlers,
    app::{Stoat, UpdateEffect},
    badge::{Anchor as BadgeAnchor, Badge, BadgeSource, BadgeState},
    code_index::build,
    editor_state::EditorState,
    nav_list::NavList,
    symbol_finder::{SymbolEntry, SymbolPicker},
    workspace::{
        diff::{scan_changed_ranges, ChangedRangesScan},
        Workspace,
    },
};
use codegraph::{Dir, EdgeKind, SymbolKey};
use std::{path::Path, sync::mpsc};
use stoat_scheduler::Task;
use stoat_text::{Anchor, Bias, BufferId};

/// How far the diff-filtered hops search before giving up.
const MAX_DIFF_HOPS: usize = 64;

/// A call-graph trail between two marked points.
///
/// Holds the start anchor (read back by [`mark_trail_end`] to resolve the
/// start symbol) and, once the end is marked, the cached path between the
/// enclosing symbols as a [`NavList`] whose cursor tracks the current position
/// along it. While only the start is marked, `path` is empty.
pub(crate) struct TrailState {
    /// Where the reader marked the start, or `None` for a path installed
    /// without marks at all. A trail computed for them rather than by them has
    /// no anchor to resolve, and never needs one, since its path is already
    /// built.
    start: Option<(BufferId, Anchor)>,
    path: NavList<SymbolKey>,
}

impl TrailState {
    /// Which stop of how many the trail sits on, one-based, or `None` while it
    /// carries no path.
    ///
    /// One-based because a person reads it. The first stop of five reads
    /// "1/5", not "0/5".
    pub(crate) fn progress(&self) -> Option<(usize, usize)> {
        (!self.is_armed()).then(|| (self.path.cursor() + 1, self.path.len()))
    }

    /// Whether a start is marked and no path is computed yet.
    ///
    /// The half-marked state, which reads differently from a walkable trail:
    /// there is nothing to step along, only a point waiting for its partner.
    pub(crate) fn is_armed(&self) -> bool {
        self.path.entries().is_empty()
    }

    /// Project the trail into its workspace badge.
    ///
    /// Total where [`AgentStatus::badge`](crate::agent_status::AgentStatus::badge)
    /// is not, though it mirrors that one otherwise. A session that ended warrants
    /// no overlay, where every trail state warrants one. A trail that no longer
    /// deserves a badge is a trail that no longer exists, and [`trail_clear`] is
    /// what makes that so.
    pub(crate) fn badge(&self) -> Badge {
        let label = match self.progress() {
            Some((at, stops)) => format!("trail {at}/{stops}"),
            None => "trail armed".to_string(),
        };
        Badge {
            source: BadgeSource::Trail,
            anchor: BadgeAnchor::BottomRight,
            state: BadgeState::Active,
            label,
            detail: None,
        }
    }
}

/// Navigate from the symbol under the cursor to one of its callers.
///
/// A no-op when the cursor is on no indexed symbol or it has no callers.
pub(crate) fn goto_caller(stoat: &mut Stoat) -> UpdateEffect {
    goto_along(stoat, EdgeKind::Calls, Dir::Up)
}

/// Navigate to the nearest caller carrying a working-tree diff, skipping
/// unchanged callers along the way.
pub(crate) fn goto_diff_caller_up(stoat: &mut Stoat) -> UpdateEffect {
    goto_nearest_diff(stoat, Dir::Up)
}

/// Navigate to the nearest callee carrying a working-tree diff, skipping
/// unchanged callees along the way.
pub(crate) fn goto_diff_callee_down(stoat: &mut Stoat) -> UpdateEffect {
    goto_nearest_diff(stoat, Dir::Down)
}

/// A diff-filtered hop whose working-tree scan has not landed yet.
///
/// Held on [`Stoat`] between the keypress that armed it and
/// [`pump_diff_nav_jump`]. The symbol the hop leaves from travels here, because
/// the scan reports which symbols changed and never where to start.
pub(crate) struct PendingDiffNavJump {
    rx: mpsc::Receiver<ChangedRangesScan>,
    _task: Task<()>,
    start: SymbolKey,
    dir: Dir,
}

/// Arm a working-tree scan, so a landed one walks the call axis to the nearest
/// changed symbol and jumps there.
///
/// Returns as soon as the scan is armed. Listing the repo's changes, reading
/// each changed file, and diffing it against HEAD are what the hop costs, so
/// all of it runs off the thread that handled the key.
///
/// A second press while one is in flight replaces it. Both presses answer the
/// same question about the same tree, so the later one is the whole answer.
fn goto_nearest_diff(stoat: &mut Stoat, dir: Dir) -> UpdateEffect {
    let Some(start) = symbol_at_cursor(stoat) else {
        return UpdateEffect::None;
    };

    let git = stoat.git_host.clone();
    let fs = stoat.fs_host.clone();
    let langs = stoat.language_registry.clone();
    let git_root = stoat.active_workspace().git_root.clone();
    let memo = stoat.active_workspace().changed_ranges_memo_snapshot();
    let redraw = stoat.redraw_notify.clone();
    let (tx, rx) = mpsc::channel();

    let task = stoat.executor.spawn_blocking(move || {
        let scan = scan_changed_ranges(git.as_ref(), fs.as_ref(), &langs, &git_root, &memo);
        let _ = tx.send(scan);
        redraw.notify_one();
    });

    stoat.pending_diff_nav_jump = Some(PendingDiffNavJump {
        rx,
        _task: task,
        start,
        dir,
    });
    UpdateEffect::Redraw
}

/// Install a landed working-tree scan and jump to the nearest changed symbol
/// along the armed direction.
///
/// The scan is installed whether or not it leads anywhere, so the fresh diff
/// serves the next hop as well. A hop that finds nothing records no jump,
/// because [`jump_to_symbol`] is what pushes the jumplist entry.
pub(crate) fn pump_diff_nav_jump(stoat: &mut Stoat) -> bool {
    let Some(pending) = stoat.pending_diff_nav_jump.take() else {
        return false;
    };
    let scan = match pending.rx.try_recv() {
        Ok(scan) => scan,
        Err(mpsc::TryRecvError::Empty) => {
            stoat.pending_diff_nav_jump = Some(pending);
            return false;
        },
        Err(mpsc::TryRecvError::Disconnected) => return false,
    };

    stoat.active_workspace_mut().install_changed_ranges(scan);
    if let Some(target) = nearest_diff_target(stoat.active_workspace(), pending.start, pending.dir)
    {
        jump_to_symbol(stoat, target);
    }
    true
}

/// The nearest symbol along `dir` from `start` whose definition overlaps a
/// working-tree diff, or `None` within [`MAX_DIFF_HOPS`].
fn nearest_diff_target(ws: &Workspace, start: SymbolKey, dir: Dir) -> Option<SymbolKey> {
    let git_root = ws.git_root.clone();
    ws.code_graph
        .nearest(
            start,
            EdgeKind::Calls,
            dir,
            |key| has_diff(ws, &git_root, key),
            MAX_DIFF_HOPS,
        )
        .and_then(|path| path.last().copied())
}

/// Whether `key`'s definition overlaps a working-tree change.
///
/// An open buffer with a live diff map is consulted directly so unsaved
/// edits count. Otherwise the cached [`Workspace::changed_ranges`] byte
/// ranges are tested against the symbol's definition span.
fn has_diff(ws: &Workspace, git_root: &Path, key: SymbolKey) -> bool {
    let Some(symbol) = ws.code_graph.symbol(key) else {
        return false;
    };
    let def = symbol.def_range.clone();
    let file = symbol.file;

    if let Some(rel) = ws.file_paths.get(&file)
        && let Some(buffer_id) = ws.buffers.id_for_path(&git_root.join(rel))
        && let Some(shared) = ws.buffers.get(buffer_id)
    {
        let guard = shared.read().expect("buffer poisoned");
        if let Some(diff_map) = &guard.diff_map {
            let rope = &guard.snapshot.visible_text;
            let start_row = rope.offset_to_point(def.start).row;
            let end_row = rope.offset_to_point(def.end).row;
            return !diff_map.hunks_in_range(start_row..end_row + 1).is_empty();
        }
    }

    ws.changed_ranges.get(&file).is_some_and(|ranges| {
        ranges
            .iter()
            .any(|r| r.start < def.end && def.start < r.end)
    })
}

/// Navigate from the symbol under the cursor to one of its callees.
///
/// A no-op when the cursor is on no indexed symbol or it has no callees.
pub(crate) fn goto_callee(stoat: &mut Stoat) -> UpdateEffect {
    goto_along(stoat, EdgeKind::Calls, Dir::Down)
}

/// Navigate from the symbol under the cursor to a symbol that references it.
///
/// Steps up the type-reference axis. A no-op when the cursor is on no indexed
/// symbol or nothing references it.
pub(crate) fn goto_references(stoat: &mut Stoat) -> UpdateEffect {
    goto_along(stoat, EdgeKind::References, Dir::Up)
}

/// Navigate from the trait under the cursor to one of its implementors.
///
/// Steps up the implements axis. A no-op when the cursor is on no indexed
/// symbol or nothing implements it.
pub(crate) fn goto_implementors(stoat: &mut Stoat) -> UpdateEffect {
    goto_along(stoat, EdgeKind::Implements, Dir::Up)
}

/// Step one hop along the `kind` axis from the cursor's symbol and navigate
/// to the result, presenting a picker when several neighbors tie.
fn goto_along(stoat: &mut Stoat, kind: EdgeKind, dir: Dir) -> UpdateEffect {
    let Some(key) = symbol_at_cursor(stoat) else {
        return UpdateEffect::None;
    };
    let targets = stoat.active_workspace().code_graph.step(key, kind, dir);
    present_or_pick(stoat, targets)
}

/// Mark the start of a call-graph trail at the cursor.
///
/// A no-op when no editor is focused. Replaces any prior trail.
pub(crate) fn mark_trail_start(stoat: &mut Stoat) -> UpdateEffect {
    let Some(start) = focused_anchor(stoat) else {
        return UpdateEffect::None;
    };
    stoat.active_workspace_mut().trail = Some(TrailState {
        start: Some(start),
        path: NavList::default(),
    });
    stoat.set_status("trail start marked");
    UpdateEffect::Redraw
}

/// Mark the end of a trail, compute the call-graph path relating the symbols
/// enclosing the two marks, and jump to the start.
///
/// Falls back to a direct two-point path when nothing relates them, reported
/// as the fallback it is. No start marked, or a mark on no indexed symbol,
/// each report which one it was rather than doing nothing.
pub(crate) fn mark_trail_end(stoat: &mut Stoat) -> UpdateEffect {
    let Some(end) = focused_anchor(stoat) else {
        return UpdateEffect::None;
    };
    let Some(start) = stoat
        .active_workspace()
        .trail
        .as_ref()
        .and_then(|trail| trail.start)
    else {
        stoat.set_status("no trail start marked");
        return UpdateEffect::Redraw;
    };

    // Related rather than merely called. A reader marks two points without
    // knowing which one calls the other, or whether either does.
    let (path, related) = {
        let ws = stoat.active_workspace();
        let git_root = ws.git_root.clone();
        let (Some(sym_a), Some(sym_b)) = (
            resolve_to_symbol(ws, &git_root, &start),
            resolve_to_symbol(ws, &git_root, &end),
        ) else {
            stoat.set_status("trail mark is on no indexed symbol");
            return UpdateEffect::Redraw;
        };
        match ws.code_graph.path_relating(sym_a, sym_b, EdgeKind::Calls) {
            Some(path) => (path, true),
            None => (vec![sym_a, sym_b], false),
        }
    };

    let first = path.first().copied();
    let stops = path.len();
    stoat.active_workspace_mut().trail = Some(TrailState {
        start: Some(start),
        path: nav_list_of(&path),
    });

    if related {
        stoat.set_status(format!("trail: {stops} stops"));
    } else {
        stoat.set_status("no call relation; direct trail");
    }
    match first {
        Some(key) => jump_to_symbol(stoat, key),
        None => UpdateEffect::Redraw,
    }
}

/// Install `path` as the active trail, sitting on its first symbol.
///
/// For a trail worked out for the reader rather than marked by them, such as
/// the one a walkthrough lays between consecutive stops. It carries no start
/// anchor, since the path is already built and there is nothing left to
/// resolve one against.
pub(crate) fn install_trail(stoat: &mut Stoat, path: &[SymbolKey]) {
    stoat.active_workspace_mut().trail = Some(TrailState {
        start: None,
        path: nav_list_of(path),
    });
}

/// `path` as a list sitting on its first entry, which is where a trail starts.
fn nav_list_of(path: &[SymbolKey]) -> NavList<SymbolKey> {
    let mut list = NavList::default();
    for &key in path {
        list.push_tip(key);
    }
    list.set_cursor(0);
    list
}

/// Forget the active trail.
///
/// Marking a new start replaces a trail as well, so this is the way to put one
/// down without picking another up. A no-op when none is set, since there is
/// nothing to report having cleared.
pub(crate) fn trail_clear(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().trail.is_none() {
        return UpdateEffect::None;
    }
    stoat.active_workspace_mut().trail = None;
    stoat.set_status("trail cleared");
    UpdateEffect::Redraw
}

/// Step forward along the trail toward the end mark.
pub(crate) fn trail_next(stoat: &mut Stoat) -> UpdateEffect {
    trail_step(stoat, 1)
}

/// Step backward along the trail toward the start mark.
pub(crate) fn trail_prev(stoat: &mut Stoat) -> UpdateEffect {
    trail_step(stoat, -1)
}

/// Move `delta` symbols along the trail (clamped) and jump there.
fn trail_step(stoat: &mut Stoat, delta: isize) -> UpdateEffect {
    let (target, at, stops) = {
        let Some(trail) = stoat.active_workspace_mut().trail.as_mut() else {
            return UpdateEffect::None;
        };
        let Some(target) = trail.path.step_clamp(delta).copied() else {
            return UpdateEffect::None;
        };
        let Some((at, stops)) = trail.progress() else {
            return UpdateEffect::None;
        };
        (target, at, stops)
    };

    // A key the graph evicted since the trail was computed still has a
    // position to report, so the name is what goes missing rather than the
    // whole message.
    let name = stoat
        .active_workspace()
        .code_graph
        .symbol(target)
        .map(|symbol| symbol.name.clone());
    match name {
        Some(name) => stoat.set_status(format!("trail {at}/{stops}: {name}")),
        None => stoat.set_status(format!("trail {at}/{stops}")),
    }
    jump_to_symbol(stoat, target)
}

/// The focused buffer id and an anchor at the cursor, or `None` when no
/// editor is focused.
fn focused_anchor(stoat: &mut Stoat) -> Option<(BufferId, Anchor)> {
    let (buffer_id, offset) = {
        let editor = action_handlers::focused_editor_mut(stoat)?;
        (editor.buffer_id, focused_offset(editor))
    };
    let shared = stoat.active_workspace().buffers.get(buffer_id)?;
    let guard = shared.read().expect("buffer poisoned");
    Some((buffer_id, guard.snapshot.anchor_at(offset, Bias::Left)))
}

/// Resolve a marked anchor to the graph symbol enclosing it.
fn resolve_to_symbol(
    ws: &Workspace,
    git_root: &Path,
    mark: &(BufferId, Anchor),
) -> Option<SymbolKey> {
    let (buffer_id, anchor) = mark;
    let offset = {
        let shared = ws.buffers.get(*buffer_id)?;
        let guard = shared.read().expect("buffer poisoned");
        guard.snapshot.resolve_anchor(anchor)
    };
    let rel = build::relpath(git_root, ws.buffers.path_for(*buffer_id)?)?;
    ws.code_graph.symbol_at(build::file_id(&rel), offset)
}

/// The graph symbol whose definition encloses the cursor.
///
/// `None` when no editor is focused, the buffer has no file under the
/// workspace root, or the cursor lies outside every indexed definition.
pub(crate) fn symbol_at_cursor(stoat: &mut Stoat) -> Option<SymbolKey> {
    let (buffer_id, offset) = {
        let editor = action_handlers::focused_editor_mut(stoat)?;
        (editor.buffer_id, focused_offset(editor))
    };
    let ws = stoat.active_workspace();
    let path = ws.buffers.path_for(buffer_id)?;
    let rel = build::relpath(&ws.git_root, path)?;
    ws.code_graph.symbol_at(build::file_id(&rel), offset)
}

/// Jump to `key`'s definition: save the jumplist, open its file, and place
/// the cursor at the definition start.
///
/// A no-op when the key is unknown or its file has no recorded path.
pub(crate) fn jump_to_symbol(stoat: &mut Stoat, key: SymbolKey) -> UpdateEffect {
    let (def_start, path) = {
        let ws = stoat.active_workspace();
        let Some(symbol) = ws.code_graph.symbol(key) else {
            return UpdateEffect::None;
        };
        let Some(path) = ws.file_paths.get(&symbol.file).cloned() else {
            return UpdateEffect::None;
        };
        (symbol.def_range.start, path)
    };

    action_handlers::jump::push_jump(stoat);
    let target = stoat.active_workspace().panes.focus();
    crate::buffer_lifecycle::open_file_in_pane(stoat, target, &path);
    action_handlers::movement::jump_to_offset(stoat, def_start)
}

/// Navigate to `candidates`, jumping straight to a lone candidate or opening
/// the symbol picker to choose among several.
///
/// A no-op for an empty list. Each picker entry jumps via [`jump_to_symbol`]
/// when chosen, so a pick works even across files.
pub(crate) fn present_or_pick(stoat: &mut Stoat, candidates: Vec<SymbolKey>) -> UpdateEffect {
    match candidates.as_slice() {
        [] => UpdateEffect::None,
        [only] => jump_to_symbol(stoat, *only),
        _ => open_symbol_pick(stoat, candidates),
    }
}

/// Populate the symbol picker with `candidates` so the user can choose one.
fn open_symbol_pick(stoat: &mut Stoat, candidates: Vec<SymbolKey>) -> UpdateEffect {
    let anchor_offset = match action_handlers::focused_editor_mut(stoat) {
        Some(editor) => focused_offset(editor),
        None => return UpdateEffect::None,
    };

    let entries: Vec<SymbolEntry> = {
        let ws = stoat.active_workspace();
        candidates
            .into_iter()
            .filter_map(|key| {
                let symbol = ws.code_graph.symbol(key)?;
                let title = match ws.file_paths.get(&symbol.file) {
                    Some(path) => format!("{}  {}", symbol.name, path.display()),
                    None => symbol.name.clone(),
                };
                Some(SymbolEntry { title, symbol: key })
            })
            .collect()
    };
    if entries.is_empty() {
        return UpdateEffect::None;
    }

    stoat.pending_symbol_picker = Some(SymbolPicker {
        entries,
        anchor_offset,
        selected_idx: 0,
    });
    UpdateEffect::Redraw
}

/// The primary selection's block-cursor cell resolved to a buffer offset.
fn focused_offset(editor: &mut EditorState) -> usize {
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
    let head_off = buffer_snapshot.resolve_anchor(&sel.head());
    stoat_text::cursor_offset(buffer_snapshot.rope(), tail_off, head_off)
}

#[cfg(test)]
mod tests {
    use super::{
        build, goto_callee, goto_caller, jump_to_symbol, mark_trail_end, mark_trail_start,
        nearest_diff_target, present_or_pick, symbol_at_cursor, trail_clear, trail_next,
        trail_prev,
    };
    use crate::{
        action_handlers::dispatch,
        app::{Stoat, UpdateEffect},
        host::FakeFs,
        test_harness::TestHarness,
    };
    use codegraph::{
        Confidence, Dir, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey, Target,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::{ops::Range, path::PathBuf, sync::Arc};
    use stoat_action::GotoDiffCalleeDown;
    use stoat_config::Settings;
    use stoat_language::SymbolKind;
    use stoat_scheduler::TestScheduler;

    fn sym(key: u8, file: FileId, name: &str, def_range: Range<usize>) -> Symbol {
        Symbol {
            key: SymbolKey([key; 16]),
            file,
            name: name.to_string(),
            kind: SymbolKind::Function,
            container: vec![],
            def_range,
            name_range: 0..1,
            body_hash: [0u8; 32],
        }
    }

    fn foo_shard(file: FileId) -> FileShard {
        FileShard {
            content_hash: [0u8; 32],
            symbols: vec![sym(1, file, "foo", 0..11)],
            edges: vec![],
        }
    }

    fn stoat_with_repo() -> Stoat {
        let scheduler = Arc::new(TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;
        stoat
    }

    #[test]
    fn jump_to_symbol_opens_file_and_symbol_at_cursor_round_trips() {
        let mut stoat = stoat_with_repo();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
        stoat.set_fs_host(fs);

        let file = build::file_id("src/a.rs");
        let key = SymbolKey([1u8; 16]);
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(foo_shard(file));
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }

        assert_ne!(jump_to_symbol(&mut stoat, key), UpdateEffect::None);
        assert_eq!(
            symbol_at_cursor(&mut stoat),
            Some(key),
            "the jump lands the cursor inside the symbol, which resolves back to it",
        );
    }

    #[test]
    fn jump_to_symbol_is_noop_for_an_unknown_key() {
        let mut stoat = stoat_with_repo();
        assert_eq!(
            jump_to_symbol(&mut stoat, SymbolKey([9u8; 16])),
            UpdateEffect::None
        );
    }

    #[test]
    fn present_or_pick_one_jumps_without_a_picker() {
        let mut stoat = stoat_with_repo();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
        stoat.set_fs_host(fs);

        let file = build::file_id("src/a.rs");
        let key = SymbolKey([1u8; 16]);
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(foo_shard(file));
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }

        assert_ne!(present_or_pick(&mut stoat, vec![key]), UpdateEffect::None);
        assert!(
            stoat.pending_symbol_picker.is_none(),
            "a lone candidate jumps directly, opening no picker",
        );
        assert_eq!(symbol_at_cursor(&mut stoat), Some(key));
    }

    #[test]
    fn present_or_pick_many_opens_the_picker() {
        let mut stoat = stoat_with_repo();
        let file = build::file_id("src/a.rs");
        let (foo, bar) = (SymbolKey([1u8; 16]), SymbolKey([2u8; 16]));
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![sym(1, file, "foo", 0..11), sym(2, file, "bar", 12..23)],
                edges: vec![],
            });
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }

        present_or_pick(&mut stoat, vec![foo, bar]);
        let picker = stoat
            .pending_symbol_picker
            .as_ref()
            .expect("several candidates open the picker");
        assert_eq!(
            picker.entries.iter().map(|e| e.symbol).collect::<Vec<_>>(),
            vec![foo, bar],
            "nav picker entries carry their symbol key",
        );
    }

    /// The picker guard reads keys ahead of the keymap and closes on anything
    /// it does not name, so an arrow it does not name dismisses the picker.
    #[test]
    fn the_symbol_picker_steps_on_the_arrows() {
        let mut stoat = stoat_with_repo();
        let file = build::file_id("src/a.rs");
        let (foo, bar) = (SymbolKey([1u8; 16]), SymbolKey([2u8; 16]));
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![sym(1, file, "foo", 0..11), sym(2, file, "bar", 12..23)],
                edges: vec![],
            });
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }
        present_or_pick(&mut stoat, vec![foo, bar]);

        let press = |stoat: &mut Stoat, code| {
            stoat.update(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
            stoat
                .pending_symbol_picker
                .as_ref()
                .expect("the arrow steps the picker rather than closing it")
                .selected_idx
        };

        let after_down = press(&mut stoat, KeyCode::Down);
        let after_up = press(&mut stoat, KeyCode::Up);

        assert_eq!(
            (after_down, after_up),
            (1, 0),
            "down walks toward the last entry and up walks back"
        );
    }

    #[test]
    fn present_or_pick_empty_is_noop() {
        let mut stoat = stoat_with_repo();
        assert_eq!(present_or_pick(&mut stoat, vec![]), UpdateEffect::None);
    }

    #[test]
    fn goto_caller_and_callee_step_the_call_axis() {
        let mut stoat = stoat_with_repo();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn caller() {}\nfn callee() {}\n");
        stoat.set_fs_host(fs);

        let file = build::file_id("src/a.rs");
        let (caller, callee) = (SymbolKey([1u8; 16]), SymbolKey([2u8; 16]));
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![
                    sym(1, file, "caller", 0..14),
                    sym(2, file, "callee", 15..29),
                ],
                edges: vec![Edge {
                    from: caller,
                    to: Target::Sym(callee),
                    kind: EdgeKind::Calls,
                    site_range: 4..10,
                    confidence: Confidence::Resolved,
                }],
            });
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }

        jump_to_symbol(&mut stoat, callee);
        assert_eq!(symbol_at_cursor(&mut stoat), Some(callee));
        goto_caller(&mut stoat);
        assert_eq!(
            symbol_at_cursor(&mut stoat),
            Some(caller),
            "GotoCaller steps up to the calling symbol",
        );

        goto_callee(&mut stoat);
        assert_eq!(
            symbol_at_cursor(&mut stoat),
            Some(callee),
            "GotoCallee steps down to the called symbol",
        );
    }

    fn call_edge(from: SymbolKey, to: SymbolKey) -> Edge {
        Edge {
            from,
            to: Target::Sym(to),
            kind: EdgeKind::Calls,
            site_range: 0..1,
            confidence: Confidence::Resolved,
        }
    }

    #[test]
    fn nearest_diff_target_skips_unchanged_symbols() {
        let mut stoat = stoat_with_repo();
        let file = FileId(0);
        let (foo, bar, baz) = (
            SymbolKey([1u8; 16]),
            SymbolKey([2u8; 16]),
            SymbolKey([3u8; 16]),
        );
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![
                    sym(1, file, "foo", 0..10),
                    sym(2, file, "bar", 10..20),
                    sym(3, file, "baz", 20..30),
                ],
                edges: vec![call_edge(foo, bar), call_edge(bar, baz)],
            });
            ws.changed_ranges.insert(file, vec![0..10, 20..30]);
        }

        assert_eq!(
            nearest_diff_target(stoat.active_workspace(), baz, Dir::Up),
            Some(foo),
            "skips the unchanged caller bar and lands on the changed caller foo",
        );
    }

    /// A repo with a four-deep call chain, the cursor parked at its unchanged
    /// head, and answers `baz`, the first changed symbol below it.
    ///
    /// `foo` calls `bar` calls `baz` calls `qux`, of which only the last two
    /// carry a working-tree change. A hop down from `foo` has an unchanged
    /// neighbor to skip past, and one landing on `baz` still has `qux` below it
    /// to move on to.
    fn stage_changed_callee(h: &mut TestHarness) -> SymbolKey {
        h.stage_review_scenario(
            "/repo",
            &[(
                "a.rs",
                "fn foo() {}\nfn bar() {}\nfn baz() {}\nfn qux() {}\n",
                "fn foo() {}\nfn bar() {}\nfn baz() { z(); }\nfn qux() { z(); }\n",
            )],
        );

        let file = build::file_id("a.rs");
        let (foo, bar, baz, qux) = (
            SymbolKey([1u8; 16]),
            SymbolKey([2u8; 16]),
            SymbolKey([3u8; 16]),
            SymbolKey([4u8; 16]),
        );
        {
            let ws = h.stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![
                    sym(1, file, "foo", 0..12),
                    sym(2, file, "bar", 12..24),
                    sym(3, file, "baz", 24..42),
                    sym(4, file, "qux", 42..60),
                ],
                edges: vec![
                    call_edge(foo, bar),
                    call_edge(bar, baz),
                    call_edge(baz, qux),
                ],
            });
            ws.file_paths.insert(file, PathBuf::from("a.rs"));
        }

        jump_to_symbol(&mut h.stoat, foo);
        assert_eq!(symbol_at_cursor(&mut h.stoat), Some(foo), "parked in foo");
        baz
    }

    /// Every press used to walk the whole changeset on the run loop. That is the
    /// repo's status, a HEAD blob and a disk read per changed file, and a diff
    /// each.
    ///
    /// What this pins is the deferral rather than the thread. The test scheduler
    /// runs a blocking closure inline, so the scan's work happens on this stack
    /// either way. When the cursor moves is what tells the two apart.
    #[test]
    fn a_diff_filtered_hop_lands_when_its_scan_does() {
        let mut h = TestHarness::with_size(40, 20);
        let baz = stage_changed_callee(&mut h);

        dispatch(&mut h.stoat, &GotoDiffCalleeDown);
        assert_eq!(
            symbol_at_cursor(&mut h.stoat),
            Some(SymbolKey([1u8; 16])),
            "the keypress moves the cursor nowhere itself",
        );
        assert!(
            h.stoat.pending_diff_nav_jump.is_some(),
            "it leaves a scan for the pump to apply",
        );

        h.settle();
        assert_eq!(
            symbol_at_cursor(&mut h.stoat),
            Some(baz),
            "which is where the hop lands, skipping past the unchanged neighbor",
        );
        assert!(
            !h.stoat.active_workspace().changed_ranges.is_empty(),
            "the landed scan installs the ranges it found",
        );
        assert!(
            h.stoat.pending_diff_nav_jump.is_none(),
            "and the scan is spent once applied",
        );
    }

    /// Two presses inside one scan window ask the same question of the same
    /// tree, so the second replaces the first rather than queueing behind it.
    #[test]
    fn a_second_hop_press_replaces_the_scan_in_flight() {
        let mut h = TestHarness::with_size(40, 20);
        let baz = stage_changed_callee(&mut h);
        let jumps = |h: &TestHarness| {
            let panes = &h.stoat.active_workspace().panes;
            panes.pane(panes.focus()).jumplist.entries().len()
        };
        let before = jumps(&h);

        dispatch(&mut h.stoat, &GotoDiffCalleeDown);
        dispatch(&mut h.stoat, &GotoDiffCalleeDown);
        h.settle();

        assert_eq!(
            symbol_at_cursor(&mut h.stoat),
            Some(baz),
            "both presses left foo, so the pair lands one hop down",
        );
        assert_eq!(
            jumps(&h) - before,
            1,
            "the burst records one jump, not one per press",
        );
    }

    #[test]
    fn trail_walks_the_path_between_two_marks() {
        let mut stoat = stoat_with_repo();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\nfn bar() {}\nfn baz() {}\n");
        stoat.set_fs_host(fs);

        let file = build::file_id("src/a.rs");
        let (foo, bar, baz) = (
            SymbolKey([1u8; 16]),
            SymbolKey([2u8; 16]),
            SymbolKey([3u8; 16]),
        );
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![
                    sym(1, file, "foo", 0..11),
                    sym(2, file, "bar", 12..23),
                    sym(3, file, "baz", 24..35),
                ],
                edges: vec![call_edge(foo, bar), call_edge(bar, baz)],
            });
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }

        jump_to_symbol(&mut stoat, foo);
        mark_trail_start(&mut stoat);
        jump_to_symbol(&mut stoat, baz);
        mark_trail_end(&mut stoat);
        assert_eq!(
            symbol_at_cursor(&mut stoat),
            Some(foo),
            "marking the end starts the trail at the start symbol",
        );

        trail_next(&mut stoat);
        assert_eq!(
            symbol_at_cursor(&mut stoat),
            Some(bar),
            "TrailNext visits bar between the endpoints",
        );

        trail_next(&mut stoat);
        assert_eq!(
            symbol_at_cursor(&mut stoat),
            Some(baz),
            "TrailNext reaches baz"
        );

        trail_prev(&mut stoat);
        assert_eq!(
            symbol_at_cursor(&mut stoat),
            Some(bar),
            "TrailPrev steps back to bar",
        );
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("trail 2/3: bar"),
            "each step says where along the trail it landed and on what",
        );
    }

    /// Which end a reader marks first says nothing about which one calls the
    /// other, so marking the callee first has to find the same code path. It
    /// reads from the mark they made first, which is the direction they asked
    /// to walk it in.
    #[test]
    fn a_trail_marked_end_first_walks_the_same_path_backward() {
        let mut stoat = stoat_with_repo();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\nfn bar() {}\nfn baz() {}\n");
        stoat.set_fs_host(fs);

        let file = build::file_id("src/a.rs");
        let (foo, bar, baz) = (
            SymbolKey([1u8; 16]),
            SymbolKey([2u8; 16]),
            SymbolKey([3u8; 16]),
        );
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![
                    sym(1, file, "foo", 0..11),
                    sym(2, file, "bar", 12..23),
                    sym(3, file, "baz", 24..35),
                ],
                edges: vec![call_edge(foo, bar), call_edge(bar, baz)],
            });
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }

        // baz is the callee, and it is marked first.
        jump_to_symbol(&mut stoat, baz);
        mark_trail_start(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("trail start marked"),
            "the start gives a sign it was taken",
        );
        assert!(
            stoat
                .active_workspace()
                .trail
                .as_ref()
                .is_some_and(|trail| trail.is_armed()),
            "a start alone arms the trail without anything to walk yet",
        );

        jump_to_symbol(&mut stoat, foo);
        mark_trail_end(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("trail: 3 stops"),
            "the reversed pair still relates, and the count says so",
        );
        assert_eq!(
            symbol_at_cursor(&mut stoat),
            Some(baz),
            "the trail starts where the reader marked first",
        );

        trail_next(&mut stoat);
        assert_eq!(symbol_at_cursor(&mut stoat), Some(bar));
        trail_next(&mut stoat);
        assert_eq!(
            symbol_at_cursor(&mut stoat),
            Some(foo),
            "and runs back up the call chain to the end mark",
        );
    }

    /// The badge is the only sign a trail is live, so its label has to say
    /// which of the two states it is in. A half-marked trail has no position
    /// to report, and a walked one has nothing else worth the room.
    #[test]
    fn the_badge_names_the_trail_state_it_is_in() {
        let mut stoat = stoat_with_repo();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\nfn bar() {}\n");
        stoat.set_fs_host(fs);

        let file = build::file_id("src/a.rs");
        let (foo, bar) = (SymbolKey([1u8; 16]), SymbolKey([2u8; 16]));
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![sym(1, file, "foo", 0..11), sym(2, file, "bar", 12..23)],
                edges: vec![call_edge(foo, bar)],
            });
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }
        let label = |stoat: &Stoat| {
            stoat
                .active_workspace()
                .trail
                .as_ref()
                .map(|trail| trail.badge().label)
        };

        assert_eq!(label(&stoat), None, "no trail, no badge");

        jump_to_symbol(&mut stoat, foo);
        mark_trail_start(&mut stoat);
        assert_eq!(
            label(&stoat).as_deref(),
            Some("trail armed"),
            "a start alone has no position to report",
        );

        jump_to_symbol(&mut stoat, bar);
        mark_trail_end(&mut stoat);
        assert_eq!(
            label(&stoat).as_deref(),
            Some("trail 1/2"),
            "computing the path parks the reader on its first stop",
        );

        trail_next(&mut stoat);
        assert_eq!(
            label(&stoat).as_deref(),
            Some("trail 2/2"),
            "and the label follows every step",
        );

        trail_clear(&mut stoat);
        assert_eq!(label(&stoat), None, "clearing takes the badge with it");
    }

    /// Marking a new start replaces a trail, so before this the only way to be
    /// rid of one was to make another. A badge showing the active trail has
    /// nothing to go dark for until a trail simply ends.
    #[test]
    fn clearing_drops_the_trail_and_says_so() {
        let mut stoat = stoat_with_repo();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\n");
        stoat.set_fs_host(fs);

        let file = build::file_id("src/a.rs");
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![sym(1, file, "foo", 0..11)],
                edges: vec![],
            });
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }

        assert_eq!(
            trail_clear(&mut stoat),
            UpdateEffect::None,
            "nothing to clear reports nothing",
        );

        jump_to_symbol(&mut stoat, SymbolKey([1u8; 16]));
        mark_trail_start(&mut stoat);
        assert!(stoat.active_workspace().trail.is_some());

        assert_eq!(trail_clear(&mut stoat), UpdateEffect::Redraw);
        assert!(
            stoat.active_workspace().trail.is_none(),
            "the trail is gone rather than replaced by another",
        );
        assert_eq!(stoat.pending_message.as_deref(), Some("trail cleared"));
    }

    /// Two ways a trail comes to nothing, which used to look identical from
    /// the outside. Both were a silent no-op.
    #[test]
    fn every_trail_dead_end_says_which_one_it_was() {
        let mut stoat = stoat_with_repo();
        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/src/a.rs", "fn foo() {}\nfn bar() {}\n");
        stoat.set_fs_host(fs);

        let file = build::file_id("src/a.rs");
        let (foo, bar) = (SymbolKey([1u8; 16]), SymbolKey([2u8; 16]));
        {
            let ws = stoat.active_workspace_mut();
            ws.code_graph.insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![sym(1, file, "foo", 0..11), sym(2, file, "bar", 12..23)],
                edges: vec![],
            });
            ws.file_paths.insert(file, PathBuf::from("src/a.rs"));
        }

        jump_to_symbol(&mut stoat, foo);
        mark_trail_end(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("no trail start marked"),
            "an end with no start says so rather than doing nothing",
        );

        mark_trail_start(&mut stoat);
        jump_to_symbol(&mut stoat, bar);
        mark_trail_end(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("no call relation; direct trail"),
            "two unrelated symbols still give a trail, named as the fallback \
             it is rather than passing for a two-stop path",
        );
    }
}
