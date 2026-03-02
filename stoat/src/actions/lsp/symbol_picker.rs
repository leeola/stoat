use crate::{
    app_state::{SymbolEntry, SymbolPickerSource},
    stoat::{Stoat, StoatEvent},
};
use gpui::Context;
use lsp_types::SymbolKind;

impl Stoat {
    pub fn lsp_symbol_picker(&mut self, cx: &mut Context<Self>) {
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

        let server_ids = lsp.active_servers();

        cx.spawn(async move |this, cx| {
            for server_id in server_ids {
                let Ok(handle) = lsp.document_symbols(server_id, uri.clone()) else {
                    continue;
                };
                let Ok(response) = handle.await else {
                    continue;
                };

                if let Ok(doc_symbols) = stoat_lsp::response::parse_document_symbols(&response) {
                    if doc_symbols.is_empty() {
                        continue;
                    }

                    let symbols: Vec<SymbolEntry> = flatten_document_symbols(&doc_symbols);

                    this.update(cx, |_, cx| {
                        cx.emit(StoatEvent::SymbolsLoaded {
                            symbols,
                            source: SymbolPickerSource::Document,
                        });
                    })
                    .ok();
                    return;
                }
            }

            this.update(cx, |_, cx| {
                cx.emit(StoatEvent::FlashMessage("No symbols found".into()));
            })
            .ok();
        })
        .detach();
    }

    pub fn lsp_workspace_symbol_picker(&mut self, cx: &mut Context<Self>) {
        let Some(lsp) = self.lsp_manager.clone() else {
            self.flash("No LSP server running", cx);
            return;
        };

        let server_ids = lsp.active_servers();

        cx.spawn(async move |this, cx| {
            for server_id in server_ids {
                let Ok(handle) = lsp.workspace_symbols(server_id, "") else {
                    continue;
                };
                let Ok(response) = handle.await else {
                    continue;
                };

                if let Ok(sym_infos) = stoat_lsp::response::parse_workspace_symbols(&response) {
                    if sym_infos.is_empty() {
                        continue;
                    }

                    let symbols: Vec<SymbolEntry> = sym_infos
                        .into_iter()
                        .map(|info| SymbolEntry {
                            name: info.name,
                            kind: info.kind,
                            range_start: info.location.range.start,
                            file_uri: Some(info.location.uri),
                        })
                        .collect();

                    this.update(cx, |_, cx| {
                        cx.emit(StoatEvent::SymbolsLoaded {
                            symbols,
                            source: SymbolPickerSource::Workspace,
                        });
                    })
                    .ok();
                    return;
                }
            }

            this.update(cx, |_, cx| {
                cx.emit(StoatEvent::FlashMessage(
                    "No workspace symbols found".into(),
                ));
            })
            .ok();
        })
        .detach();
    }
}

fn flatten_document_symbols(symbols: &[lsp_types::DocumentSymbol]) -> Vec<SymbolEntry> {
    let mut result = Vec::new();
    for sym in symbols {
        result.push(SymbolEntry {
            name: sym.name.clone(),
            kind: sym.kind,
            range_start: sym.selection_range.start,
            file_uri: None,
        });
        if let Some(children) = &sym.children {
            result.extend(flatten_document_symbols(children));
        }
    }
    result
}

pub fn symbol_kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::FUNCTION | SymbolKind::METHOD => "fn",
        SymbolKind::STRUCT => "struct",
        SymbolKind::ENUM => "enum",
        SymbolKind::MODULE | SymbolKind::NAMESPACE => "mod",
        SymbolKind::CONSTANT => "const",
        SymbolKind::INTERFACE => "trait",
        SymbolKind::TYPE_PARAMETER => "type",
        SymbolKind::FIELD | SymbolKind::PROPERTY => "field",
        SymbolKind::VARIABLE => "var",
        SymbolKind::CLASS => "class",
        SymbolKind::CONSTRUCTOR => "new",
        SymbolKind::ENUM_MEMBER => "variant",
        _ => "sym",
    }
}
