//! Cursor-to-symbol resolution and symbol jumps over the code graph.
//!
//! These are the shared entry points the graph-navigation actions build on.
//! They resolve the cursor to a [`SymbolKey`] and jump to a symbol's
//! definition.

// FIXME: drop this allow once the navigation actions (GotoCaller/Callee,
// diff hops, trail) dispatch into these entry points. They are the shared
// nav API, with no action wired to them yet.
#![allow(dead_code)]

use crate::{
    action_handlers,
    action_handlers::lsp::{SymbolEntry, SymbolPicker},
    app::{Stoat, UpdateEffect},
    code_index::build,
    editor_state::EditorState,
};
use codegraph::SymbolKey;

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
    use super::{build, jump_to_symbol, present_or_pick, symbol_at_cursor};
    use crate::{
        app::{Stoat, UpdateEffect},
        host::FakeFs,
    };
    use codegraph::{FileId, FileShard, Symbol, SymbolKey};
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
}
