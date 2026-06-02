//! Diagnostics panel dock item.
//!
//! Lists every diagnostic the workspace's
//! [`crate::diagnostics::DiagnosticSet`] holds, grouped under a
//! relative-path header per file with a colored severity glyph,
//! position, and message per row. The `ToggleDiagnosticsPanel` action
//! opens and closes it in a right [`crate::dock::Dock`]. The panel
//! subscribes to the set and re-reads it on every change, so it stays
//! current without caching. Clicking a row opens the file and jumps the
//! cursor to the diagnostic.

use crate::{
    buffer::Buffer,
    diagnostics::{DiagnosticSet, DiagnosticSetEvent},
    editor::Editor,
    item::{DeserializeSnafu, ItemError, ItemKind, ItemView},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, rems, App, Context, Entity, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window,
};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use serde_json::Value;
use std::path::{Path, PathBuf};
use stoat_text::{Bias, Point, Selection, SelectionGoal};

const MESSAGE_MAX_CHARS: usize = 100;

/// One diagnostic within a file group: `(line, column)` are 0-based
/// LSP coordinates; `message` is newline-stripped and length-capped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagRow {
    pub line: u32,
    pub column: u32,
    pub severity: Option<DiagnosticSeverity>,
    pub message: String,
}

/// Every diagnostic for one file, sorted by position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGroup {
    pub path: PathBuf,
    pub rows: Vec<DiagRow>,
}

pub struct DiagnosticsPanel {
    workspace: WeakEntity<Workspace>,
    diagnostics: Entity<DiagnosticSet>,
    git_root: PathBuf,
    _subscription: Subscription,
}

impl DiagnosticsPanel {
    pub fn new(
        workspace: Entity<Workspace>,
        diagnostics: Entity<DiagnosticSet>,
        git_root: PathBuf,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let subscription = cx.subscribe(&diagnostics, |_, _, _event: &DiagnosticSetEvent, cx| {
            cx.notify();
        });
        Self {
            workspace: workspace.downgrade(),
            diagnostics,
            git_root,
            _subscription: subscription,
        }
    }

    fn jump(
        &mut self,
        path: PathBuf,
        line: u32,
        column: u32,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        // Defer past any active update lease before re-entering the
        // workspace, matching the diagnostics picker's confirm path.
        window.defer(cx, move |_window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_paths(std::slice::from_ref(&path), cx);
                let Some(editor) = workspace
                    .buffer_for_path(&path, cx)
                    .and_then(|buffer| editor_for_buffer(workspace, &buffer, cx))
                else {
                    return;
                };
                set_cursor_to_point(&editor, line, column, cx);
            });
        });
    }
}

/// Group every `(path, diagnostics)` pair into per-file lists sorted by
/// path, each file's rows sorted by `(line, column)`. Files with no
/// diagnostics are dropped.
pub fn file_groups<'a, I>(pairs: I) -> Vec<FileGroup>
where
    I: IntoIterator<Item = (&'a Path, &'a [Diagnostic])>,
{
    let mut groups: Vec<FileGroup> = pairs
        .into_iter()
        .filter(|(_, diags)| !diags.is_empty())
        .map(|(path, diags)| {
            let mut rows: Vec<DiagRow> = diags
                .iter()
                .map(|d| DiagRow {
                    line: d.range.start.line,
                    column: d.range.start.character,
                    severity: d.severity,
                    message: render_message(&d.message),
                })
                .collect();
            rows.sort_by_key(|r| (r.line, r.column));
            FileGroup {
                path: path.to_path_buf(),
                rows,
            }
        })
        .collect();
    groups.sort_by(|a, b| a.path.cmp(&b.path));
    groups
}

fn render_message(raw: &str) -> String {
    raw.replace('\n', " ")
        .chars()
        .take(MESSAGE_MAX_CHARS)
        .collect()
}

fn severity_glyph(severity: Option<DiagnosticSeverity>) -> &'static str {
    match severity {
        Some(DiagnosticSeverity::ERROR) => "E",
        Some(DiagnosticSeverity::WARNING) => "W",
        Some(DiagnosticSeverity::INFORMATION) => "I",
        Some(DiagnosticSeverity::HINT) => "H",
        _ => " ",
    }
}

fn severity_color(severity: Option<DiagnosticSeverity>, cx: &App) -> Hsla {
    match severity {
        Some(DiagnosticSeverity::WARNING) => cx.theme().diagnostic_warning,
        Some(DiagnosticSeverity::INFORMATION) => cx.theme().diagnostic_info,
        Some(DiagnosticSeverity::HINT) => cx.theme().diagnostic_hint,
        _ => cx.theme().diagnostic_error,
    }
}

fn display_path(path: &Path, git_root: &Path) -> String {
    match path.strip_prefix(git_root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

fn editor_for_buffer(
    workspace: &Workspace,
    buffer: &Entity<Buffer>,
    cx: &App,
) -> Option<Entity<Editor>> {
    let target_id = buffer.entity_id();
    let pane_tree = workspace.pane_tree().read(cx);
    for pane_id in pane_tree.split_pane_ids() {
        let pane = pane_tree.pane(pane_id)?;
        for item in pane.read(cx).items() {
            let Ok(editor) = item.to_any_view().downcast::<Editor>() else {
                continue;
            };
            let singleton = editor
                .read(cx)
                .multi_buffer()
                .read(cx)
                .as_singleton()
                .cloned();
            if singleton.as_ref().map(Entity::entity_id) == Some(target_id) {
                return Some(editor);
            }
        }
    }
    None
}

fn set_cursor_to_point(
    editor: &Entity<Editor>,
    line: u32,
    column: u32,
    cx: &mut Context<'_, Workspace>,
) {
    editor.update(cx, |ed, cx| {
        let snapshot = ed.multi_buffer().read(cx).snapshot();
        let rope = snapshot.rope();
        let offset = rope
            .point_to_offset(Point::new(line, column))
            .min(rope.len());
        let anchor = snapshot.anchor_at(offset, Bias::Left);
        let new_id = ed
            .selections()
            .all_anchors()
            .iter()
            .map(|s| s.id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        let selection = Selection {
            id: new_id,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: SelectionGoal::None,
        };
        ed.selections_mut().replace_with(vec![selection], &snapshot);
    });
}

impl Render for DiagnosticsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let header_color = cx.theme().muted_text;
        let groups = file_groups(self.diagnostics.read(cx).iter());
        let git_root = self.git_root.clone();
        let mut container = div().flex().flex_col().size_full();
        let mut row_id = 0usize;
        for group in groups {
            let header = display_path(&group.path, &git_root);
            container = container.child(
                div()
                    .px_2()
                    .text_color(header_color)
                    .child(SharedString::from(header)),
            );
            for row in group.rows {
                let color = severity_color(row.severity, cx);
                let label = format!(
                    "{} {:>4}:{:<3} {}",
                    severity_glyph(row.severity),
                    row.line + 1,
                    row.column + 1,
                    row.message
                );
                let path = group.path.clone();
                let (line, column) = (row.line, row.column);
                container = container.child(
                    div()
                        .id(("diagnostics-row", row_id))
                        .pl(rems(1.5))
                        .pr_2()
                        .text_color(color)
                        .child(SharedString::from(label))
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            this.jump(path.clone(), line, column, window, cx)
                        })),
                );
                row_id += 1;
            }
        }
        container
    }
}

impl ItemView for DiagnosticsPanel {
    fn tab_label(&self, _cx: &App) -> SharedString {
        "Diagnostics".into()
    }

    fn item_kind(&self) -> ItemKind {
        ItemKind::DiagnosticsPanel
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
    where
        Self: Sized,
    {
        DeserializeSnafu {
            reason: "DiagnosticsPanel is transient and not persisted",
        }
        .fail()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn diag(line: u32, column: u32, message: &str, severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: column,
                },
                end: Position {
                    line,
                    character: column + 1,
                },
            },
            severity: Some(severity),
            code: None,
            code_description: None,
            source: None,
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[test]
    fn file_groups_sorts_files_and_rows_and_drops_empty() {
        let a = PathBuf::from("/repo/a.rs");
        let b = PathBuf::from("/repo/b.rs");
        let empty = PathBuf::from("/repo/empty.rs");
        let b_diags = vec![
            diag(3, 0, "b-late", DiagnosticSeverity::WARNING),
            diag(1, 2, "b-early", DiagnosticSeverity::ERROR),
        ];
        let a_diags = vec![diag(0, 0, "a-only", DiagnosticSeverity::ERROR)];
        let pairs: Vec<(&Path, &[Diagnostic])> = vec![
            (b.as_path(), b_diags.as_slice()),
            (empty.as_path(), &[]),
            (a.as_path(), a_diags.as_slice()),
        ];

        let groups = file_groups(pairs);

        let paths: Vec<&Path> = groups.iter().map(|g| g.path.as_path()).collect();
        assert_eq!(
            paths,
            vec![a.as_path(), b.as_path()],
            "files sort by path, empty dropped"
        );
        let b_messages: Vec<&str> = groups[1].rows.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(
            b_messages,
            vec!["b-early", "b-late"],
            "rows sort by position"
        );
    }

    #[test]
    fn render_message_strips_newlines_and_truncates() {
        let long = "x".repeat(200);
        let rendered = render_message(&format!("first\nsecond\n{long}"));
        assert_eq!(rendered.chars().count(), MESSAGE_MAX_CHARS);
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn severity_glyph_maps_each_level() {
        assert_eq!(severity_glyph(Some(DiagnosticSeverity::ERROR)), "E");
        assert_eq!(severity_glyph(Some(DiagnosticSeverity::WARNING)), "W");
        assert_eq!(severity_glyph(Some(DiagnosticSeverity::INFORMATION)), "I");
        assert_eq!(severity_glyph(Some(DiagnosticSeverity::HINT)), "H");
        assert_eq!(severity_glyph(None), " ");
    }
}
