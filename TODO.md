# Stoat Rebuild Plan - Simplified First Principles

## Overview
Complete rebuild focusing on bare-bones text buffer rendering with optimal performance. Delete existing core and GUI crates, rebuild from scratch with minimal complexity.

## Phase 1: Clean Slate
- [ ] Delete `core/` crate entirely
- [ ] Delete `gui/` crate entirely  
- [ ] Keep workspace structure, update Cargo.toml members

## Phase 2: Minimal Stoat Core (`stoat/src/`)
Simple text buffer management without complexity:

### 2.1 Buffer Structure
```rust
// stoat/src/buffer.rs
pub struct Buffer {
    lines: Vec<String>,  // Line-based for efficient rendering
    dirty: bool,
}
```

### 2.2 Editor State
```rust
// stoat/src/editor.rs
pub struct Editor {
    buffer: Buffer,
    cursor: Position,
    viewport: Viewport,
}

pub struct Position {
    line: usize,
    column: usize,
}

pub struct Viewport {
    top_line: usize,     // First visible line
    visible_lines: usize, // Number of lines that fit
}
```

### 2.3 Public API
```rust
// stoat/src/lib.rs
pub struct Stoat {
    editor: Editor,
}

impl Stoat {
    pub fn new() -> Self;
    pub fn lines(&self) -> &[String];
    pub fn set_content(&mut self, text: String);
    pub fn insert_char(&mut self, ch: char);
    pub fn delete_char(&mut self);
    pub fn cursor(&self) -> Position;
    pub fn move_cursor(&mut self, line: isize, col: isize);
    pub fn visible_lines(&self) -> &[String];
}
```

## Phase 3: Minimal GUI (`gui/src/`)
Use iced's native text rendering efficiently:

### 3.1 Rendering Strategy
- Use `Column` of `Text` widgets for each visible line
- Virtual scrolling - only create widgets for visible lines
- Use `Scrollable` for smooth scrolling
- Leverage iced's built-in text layout and rendering

### 3.2 GUI Structure
```rust
// gui/src/app.rs
pub struct App {
    stoat: Stoat,
    scroll_offset: f32,
}

// gui/src/editor_widget.rs
pub struct EditorWidget {
    // Custom widget that creates Text widgets for visible lines only
}

impl EditorWidget {
    pub fn view(&self, stoat: &Stoat) -> Element<Message> {
        let visible = stoat.visible_lines();
        
        Column::new()
            .extend(visible.iter().map(|line| {
                Text::new(line)
                    .font(Font::MONOSPACE)
                    .size(14)
            }))
    }
}
```

### 3.3 Performance Optimizations
- Virtual scrolling (only render visible lines)
- Use `lazy` widgets for deferred rendering
- Minimize widget tree rebuilds with targeted updates
- Use iced's built-in caching mechanisms
- Keep widget tree shallow and simple

## Phase 4: Efficient Updates
- Track dirty regions (which lines changed)
- Use iced's `Command` for async operations
- Batch updates to avoid excessive redraws
- Use subscription for keyboard input handling

## Implementation Order
1. Create new `stoat/src/buffer.rs` with line-based buffer
2. Create new `stoat/src/editor.rs` with minimal editor state
3. Create new `stoat/src/lib.rs` exposing simple API
4. Create new `gui/src/app.rs` with basic iced app
5. Create new `gui/src/editor_widget.rs` with virtual scrolling
6. Test with large files (100K+ lines) to verify performance

## Key Design Decisions
- **Line-based storage**: Optimizes for line-oriented rendering
- **Virtual scrolling**: Only render what's visible
- **Native widgets**: Use iced's optimized text rendering
- **Simple data flow**: Unidirectional updates from Stoat to GUI

## Non-Goals (Explicitly NOT Doing)
- No node system
- No workspace management
- No multiple buffers
- No complex text operations
- No modal input system
- No configuration
- No file I/O (just set_content API)
- No syntax highlighting
- No undo/redo
- No selections

## Success Criteria
- Can display a 100K line file
- Smooth scrolling at 60 FPS
- Minimal memory footprint
- Sub-millisecond character insertion
- Clean, understandable codebase under 500 LOC total