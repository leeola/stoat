use input::Input;
use plugin::Plugin;
use workspace::Workspace;

pub mod error;
pub mod config {
    pub struct Config;
    // TODO: impl loading over generic FS.
}
pub mod persist {
    // TODO: save state over generic FS. Ideally configurable serialization format.
}
pub mod input;
pub mod mode;
pub mod node;
pub mod plugin;
pub mod view;
pub mod workspace;

#[derive(Default)]
pub struct Stoat {
    plugins: Vec<Box<dyn Plugin>>,
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
    pub fn std(self) -> Self {
        self
    }
    pub fn plugin(mut self, node_init: impl Plugin + 'static) -> Self {
        self.0.plugins.push(Box::new(node_init));
        self
    }
    pub fn build(self) -> Stoat {
        self.0
    }
}

// Workspace -> Node <->  Link
