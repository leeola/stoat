use gpui::TestAppContext;
use lsp_types::Position;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use stoat_lsp::{response::parse_hover_response, test::MockLspServer, LspManager, StdioTransport};

/// Mock receives JSON without `"id"` from LspManager::request(), causing
/// parse_message() to classify it as Notification -> bail.
#[gpui::test]
async fn hover_with_mock_server(cx: &mut TestAppContext) {
    let manager = cx.update(|cx| {
        Arc::new(LspManager::new(
            cx.background_executor().clone(),
            Duration::from_secs(5),
        ))
    });

    let hover_result = json!({
        "contents": {
            "kind": "markdown",
            "value": "```rust\nfn greet() -> String\n```"
        }
    });

    let mock =
        Arc::new(MockLspServer::rust_analyzer().with_response("textDocument/hover", hover_result));

    let server_id = manager.add_server("rust-analyzer", mock);

    let uri: lsp_types::Uri = "file:///test/src/main.rs".parse().unwrap();
    let position = Position {
        line: 1,
        character: 21,
    };

    let handle = manager.hover(server_id, uri, position).unwrap();
    let response = handle.await.unwrap();

    let hover_text = parse_hover_response(&response)
        .expect("parse_hover_response failed")
        .expect("hover returned None");
    assert!(
        hover_text.contains("fn greet()"),
        "Expected hover to contain 'fn greet()', got: {hover_text}"
    );
}

/// Real rust-analyzer E2E test. Gated behind STOAT_REAL_LSP env var.
#[gpui::test]
async fn hover_with_real_rust_analyzer(cx: &mut TestAppContext) {
    if std::env::var("STOAT_REAL_LSP").is_err() {
        eprintln!("Skipping real LSP test (set STOAT_REAL_LSP=1 to enable)");
        return;
    }

    let ra_path = match which::which("rust-analyzer") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("Skipping: rust-analyzer not found on PATH");
            return;
        },
    };

    let tmp = tempfile::TempDir::new().unwrap();
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    let main_rs = src_dir.join("main.rs");
    let source =
        "fn greet() -> String { String::from(\"hello\") }\nfn main() { let _x = greet(); }\n";
    std::fs::write(&main_rs, source).unwrap();

    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"test_proj\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let manager = cx.update(|cx| {
        let executor = cx.background_executor().clone();
        let transport =
            Arc::new(StdioTransport::spawn(ra_path, vec![], None, executor.clone()).unwrap());
        let mgr = Arc::new(LspManager::new(executor, Duration::from_secs(30)));
        let server_id = mgr.add_server("rust-analyzer", transport);
        (mgr, server_id)
    });
    let (manager, server_id) = manager;

    let init_request = json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": format!("file://{}", tmp.path().display()),
            "capabilities": {
                "textDocument": {
                    "hover": { "contentFormat": ["markdown", "plaintext"] }
                }
            }
        }
    });

    let response = manager
        .request(server_id, init_request)
        .unwrap()
        .await
        .unwrap();
    eprintln!(
        "Initialize response (truncated): {}...",
        &response[..response.len().min(200)]
    );

    manager
        .notify(
            server_id,
            json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        )
        .await
        .unwrap();

    manager.start_listener(server_id).unwrap();

    let uri_str = format!("file://{}", main_rs.display());
    let uri: lsp_types::Uri = uri_str.parse().unwrap();
    manager
        .did_open(
            server_id,
            uri.clone(),
            "rust".to_string(),
            1,
            source.to_string(),
        )
        .await
        .unwrap();

    cx.background_executor.timer(Duration::from_secs(5)).await;

    let handle = manager
        .hover(
            server_id,
            uri,
            Position {
                line: 1,
                character: 22,
            },
        )
        .unwrap();
    let response = handle.await.unwrap();
    eprintln!("Hover response: {response}");

    let hover_text = parse_hover_response(&response)
        .expect("parse_hover_response failed")
        .expect("hover returned None -- rust-analyzer gave no info");
    assert!(
        hover_text.contains("greet"),
        "Expected hover to mention 'greet', got: {hover_text}"
    );

    manager.shutdown_all();
}
