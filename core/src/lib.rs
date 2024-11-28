use input::Input;
use node::NodeInit;
use workspace::Workspace;

pub mod error;
pub mod input;
pub mod mode;
pub mod output;
pub mod workspace;

// TODO: New name, don't like Node. Close enough for now.
pub mod node;

#[derive(Default)]
pub struct Stoat {
    node_inits: Vec<Box<dyn NodeInit>>,
    workspaces: Vec<Workspace>,
}
impl Stoat {
    pub fn new() -> Self {
        Self::builder().std().build()
    }
    /// Push an input into Stoat.
    pub fn input(&mut self, _input: impl Into<Input>) {
        todo!()
    }
    /// Push multiple inputs into Stoat.
    pub fn inputs<T: Into<Input>>(&mut self, inputs: impl IntoIterator<Item = T>) {
        for t in inputs {
            self.input(t)
        }
    }
    pub fn builder() -> StoatBuilder {
        Default::default()
    }
}

#[derive(Default)]
pub struct StoatBuilder(Stoat);
impl StoatBuilder {
    /// Include standard configuration and plugins.
    pub fn std(mut self) -> Self {
        self
    }
    pub fn node_init(mut self, node_init: impl NodeInit + 'static) -> Self {
        self.0.node_inits.push(Box::new(node_init));
        self
    }
    pub fn build(self) -> Stoat {
        self.0
    }
}

// Workspace -> Node <->  Link
