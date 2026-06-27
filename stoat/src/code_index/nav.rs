//! Cursor-to-symbol resolution and symbol jumps over the code graph.
//!
//! These are the shared entry points the graph-navigation actions build on.
//! They resolve the cursor to a [`SymbolKey`] and jump to a symbol's
//! definition.

use crate::{
    action_handlers,
    action_handlers::lsp::{SymbolEntry, SymbolPicker},
    app::{Stoat, UpdateEffect},
    code_index::build,
    editor_state::EditorState,
    workspace::Workspace,
};
use codegraph::{Dir, EdgeKind, SymbolKey};
use std::path::Path;
use stoat_text::{Anchor, Bias, BufferId};

/// How far the diff-filtered hops search before giving up.
const MAX_DIFF_HOPS: usize = 64;

/// A call-graph trail between two marked points.
///
/// Holds the start anchor (read back by [`mark_trail_end`] to resolve the
/// start symbol) and, once the end is marked, the cached path between the
/// enclosing symbols plus the current position along it. While only the
/// start is marked, `path` is empty.
pub(crate) struct TrailState {
    start: (BufferId, Anchor),
    path: Vec<SymbolKey>,
    idx: usize,
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

/// Refresh the working-tree diff, then walk the call axis to the nearest
/// changed symbol and jump there.
fn goto_nearest_diff(stoat: &mut Stoat, dir: Dir) -> UpdateEffect {
    let Some(start) = symbol_at_cursor(stoat) else {
        return UpdateEffect::None;
    };
    {
        let git = stoat.git_host.clone();
        let fs = stoat.fs_host.clone();
        let langs = stoat.language_registry.clone();
        stoat
            .active_workspace_mut()
            .refresh_changed_ranges(git.as_ref(), fs.as_ref(), &langs);
    }
    let Some(target) = nearest_diff_target(stoat.active_workspace(), start, dir) else {
        return UpdateEffect::None;
    };
    jump_to_symbol(stoat, target)
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
        start,
        path: Vec::new(),
        idx: 0,
    });
    UpdateEffect::None
}

/// Mark the end of a trail, compute the call-graph path between the symbols
/// enclosing the two marks, and jump to the start.
///
/// Falls back to a direct two-point path when no call path connects them.
/// A no-op when no start is marked or either end is on no indexed symbol.
pub(crate) fn mark_trail_end(stoat: &mut Stoat) -> UpdateEffect {
    let Some(end) = focused_anchor(stoat) else {
        return UpdateEffect::None;
    };
    let Some(start) = stoat
        .active_workspace()
        .trail
        .as_ref()
        .map(|trail| trail.start)
    else {
        return UpdateEffect::None;
    };

    let path = {
        let ws = stoat.active_workspace();
        let git_root = ws.git_root.clone();
        let (Some(sym_a), Some(sym_b)) = (
            resolve_to_symbol(ws, &git_root, &start),
            resolve_to_symbol(ws, &git_root, &end),
        ) else {
            return UpdateEffect::None;
        };
        ws.code_graph
            .path_between(sym_a, sym_b, EdgeKind::Calls)
            .unwrap_or_else(|| vec![sym_a, sym_b])
    };

    let first = path.first().copied();
    stoat.active_workspace_mut().trail = Some(TrailState {
        start,
        path,
        idx: 0,
    });
    match first {
        Some(key) => jump_to_symbol(stoat, key),
        None => UpdateEffect::None,
    }
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
    let target = {
        let Some(trail) = stoat.active_workspace_mut().trail.as_mut() else {
            return UpdateEffect::None;
        };
        if trail.path.is_empty() {
            return UpdateEffect::None;
        }
        let last = (trail.path.len() - 1) as isize;
        trail.idx = (trail.idx as isize + delta).clamp(0, last) as usize;
        trail.path[trail.idx]
    };
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

    if let Some(editor) = action_handlers::focused_editor_mut(stoat) {
        let offset = focused_offset(editor);
        editor.jumplist.save(offset);
    }
    let target = stoat.active_workspace().panes.focus();
    action_handlers::file::open_file_in_pane(stoat, target, &path);
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
                Some(SymbolEntry {
                    title,
                    anchor_offset: symbol.def_range.start,
                    symbol: Some(key),
                })
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

/// The primary selection head resolved to a buffer offset.
fn focused_offset(editor: &mut EditorState) -> usize {
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let head = editor.selections.newest_anchor().head();
    buffer_snapshot.resolve_anchor(&head)
}

#[cfg(test)]
mod tests {
    use super::{
        build, goto_callee, goto_caller, jump_to_symbol, mark_trail_end, mark_trail_start,
        nearest_diff_target, present_or_pick, symbol_at_cursor, trail_next, trail_prev,
    };
    use crate::{
        app::{Stoat, UpdateEffect},
        host::FakeFs,
    };
    use codegraph::{
        Confidence, Dir, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey, Target,
    };
    use std::{ops::Range, path::PathBuf, sync::Arc};
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
        assert_eq!(picker.entries.len(), 2);
        assert!(
            picker.entries.iter().all(|e| e.symbol.is_some()),
            "nav picker entries carry their symbol key",
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
    }
}
