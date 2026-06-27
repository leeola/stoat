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

/// The primary selection head resolved to a buffer offset.
fn focused_offset(editor: &mut EditorState) -> usize {
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let head = editor.selections.newest_anchor().head();
    buffer_snapshot.resolve_anchor(&head)
}

#[cfg(test)]
mod tests {
    use super::{build, jump_to_symbol, symbol_at_cursor};
    use crate::{
        app::{Stoat, UpdateEffect},
        host::FakeFs,
    };
    use codegraph::{FileId, FileShard, Symbol, SymbolKey};
    use std::{path::PathBuf, sync::Arc};
    use stoat_config::Settings;
    use stoat_language::SymbolKind;
    use stoat_scheduler::TestScheduler;

    fn foo_shard(file: FileId) -> FileShard {
        FileShard {
            content_hash: [0u8; 32],
            symbols: vec![Symbol {
                key: SymbolKey([1u8; 16]),
                file,
                name: "foo".to_string(),
                kind: SymbolKind::Function,
                container: vec![],
                def_range: 0..11,
                name_range: 3..6,
                body_hash: [0u8; 32],
            }],
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
}
