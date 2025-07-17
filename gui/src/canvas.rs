use crate::{
    state::{NodeContent, NodeRenderData, NodeState, RenderState},
    theme::{Layout, Theme},
};
use iced::{
    Font, Point, Rectangle, Size, Vector,
    widget::canvas::{self, Frame, Geometry, Path, Stroke, Text},
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

        // Fill background
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), Theme::CANVAS_BACKGROUND);

        // Apply viewport transformation
        frame.translate(Vector::new(
            self.render_state.viewport.offset.0,
            self.render_state.viewport.offset.1,
        ));
        frame.scale(self.render_state.viewport.zoom);

        // Draw all nodes
        for node in &self.render_state.nodes {
            draw_node(&mut frame, node);
        }

        vec![frame.into_geometry()]
    }
}

fn draw_node(frame: &mut Frame, node: &NodeRenderData) {
    let position = Point::new(node.position.0, node.position.1);
    let size = Size::new(node.size.0, node.size.1);

    // Determine border color based on state
    let border_color = match node.state {
        NodeState::Normal => Theme::NODE_BORDER,
        NodeState::Focused => Theme::NODE_BORDER_FOCUSED,
        NodeState::Selected => Theme::NODE_BORDER_SELECTED,
    };

    // Draw node background with border
    let background_path = Path::new(|builder| {
        builder.move_to(position);
        builder.rectangle(position, size);
    });

    frame.fill(&background_path, Theme::NODE_BACKGROUND);
    frame.stroke(
        &background_path,
        Stroke::default()
            .with_width(Layout::NODE_BORDER_WIDTH)
            .with_color(border_color),
    );

    // Draw title bar
    let title_size = Size::new(size.width, Layout::NODE_TITLE_HEIGHT);
    let title_path = Path::new(|builder| {
        builder.move_to(position);
        builder.rectangle(position, title_size);
    });

    frame.fill(&title_path, Theme::NODE_TITLE_BACKGROUND);

    // Draw title text
    let title_position = Point::new(
        position.x + Layout::NODE_TITLE_PADDING,
        position.y + Layout::NODE_TITLE_HEIGHT / 2.0,
    );

    frame.fill_text(Text {
        content: node.title.clone(),
        position: title_position,
        font: Font::default(),
        size: Layout::TEXT_SIZE.into(),
        color: Theme::NODE_TITLE_TEXT,
        horizontal_alignment: iced::alignment::Horizontal::Left,
        vertical_alignment: iced::alignment::Vertical::Center,
        line_height: iced::widget::text::LineHeight::default(),
        shaping: iced::widget::text::Shaping::Basic,
    });

    // Draw content
    let content_position = Point::new(
        position.x + Layout::NODE_CONTENT_PADDING,
        position.y + Layout::NODE_TITLE_HEIGHT + Layout::NODE_CONTENT_PADDING,
    );

    match &node.content {
        NodeContent::Text {
            lines,
            cursor_position,
            selection: _,
        } => {
            draw_text_content(frame, content_position, lines, cursor_position.as_ref());
        },
        NodeContent::Empty => {},
    }
}

fn draw_text_content(
    frame: &mut Frame,
    start_position: Point,
    lines: &[String],
    cursor_position: Option<&crate::state::CursorPosition>,
) {
    // Draw each line of text
    for (line_index, line) in lines.iter().enumerate() {
        let line_position = Point::new(
            start_position.x,
            start_position.y + (line_index as f32 * Layout::LINE_HEIGHT),
        );

        frame.fill_text(Text {
            content: line.clone(),
            position: line_position,
            font: Font::MONOSPACE,
            size: Layout::TEXT_SIZE.into(),
            color: Theme::TEXT_PRIMARY,
            horizontal_alignment: iced::alignment::Horizontal::Left,
            vertical_alignment: iced::alignment::Vertical::Top,
            line_height: iced::widget::text::LineHeight::default(),
            shaping: iced::widget::text::Shaping::Basic,
        });
    }

    // Draw cursor if present
    if let Some(cursor) = cursor_position {
        let cursor_x = start_position.x + (cursor.column as f32 * 8.0); // Approximate char width
        let cursor_y = start_position.y + (cursor.line as f32 * Layout::LINE_HEIGHT);

        let cursor_path = Path::new(|builder| {
            builder.move_to(Point::new(cursor_x, cursor_y));
            builder.line_to(Point::new(cursor_x, cursor_y + Layout::LINE_HEIGHT));
        });

        frame.stroke(
            &cursor_path,
            Stroke::default()
                .with_width(Layout::CURSOR_WIDTH)
                .with_color(Theme::CURSOR_COLOR),
        );
    }
}
