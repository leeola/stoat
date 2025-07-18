use crate::{
    state::RenderState,
    widget::{theme::Colors, Node},
};
use iced::{
    widget::canvas::{self, Frame, Geometry},
    Point, Rectangle, Size, Vector,
};

/// Canvas widget that renders the node editor
pub struct NodeCanvas<'a> {
    render_state: &'a RenderState,
}

impl<'a> NodeCanvas<'a> {
    pub fn new(render_state: &'a RenderState) -> Self {
        Self { render_state }
    }
}

impl<'a> canvas::Program<crate::Message> for NodeCanvas<'a> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());

        // Fill background with improved color
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), Colors::CANVAS_BACKGROUND);

        // Apply viewport transformation
        frame.translate(Vector::new(
            self.render_state.viewport.offset.0,
            self.render_state.viewport.offset.1,
        ));
        frame.scale(self.render_state.viewport.zoom);

        // Draw all nodes using the new widget
        for node_data in &self.render_state.nodes {
            let node = Node::new(node_data);
            let position = Point::new(node_data.position.0, node_data.position.1);
            let size = Size::new(node_data.size.0, node_data.size.1);
            node.draw(&mut frame, position, size);
        }

        vec![frame.into_geometry()]
    }
}
