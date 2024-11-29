use crate::node::Node;
use std::future::Future;

pub trait Plugin {
    /// Describe the node type that this impl represents.
    fn node_type(&self) -> &'static str;
    fn init(&self) -> Box<dyn Future<Output = Box<dyn Node>>>;
}
type _EnsureDynNodeInit = Box<dyn Plugin>;
