//! Per-session helpers for the Claude test harness.
//!
//! [`ClaudeSessionHandle`] enqueues messages on the underlying
//! [`FakeClaudeCode`], settles the harness to deliver them through the
//! real polling pipeline, and captures a frame. [`ToolUseHandle`] is the
//! handle returned by tool helpers, carrying the auto-generated tool id
//! forward so the paired result can be emitted without manual id
//! threading.

use crate::{
    host::{
        AgentMessage, ClaudeSessionId, FakeClaudeCode, PlanEntry, TokenUsage, ToolCallContent,
        ToolCallLocation, ToolCallStatus, ToolKind,
    },
    test_harness::{
        claude::spec::{ResultSpec, StreamOpts},
        TestHarness,
    },
};
use std::{path::PathBuf, sync::Arc};

/// Per-session helper returned by [`super::ClaudeHarness::get_session`].
/// Every method enqueues the corresponding message on the underlying
/// fake, drives [`TestHarness::settle`] to deliver it, and captures a
/// frame.
pub struct ClaudeSessionHandle<'a> {
    th: &'a mut TestHarness,
    id: ClaudeSessionId,
}

impl<'a> ClaudeSessionHandle<'a> {
    pub(super) fn new(th: &'a mut TestHarness, id: ClaudeSessionId) -> Self {
        Self { th, id }
    }

    pub fn id(&self) -> ClaudeSessionId {
        self.id
    }

    // ---- Raw message enqueue -----------------------------------------

    /// Enqueue an `Init` message. Most tests don't need to call this;
    /// [`super::ClaudeHarness::init_sessions`] does not emit one either.
    pub fn init(&mut self) -> &mut Self {
        self.push(|f| f.push_init())
    }

    pub fn text(&mut self, text: &str) -> &mut Self {
        self.push(|f| f.push_text(text))
    }

    pub fn partial(&mut self, text: &str) -> &mut Self {
        self.push(|f| f.push_partial_text(text))
    }

    pub fn thinking(&mut self, text: &str) -> &mut Self {
        self.push(|f| f.push_thinking(text))
    }

    pub fn result(&mut self) -> &mut Self {
        self.result_with(ResultSpec::default())
    }

    pub fn result_with(&mut self, spec: ResultSpec) -> &mut Self {
        self.push(|f| f.push_result_with(spec.cost_usd, spec.duration_ms, spec.num_turns))
    }

    pub fn error(&mut self, message: &str) -> &mut Self {
        self.push(|f| f.push_error(message))
    }

    pub fn usage(&mut self, last: TokenUsage) -> &mut Self {
        let accumulated = last.clone();
        self.push(|f| f.push_usage(accumulated, last))
    }

    pub fn plan(&mut self, entries: Vec<PlanEntry>) -> &mut Self {
        self.push(|f| f.push_plan(entries))
    }

    /// Enqueue a fully-formed [`AgentMessage`]. Use when none of the typed
    /// helpers match your scenario.
    pub fn raw(&mut self, msg: AgentMessage) -> &mut Self {
        self.push(|f| f.push_raw(msg))
    }

    // ---- Tool helpers ------------------------------------------------

    pub fn bash(&mut self, cmd: &str) -> ToolUseHandle<'_, 'a> {
        let tool_id = self.next_tool_id();
        let input = serde_json::json!({ "command": cmd }).to_string();
        let title = truncate_title(first_line(cmd));
        let content = if cmd.is_empty() {
            vec![]
        } else {
            vec![ToolCallContent::Text {
                text: format!("```bash\n{cmd}\n```"),
            }]
        };
        self.push_tool_use(
            tool_id.clone(),
            "Bash",
            input,
            ToolKind::Execute,
            title,
            content,
            vec![],
        );
        ToolUseHandle::new(self, tool_id, ToolKind::Execute)
    }

    pub fn read(&mut self, path: impl Into<PathBuf>) -> ToolUseHandle<'_, 'a> {
        let path = path.into();
        let path_str = path.display().to_string();
        let tool_id = self.next_tool_id();
        let input = serde_json::json!({ "file_path": path_str }).to_string();
        let title = if path_str.is_empty() {
            "Read".into()
        } else {
            truncate_title(format!("Read {path_str}"))
        };
        let locations = if path_str.is_empty() {
            vec![]
        } else {
            vec![ToolCallLocation {
                path: path.clone(),
                line: None,
            }]
        };
        self.push_tool_use(
            tool_id.clone(),
            "Read",
            input,
            ToolKind::Read,
            title,
            vec![],
            locations,
        );
        ToolUseHandle::new(self, tool_id, ToolKind::Read)
    }

    pub fn write(
        &mut self,
        path: impl Into<PathBuf>,
        content: impl Into<String>,
    ) -> ToolUseHandle<'_, 'a> {
        let path = path.into();
        let content: String = content.into();
        let path_str = path.display().to_string();
        let tool_id = self.next_tool_id();
        let input = serde_json::json!({ "file_path": path_str, "content": content }).to_string();
        let title = if path_str.is_empty() {
            "Write".into()
        } else {
            truncate_title(format!("Write {path_str}"))
        };
        let (call_content, locations) = if path_str.is_empty() {
            (vec![], vec![])
        } else {
            (
                vec![ToolCallContent::Diff {
                    path: path.clone(),
                    old_text: None,
                    new_text: content,
                }],
                vec![ToolCallLocation {
                    path: path.clone(),
                    line: None,
                }],
            )
        };
        self.push_tool_use(
            tool_id.clone(),
            "Write",
            input,
            ToolKind::Edit,
            title,
            call_content,
            locations,
        );
        ToolUseHandle::new(self, tool_id, ToolKind::Edit)
    }

    pub fn edit(
        &mut self,
        path: impl Into<PathBuf>,
        old: impl Into<String>,
        new: impl Into<String>,
    ) -> ToolUseHandle<'_, 'a> {
        let path = path.into();
        let old: String = old.into();
        let new: String = new.into();
        let path_str = path.display().to_string();
        let tool_id = self.next_tool_id();
        let input = serde_json::json!({
            "file_path": path_str,
            "old_string": old,
            "new_string": new,
        })
        .to_string();
        let title = if path_str.is_empty() {
            "Edit".into()
        } else {
            truncate_title(format!("Edit {path_str}"))
        };
        let (call_content, locations) = if path_str.is_empty() {
            (vec![], vec![])
        } else {
            (
                vec![ToolCallContent::Diff {
                    path: path.clone(),
                    old_text: Some(old),
                    new_text: new,
                }],
                vec![ToolCallLocation {
                    path: path.clone(),
                    line: None,
                }],
            )
        };
        self.push_tool_use(
            tool_id.clone(),
            "Edit",
            input,
            ToolKind::Edit,
            title,
            call_content,
            locations,
        );
        ToolUseHandle::new(self, tool_id, ToolKind::Edit)
    }

    pub fn glob(&mut self, pattern: &str) -> ToolUseHandle<'_, 'a> {
        let tool_id = self.next_tool_id();
        let input = serde_json::json!({ "pattern": pattern }).to_string();
        let title = if pattern.is_empty() {
            "Glob".into()
        } else {
            truncate_title(format!("Find `{pattern}`"))
        };
        self.push_tool_use(
            tool_id.clone(),
            "Glob",
            input,
            ToolKind::Search,
            title,
            vec![],
            vec![],
        );
        ToolUseHandle::new(self, tool_id, ToolKind::Search)
    }

    pub fn grep(&mut self, pattern: &str) -> ToolUseHandle<'_, 'a> {
        let tool_id = self.next_tool_id();
        let input = serde_json::json!({ "pattern": pattern }).to_string();
        let title = truncate_title(format!("grep {pattern}"));
        self.push_tool_use(
            tool_id.clone(),
            "Grep",
            input,
            ToolKind::Search,
            title,
            vec![],
            vec![],
        );
        ToolUseHandle::new(self, tool_id, ToolKind::Search)
    }

    /// Custom tool use for names the typed helpers don't cover.
    /// `input_json` is embedded verbatim into the `ToolUse::input` field.
    pub fn custom_tool(
        &mut self,
        name: &str,
        input_json: impl Into<String>,
    ) -> ToolUseHandle<'_, 'a> {
        let tool_id = self.next_tool_id();
        self.push_tool_use(
            tool_id.clone(),
            name,
            input_json.into(),
            ToolKind::Other,
            name.to_string(),
            vec![],
            vec![],
        );
        ToolUseHandle::new(self, tool_id, ToolKind::Other)
    }

    // ---- High-level scenarios ----------------------------------------

    /// Realistic assistant turn: cumulative `PartialText` chunks, then a
    /// final `Text`, then a `Result`. See [`StreamOpts`] for configuration.
    pub fn stream_message(&mut self, text: &str) -> &mut Self {
        self.stream_message_with(text, StreamOpts::default())
    }

    pub fn stream_message_with(&mut self, text: &str, opts: StreamOpts) -> &mut Self {
        if let Some(chunk_size) = opts.chunk_size {
            for prefix in cumulative_chunks(text, chunk_size) {
                self.partial(&prefix);
            }
        }
        self.text(text);
        if opts.terminate_turn {
            let spec = opts.result.unwrap_or_default();
            self.result_with(spec);
        }
        self
    }

    /// `Text` followed by `Result`, no partial streaming.
    pub fn say(&mut self, text: &str) -> &mut Self {
        self.text(text).result()
    }

    // ---- Snapshot control --------------------------------------------

    pub fn snap(&mut self, label: &str) -> &mut Self {
        self.th.assert_snapshot(label);
        self
    }

    // ---- Assertions --------------------------------------------------

    pub fn sent_messages(&self) -> Vec<String> {
        self.fake().sent_messages()
    }

    pub fn assert_sent(&self, expected: &[&str]) {
        let actual = self.sent_messages();
        let actual_refs: Vec<&str> = actual.iter().map(String::as_str).collect();
        assert_eq!(
            actual_refs, expected,
            "sent messages for session {:?} mismatch",
            self.id
        );
    }

    pub fn assert_send_count(&self, n: usize) {
        self.fake().assert_send_count(n);
    }

    // ---- Internals ---------------------------------------------------

    fn fake(&self) -> &Arc<FakeClaudeCode> {
        self.th
            .claude_fakes
            .get(&self.id)
            .expect("session tracked by harness")
    }

    fn push<F: FnOnce(&Arc<FakeClaudeCode>)>(&mut self, f: F) -> &mut Self {
        f(self.fake());
        self.th.settle();
        self.th.capture("claude_message");
        self
    }

    fn push_tool_use(
        &mut self,
        id: String,
        name: &str,
        input: String,
        kind: ToolKind,
        title: String,
        content: Vec<ToolCallContent>,
        locations: Vec<ToolCallLocation>,
    ) {
        let msg = AgentMessage::ToolUse {
            id,
            name: name.into(),
            input,
            kind,
            title,
            content,
            locations,
        };
        self.push(|f| f.push_raw(msg));
    }

    fn next_tool_id(&mut self) -> String {
        let n = self.th.claude_tool_id_counter;
        self.th.claude_tool_id_counter += 1;
        format!("toolu_{n:024x}")
    }
}

/// Handle for pairing a `ToolUse` with its subsequent `ToolResult`.
/// Returned by [`ClaudeSessionHandle::bash`] and siblings. Carries the
/// auto-generated tool id and classified kind forward so the result
/// message matches the use without manual id threading.
pub struct ToolUseHandle<'h, 'a> {
    session: &'h mut ClaudeSessionHandle<'a>,
    tool_id: String,
    kind: ToolKind,
}

impl<'h, 'a> ToolUseHandle<'h, 'a> {
    fn new(session: &'h mut ClaudeSessionHandle<'a>, tool_id: String, kind: ToolKind) -> Self {
        Self {
            session,
            tool_id,
            kind,
        }
    }

    pub fn id(&self) -> &str {
        &self.tool_id
    }

    /// Snapshot without consuming the handle. Useful for asserting the
    /// "tool_use queued but result not yet arrived" state before calling
    /// [`Self::result`].
    pub fn snap(self, label: &str) -> Self {
        self.session.th.assert_snapshot(label);
        self
    }

    /// Emit a successful `ToolResult` paired with the owning `ToolUse`.
    pub fn result(self, content: &str) -> &'h mut ClaudeSessionHandle<'a> {
        self.push_result(content, ToolCallStatus::Completed)
    }

    pub fn failed(self, content: &str) -> &'h mut ClaudeSessionHandle<'a> {
        self.push_result(content, ToolCallStatus::Failed)
    }

    pub fn progress(self) -> &'h mut ClaudeSessionHandle<'a> {
        self.push_result("", ToolCallStatus::InProgress)
    }

    /// Drop the handle without emitting a `ToolResult`. Matches the
    /// "tool_use pending, no result yet" scenario.
    pub fn pending(self) -> &'h mut ClaudeSessionHandle<'a> {
        self.session
    }

    fn push_result(self, content: &str, status: ToolCallStatus) -> &'h mut ClaudeSessionHandle<'a> {
        let msg = AgentMessage::ToolResult {
            id: self.tool_id,
            content: content.to_string(),
            status,
            kind: self.kind,
            terminal_meta: None,
        };
        self.session.push(|f| f.push_raw(msg));
        self.session
    }
}

const MAX_TITLE_LEN: usize = 256;

fn truncate_title(s: impl Into<String>) -> String {
    let s = s.into();
    if s.chars().count() <= MAX_TITLE_LEN {
        return s;
    }
    let mut out = String::with_capacity(MAX_TITLE_LEN);
    for (i, ch) in s.chars().enumerate() {
        if i >= MAX_TITLE_LEN {
            break;
        }
        out.push(ch);
    }
    out
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

/// Produce cumulative prefixes of `text` at roughly `chunk_size` byte
/// boundaries. Breaks only on UTF-8 character boundaries. The final prefix
/// (equal to `text` itself) is omitted because callers follow it with a
/// full `Text` message.
fn cumulative_chunks(text: &str, chunk_size: usize) -> Vec<String> {
    if chunk_size == 0 || text.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    let mut last_boundary = 0;
    let total = text.len();
    while last_boundary + chunk_size < total {
        let target = last_boundary + chunk_size;
        let mut boundary = target.min(total);
        while boundary < total && !text.is_char_boundary(boundary) {
            boundary += 1;
        }
        if boundary >= total {
            break;
        }
        out.push(text[..boundary].to_string());
        last_boundary = boundary;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_chunks_breaks_on_utf8_boundaries() {
        let chunks = cumulative_chunks("abcdefghij", 3);
        assert_eq!(
            chunks,
            vec![
                "abc".to_string(),
                "abcdef".to_string(),
                "abcdefghi".to_string()
            ]
        );
    }

    #[test]
    fn cumulative_chunks_never_splits_multibyte_char() {
        let chunks = cumulative_chunks("aé", 2);
        assert_eq!(chunks, Vec::<String>::new());
    }

    #[test]
    fn cumulative_chunks_empty_input() {
        assert_eq!(cumulative_chunks("", 3), Vec::<String>::new());
    }

    #[test]
    fn cumulative_chunks_zero_size() {
        assert_eq!(cumulative_chunks("abc", 0), Vec::<String>::new());
    }
}
