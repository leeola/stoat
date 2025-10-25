//! Language Server Protocol (LSP) integration for Stoat.
//!
//! Provides LSP client implementation with anchor-based diagnostic tracking
//! and comprehensive mock-based testing infrastructure.
//!
//! # Architecture
//!
//! The implementation follows a layered architecture:
//!
//! ```text
//! LspTransport trait (abstraction)
//!   |
//!   +-- StdioTransport (production)
//!   +-- MockLspServer (testing)
//! ```
//!
//! # Testing Strategy
//!
//! Uses mock-first testing for fast, deterministic unit tests. Mock behavior
//! is validated against real language servers separately. See `.claude/LSP.md`
//! for design details.

pub mod conversion;
pub mod diagnostic;
pub mod diagnostic_set;
pub mod protocol;
pub mod transport;

// Make test utilities available for both unit and integration tests
#[cfg(any(test, feature = "test-support"))]
pub mod test;

pub use conversion::*;
pub use diagnostic::*;
pub use diagnostic_set::*;
pub use lsp_types;
pub use protocol::*;
pub use transport::*;
