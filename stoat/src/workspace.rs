use crate::{buffer::BufferStore, editor::Editor, pane::Pane, view::View};
use std::{io, path::PathBuf};

pub struct Workspace {
    pub name: String,
    pub root_path: Option<PathBuf>,
    pub buffer_store: BufferStore,
    panes: Vec<Pane>,
    active_pane: usize,
}

impl Workspace {
    pub fn new(name: String, root_path: Option<PathBuf>) -> Self {
        let panes = vec![Pane::new()];
        Self {
            name,
            root_path,
            buffer_store: BufferStore::new(),
            panes,
            active_pane: 0,
        }
    }

    pub fn open_file(&mut self, path: PathBuf) -> io::Result<()> {
        let (buffer_id, buffer) = self.buffer_store.open(path)?;
        let editor = Editor::new(buffer_id, buffer);
        let view = View::Editor(editor);
        self.active_pane_mut().add_view(view);
        Ok(())
    }

    pub fn active_pane(&self) -> &Pane {
        &self.panes[self.active_pane]
    }

    pub fn active_pane_mut(&mut self) -> &mut Pane {
        &mut self.panes[self.active_pane]
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new("Default".to_string(), None)
    }
}
