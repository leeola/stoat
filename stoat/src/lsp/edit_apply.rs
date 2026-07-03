// Consumers of this module (`rename_symbol`, `code_action.apply`,
// completion items with multi-edit `text_edits`, the
// `workspace/applyEdit` reply path) do not exist yet; the trait
// surface and FakeLsp infrastructure below land in this commit
// and the consumers wire in as they are built. `#[allow(dead_code)]`
// covers the gap until the first caller arrives.
#![allow(dead_code)]

//! Apply an LSP [`WorkspaceEdit`] to stoat's buffer state plus the
//! filesystem (via [`crate::host::FsHost`]).
//!
//! `WorkspaceEdit` carries three mutually-exclusive carriers:
//! [`changes`] (per-doc text edits, no resource ops), [`document_changes`]
//! `Edits` (per-doc text edits with versioning), and [`document_changes`]
//! `Operations` (text edits mixed with create / rename / delete file
//! operations). Per LSP precedence, `document_changes` wins when both are
//! present.
//!
//! Edits within a single document apply right-to-left so earlier byte
//! offsets stay valid through the run. Across documents, application is
//! best-effort: URI / range / file-existence errors surface before any
//! buffer is mutated, but a `Buffer::edit` that succeeds before a later
//! one fails leaves a half-applied state -- stoat cannot truly roll back
//! a successful edit. Mirrors the multi-cursor edit pattern under
//! `delete_selection` / `align_selections`.
//!
//! Buffer resolution: an unopened path is read through [`FsHost::read`]
//! and seeded into [`crate::buffer_registry::BufferRegistry`] before the
//! edit fires. A `Rename` op also updates the registry's path mapping so
//! the buffer remains addressable by its new path.

use crate::{app::Stoat, buffer::BufferId, lsp::util::lsp_range_to_byte_range};
use lsp_types::{
    DocumentChangeOperation, DocumentChanges, ResourceOp, TextEdit, Uri, WorkspaceEdit,
};
use snafu::{ResultExt, Snafu};
use std::path::{Path, PathBuf};

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum WorkspaceEditError {
    #[snafu(display("WorkspaceEdit URI must use the file: scheme: {uri}"))]
    UriNotFile {
        uri: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to read {} for WorkspaceEdit application", path.display()))]
    PathReadFailed {
        source: std::io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to create file {}", path.display()))]
    ResourceCreate {
        source: std::io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to rename {} -> {}", from.display(), to.display()))]
    ResourceRename {
        source: std::io::Error,
        from: PathBuf,
        to: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("Failed to delete file {}", path.display()))]
    ResourceDelete {
        source: std::io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

#[derive(Debug, Default, Clone)]
pub struct WorkspaceEditOutcome {
    pub buffers_edited: Vec<BufferId>,
    pub files_created: Vec<PathBuf>,
    pub files_renamed: Vec<(PathBuf, PathBuf)>,
    pub files_deleted: Vec<PathBuf>,
}

/// Apply `edit` to the workspace.
///
/// Per LSP precedence, [`WorkspaceEdit::document_changes`] takes priority
/// over [`WorkspaceEdit::changes`] when both are present. The legacy
/// `changes` map carries no version information and no resource ops,
/// matching `Some(DocumentChanges::Edits(...))`'s behaviour without the
/// version.
pub fn apply_workspace_edit(
    stoat: &mut Stoat,
    edit: WorkspaceEdit,
) -> Result<WorkspaceEditOutcome, WorkspaceEditError> {
    let mut outcome = WorkspaceEditOutcome::default();
    if let Some(changes) = edit.document_changes {
        match changes {
            DocumentChanges::Edits(text_doc_edits) => {
                validate_uris(text_doc_edits.iter().map(|e| &e.text_document.uri))?;
                for tde in text_doc_edits {
                    let path = uri_to_path(&tde.text_document.uri)?;
                    let edits: Vec<TextEdit> = tde
                        .edits
                        .into_iter()
                        .map(|annotated| match annotated {
                            lsp_types::OneOf::Left(e) => e,
                            lsp_types::OneOf::Right(annotated) => annotated.text_edit,
                        })
                        .collect();
                    let id = apply_text_edits_to_buffer(stoat, &path, edits)?;
                    outcome.buffers_edited.push(id);
                }
            },
            DocumentChanges::Operations(ops) => {
                for op in &ops {
                    if let DocumentChangeOperation::Edit(tde) = op {
                        validate_uris(std::iter::once(&tde.text_document.uri))?;
                    }
                }
                for op in ops {
                    match op {
                        DocumentChangeOperation::Edit(tde) => {
                            let path = uri_to_path(&tde.text_document.uri)?;
                            let edits: Vec<TextEdit> = tde
                                .edits
                                .into_iter()
                                .map(|annotated| match annotated {
                                    lsp_types::OneOf::Left(e) => e,
                                    lsp_types::OneOf::Right(annotated) => annotated.text_edit,
                                })
                                .collect();
                            let id = apply_text_edits_to_buffer(stoat, &path, edits)?;
                            outcome.buffers_edited.push(id);
                        },
                        DocumentChangeOperation::Op(resource_op) => {
                            apply_resource_op(stoat, resource_op, &mut outcome)?;
                        },
                    }
                }
            },
        }
    } else if let Some(map) = edit.changes {
        validate_uris(map.keys())?;
        let mut entries: Vec<(Uri, Vec<TextEdit>)> = map.into_iter().collect();
        entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        for (uri, edits) in entries {
            let path = uri_to_path(&uri)?;
            let id = apply_text_edits_to_buffer(stoat, &path, edits)?;
            outcome.buffers_edited.push(id);
        }
    }
    Ok(outcome)
}

fn validate_uris<'a, I>(uris: I) -> Result<(), WorkspaceEditError>
where
    I: IntoIterator<Item = &'a Uri>,
{
    for uri in uris {
        uri_to_path(uri)?;
    }
    Ok(())
}

fn uri_to_path(uri: &Uri) -> Result<PathBuf, WorkspaceEditError> {
    if uri.scheme().map(|s| s.as_str()) != Some("file") {
        return UriNotFileSnafu {
            uri: uri.as_str().to_string(),
        }
        .fail();
    }
    Ok(PathBuf::from(uri.path().as_str()))
}

/// Apply a set of LSP [`TextEdit`]s to the buffer for `path`, opening it
/// if needed. Edits are sorted descending and applied right-to-left so
/// earlier ranges keep their offsets, converting each range against the
/// live rope. The spec guarantees the edits do not overlap.
pub(crate) fn apply_text_edits_to_buffer(
    stoat: &mut Stoat,
    path: &Path,
    edits: Vec<TextEdit>,
) -> Result<BufferId, WorkspaceEditError> {
    let buffer_id = ensure_open(stoat, path)?;
    if edits.is_empty() {
        return Ok(buffer_id);
    }
    let encoding = stoat.lsp_host.offset_encoding();
    let buffer = stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer was just opened");
    let mut sorted = edits;
    sorted.sort_by(|a, b| {
        b.range
            .start
            .line
            .cmp(&a.range.start.line)
            .then_with(|| b.range.start.character.cmp(&a.range.start.character))
    });
    let mut guard = buffer.write().expect("buffer poisoned");
    for edit in sorted {
        let byte_range = lsp_range_to_byte_range(guard.rope(), edit.range, encoding);
        guard.edit(byte_range, &edit.new_text);
    }
    Ok(buffer_id)
}

fn ensure_open(stoat: &mut Stoat, path: &Path) -> Result<BufferId, WorkspaceEditError> {
    if let Some(id) = stoat.active_workspace().buffers.id_for_path(path) {
        return Ok(id);
    }
    let mut bytes = Vec::new();
    stoat
        .fs_host
        .read(path, &mut bytes)
        .with_context(|_| PathReadFailedSnafu {
            path: path.to_path_buf(),
        })?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let workspace = stoat.active_workspace_mut();
    let (id, _buffer) = workspace.buffers.open(path, &text);
    Ok(id)
}

fn apply_resource_op(
    stoat: &mut Stoat,
    op: ResourceOp,
    outcome: &mut WorkspaceEditOutcome,
) -> Result<(), WorkspaceEditError> {
    match op {
        ResourceOp::Create(create) => {
            let path = uri_to_path(&create.uri)?;
            let overwrite = create
                .options
                .as_ref()
                .and_then(|o| o.overwrite)
                .unwrap_or(false);
            let ignore_if_exists = create
                .options
                .as_ref()
                .and_then(|o| o.ignore_if_exists)
                .unwrap_or(false);
            if !overwrite && ignore_if_exists && stoat.fs_host.exists(&path) {
                return Ok(());
            }
            stoat
                .fs_host
                .write(&path, b"")
                .with_context(|_| ResourceCreateSnafu { path: path.clone() })?;
            outcome.files_created.push(path);
        },
        ResourceOp::Rename(rename) => {
            let from = uri_to_path(&rename.old_uri)?;
            let to = uri_to_path(&rename.new_uri)?;
            stoat
                .fs_host
                .rename(&from, &to)
                .with_context(|_| ResourceRenameSnafu {
                    from: from.clone(),
                    to: to.clone(),
                })?;
            stoat.active_workspace_mut().buffers.rename_path(&from, &to);
            outcome.files_renamed.push((from, to));
        },
        ResourceOp::Delete(delete) => {
            let path = uri_to_path(&delete.uri)?;
            stoat
                .fs_host
                .remove_file(&path)
                .with_context(|_| ResourceDeleteSnafu { path: path.clone() })?;
            outcome.files_deleted.push(path);
        },
    }
    Ok(())
}

#[cfg(test)]
// The `HashMap<Uri, _>` keys come from `lsp_types::WorkspaceEdit::changes` and
// are built then consumed without mutation, so `mutable_key_type` is a false
// positive here.
#[allow(clippy::mutable_key_type)]
mod tests {
    use super::*;
    use crate::{host::FsHost, test_harness::TestHarness};
    use lsp_types::{
        CreateFile, DeleteFile, OneOf, OptionalVersionedTextDocumentIdentifier, Position, Range,
        RenameFile, TextDocumentEdit,
    };
    use std::{collections::HashMap, str::FromStr};

    fn file_uri(path: &Path) -> Uri {
        Uri::from_str(&format!("file://{}", path.display())).expect("valid file uri")
    }

    fn point(line: u32, character: u32) -> Position {
        Position::new(line, character)
    }

    fn text_edit(line: u32, sc: u32, ec: u32, text: &str) -> TextEdit {
        TextEdit {
            range: Range::new(point(line, sc), point(line, ec)),
            new_text: text.to_string(),
        }
    }

    fn open_buffer_with_text(h: &mut TestHarness, path: &Path, text: &str) {
        h.fake_fs().insert_file(path, text.as_bytes());
        let workspace = h.stoat.active_workspace_mut();
        workspace.buffers.open(path, text);
    }

    fn buffer_text(h: &TestHarness, path: &Path) -> String {
        let id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(path)
            .expect("buffer open");
        let buffer = h.stoat.active_workspace().buffers.get(id).expect("buffer");
        let guard = buffer.read().expect("buffer poisoned");
        guard.rope().to_string()
    }

    #[test]
    fn applies_changes_map_to_open_buffer() {
        let mut h = TestHarness::with_size(80, 24);
        let path = PathBuf::from("/ws/a.rs");
        open_buffer_with_text(&mut h, &path, "abcde\n");
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(file_uri(&path), vec![text_edit(0, 1, 4, "X")]);
        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        let outcome = apply_workspace_edit(&mut h.stoat, edit).expect("apply");
        assert_eq!(outcome.buffers_edited.len(), 1);
        assert_eq!(buffer_text(&h, &path), "aXe\n");
    }

    #[test]
    fn applies_text_edits_right_to_left() {
        let mut h = TestHarness::with_size(80, 24);
        let path = PathBuf::from("/ws/a.rs");
        open_buffer_with_text(&mut h, &path, "abcdef\n");
        let edits = vec![text_edit(0, 1, 2, "B"), text_edit(0, 4, 5, "E")];
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(file_uri(&path), edits);
        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        apply_workspace_edit(&mut h.stoat, edit).expect("apply");
        assert_eq!(buffer_text(&h, &path), "aBcdEf\n");
    }

    #[test]
    fn applies_document_changes_edits_variant() {
        let mut h = TestHarness::with_size(80, 24);
        let path = PathBuf::from("/ws/a.rs");
        open_buffer_with_text(&mut h, &path, "abc\n");
        let tde = TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: file_uri(&path),
                version: None,
            },
            edits: vec![OneOf::Left(text_edit(0, 0, 0, "X"))],
        };
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(vec![tde])),
            change_annotations: None,
        };
        apply_workspace_edit(&mut h.stoat, edit).expect("apply");
        assert_eq!(buffer_text(&h, &path), "Xabc\n");
    }

    #[test]
    fn loads_unopened_file_via_fs_host() {
        let mut h = TestHarness::with_size(80, 24);
        let path = PathBuf::from("/ws/closed.rs");
        h.fake_fs().insert_file(&path, b"hello\n");
        assert!(h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&path)
            .is_none());
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(file_uri(&path), vec![text_edit(0, 5, 5, "!")]);
        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        apply_workspace_edit(&mut h.stoat, edit).expect("apply");
        assert!(h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&path)
            .is_some());
        assert_eq!(buffer_text(&h, &path), "hello!\n");
    }

    #[test]
    fn applies_create_file_resource_op() {
        let mut h = TestHarness::with_size(80, 24);
        let path = PathBuf::from("/ws/new.rs");
        let op = DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
            uri: file_uri(&path),
            options: None,
            annotation_id: None,
        }));
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![op])),
            change_annotations: None,
        };
        let outcome = apply_workspace_edit(&mut h.stoat, edit).expect("apply");
        assert_eq!(outcome.files_created, vec![path.clone()]);
        assert!(h.fake_fs().exists(&path));
    }

    #[test]
    fn applies_rename_file_resource_op() {
        let mut h = TestHarness::with_size(80, 24);
        let from = PathBuf::from("/ws/old.rs");
        let to = PathBuf::from("/ws/new.rs");
        open_buffer_with_text(&mut h, &from, "x\n");
        let op = DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
            old_uri: file_uri(&from),
            new_uri: file_uri(&to),
            options: None,
            annotation_id: None,
        }));
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![op])),
            change_annotations: None,
        };
        let outcome = apply_workspace_edit(&mut h.stoat, edit).expect("apply");
        assert_eq!(outcome.files_renamed, vec![(from.clone(), to.clone())]);
        assert!(!h.fake_fs().exists(&from));
        assert!(h.fake_fs().exists(&to));
        assert!(h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&to)
            .is_some());
        assert!(h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&from)
            .is_none());
    }

    #[test]
    fn applies_delete_file_resource_op() {
        let mut h = TestHarness::with_size(80, 24);
        let path = PathBuf::from("/ws/gone.rs");
        h.fake_fs().insert_file(&path, b"x\n");
        let op = DocumentChangeOperation::Op(ResourceOp::Delete(DeleteFile {
            uri: file_uri(&path),
            options: None,
        }));
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![op])),
            change_annotations: None,
        };
        let outcome = apply_workspace_edit(&mut h.stoat, edit).expect("apply");
        assert_eq!(outcome.files_deleted, vec![path.clone()]);
        assert!(!h.fake_fs().exists(&path));
    }

    #[test]
    fn errors_on_non_file_uri() {
        let mut h = TestHarness::with_size(80, 24);
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(
            Uri::from_str("https://example.com/a.rs").expect("uri"),
            vec![text_edit(0, 0, 0, "X")],
        );
        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        let err = apply_workspace_edit(&mut h.stoat, edit).expect_err("expected error");
        assert!(matches!(err, WorkspaceEditError::UriNotFile { .. }));
    }
}
