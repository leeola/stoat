//! Result message payloads and token-usage accounting.

use serde::{Deserialize, Serialize};

/// Result message subtypes indicating completion status.
///
/// These appear in the final `Result` message to indicate how
/// the conversation ended and whether it was successful.
///
/// Unrecognized subtype strings are captured in [`ResultSubtype::Unknown`]
/// so the parser does not reject messages when the CLI introduces a new
/// error category.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultSubtype {
    /// Normal successful completion.
    ///
    /// The conversation completed without errors. Claude provided
    /// all requested responses and any tool executions succeeded.
    Success,

    /// Conversation hit the maximum turn limit.
    ///
    /// The configured `max_turns` was reached before the conversation
    /// naturally completed. This is a safety mechanism to prevent
    /// infinite loops in tool usage.
    ErrorMaxTurns,

    /// Conversation hit a configured USD budget ceiling (`--max-budget-usd`).
    ErrorMaxBudgetUsd,

    /// Structured-output retries hit their configured cap.
    ErrorMaxStructuredOutputRetries,

    /// An error occurred during execution.
    ///
    /// This indicates a runtime error such as:
    /// - Tool execution failure
    /// - API communication error
    /// - Invalid message format
    /// - Process crash or timeout
    ErrorDuringExecution,

    /// Unrecognized subtype. Preserves the raw string so callers can
    /// surface or log the value without the message being dropped.
    #[serde(untagged)]
    Unknown(String),
}

/// Token usage accounting for a single assistant turn.
///
/// Emitted on `Assistant` messages and aggregated on `Result`. Cache
/// fields are optional because the API only populates them when prompt
/// caching is active on the turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

/// Per-model usage breakdown emitted on the terminal `Result` message
/// when the session touched more than one model (e.g. primary + sub-agent).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Reason the model stopped producing the current turn. Mirrors the
/// Anthropic API's `stop_reason` values plus a fallback for unknown strings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    /// Session was cancelled mid-turn.
    Cancelled,
    #[serde(untagged)]
    Unknown(String),
}
