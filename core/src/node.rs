use async_trait::async_trait;
use std::future::Future;

// TODO: New name, don't like Init. Works well enough, i suppose.
pub trait NodeInit {
    /// Describe the node type that this impl represents.
    fn node_type(&self) -> &'static str;
    fn init(&self) -> Box<dyn Future<Output = Box<dyn Node>>>;
}
type _EnsureDynNodeInit = Box<dyn NodeInit>;

pub trait Node {}
type _EnsureDynNode = Box<dyn Node>;
