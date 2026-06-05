//! Agent->client `fs/read_text_file` and `fs/write_text_file` request
//! handlers, routed through the injected [`FsHost`] so tests stay pure
//! against `FakeFs`.

use crate::rpc::{error, parse_params, INTERNAL_ERROR, INVALID_PARAMS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io, path::Path, sync::Arc};
use stoat::host::FsHost;
use stoat_agent_claude_code::jsonrpc::RpcError;

pub(crate) const FS_READ_TEXT_FILE: &str = "fs/read_text_file";
pub(crate) const FS_WRITE_TEXT_FILE: &str = "fs/write_text_file";

/// Answer an agent->client fs request through `fs`, or `None` if `method`
/// is not an fs request this module owns (the caller rejects it).
pub(crate) fn handle_fs_request(
    method: &str,
    params: Option<&Value>,
    fs: &Arc<dyn FsHost>,
) -> Option<Result<Value, RpcError>> {
    match method {
        FS_READ_TEXT_FILE => Some(read_text_file(params, fs)),
        FS_WRITE_TEXT_FILE => Some(write_text_file(params, fs)),
        _ => None,
    }
}

/// `fs/read_text_file` params. `sessionId` is accepted on the wire but
/// ignored: reads route through the connection's [`FsHost`], not a
/// session.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadTextFileParams {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadTextFileResult {
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteTextFileParams {
    path: String,
    content: String,
}

fn read_text_file(params: Option<&Value>, fs: &Arc<dyn FsHost>) -> Result<Value, RpcError> {
    let params: ReadTextFileParams = parse_params(params)?;

    let mut buf = Vec::new();
    fs.read(Path::new(&params.path), &mut buf)
        .map_err(|source| io_error(&source))?;
    let content =
        String::from_utf8(buf).map_err(|_| error(INVALID_PARAMS, "file is not valid UTF-8"))?;

    to_value(ReadTextFileResult {
        content: slice_lines(&content, params.line, params.limit),
    })
}

fn write_text_file(params: Option<&Value>, fs: &Arc<dyn FsHost>) -> Result<Value, RpcError> {
    let params: WriteTextFileParams = parse_params(params)?;
    fs.write(Path::new(&params.path), params.content.as_bytes())
        .map_err(|source| io_error(&source))?;
    Ok(Value::Null)
}

/// Select the `limit` lines starting at the 1-based `line`. With neither
/// bound the content is returned verbatim (preserving a trailing
/// newline); slicing joins the selected lines with `\n`.
fn slice_lines(content: &str, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_none() && limit.is_none() {
        return content.to_string();
    }
    let start = line.unwrap_or(1).max(1) as usize - 1;
    let mut lines: Vec<&str> = content.lines().skip(start).collect();
    if let Some(limit) = limit {
        lines.truncate(limit as usize);
    }
    lines.join("\n")
}

fn to_value<T: Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|source| error(INTERNAL_ERROR, source.to_string()))
}

fn io_error(source: &io::Error) -> RpcError {
    error(INTERNAL_ERROR, source.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use stoat::host::FakeFs;

    fn fs_with(path: &str, contents: &str) -> Arc<dyn FsHost> {
        let fs = FakeFs::new();
        fs.write(Path::new(path), contents.as_bytes()).unwrap();
        Arc::new(fs)
    }

    #[test]
    fn read_returns_full_contents() {
        let fs = fs_with("/a.txt", "hello\nworld\n");
        let result = handle_fs_request(FS_READ_TEXT_FILE, Some(&json!({ "path": "/a.txt" })), &fs)
            .expect("fs method")
            .expect("ok");
        assert_eq!(result, json!({ "content": "hello\nworld\n" }));
    }

    #[test]
    fn read_slices_by_line_and_limit() {
        let fs = fs_with("/a.txt", "one\ntwo\nthree\nfour");
        let result = handle_fs_request(
            FS_READ_TEXT_FILE,
            Some(&json!({ "path": "/a.txt", "line": 2, "limit": 2 })),
            &fs,
        )
        .expect("fs method")
        .expect("ok");
        assert_eq!(result, json!({ "content": "two\nthree" }));
    }

    #[test]
    fn write_persists_through_fs() {
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        let result = handle_fs_request(
            FS_WRITE_TEXT_FILE,
            Some(&json!({ "path": "/out.txt", "content": "saved" })),
            &fs,
        )
        .expect("fs method")
        .expect("ok");
        assert_eq!(result, Value::Null);

        let mut buf = Vec::new();
        fs.read(Path::new("/out.txt"), &mut buf).unwrap();
        assert_eq!(buf, b"saved");
    }

    #[test]
    fn read_missing_file_is_an_error() {
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        let result = handle_fs_request(FS_READ_TEXT_FILE, Some(&json!({ "path": "/nope" })), &fs)
            .expect("fs method");
        assert!(result.is_err());
    }

    #[test]
    fn unknown_method_is_not_owned() {
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        assert!(handle_fs_request("fs/chmod", None, &fs).is_none());
    }
}
