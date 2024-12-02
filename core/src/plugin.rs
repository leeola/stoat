use crate::{value::Value, Result};
use std::future::Future;

pub trait IoPlugin {
    /// Describe the data source that this impl represents.
    fn name(&self) -> &'static str;
    fn methods(&self) -> IoMethodType;
    fn init(&self) -> Box<dyn Future<Output = Box<dyn IoMethod>>>;
}

// TODO: impl DataPlugin for a generic `Box<dyn DataMethod>`. Allowing for easy closure additions of
// methods.

#[derive(Debug)]
pub struct IoMethodType {
    pub name: &'static str,
    // TODO: impl streaming support. Likely via a separate trait, `DataMethodStreaming` or
    // something, though unlike `call()` i suspect it'll require different semantics for
    // pub streaming: bool,
    // TODO: impl Schema.
    pub input: (),
    pub output: (),
}

pub trait IoMethod {
    // NIT: I might want to include a native Result-like into Value?
    fn call(&self, input: Value) -> Box<dyn Future<Output = Result<Value>>>;
}
