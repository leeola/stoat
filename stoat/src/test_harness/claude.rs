//! Test harness for driving Claude Code sessions. See
//! [`super::TestHarness::claude`] for the entry point and [`ClaudeHarness`]
//! for the facade, [`ClaudeSessionHandle`] for per-session helpers, and
//! [`ToolUseHandle`] for pairing tool calls with their results.
//!
//! Every helper routes through the real polling task and notification
//! channel via [`super::TestHarness::settle`], so tests exercise the full
//! data flow from fake host to rendered chat state.

mod handle;
mod spec;

use crate::{
    host::{ClaudeSessionId, FakeClaudeCode},
    test_harness::TestHarness,
};
pub use handle::ClaudeSessionHandle;
pub use spec::{ResultSpec, SessionSpec};

/// Facade returned by [`TestHarness::claude`]. Wraps a borrow of the
/// parent harness; re-borrowed per call so handles stay short-lived.
pub struct ClaudeHarness<'a> {
    th: &'a mut TestHarness,
}

impl<'a> ClaudeHarness<'a> {
    pub(crate) fn new(th: &'a mut TestHarness) -> Self {
        Self { th }
    }

    /// Seed `specs` sessions. Each `visible` spec opens through the
    /// [`stoat_action::OpenClaude`] dispatch path (and becomes the active
    /// chat); the rest are reserved and filled directly. Every spec also
    /// registers a [`crate::host::ClaudeSessionSummary`] that
    /// [`crate::host::ClaudeCodeHost::list_sessions`] will return. Returns
    /// ids in the order of `specs`.
    pub fn init_sessions<I, T>(&mut self, specs: I) -> Vec<ClaudeSessionId>
    where
        I: IntoIterator<Item = T>,
        T: Into<SessionSpec>,
    {
        let specs: Vec<SessionSpec> = specs.into_iter().map(Into::into).collect();
        let mut ids = Vec::with_capacity(specs.len());
        for spec in specs {
            let seq = self.th.claude_fakes.len();
            let summary = spec.to_summary(seq);
            self.th.fake_claude_host.register_summary(summary);
            let id = if spec.visible {
                self.th.open_claude_with_fake(FakeClaudeCode::new())
            } else {
                self.th.create_background_session(FakeClaudeCode::new())
            };
            ids.push(id);
        }
        ids
    }

    /// Convenience: open a single visible session with default spec.
    pub fn open(&mut self) -> ClaudeSessionId {
        let seq = self.th.claude_fakes.len();
        let summary = SessionSpec::default().to_summary(seq);
        self.th.fake_claude_host.register_summary(summary);
        self.th.open_claude_with_fake(FakeClaudeCode::new())
    }

    /// Borrow a per-session handle. Panics if `id` is unknown to this
    /// harness (i.e. was not created through [`Self::init_sessions`],
    /// [`Self::open`], or the lower-level `open_claude_with_fake` /
    /// `create_background_session` paths on [`TestHarness`]).
    pub fn get_session(&mut self, id: ClaudeSessionId) -> ClaudeSessionHandle<'_> {
        assert!(
            self.th.claude_fakes.contains_key(&id),
            "claude session {id:?} not tracked by this harness"
        );
        ClaudeSessionHandle::new(self.th, id)
    }

    /// Session ids known to this harness. Insertion order is not
    /// guaranteed; use this to assert the *set* of seeded sessions.
    pub fn session_ids(&self) -> Vec<ClaudeSessionId> {
        self.th.claude_fakes.keys().copied().collect()
    }
}
