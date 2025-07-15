//! Tree structure utilities

use crate::syntax::Syntax;

/// Builder for constructing syntax trees
pub struct TreeBuilder<S: Syntax> {
    // TODO: Implement tree building
    _phantom: std::marker::PhantomData<S>,
}

impl<S: Syntax> TreeBuilder<S> {
    /// Create a new tree builder
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<S: Syntax> Default for TreeBuilder<S> {
    fn default() -> Self {
        Self::new()
    }
}
