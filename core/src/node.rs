// TODO: New name, don't like Node. Close enough for now.
pub trait Node {}
type _EnsureDynNode = Box<dyn Node>;
