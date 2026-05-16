use crate::{
    buffer::Buffer,
    editor::Editor,
    picker::{Picker, PickerDelegate, PickerSecondary},
    theme::statusbar_text_color,
    workspace::Workspace,
};
use gpui::{div, AnyElement, Context, IntoElement, ParentElement, Styled, Task, WeakEntity};
use lsp_types::{
    CodeAction as LspCodeAction, CodeActionOrCommand, Command as LspCommand,
    DocumentChangeOperation, DocumentChanges, ExecuteCommandParams, OneOf, TextEdit, Uri,
    WorkspaceEdit,
};
use std::sync::Arc;
use stoat::{
    host::{LspServer, OffsetEncoding},
    lsp::util::lsp_range_to_byte_range,
};

/// One pickable code action.
///
/// `Direct` carries the edit (and an optional follow-up command) the
/// server returned eagerly. `Command` carries a server-side command
/// the editor only dispatches. `NeedsResolve` carries an unresolved
/// `CodeAction` whose `data` payload the server fills in on a
/// follow-up `codeAction/resolve` request -- v1 servers that defer
/// edits this way still flow through the picker.
#[derive(Clone)]
pub enum CodeActionEntry {
    Direct {
        title: String,
        edit: Box<WorkspaceEdit>,
        command: Option<LspCommand>,
    },
    NeedsResolve {
        title: String,
        action: Box<LspCodeAction>,
    },
    Command {
        title: String,
        command: LspCommand,
    },
}

impl CodeActionEntry {
    pub fn title(&self) -> &str {
        match self {
            Self::Direct { title, .. }
            | Self::NeedsResolve { title, .. }
            | Self::Command { title, .. } => title,
        }
    }
}

/// Picker delegate for LSP code actions. Items are pre-fetched at
/// dispatch time and handed in via [`Self::new`]; `update_matches`
/// is a no-op because the list is short and bounded by the server.
///
/// On confirm:
/// - `Direct` entries apply the carried `WorkspaceEdit` across every open buffer the workspace
///   tracks, including the active editor's; edits targeting paths the workspace has not opened are
///   silently dropped (a future iteration may load them from disk).
/// - `Command` and the `Direct` follow-up command are dispatched through the held `Arc<dyn
///   LspServer>` via the canonical `Executor` so the request runs in the background.
pub struct CodeActionPickerDelegate {
    entries: Vec<CodeActionEntry>,
    selected: usize,
    editor: WeakEntity<Editor>,
    workspace: WeakEntity<Workspace>,
    uri: Uri,
    rope: stoat_text::Rope,
    encoding: OffsetEncoding,
    server: Arc<dyn LspServer>,
    executor: stoat_scheduler::Executor,
}

impl CodeActionPickerDelegate {
    pub fn new(
        entries: Vec<CodeActionEntry>,
        editor: WeakEntity<Editor>,
        workspace: WeakEntity<Workspace>,
        uri: Uri,
        rope: stoat_text::Rope,
        encoding: OffsetEncoding,
        server: Arc<dyn LspServer>,
        executor: stoat_scheduler::Executor,
    ) -> Self {
        Self {
            entries,
            selected: 0,
            editor,
            workspace,
            uri,
            rope,
            encoding,
            server,
            executor,
        }
    }
}

impl PickerDelegate for CodeActionPickerDelegate {
    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if ix < self.entries.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, _query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: Option<PickerSecondary>, cx: &mut Context<'_, Picker<Self>>) {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return;
        };
        match entry {
            CodeActionEntry::Direct { edit, command, .. } => {
                self.apply_workspace_edit(&edit, cx);
                if let Some(command) = command {
                    self.dispatch_command(command);
                }
            },
            CodeActionEntry::NeedsResolve { action, .. } => self.spawn_resolve(*action, cx),
            CodeActionEntry::Command { command, .. } => {
                self.dispatch_command(command);
            },
        }
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let title = self
            .entries
            .get(ix)
            .map(|e| e.title().to_string())
            .unwrap_or_default();
        let color = statusbar_text_color(cx);
        let mut row = div().px_2().text_color(color).child(title);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }
}

impl CodeActionPickerDelegate {
    fn apply_workspace_edit(
        &self,
        edit: &WorkspaceEdit,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> usize {
        apply_workspace_edit_to_buffer(
            edit,
            &self.uri,
            &self.rope,
            self.encoding,
            &self.editor,
            &self.workspace,
            cx,
        )
    }

    /// Spawn a `codeAction/resolve` request for `action` and apply
    /// the resolved edit / command to the active editor's buffer.
    /// Server returned `Err` or a still-unresolved action -> log a
    /// warning and drop. A resolved action that carries neither an
    /// `edit` nor a `command` is silently dropped (the server told
    /// us it had nothing to do).
    fn spawn_resolve(&self, action: LspCodeAction, cx: &mut Context<'_, Picker<Self>>) {
        let server = self.server.clone();
        let uri = self.uri.clone();
        let rope = self.rope.clone();
        let encoding = self.encoding;
        let editor = self.editor.clone();
        let workspace = self.workspace.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |_, cx| {
            let resolved = match server.code_action_resolve(action).await {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(
                        target: "stoat_gui::lsp::code_action",
                        ?err,
                        "codeAction/resolve request failed",
                    );
                    return;
                },
            };
            let edit = resolved.edit;
            let command = resolved.command;
            let _ = cx.update(|cx| {
                if let Some(edit) = edit {
                    apply_workspace_edit_to_buffer(
                        &edit, &uri, &rope, encoding, &editor, &workspace, cx,
                    );
                }
                if let Some(command) = command {
                    let server = server.clone();
                    let label = command.command.clone();
                    let params = ExecuteCommandParams {
                        command: command.command,
                        arguments: command.arguments.unwrap_or_default(),
                        work_done_progress_params: Default::default(),
                    };
                    executor
                        .spawn(async move {
                            if let Err(err) = server.execute_command(params).await {
                                tracing::warn!(
                                    target: "stoat_gui::lsp::code_action",
                                    ?err,
                                    command = %label,
                                    "workspace/executeCommand request failed",
                                );
                            }
                        })
                        .detach();
                }
            });
        })
        .detach();
    }

    fn dispatch_command(&self, command: LspCommand) {
        let server = self.server.clone();
        let label = command.command.clone();
        let params = ExecuteCommandParams {
            command: command.command,
            arguments: command.arguments.unwrap_or_default(),
            work_done_progress_params: Default::default(),
        };
        self.executor
            .spawn(async move {
                if let Err(err) = server.execute_command(params).await {
                    tracing::warn!(
                        target: "stoat_gui::lsp::code_action",
                        ?err,
                        command = %label,
                        "workspace/executeCommand request failed",
                    );
                }
            })
            .detach();
    }
}

/// Apply a `WorkspaceEdit` across every open buffer the workspace
/// tracks. For each URI in the edit, the active editor's buffer is
/// used when the URI matches (cheap path, also covers the
/// single-file case); other URIs are looked up via
/// [`Workspace::buffer_for_path`]. Edits whose URI is not opened are
/// silently dropped. Each per-buffer apply sorts its text edits in
/// reverse byte order so earlier ranges stay stable as later edits
/// land first. Returns the number of buffers actually mutated.
fn apply_workspace_edit_to_buffer(
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
        if &uri == active_uri {
            if let Some(buffer) = editor
                .upgrade()
                .and_then(|e| e.read(cx).multi_buffer().read(cx).as_singleton().cloned())
            {
                apply_text_edits(&buffer, text_edits, active_rope, encoding, cx);
                buffers_touched += 1;
                continue;
            }
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
    buffer: &gpui::Entity<Buffer>,
    text_edits: Vec<TextEdit>,
    rope: &stoat_text::Rope,
    encoding: OffsetEncoding,
    cx: &mut gpui::App,
) {
    let mut byte_edits: Vec<(std::ops::Range<usize>, String)> = text_edits
        .into_iter()
        .map(|te| {
            (
                lsp_range_to_byte_range(rope, te.range, encoding),
                te.new_text,
            )
        })
        .collect();
    byte_edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
    buffer.update(cx, |b, cx| {
        for (range, text) in byte_edits {
            b.edit(range, &text, cx);
        }
    });
}

fn uri_to_path(uri: &Uri) -> Option<std::path::PathBuf> {
    let s = uri.as_str();
    let path = s.strip_prefix("file://").unwrap_or(s);
    Some(std::path::PathBuf::from(path))
}

/// Translate raw LSP code-action items into the picker's enum.
/// Filters out `CodeAction` entries that have neither a
/// `WorkspaceEdit` nor a command (the resolve-pipeline is a v2
/// concern); standalone `Command` entries pass through as
/// `CodeActionEntry::Command`.
pub fn translate_actions(actions: Vec<CodeActionOrCommand>) -> Vec<CodeActionEntry> {
    actions
        .into_iter()
        .filter_map(|item| match item {
            CodeActionOrCommand::CodeAction(ca) => translate_code_action(ca),
            CodeActionOrCommand::Command(command) => Some(CodeActionEntry::Command {
                title: command.title.clone(),
                command,
            }),
        })
        .collect()
}

fn translate_code_action(ca: LspCodeAction) -> Option<CodeActionEntry> {
    if ca.edit.is_none() && ca.command.is_none() && ca.data.is_some() {
        return Some(CodeActionEntry::NeedsResolve {
            title: ca.title.clone(),
            action: Box::new(ca),
        });
    }
    match (ca.edit, ca.command) {
        (Some(edit), command) => Some(CodeActionEntry::Direct {
            title: ca.title,
            edit: Box::new(edit),
            command,
        }),
        (None, Some(command)) => Some(CodeActionEntry::Command {
            title: ca.title,
            command,
        }),
        (None, None) => None,
    }
}

/// Group every text edit in `edit` by target URI. Honors LSP
/// precedence: `document_changes` (when set) wins over the legacy
/// `changes` map, mirroring the spec. `DocumentChanges::Operations`
/// resource ops (Create / Delete / Rename) are dropped; only
/// `Edit` ops contribute text edits.
fn collect_text_edits_by_uri(
    edit: &WorkspaceEdit,
) -> std::collections::HashMap<Uri, Vec<TextEdit>> {
    use std::collections::HashMap;
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
    use lsp_types::{Command, Position, Range};

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

    #[test]
    fn translate_drops_action_without_edit_command_or_data() {
        let items = vec![CodeActionOrCommand::CodeAction(LspCodeAction {
            title: "no-op".into(),
            ..Default::default()
        })];
        assert!(translate_actions(items).is_empty());
    }

    #[test]
    fn translate_data_only_action_yields_needs_resolve() {
        let items = vec![CodeActionOrCommand::CodeAction(LspCodeAction {
            title: "resolve-me".into(),
            data: Some(serde_json::json!({"id": "abc"})),
            ..Default::default()
        })];
        let entries = translate_actions(items);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title(), "resolve-me");
        assert!(matches!(entries[0], CodeActionEntry::NeedsResolve { .. }));
    }

    #[test]
    fn translate_keeps_direct_and_command() {
        let mut changes = std::collections::HashMap::new();
        changes.insert(
            make_uri("file:///tmp/a.rs"),
            vec![TextEdit {
                range: rng(0, 0, 0, 1),
                new_text: "X".into(),
            }],
        );
        let direct = CodeActionOrCommand::CodeAction(LspCodeAction {
            title: "fix it".into(),
            edit: Some(WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }),
            ..Default::default()
        });
        let cmd = CodeActionOrCommand::Command(Command {
            title: "do thing".into(),
            command: "thing".into(),
            arguments: None,
        });
        let entries = translate_actions(vec![direct, cmd]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title(), "fix it");
        assert_eq!(entries[1].title(), "do thing");
        assert!(matches!(entries[0], CodeActionEntry::Direct { .. }));
        assert!(matches!(entries[1], CodeActionEntry::Command { .. }));
    }

    #[test]
    fn collect_text_edits_by_uri_groups_changes_map_per_target() {
        let target_a = make_uri("file:///tmp/a.rs");
        let target_b = make_uri("file:///tmp/other.rs");
        let mut changes = std::collections::HashMap::new();
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

    #[test]
    fn collect_text_edits_returns_empty_when_uri_absent() {
        let target = make_uri("file:///tmp/a.rs");
        let mut changes = std::collections::HashMap::new();
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
        assert!(grouped.get(&target).is_none());
    }
}
