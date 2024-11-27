use node::NodeInit;
use workspace::Workspace;

pub mod workspace {
    pub struct Workspace;
}

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
