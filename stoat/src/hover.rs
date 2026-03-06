use stoat_lsp::response::HoverBlock;

#[derive(Clone, Debug, Default)]
pub struct HoverState {
    pub blocks: Vec<HoverBlock>,
    pub visible: bool,
}

impl HoverState {
    pub fn dismiss(&mut self) {
        self.visible = false;
        self.blocks.clear();
    }
}
