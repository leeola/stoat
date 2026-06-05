//! Shared JSON-RPC error helpers for the agent->client request handlers.

use serde::de::DeserializeOwned;
use serde_json::Value;
use stoat_agent_claude_code::jsonrpc::RpcError;

pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const INTERNAL_ERROR: i64 = -32603;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;

/// Build an [`RpcError`] with the given code and message and no `data`.
pub(crate) fn error(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: None,
    }
}

/// A JSON-RPC method-not-found error for `method`.
pub(crate) fn method_not_found(method: &str) -> RpcError {
    error(METHOD_NOT_FOUND, format!("method not found: {method}"))
}

/// Deserialize request params into `T`, mapping absence or a schema
/// mismatch to an invalid-params error.
pub(crate) fn parse_params<T: DeserializeOwned>(params: Option<&Value>) -> Result<T, RpcError> {
    let value = params.ok_or_else(|| error(INVALID_PARAMS, "missing params"))?;
    serde_json::from_value(value.clone())
        .map_err(|source| error(INVALID_PARAMS, source.to_string()))
}
