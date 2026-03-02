//! Parsers for LSP JSON-RPC response payloads.

use anyhow::{Context, Result};
use lsp_types::{
    CodeActionOrCommand, DocumentSymbol, DocumentSymbolResponse, GotoDefinitionResponse, Hover,
    HoverContents, Location, MarkedString, SymbolInformation, WorkspaceEdit,
};

/// Extract hover text from an LSP hover response.
pub fn parse_hover_response(json: &str) -> Result<Option<String>> {
    let envelope: serde_json::Value = serde_json::from_str(json).context("Invalid hover JSON")?;
    let result = &envelope["result"];
    if result.is_null() {
        return Ok(None);
    }
    let hover: Hover = serde_json::from_value(result.clone()).context("Invalid Hover object")?;
    Ok(Some(hover_contents_to_string(&hover.contents)))
}

/// Parse a goto (definition/typeDefinition/implementation) response into locations.
pub fn parse_goto_response(json: &str) -> Result<Vec<Location>> {
    let envelope: serde_json::Value = serde_json::from_str(json).context("Invalid goto JSON")?;
    let result = &envelope["result"];
    if result.is_null() {
        return Ok(vec![]);
    }
    let resp: GotoDefinitionResponse =
        serde_json::from_value(result.clone()).context("Invalid goto result")?;
    Ok(match resp {
        GotoDefinitionResponse::Scalar(loc) => vec![loc],
        GotoDefinitionResponse::Array(locs) => locs,
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|link| Location {
                uri: link.target_uri,
                range: link.target_selection_range,
            })
            .collect(),
    })
}

/// Parse code action response.
pub fn parse_code_actions(json: &str) -> Result<Vec<CodeActionOrCommand>> {
    let envelope: serde_json::Value =
        serde_json::from_str(json).context("Invalid code action JSON")?;
    let result = &envelope["result"];
    if result.is_null() {
        return Ok(vec![]);
    }
    serde_json::from_value(result.clone()).context("Invalid code actions array")
}

/// Parse rename response into a workspace edit.
pub fn parse_rename_response(json: &str) -> Result<Option<WorkspaceEdit>> {
    let envelope: serde_json::Value = serde_json::from_str(json).context("Invalid rename JSON")?;
    let result = &envelope["result"];
    if result.is_null() {
        return Ok(None);
    }
    let edit: WorkspaceEdit =
        serde_json::from_value(result.clone()).context("Invalid WorkspaceEdit")?;
    Ok(Some(edit))
}

/// Parse document symbols response (handles both flat and hierarchical forms).
pub fn parse_document_symbols(json: &str) -> Result<Vec<DocumentSymbol>> {
    let envelope: serde_json::Value =
        serde_json::from_str(json).context("Invalid document symbols JSON")?;
    let result = &envelope["result"];
    if result.is_null() {
        return Ok(vec![]);
    }
    let resp: DocumentSymbolResponse =
        serde_json::from_value(result.clone()).context("Invalid document symbols")?;
    Ok(match resp {
        DocumentSymbolResponse::Flat(infos) => infos
            .into_iter()
            .map(symbol_info_to_document_symbol)
            .collect(),
        DocumentSymbolResponse::Nested(symbols) => symbols,
    })
}

/// Parse workspace symbols response.
pub fn parse_workspace_symbols(json: &str) -> Result<Vec<SymbolInformation>> {
    let envelope: serde_json::Value =
        serde_json::from_str(json).context("Invalid workspace symbols JSON")?;
    let result = &envelope["result"];
    if result.is_null() {
        return Ok(vec![]);
    }
    serde_json::from_value(result.clone()).context("Invalid workspace symbols array")
}

fn hover_contents_to_string(contents: &HoverContents) -> String {
    match contents {
        HoverContents::Scalar(s) => marked_string_text(s),
        HoverContents::Array(arr) => arr
            .iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value.clone(),
    }
}

fn marked_string_text(s: &MarkedString) -> String {
    match s {
        MarkedString::String(text) => text.clone(),
        MarkedString::LanguageString(ls) => ls.value.clone(),
    }
}

#[allow(deprecated)]
fn symbol_info_to_document_symbol(info: SymbolInformation) -> DocumentSymbol {
    DocumentSymbol {
        name: info.name,
        detail: None,
        kind: info.kind,
        tags: info.tags,
        deprecated: info.deprecated,
        range: info.location.range,
        selection_range: info.location.range,
        children: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_markup_content() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":{"kind":"markdown","value":"**fn** `main()`\n\nEntry point"}}}"#;
        let result = parse_hover_response(json).unwrap();
        assert_eq!(result.unwrap(), "**fn** `main()`\n\nEntry point");
    }

    #[test]
    fn hover_null_result() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        assert!(parse_hover_response(json).unwrap().is_none());
    }

    #[test]
    fn hover_marked_string() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":"simple hover text"}}"#;
        let result = parse_hover_response(json).unwrap();
        assert_eq!(result.unwrap(), "simple hover text");
    }

    #[test]
    fn hover_array_contents() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":[{"language":"rust","value":"fn main()"},{"language":"","value":"docs here"}]}}"#;
        let result = parse_hover_response(json).unwrap();
        assert_eq!(result.unwrap(), "fn main()\n\ndocs here");
    }

    #[test]
    fn goto_single_location() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"uri":"file:///foo.rs","range":{"start":{"line":10,"character":0},"end":{"line":10,"character":5}}}}"#;
        let locs = parse_goto_response(json).unwrap();
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].range.start.line, 10);
    }

    #[test]
    fn goto_array_locations() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":[{"uri":"file:///a.rs","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":3}}},{"uri":"file:///b.rs","range":{"start":{"line":5,"character":0},"end":{"line":5,"character":3}}}]}"#;
        let locs = parse_goto_response(json).unwrap();
        assert_eq!(locs.len(), 2);
    }

    #[test]
    fn goto_null_result() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        assert!(parse_goto_response(json).unwrap().is_empty());
    }

    #[test]
    fn goto_location_links() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":[{"targetUri":"file:///foo.rs","targetRange":{"start":{"line":0,"character":0},"end":{"line":5,"character":0}},"targetSelectionRange":{"start":{"line":1,"character":0},"end":{"line":1,"character":10}}}]}"#;
        let locs = parse_goto_response(json).unwrap();
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].range.start.line, 1);
    }

    #[test]
    fn code_actions_empty() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        assert!(parse_code_actions(json).unwrap().is_empty());
    }

    #[test]
    fn code_actions_with_items() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":[{"title":"Extract function","kind":"refactor.extract"}]}"#;
        let actions = parse_code_actions(json).unwrap();
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn rename_null() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        assert!(parse_rename_response(json).unwrap().is_none());
    }

    #[test]
    fn rename_with_changes() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"changes":{"file:///foo.rs":[{"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":7}},"newText":"bar"}]}}}"#;
        let edit = parse_rename_response(json).unwrap().unwrap();
        assert!(edit.changes.is_some());
    }

    #[test]
    fn document_symbols_nested() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":[{"name":"main","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":5,"character":1}},"selectionRange":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}}}]}"#;
        let syms = parse_document_symbols(json).unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "main");
    }

    #[test]
    fn workspace_symbols_list() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":[{"name":"Foo","kind":5,"location":{"uri":"file:///foo.rs","range":{"start":{"line":0,"character":0},"end":{"line":10,"character":0}}}}]}"#;
        let syms = parse_workspace_symbols(json).unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Foo");
    }

    #[test]
    fn workspace_symbols_null() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        assert!(parse_workspace_symbols(json).unwrap().is_empty());
    }
}
