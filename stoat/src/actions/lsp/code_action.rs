use crate::stoat::{Stoat, StoatEvent};
use gpui::Context;
use lsp_types::CodeActionOrCommand;
use stoat_lsp::{anchors_to_lsp_range, point_to_lsp_position};

impl Stoat {
    pub fn lsp_code_action(&mut self, cx: &mut Context<Self>) {
        let Some(lsp) = self.lsp_manager.clone() else {
            self.flash("No LSP server running", cx);
            return;
        };

        let Some(file_path) = self.current_file_path.clone() else {
            self.flash("No file open", cx);
            return;
        };

        let worktree_root = self.worktree.lock().root().to_path_buf();
        let abs_path = worktree_root.join(&file_path);
        let uri_str = format!("file://{}", abs_path.display());
        let Ok(uri) = uri_str.parse::<lsp_types::Uri>() else {
            self.flash("Invalid file path", cx);
            return;
        };

        let buffer_item = self.active_buffer(cx);
        let snapshot = buffer_item.read(cx).buffer_snapshot(cx);

        let cursor_pos = self.cursor.position();
        let lsp_pos = point_to_lsp_position(cursor_pos, &snapshot);
        let range = lsp_types::Range {
            start: lsp_pos,
            end: lsp_pos,
        };

        let row_diagnostics: Vec<lsp_types::Diagnostic> = buffer_item
            .read(cx)
            .diagnostics_for_row(cursor_pos.row, &snapshot)
            .map(|d| {
                let diag_range = anchors_to_lsp_range(&d.range, &snapshot);
                lsp_types::Diagnostic {
                    range: diag_range,
                    severity: Some(match d.severity {
                        stoat_lsp::DiagnosticSeverity::Error => {
                            lsp_types::DiagnosticSeverity::ERROR
                        },
                        stoat_lsp::DiagnosticSeverity::Warning => {
                            lsp_types::DiagnosticSeverity::WARNING
                        },
                        stoat_lsp::DiagnosticSeverity::Information => {
                            lsp_types::DiagnosticSeverity::INFORMATION
                        },
                        stoat_lsp::DiagnosticSeverity::Hint => lsp_types::DiagnosticSeverity::HINT,
                    }),
                    message: d.message.clone(),
                    code: d
                        .code
                        .as_ref()
                        .map(|c| lsp_types::NumberOrString::String(c.clone())),
                    source: d.source.clone(),
                    ..Default::default()
                }
            })
            .collect();

        let server_ids = lsp.active_servers();

        cx.spawn(async move |this, cx| {
            for server_id in server_ids {
                let Ok(handle) =
                    lsp.code_action(server_id, uri.clone(), range, row_diagnostics.clone())
                else {
                    continue;
                };
                let Ok(response) = handle.await else {
                    continue;
                };

                if let Ok(actions) = stoat_lsp::response::parse_code_actions(&response) {
                    if actions.is_empty() {
                        continue;
                    }

                    if actions.len() == 1 {
                        if let Some(edit) = extract_workspace_edit(&actions[0]) {
                            this.update(cx, |s, cx| match s.apply_workspace_edit(&edit, cx) {
                                Ok(n) => s.flash(format!("Applied {n} edit(s)"), cx),
                                Err(e) => s.flash(format!("Failed: {e}"), cx),
                            })
                            .ok();
                            return;
                        }
                    }

                    let titles: Vec<String> = actions.iter().map(action_title).collect();
                    let msg = format!("{} code action(s): {}", actions.len(), titles.join(", "));
                    this.update(cx, |_, cx| {
                        cx.emit(StoatEvent::FlashMessage(msg));
                    })
                    .ok();
                    return;
                }
            }

            this.update(cx, |_, cx| {
                cx.emit(StoatEvent::FlashMessage("No code actions available".into()));
            })
            .ok();
        })
        .detach();
    }
}

fn extract_workspace_edit(action: &CodeActionOrCommand) -> Option<lsp_types::WorkspaceEdit> {
    match action {
        CodeActionOrCommand::CodeAction(ca) => ca.edit.clone(),
        CodeActionOrCommand::Command(_) => None,
    }
}

fn action_title(action: &CodeActionOrCommand) -> String {
    match action {
        CodeActionOrCommand::CodeAction(ca) => ca.title.clone(),
        CodeActionOrCommand::Command(cmd) => cmd.title.clone(),
    }
}
