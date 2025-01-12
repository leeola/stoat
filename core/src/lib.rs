use input::Input;
use plugin::IoPlugin;
use view::View;
use workspace::Workspace;

pub mod config;
pub mod error;
pub mod persist {
    // TODO: save state over generic FS. Ideally configurable serialization format.
}
pub mod data;
pub mod input;
pub mod mode;
pub mod node;
pub mod plugin;
pub mod value;
pub mod view;
pub mod workspace;

pub use error::{Error, Result};

#[derive(Default)]
pub struct Stoat {
    io_plugin: Vec<Box<dyn IoPlugin>>,
    workspaces: Vec<Workspace>,
    active: Workspace,
}
impl Stoat {
    pub fn new() -> Self {
        Self::builder().std().build()
    }
    pub fn builder() -> StoatBuilder {
        Default::default()
    }
    // TODO: Make async?
    // TODO: Make Result. I want to switch to Snafu first, or at least try.
    pub fn load_state(&self) {
        // Just loading fake state atm, resolving to an initial hello world, as if it's a new
        // session/workspace.
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
    pub fn view(&self) -> &View {
        self.active.view()
    }
}

#[derive(Default)]
pub struct StoatBuilder(Stoat);
impl StoatBuilder {
    /// Include standard configuration and plugins.
    pub fn std(self) -> Self {
        self
    }
    pub fn plugin(mut self, node_init: impl IoPlugin + 'static) -> Self {
        self.0.io_plugin.push(Box::new(node_init));
        self
    }
    pub fn build(self) -> Stoat {
        self.0
    }
}

// Workspace -> Node <->  Link
