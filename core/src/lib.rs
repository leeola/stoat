/// The core implementation the [`Stoat`] editor runtime. Ie the editor minus the GUI/TUI/CLI
/// interfaces.
use input::UserInput;
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

/// The primary interface for Stoat, a canvas and node based data and text editor.
#[derive(Default)]
pub struct Stoat {
    /// The activate workspace taking user inputs.
    active: Workspace,
    //
    // A dedicated multi-workspace manager that manages workspaces by keys, order, finds, toggles,
    // etc.
    //
    // workspaces: Workspaces,
}

impl Stoat {
    pub fn new() -> Self {
        Self::builder().std().build()
    }
    pub fn builder() -> StoatBuilder {
        Default::default()
    }
    // TODO: Make async?
    pub fn load_state(&self) -> Result<()> {
        // Just loading fake state atm, resolving to an initial hello world, as if it's a new
        // session/workspace.
        todo!()
    }
    /// Push a user input into Stoat.
    //
    // TODO: UserInput needs to return available actions? For automatic ? and client validation?
    pub fn user_input(&mut self, _ue: impl Into<UserInput>) {
        todo!()
    }
    /// Push multiple user events into Stoat.
    pub fn user_inputs<T: Into<UserInput>>(&mut self, user_inputs: impl IntoIterator<Item = T>) {
        for ue in user_inputs {
            self.user_input(ue)
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
    pub fn build(self) -> Stoat {
        self.0
    }
}

// Workspace -> Node <->  Link
