// TODO: New name, don't like Init. Works well enough, i suppose.
pub trait NodeInit {
    /// Describe the node type that this impl represents.
    fn node_type(&self) -> &'static str;
}
