use crate::{
    node::{NodeId, SSTNode},
    syntax::{LanguageId, SemanticKind},
    tree::SyntaxTree,
};
use ropey::Rope;
use std::collections::HashMap;

/// Builder for constructing syntax trees
pub struct TreeBuilder {
    pub(crate) rope: Rope,
    pub(crate) nodes: HashMap<NodeId, SSTNode>,
    pub(crate) next_id: usize,
    pub(crate) current_parent: Option<NodeId>,
    pub(crate) language: LanguageId,
}

impl TreeBuilder {
    /// Create a new tree builder
    pub fn new(text: &str, language: LanguageId) -> Self {
        unimplemented!()
    }

    /// Start a new composite node
    pub fn start_node(&mut self, semantic_kind: SemanticKind, syntax_kind: String, start: usize) {
        unimplemented!()
    }

    /// Finish the current composite node
    pub fn finish_node(&mut self, end: usize) -> NodeId {
        unimplemented!()
    }

    /// Add a token node
    pub fn add_token(
        &mut self,
        semantic_kind: SemanticKind,
        syntax_kind: String,
        start: usize,
        end: usize,
    ) -> NodeId {
        unimplemented!()
    }

    /// Build the final syntax tree
    pub fn finish(self) -> SyntaxTree {
        unimplemented!()
    }
}
