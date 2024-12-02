use crate::node::Node;
use std::future::Future;

pub trait IoPlugin {
    /// Describe the data source that this impl represents.
    fn name(&self) -> &'static str;
    fn init(&self) -> Box<dyn Future<Output = Box<dyn Node>>>;
}
type _EnsureDynIoPlugin = Box<dyn IoPlugin>;
