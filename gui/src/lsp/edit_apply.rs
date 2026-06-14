use crate::{buffer::Buffer, editor::Editor, workspace::Workspace};
use gpui::{Entity, WeakEntity};
use lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf, TextEdit, Uri, WorkspaceEdit};
use std::{cmp::Reverse, collections::HashMap, ops::Range, path::PathBuf};
use stoat::{host::OffsetEncoding, lsp::util::lsp_range_to_byte_range};

/// Apply a `WorkspaceEdit` across every open buffer the workspace
/// tracks. For each URI in the edit, the active editor's buffer is
/// used when the URI matches (cheap path, also covers the
/// single-file case); other URIs are looked up via
/// [`Workspace::buffer_for_path`]. Edits whose URI is not opened are
/// silently dropped. Each per-buffer apply sorts its text edits in
/// reverse byte order so earlier ranges stay stable as later edits
/// land first. Returns the number of buffers actually mutated.
// Uri's Hash/Eq don't observe its interior-mutability cache; sound as a map key.
#[allow(clippy::mutable_key_type)]
pub fn apply_workspace_edit_to_buffer(
    edit: &WorkspaceEdit,
    active_uri: &Uri,
    active_rope: &stoat_text::Rope,
    encoding: OffsetEncoding,
    editor: &WeakEntity<Editor>,
    workspace: &WeakEntity<Workspace>,
    cx: &mut gpui::App,
) -> usize {
    let by_uri = collect_text_edits_by_uri(edit);
    if by_uri.is_empty() {
        return 0;
    }
    let mut buffers_touched = 0usize;
    for (uri, text_edits) in by_uri {
        if &uri == active_uri
            && let Some(buffer) = editor
                .upgrade()
                .and_then(|e| e.read(cx).multi_buffer().read(cx).as_singleton().cloned())
        {
            apply_text_edits(&buffer, text_edits, active_rope, encoding, cx);
            buffers_touched += 1;
            continue;
        }
        let path = match uri_to_path(&uri) {
            Some(p) => p,
            None => continue,
        };
        let Some(workspace) = workspace.upgrade() else {
            continue;
        };
        let Some(buffer) = workspace.read(cx).buffer_for_path(&path, cx) else {
            continue;
        };
        let rope = buffer.read(cx).read(|tb| tb.rope().clone());
        apply_text_edits(&buffer, text_edits, &rope, encoding, cx);
        buffers_touched += 1;
    }
    buffers_touched
}

fn apply_text_edits(
    buffer: &Entity<Buffer>,
    text_edits: Vec<TextEdit>,
    rope: &stoat_text::Rope,
    encoding: OffsetEncoding,
    cx: &mut gpui::App,
) {
    let mut byte_edits: Vec<(Range<usize>, String)> = text_edits
        .into_iter()
        .map(|te| {
            (
                lsp_range_to_byte_range(rope, te.range, encoding),
                te.new_text,
            )
        })
        .collect();
    byte_edits.sort_by_key(|b| Reverse(b.0.start));
    buffer.update(cx, |b, cx| {
        for (range, text) in byte_edits {
            b.edit(range, &text, cx);
        }
    });
}

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let path = s.strip_prefix("file://").unwrap_or(s);
    Some(PathBuf::from(path))
}

/// Group every text edit in `edit` by target URI. Honors LSP
/// precedence: `document_changes` (when set) wins over the legacy
/// `changes` map, mirroring the spec. `DocumentChanges::Operations`
/// resource ops (Create / Delete / Rename) are dropped; only
/// `Edit` ops contribute text edits.
// Uri's Hash/Eq don't observe its interior-mutability cache; sound as a map key.
#[allow(clippy::mutable_key_type)]
pub fn collect_text_edits_by_uri(edit: &WorkspaceEdit) -> HashMap<Uri, Vec<TextEdit>> {
    let mut out: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    if let Some(changes) = &edit.document_changes {
        match changes {
            DocumentChanges::Edits(text_doc_edits) => {
                for tde in text_doc_edits {
                    let entry = out.entry(tde.text_document.uri.clone()).or_default();
                    for annotated in &tde.edits {
                        entry.push(match annotated {
                            OneOf::Left(e) => e.clone(),
                            OneOf::Right(a) => a.text_edit.clone(),
                        });
                    }
                }
            },
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    if let DocumentChangeOperation::Edit(tde) = op {
                        let entry = out.entry(tde.text_document.uri.clone()).or_default();
                        for annotated in &tde.edits {
                            entry.push(match annotated {
                                OneOf::Left(e) => e.clone(),
                                OneOf::Right(a) => a.text_edit.clone(),
                            });
                        }
                    }
                }
            },
        }
        return out;
    }
    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            out.entry(uri.clone()).or_default().extend(edits.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn rng(line: u32, char: u32, end_line: u32, end_char: u32) -> Range {
        Range {
            start: Position {
                line,
                character: char,
            },
            end: Position {
                line: end_line,
                character: end_char,
            },
        }
    }

    fn make_uri(s: &str) -> Uri {
        use std::str::FromStr;
        Uri::from_str(s).unwrap()
    }

    // Uri's Hash/Eq don't observe its interior-mutability cache; sound as a map key.
    #[allow(clippy::mutable_key_type)]
    #[test]
    fn collect_text_edits_by_uri_groups_changes_map_per_target() {
        let target_a = make_uri("file:///tmp/a.rs");
        let target_b = make_uri("file:///tmp/other.rs");
        let mut changes = HashMap::new();
        changes.insert(
            target_a.clone(),
            vec![TextEdit {
                range: rng(0, 0, 0, 1),
                new_text: "X".into(),
            }],
        );
        changes.insert(
            target_b.clone(),
            vec![TextEdit {
                range: rng(1, 0, 1, 1),
                new_text: "Y".into(),
            }],
        );
        let edit = WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        };
        let grouped = collect_text_edits_by_uri(&edit);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped.get(&target_a).unwrap()[0].new_text, "X");
        assert_eq!(grouped.get(&target_b).unwrap()[0].new_text, "Y");
    }

    // Uri's Hash/Eq don't observe its interior-mutability cache; sound as a map key.
    #[allow(clippy::mutable_key_type)]
    #[test]
    fn collect_text_edits_returns_empty_when_uri_absent() {
        let target = make_uri("file:///tmp/a.rs");
        let mut changes = HashMap::new();
        changes.insert(
            make_uri("file:///tmp/other.rs"),
            vec![TextEdit {
                range: rng(1, 0, 1, 1),
                new_text: "Y".into(),
            }],
        );
        let edit = WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        };
        let grouped = collect_text_edits_by_uri(&edit);
        assert!(!grouped.contains_key(&target));
    }
}
