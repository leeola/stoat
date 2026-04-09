//! Persistent (immutable, structurally-shared) linked-list stack.
//!
//! Difftastic uses this to share the [`super::graph::EnteredDelimiter`]
//! stack across millions of [`super::graph::Vertex`] allocations
//! without paying the cost of cloning the full stack on every push.
//! Two vertices that branch off the same parent share their tail
//! through an `Arc`.
//!
//! Reference: `references/difftastic/src/diff/stack.rs`. We follow the
//! same shape but with [`Arc`] instead of `bumpalo` for simplicity;
//! perf is dominated by the Dijkstra search itself, not stack
//! allocations.

use std::sync::Arc;

/// A persistent, immutable linked-list stack. `O(1)` push and pop;
/// clone is `O(1)` because the inner `Arc` is bumped without
/// duplicating the tail.
#[derive(Clone)]
pub struct Stack<T> {
    head: Option<Arc<Node<T>>>,
}

struct Node<T> {
    value: T,
    next: Option<Arc<Node<T>>>,
}

impl<T> Stack<T> {
    pub fn new() -> Self {
        Self { head: None }
    }

    /// Push `value` and return the new stack. The original stack is
    /// not modified; both stacks share the tail.
    #[must_use]
    pub fn push(&self, value: T) -> Self {
        Self {
            head: Some(Arc::new(Node {
                value,
                next: self.head.clone(),
            })),
        }
    }

    /// Borrow the top value if any.
    pub fn peek(&self) -> Option<&T> {
        self.head.as_ref().map(|n| &n.value)
    }

    /// Return a stack with the top value removed (and the popped
    /// value alongside it). The original stack is not modified.
    pub fn pop(&self) -> Option<(&T, Self)> {
        let head = self.head.as_ref()?;
        Some((
            &head.value,
            Self {
                head: head.next.clone(),
            },
        ))
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    pub fn len(&self) -> usize {
        let mut cur = self.head.as_deref();
        let mut n = 0usize;
        while let Some(node) = cur {
            n += 1;
            cur = node.next.as_deref();
        }
        n
    }
}

impl<T> Default for Stack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Stack<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut entries = Vec::new();
        let mut cur = self.head.as_deref();
        while let Some(node) = cur {
            entries.push(&node.value);
            cur = node.next.as_deref();
        }
        f.debug_list().entries(entries).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_stack_has_no_top() {
        let s: Stack<u32> = Stack::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert!(s.peek().is_none());
        assert!(s.pop().is_none());
    }

    #[test]
    fn push_and_peek_returns_top_value() {
        let s = Stack::<u32>::new().push(1).push(2).push(3);
        assert_eq!(s.peek(), Some(&3));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn pop_returns_value_and_smaller_stack() {
        let s = Stack::<u32>::new().push(1).push(2);
        let (top, rest) = s.pop().unwrap();
        assert_eq!(*top, 2);
        assert_eq!(rest.peek(), Some(&1));
        // Original stack unchanged.
        assert_eq!(s.peek(), Some(&2));
    }

    #[test]
    fn structural_sharing_keeps_originals_intact() {
        let base = Stack::<u32>::new().push(10);
        let branch_a = base.push(20);
        let branch_b = base.push(30);
        assert_eq!(base.peek(), Some(&10));
        assert_eq!(branch_a.peek(), Some(&20));
        assert_eq!(branch_b.peek(), Some(&30));
        // Both branches share the same tail.
        assert_eq!(branch_a.pop().unwrap().1.peek(), Some(&10));
        assert_eq!(branch_b.pop().unwrap().1.peek(), Some(&10));
    }
}
