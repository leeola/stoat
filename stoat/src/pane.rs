use crate::view::View;

pub struct Pane {
    views: Vec<View>,
    active_index: usize,
}

impl Pane {
    pub fn new() -> Self {
        Self {
            views: Vec::new(),
            active_index: 0,
        }
    }

    pub fn add_view(&mut self, view: View) {
        self.views.push(view);
        self.active_index = self.views.len() - 1;
    }

    pub fn active_view(&self) -> Option<&View> {
        self.views.get(self.active_index)
    }

    pub fn active_view_mut(&mut self) -> Option<&mut View> {
        self.views.get_mut(self.active_index)
    }
}

impl Default for Pane {
    fn default() -> Self {
        Self::new()
    }
}
