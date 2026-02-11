//! Diagnostic types and conversions.
//!
//! Provides two representations of diagnostics:
//! - [`LspDiagnostic`] - Raw diagnostic from LSP server
//! - [`BufferDiagnostic`] - Diagnostic with anchor-based positions for tracking through edits

use lsp_types::{DiagnosticSeverity as LspSeverity, NumberOrString};
use std::ops::Range;
use text::Anchor;

/// Diagnostic from an LSP server.
///
/// This is the raw format received from the language server. It uses
/// LSP position types which have UTF-16 code unit offsets.
#[derive(Clone, Debug, PartialEq)]
pub struct LspDiagnostic {
    /// Range where the diagnostic applies (UTF-16 offsets)
    pub range: lsp_types::Range,
    /// Severity level
    pub severity: Option<LspSeverity>,
    /// Diagnostic code (e.g., "E0308")
    pub code: Option<NumberOrString>,
    /// Source of the diagnostic (e.g., "rust-analyzer")
    pub source: Option<String>,
    /// Diagnostic message
    pub message: String,
    /// Related diagnostics (for additional context)
    pub related_information: Vec<lsp_types::DiagnosticRelatedInformation>,
}

impl From<lsp_types::Diagnostic> for LspDiagnostic {
    fn from(diag: lsp_types::Diagnostic) -> Self {
        Self {
            range: diag.range,
            severity: diag.severity,
            code: diag.code,
            source: diag.source,
            message: diag.message,
            related_information: diag.related_information.unwrap_or_default(),
        }
    }
}

/// Diagnostic with buffer-relative anchor positions.
///
/// Uses anchors instead of points so the diagnostic automatically tracks
/// through buffer edits. The anchors are resolved to points only when needed
/// for rendering or queries.
#[derive(Clone, Debug)]
pub struct BufferDiagnostic {
    /// Range in anchor coordinates (tracks through edits)
    pub range: Range<Anchor>,
    /// Severity level
    pub severity: DiagnosticSeverity,
    /// Diagnostic code (e.g., "E0308")
    pub code: Option<String>,
    /// Source of the diagnostic (e.g., "rust-analyzer")
    pub source: Option<String>,
    /// Diagnostic message
    pub message: String,
    /// Server ID that produced this diagnostic
    pub server_id: usize,
}

/// Diagnostic severity levels.
///
/// Ordered from most severe to least severe for merging purposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

impl From<LspSeverity> for DiagnosticSeverity {
    fn from(severity: LspSeverity) -> Self {
        match severity {
            LspSeverity::ERROR => DiagnosticSeverity::Error,
            LspSeverity::WARNING => DiagnosticSeverity::Warning,
            LspSeverity::INFORMATION => DiagnosticSeverity::Information,
            LspSeverity::HINT => DiagnosticSeverity::Hint,
            _ => DiagnosticSeverity::Hint,
        }
    }
}

impl From<DiagnosticSeverity> for LspSeverity {
    fn from(severity: DiagnosticSeverity) -> Self {
        match severity {
            DiagnosticSeverity::Error => LspSeverity::ERROR,
            DiagnosticSeverity::Warning => LspSeverity::WARNING,
            DiagnosticSeverity::Information => LspSeverity::INFORMATION,
            DiagnosticSeverity::Hint => LspSeverity::HINT,
        }
    }
}

impl BufferDiagnostic {
    /// Get the most severe diagnostic from a set.
    ///
    /// Used when merging diagnostics from multiple sources at the same location.
    pub fn most_severe(diagnostics: &[BufferDiagnostic]) -> Option<&BufferDiagnostic> {
        diagnostics.iter().min_by_key(|d| d.severity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ordering() {
        // More severe = lower value for Ord
        assert!(DiagnosticSeverity::Error < DiagnosticSeverity::Warning);
        assert!(DiagnosticSeverity::Warning < DiagnosticSeverity::Information);
        assert!(DiagnosticSeverity::Information < DiagnosticSeverity::Hint);
    }

    #[test]
    fn severity_conversion_roundtrip() {
        let lsp_error = LspSeverity::ERROR;
        let diag_error = DiagnosticSeverity::from(lsp_error);
        let back = LspSeverity::from(diag_error);
        assert_eq!(lsp_error, back);
    }

    #[test]
    fn lsp_diagnostic_conversion() {
        let lsp_diag = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 10,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 15,
                },
            },
            severity: Some(LspSeverity::ERROR),
            code: Some(NumberOrString::String("E0308".to_string())),
            source: Some("rust-analyzer".to_string()),
            message: "type mismatch".to_string(),
            related_information: None,
            tags: None,
            code_description: None,
            data: None,
        };

        let converted = LspDiagnostic::from(lsp_diag.clone());
        assert_eq!(converted.range, lsp_diag.range);
        assert_eq!(converted.severity, lsp_diag.severity);
        assert_eq!(converted.message, "type mismatch");
    }
}
