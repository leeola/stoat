//! [`ChainedPermissionPolicy`] composes multiple
//! [`PermissionCallback`]s into a single ordered gate.
//!
//! Each callback runs in turn until one returns a non-`Allow`
//! outcome; the chain returns that outcome immediately. Walking
//! past every callback yields `Allow`, matching the trait's
//! "unconfigured == permit" default.
//!
//! Composes a hardcoded baseline like [`super::denial::BashDenialPolicy`]
//! ahead of any configured policy so the baseline cannot be undermined
//! by a user `always_allow` rule.

use super::permission::{PermissionCallback, PermissionResult, ToolPermissionContext};
use async_trait::async_trait;
use std::sync::Arc;

pub struct ChainedPermissionPolicy {
    callbacks: Vec<Arc<dyn PermissionCallback>>,
}

impl ChainedPermissionPolicy {
    pub fn new(callbacks: Vec<Arc<dyn PermissionCallback>>) -> Self {
        Self { callbacks }
    }
}

#[async_trait]
impl PermissionCallback for ChainedPermissionPolicy {
    async fn can_use_tool(
        &self,
        tool_name: &str,
        input_json: &str,
        context: ToolPermissionContext<'_>,
    ) -> PermissionResult {
        for callback in &self.callbacks {
            let result = callback
                .can_use_tool(tool_name, input_json, context.clone())
                .await;
            match result {
                PermissionResult::Allow { .. } => continue,
                other => return other,
            }
        }
        PermissionResult::allow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting {
        result: PermissionResult,
        calls: Arc<AtomicUsize>,
    }

    impl Counting {
        fn new(result: PermissionResult) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    result,
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    #[async_trait]
    impl PermissionCallback for Counting {
        async fn can_use_tool(
            &self,
            _tool: &str,
            _input: &str,
            _ctx: ToolPermissionContext<'_>,
        ) -> PermissionResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn empty_chain_allows() {
        let chain = ChainedPermissionPolicy::new(vec![]);
        let result = chain
            .can_use_tool("Bash", "{}", ToolPermissionContext::bare())
            .await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
    }

    #[tokio::test]
    async fn first_deny_short_circuits() {
        let (first, first_calls) = Counting::new(PermissionResult::deny("nope"));
        let (second, second_calls) = Counting::new(PermissionResult::allow());
        let chain = ChainedPermissionPolicy::new(vec![Arc::new(first), Arc::new(second)]);
        let result = chain
            .can_use_tool("Bash", "{}", ToolPermissionContext::bare())
            .await;
        assert!(matches!(result, PermissionResult::Deny { .. }));
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn allow_falls_through() {
        let (first, first_calls) = Counting::new(PermissionResult::allow());
        let (second, second_calls) = Counting::new(PermissionResult::deny("from second"));
        let chain = ChainedPermissionPolicy::new(vec![Arc::new(first), Arc::new(second)]);
        let result = chain
            .can_use_tool("Bash", "{}", ToolPermissionContext::bare())
            .await;
        match result {
            PermissionResult::Deny { message, .. } => assert_eq!(message, "from second"),
            other => panic!("expected Deny, got {other:?}"),
        }
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn all_allow_returns_allow() {
        let (first, _) = Counting::new(PermissionResult::allow());
        let (second, _) = Counting::new(PermissionResult::allow());
        let chain = ChainedPermissionPolicy::new(vec![Arc::new(first), Arc::new(second)]);
        let result = chain
            .can_use_tool("Bash", "{}", ToolPermissionContext::bare())
            .await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
    }

    #[tokio::test]
    async fn cancel_short_circuits() {
        let (first, _) = Counting::new(PermissionResult::cancel());
        let (second, second_calls) = Counting::new(PermissionResult::allow());
        let chain = ChainedPermissionPolicy::new(vec![Arc::new(first), Arc::new(second)]);
        let result = chain
            .can_use_tool("Bash", "{}", ToolPermissionContext::bare())
            .await;
        assert!(matches!(result, PermissionResult::Cancel));
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }
}
