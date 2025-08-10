use crate::grid_layout::GridLayout;

/// Complete visual state for rendering the canvas
#[derive(Debug, Clone)]
pub struct RenderState {
    /// Viewport for the canvas (position and zoom)
    pub viewport: Viewport,
    /// All nodes to render
    pub nodes: Vec<NodeRenderData>,
    /// Currently focused node ID (if any)
    pub focused_node: Option<NodeId>,
    /// Grid layout for coordinate conversion
    pub grid_layout: GridLayout,
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
    /// Text editor content (legacy)
    Text {
        lines: Vec<String>,
        cursor_position: Option<CursorPosition>,
        selection: Option<TextSelection>,
    },
    /// Rope-based text editor content (zero-allocation)
    RopeText {
        /// Reference to the text buffer for zero-allocation line access
        buffer_id: u64,
        /// Visible line range for viewport culling
        viewport: std::ops::Range<usize>,
        /// Actual text lines within the viewport
        lines: Vec<String>,
        /// Current cursor positions
        cursors: Vec<CursorPosition>,
        /// Text selection ranges
        selections: Vec<TextSelection>,
    },
    /// Interactive text editor content with actual text editing capabilities
    InteractiveText {
        /// Text content as string (since Content doesn't implement Clone)
        text: String,
        /// Cursor position in text
        cursor_position: usize,
        /// Whether this text editor has focus
        focused: bool,
        /// Placeholder text for empty content
        placeholder: String,
        /// Buffer ID for connection to TextEditNode
        buffer_id: u64,
    },
    /// Agentic chat widget content
    AgenticChat,
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
