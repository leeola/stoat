# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview Node-Based Text Editor Backend

Stoat is an experimental canvas-based, relational and structured text editor in the exploration/prototyping phase. It combines canvas-based visualization with node-based data manipulation, taking a unique approach to text editing.

The backend for the text editor represents both code and data as interconnected nodes in a graph. Think of it as combining the power of a dataflow programming environment (like Node-RED or Max/MSP) with a code editor, where functions, text blocks, and data sources all become nodes that can be wired together.

**Core Concept**: Unlike traditional text editors that treat code as linear files, this editor treats everything as nodes in a 2D canvas/workspace. A function becomes a node with input/output ports. A CSV file becomes a data source node. SQL queries, text snippets, and code blocks all become nodes that can be connected to show relationships and data flow.

**Key Vision Points**:
- **Unified Interface**: Both data (CSV, SQL results) and code (functions, classes) are nodes with contracts for how they exchange information
- **Semantic Relationships**: Users can create links between nodes that the editor doesn't understand (like marking that a function sends data to a specific channel), adding semantic meaning beyond what static analysis can infer
- **Visual Debugging**: Instead of stepping through linear code, developers can trace data flow through the node graph
- **Living Documentation**: The node connections serve as executable documentation of how data flows through a system

**Future Implications** (this helps with naming/architecture decisions):
- Will eventually have a GUI showing nodes on a 2D canvas with visual links between them
- Nodes will support real-time execution where changing one node's output updates downstream nodes
- Will support a DSL for querying the graph (e.g., "find all functions that eventually send data to this channel")
- Should support extensible custom node types
- Will need to handle large codebases with thousands of nodes efficiently

The initial CLI phase is about proving the core concepts work - nodes, contracts, links, and workspaces - before adding the visual layer. Every CLI command is designing the API that the future GUI will use.

## Architecture

### Workspace Structure
- **`core/`** (`stoat_core`) - Core editor runtime without UI
- **`stoat/`** (`stoat`) - Main library with feature flags  
- **`bin/`** (`stoat_bin`) - Binary executable

### Key Design Patterns
- **Canvas-Based Model**: Workspace → View → NodeView (positioned nodes on canvas)
- **Input Pipeline**: UserInput (keyboard only, mouse disabled) → Mode → Command → Node operations
- **Unified Value System**: All data represented as JSON-like `Value` enum (Bool, I64, U64, Float, String, Array, Map, Empty, Null)
- **Node System**: Trait-based node architecture with async `Value` input/output

### Core Modules
- `workspace.rs` - Editor workspace management
- `view.rs` - Visual representation with node-based views  
- `node.rs` - Canvas node system (trait-based)
- `input/user.rs` - User input abstraction (keyboard-focused)
- `mode.rs` - Mode-based input handling (vim-like)
- `value.rs` - Unified data format

## Development Commands

```bash
# Build and run
cargo build                # Build all workspace members
cargo run --bin stoat      # Run main binary
cargo test                 # Run tests across workspace
cargo fmt                  # Format with custom rustfmt rules

# Single test
cargo test <test_name>         # Run specific test
cargo test --package stoat_core <test_name>  # Run test in specific package
```

## Key Implementation Notes

- **Rust 1.87.0** with custom rustfmt configuration
- **Feature gates**: `cli_bin`, `cli_config`, `gui` for modular compilation
- **Async runtime**: tokio for node operations
- **Serialization**: Multiple formats (serde, ron, rkyv) for different needs
- **Data optimization**: Uses `compact_str`, `ordered-float`, `indexmap` for performance
- **Error handling**: `snafu` for structured error types
- **Nix flake** available for development environment setup

## Current Status

The project is in active development with core data structures and node traits implemented. GUI implementation and state persistence are key areas still being developed. The architecture supports both traditional editor concepts (modes, workspaces) and modern visual programming paradigms (canvas, spatial node organization).
