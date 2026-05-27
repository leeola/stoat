use crate::{editor::Editor, workspace::Workspace};
use gpui::{Entity, WeakEntity, Window};
use lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, TextDocumentIdentifier,
    TextDocumentPositionParams, Uri,
};
use std::{path::PathBuf, str::FromStr};
use stoat::{
    host::{LanguageServerFeature, LspServer, OffsetEncoding},
    lsp::util::{byte_offset_to_lsp_pos, lsp_pos_to_byte_offset},
};
use stoat_text::Bias;

/// Distinguishes the three LSP "jump" methods so the same task body
/// can dispatch to whichever one the caller asked for. The picker
/// pattern for `textDocument/references` is intentionally NOT
/// included here -- references is a multi-result picker, not a
/// single jump, and lives in a separate item.
#[derive(Clone, Copy, Debug)]
pub enum LspGotoKind {
    Definition,
    TypeDefinition,
    Implementation,
}

impl LspGotoKind {
    fn feature(self) -> LanguageServerFeature {
        match self {
            Self::Definition => LanguageServerFeature::GotoDefinition,
            Self::TypeDefinition => LanguageServerFeature::GotoTypeDefinition,
            Self::Implementation => LanguageServerFeature::GotoImplementation,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::TypeDefinition => "textDocument/typeDefinition",
            Self::Implementation => "textDocument/implementation",
        }
    }
}

/// Issue an LSP jump-style request for the active editor's primary
/// cursor and move the cursor (in the active editor, when the
/// target is the same file; in a freshly-opened editor otherwise)
/// to the response's first location.
///
/// No-op when no editor is active, the buffer has no path, the
/// language registry has no entry for the path, or the server does
/// not advertise the matching feature. Errors and empty responses
/// are logged via tracing and swallowed -- the cursor stays put.
///
/// FIXME: launches a fresh language server per request (same caveat
/// as the popups); the per-language LSP server cache will fix every
/// LSP callsite at once.
pub fn spawn_goto(
    workspace: &mut Workspace,
    kind: LspGotoKind,
    window: &mut Window,
    cx: &mut gpui::Context<'_, Workspace>,
) {
    let Some(editor) = workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|w| w.upgrade())
    else {
        return;
    };
    let Some(path) = editor
        .read(cx)
        .file_path()
        .map(std::path::Path::to_path_buf)
    else {
        return;
    };
    let registry = &cx.global::<crate::globals::LanguageRegistry>().0;
    let Some(language) = registry.for_path(&path) else {
        return;
    };
    let host = cx.global::<crate::globals::LspHostGlobal>().0.clone();
    let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
    let rope = mb_snapshot.rope().clone();
    let Some(primary) = editor.read(cx).selections().all_anchors().first().cloned() else {
        return;
    };
    let cursor_offset = mb_snapshot.resolve_anchor(&primary.head());
    let Some(source_uri) = path_to_file_uri(&path) else {
        return;
    };
    let workspace_root = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| path.clone());

    let weak_workspace = cx.weak_entity();
    let weak_editor = editor.downgrade();
    let source_path = path.clone();
    let request_id = workspace.bump_lsp_goto_request_id();
    cx.spawn_in(window, async move |_, cx| {
        let server = match host.launch(&language, &workspace_root).await {
            Ok(s) => std::sync::Arc::<dyn LspServer>::from(s),
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::lsp::goto",
                    ?err,
                    "failed to launch LSP server for goto"
                );
                return;
            },
        };
        let _ = server.initialize(Some(source_uri.clone())).await;
        if !server.supports_feature(kind.feature()) {
            return;
        }
        let encoding = server.offset_encoding();
        let position = byte_offset_to_lsp_pos(&rope, cursor_offset, encoding);
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: source_uri },
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let result = match kind {
            LspGotoKind::Definition => server.goto_definition(params).await,
            LspGotoKind::TypeDefinition => server.goto_type_definition(params).await,
            LspGotoKind::Implementation => server.goto_implementation(params).await,
        };
        let response = match result {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::lsp::goto",
                    request = kind.label(),
                    ?err,
                    "lsp goto request failed",
                );
                return;
            },
        };
        let Some(target) = first_jump_target(response) else {
            return;
        };

        let _ = weak_workspace.update_in(cx, |workspace, _window, cx| {
            if request_id != workspace.lsp_goto_request_id() {
                // A newer goto request superseded this one before the
                // response arrived; dropping the stale reply leaves the
                // cursor where the user has since moved it.
                return;
            }
            apply_jump(
                workspace,
                &weak_editor,
                &source_path,
                target,
                encoding,
                &rope,
                cx,
            );
        });
    })
    .detach();
}

fn apply_jump(
    workspace: &mut Workspace,
    active_editor: &WeakEntity<Editor>,
    source_path: &std::path::Path,
    target: (PathBuf, lsp_types::Position),
    encoding: OffsetEncoding,
    source_rope: &stoat_text::Rope,
    cx: &mut gpui::Context<'_, Workspace>,
) {
    let (target_path, target_pos) = target;
    if target_path == source_path {
        let Some(editor) = active_editor.upgrade() else {
            return;
        };
        let offset = lsp_pos_to_byte_offset(source_rope, target_pos, encoding);
        set_cursor_to_offset(&editor, offset, cx);
        return;
    }
    workspace.open_paths(&[target_path.clone()], cx);
    let Some(editor) = workspace
        .buffer_for_path(&target_path, cx)
        .and_then(|buffer| editor_for_buffer(workspace, &buffer, cx))
    else {
        return;
    };
    let mb_snapshot = editor.read(cx).multi_buffer().read(cx).snapshot();
    let target_rope = mb_snapshot.rope().clone();
    let offset = lsp_pos_to_byte_offset(&target_rope, target_pos, encoding);
    set_cursor_to_offset(&editor, offset, cx);
}

fn set_cursor_to_offset(
    editor: &Entity<Editor>,
    offset: usize,
    cx: &mut gpui::Context<'_, Workspace>,
) {
    editor.update(cx, |ed, cx| {
        let snapshot = ed.multi_buffer().read(cx).snapshot();
        let anchor = snapshot.anchor_at(offset, Bias::Left);
        let new_id = ed
            .selections()
            .all_anchors()
            .iter()
            .map(|s| s.id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        let selection = stoat_text::Selection {
            id: new_id,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: stoat_text::SelectionGoal::None,
        };
        ed.selections_mut().replace_with(vec![selection], &snapshot);
    });
}

/// Walk the workspace pane tree and return the editor entity whose
/// multi-buffer's singleton buffer matches `buffer`. Linear-scan; the
/// pane tree is small in practice. Returns `None` when no editor
/// references that buffer (the buffer might be open but its editor
/// not yet attached to a pane).
fn editor_for_buffer(
    workspace: &Workspace,
    buffer: &Entity<crate::buffer::Buffer>,
    cx: &gpui::App,
) -> Option<Entity<Editor>> {
    let target_id = buffer.entity_id();
    let pane_tree = workspace.pane_tree().read(cx);
    for pane_id in pane_tree.split_pane_ids() {
        let pane = pane_tree.pane(pane_id)?;
        for item in pane.read(cx).items() {
            let Ok(editor) = item.to_any_view().downcast::<Editor>() else {
                continue;
            };
            let mb_singleton = editor
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .cloned();
            if mb_singleton.as_ref().map(Entity::entity_id) == Some(target_id) {
                return Some(editor);
            }
        }
    }
    None
}

fn first_jump_target(response: GotoDefinitionResponse) -> Option<(PathBuf, lsp_types::Position)> {
    let (uri, position) = match response {
        GotoDefinitionResponse::Scalar(loc) => (loc.uri, loc.range.start),
        GotoDefinitionResponse::Array(locs) => {
            let loc = locs.into_iter().next()?;
            (loc.uri, loc.range.start)
        },
        GotoDefinitionResponse::Link(links) => {
            let link = links.into_iter().next()?;
            (link.target_uri, link.target_range.start)
        },
    };
    let path = uri_to_path(&uri)?;
    Some((path, position))
}

fn path_to_file_uri(path: &std::path::Path) -> Option<Uri> {
    let s = path.to_str()?;
    Uri::from_str(&format!("file://{s}")).ok()
}

fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://").unwrap_or(s);
    Some(PathBuf::from(stripped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Location, LocationLink, Position, Range};

    fn uri(s: &str) -> Uri {
        Uri::from_str(s).unwrap()
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn rng(start: Position, end: Position) -> Range {
        Range { start, end }
    }

    #[test]
    fn first_jump_target_scalar() {
        let response = GotoDefinitionResponse::Scalar(Location {
            uri: uri("file:///tmp/main.rs"),
            range: rng(pos(2, 3), pos(2, 7)),
        });
        let (path, position) = first_jump_target(response).expect("scalar yields a target");
        assert_eq!(path, PathBuf::from("/tmp/main.rs"));
        assert_eq!(position, pos(2, 3));
    }

    #[test]
    fn first_jump_target_array_takes_first() {
        let response = GotoDefinitionResponse::Array(vec![
            Location {
                uri: uri("file:///tmp/a.rs"),
                range: rng(pos(0, 0), pos(0, 1)),
            },
            Location {
                uri: uri("file:///tmp/b.rs"),
                range: rng(pos(5, 5), pos(5, 6)),
            },
        ]);
        let (path, position) = first_jump_target(response).expect("array yields a target");
        assert_eq!(path, PathBuf::from("/tmp/a.rs"));
        assert_eq!(position, pos(0, 0));
    }

    #[test]
    fn first_jump_target_link_uses_target_range() {
        let response = GotoDefinitionResponse::Link(vec![LocationLink {
            origin_selection_range: None,
            target_uri: uri("file:///tmp/lib.rs"),
            target_range: rng(pos(8, 0), pos(8, 4)),
            target_selection_range: rng(pos(8, 0), pos(8, 4)),
        }]);
        let (path, position) = first_jump_target(response).expect("link yields a target");
        assert_eq!(path, PathBuf::from("/tmp/lib.rs"));
        assert_eq!(position, pos(8, 0));
    }

    #[test]
    fn first_jump_target_empty_array_is_none() {
        let response = GotoDefinitionResponse::Array(Vec::new());
        assert!(first_jump_target(response).is_none());
    }

    #[test]
    fn uri_to_path_strips_file_scheme() {
        assert_eq!(
            uri_to_path(&uri("file:///tmp/a.rs")),
            Some(PathBuf::from("/tmp/a.rs"))
        );
    }

    #[test]
    fn path_to_file_uri_roundtrips() {
        let original = std::path::Path::new("/tmp/example.rs");
        let u = path_to_file_uri(original).expect("path encodes");
        assert_eq!(u.as_str(), "file:///tmp/example.rs");
    }
}
