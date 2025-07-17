/// Complete visual state for rendering the canvas
#[derive(Debug, Clone)]
pub struct RenderState {
    /// Viewport for the canvas (position and zoom)
    pub viewport: Viewport,
    /// All nodes to render
    pub nodes: Vec<NodeRenderData>,
    /// Currently focused node ID (if any)
    pub focused_node: Option<NodeId>,
}

/// Canvas viewport state
#[derive(Debug, Clone)]
pub struct Viewport {
    /// Offset in canvas coordinates
    pub offset: (f32, f32),
    /// Zoom level (1.0 = 100%)
    pub zoom: f32,
}

/// Temporary node ID type for prototyping
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

/// All data needed to render a single node
#[derive(Debug, Clone)]
pub struct NodeRenderData {
    pub id: NodeId,
    /// Position on canvas
    pub position: (f32, f32),
    /// Size of the node
    pub size: (f32, f32),
    /// Node title
    pub title: String,
    /// Content to display (for text editor nodes)
    pub content: NodeContent,
    /// Visual state
    pub state: NodeState,
}

/// Node visual state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NodeState {
    Normal,
    Focused,
    Selected,
}

/// Node content types
#[derive(Debug, Clone)]
pub enum NodeContent {
    /// Text editor content
    Text {
        lines: Vec<String>,
        cursor_position: Option<CursorPosition>,
        selection: Option<TextSelection>,
    },
    /// Empty node
    Empty,
}

/// Cursor position in text
#[derive(Debug, Clone, Copy)]
pub struct CursorPosition {
    pub line: usize,
    pub column: usize,
}

/// Text selection range
#[derive(Debug, Clone)]
pub struct TextSelection {
    pub start: CursorPosition,
    pub end: CursorPosition,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            offset: (0.0, 0.0),
            zoom: 1.0,
        }
    }
}

impl RenderState {
    /// Create a stub render state for prototyping
    pub fn stub() -> Self {
        Self {
            viewport: Viewport::default(),
            nodes: vec![
                NodeRenderData {
                    id: NodeId(1),
                    position: (100.0, 100.0),
                    size: (400.0, 300.0),
                    title: "main.rs".to_string(),
                    content: NodeContent::Text {
                        lines: vec![
                            "fn main() {".to_string(),
                            "    println!(\"Hello, Stoat!\");".to_string(),
                            "}".to_string(),
                        ],
                        cursor_position: Some(CursorPosition { line: 1, column: 4 }),
                        selection: None,
                    },
                    state: NodeState::Focused,
                },
                NodeRenderData {
                    id: NodeId(2),
                    position: (550.0, 200.0),
                    size: (300.0, 200.0),
                    title: "notes.txt".to_string(),
                    content: NodeContent::Text {
                        lines: vec![
                            "TODO:".to_string(),
                            "- Implement node connections".to_string(),
                            "- Add keyboard navigation".to_string(),
                        ],
                        cursor_position: None,
                        selection: None,
                    },
                    state: NodeState::Normal,
                },
            ],
            focused_node: Some(NodeId(1)),
        }
    }
}
