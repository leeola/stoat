use crate::syntax::{HighlightMap, SyntaxTheme};
use gpui::{px, rgb, Font, FontStyle, FontWeight, Hsla, Pixels, SharedString};

/// Style configuration for the editor
#[derive(Clone)]
pub struct EditorStyle {
    pub text_color: Hsla,
    pub background: Hsla,
    pub line_height: Pixels,
    pub font_size: Pixels,
    /// Cached font instance (stable font ID for GPUI's LineLayoutCache)
    pub font: Font,
    pub padding: Pixels,
    /// Whether to show line numbers in the gutter
    pub show_line_numbers: bool,
    /// Whether to show diff indicators in the gutter
    pub show_diff_indicators: bool,
    /// Background color for the gutter area
    pub gutter_background_color: Hsla,
    /// Spacing between gutter content and editor text
    pub gutter_right_padding: Pixels,
    /// Padding added around diff indicator width (provides spacing on left/right of indicator)
    pub diff_indicator_padding: Pixels,
    /// Color for added line indicators (green)
    pub diff_added_color: Hsla,
    /// Color for modified line indicators (blue)
    pub diff_modified_color: Hsla,
    /// Color for deleted line indicators (red)
    pub diff_deleted_color: Hsla,
    /// Desaturated color for staged added lines
    pub diff_staged_added_color: Hsla,
    /// Desaturated color for staged modified lines
    pub diff_staged_modified_color: Hsla,
    /// Desaturated color for staged deleted lines
    pub diff_staged_deleted_color: Hsla,
    /// Purple color for committed added lines (HeadVsParent mode)
    pub diff_committed_added_color: Hsla,
    /// Purple color for committed modified lines (HeadVsParent mode)
    pub diff_committed_modified_color: Hsla,
    /// Purple color for committed deleted lines (HeadVsParent mode)
    pub diff_committed_deleted_color: Hsla,
    /// Color for error diagnostics (red)
    pub diagnostic_error_color: Hsla,
    /// Color for warning diagnostics (orange)
    pub diagnostic_warning_color: Hsla,
    /// Color for info diagnostics (blue)
    pub diagnostic_info_color: Hsla,
    /// Color for hint diagnostics (gray)
    pub diagnostic_hint_color: Hsla,
    /// Whether to show the minimap
    pub show_minimap: bool,
    /// Color for the minimap viewport thumb
    pub minimap_thumb_color: Hsla,
    /// Border color for the minimap viewport thumb
    pub minimap_thumb_border_color: Hsla,
    /// Maximum width of minimap in columns
    pub minimap_max_columns: f32,
    /// Syntax highlighting theme (cached to avoid recreation every frame)
    pub syntax_theme: SyntaxTheme,
    /// Highlight map for efficient token -> color lookup (cached)
    pub highlight_map: HighlightMap,
}

impl EditorStyle {
    /// Returns the color for a given diagnostic severity.
    pub fn diagnostic_color(&self, severity: stoat_lsp::DiagnosticSeverity) -> Hsla {
        use stoat_lsp::DiagnosticSeverity;
        match severity {
            DiagnosticSeverity::Error => self.diagnostic_error_color,
            DiagnosticSeverity::Warning => self.diagnostic_warning_color,
            DiagnosticSeverity::Information => self.diagnostic_info_color,
            DiagnosticSeverity::Hint => self.diagnostic_hint_color,
        }
    }

    /// Create a new editor style from configuration.
    ///
    /// Takes font settings (family and size) from the provided [`crate::Config`].
    /// Other style properties use hardcoded defaults.
    pub fn new(config: &crate::Config) -> Self {
        let syntax_theme = SyntaxTheme::default();
        let highlight_map = HighlightMap::new(&syntax_theme);

        // Create font once (stable font ID for GPUI's LineLayoutCache)
        let font = Font {
            family: SharedString::from(config.buffer_font_family.clone()),
            features: Default::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: None,
        };

        Self {
            text_color: rgb(0xcccccc).into(),
            background: rgb(0x1e1e1e).into(),
            line_height: px(20.0),
            font_size: px(config.buffer_font_size),
            font,
            padding: px(4.0),
            show_line_numbers: true,
            show_diff_indicators: true,
            gutter_background_color: rgb(0x1e1e1e).into(), // Same as editor bg
            gutter_right_padding: px(0.0),                 // No gap between gutter and text
            diff_indicator_padding: px(4.0),               /* 4px padding around indicator (2px
                                                            * each side) */
            diff_added_color: rgb(0x4ec9b0).into(), // Green (VS Code green)
            diff_modified_color: rgb(0x569cd6).into(), // Blue (VS Code blue)
            diff_deleted_color: rgb(0xf44747).into(), // Red (VS Code red)
            diff_staged_added_color: rgb(0xbbb529).into(), // Yellow-green
            diff_staged_modified_color: rgb(0xd4aa32).into(), // Gold
            diff_staged_deleted_color: rgb(0xd08840).into(), // Amber
            diff_committed_added_color: rgb(0x9b7ed8).into(), // Light purple
            diff_committed_modified_color: rgb(0x8470c4).into(), // Medium purple
            diff_committed_deleted_color: rgb(0xb07cc0).into(), // Mauve
            diagnostic_error_color: rgb(0xf44747).into(), // Red (VS Code red)
            diagnostic_warning_color: rgb(0xdcdcaa).into(), // Yellow (VS Code yellow)
            diagnostic_info_color: rgb(0x569cd6).into(), // Blue (VS Code blue)
            diagnostic_hint_color: rgb(0x808080).into(), // Gray
            show_minimap: true,                     /* Enabled: Now using persistent
                                                     * MinimapView
                                                     * entity */
            minimap_thumb_color: Hsla {
                h: 0.0,
                s: 0.0,
                l: 1.0,
                a: 0.15, // 15% opacity - subtle but visible
            },
            minimap_thumb_border_color: Hsla {
                h: 0.0,
                s: 0.0,
                l: 1.0,
                a: 0.25, // Slightly more opaque for visibility
            },
            minimap_max_columns: 120.0, // Reasonable max width
            syntax_theme,
            highlight_map,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_lsp::DiagnosticSeverity;

    #[test]
    fn diagnostic_color_mapping() {
        let config = crate::Config::default();
        let style = EditorStyle::new(&config);

        let error_color = style.diagnostic_color(DiagnosticSeverity::Error);
        let warning_color = style.diagnostic_color(DiagnosticSeverity::Warning);
        let info_color = style.diagnostic_color(DiagnosticSeverity::Information);
        let hint_color = style.diagnostic_color(DiagnosticSeverity::Hint);

        assert_eq!(error_color, style.diagnostic_error_color);
        assert_eq!(warning_color, style.diagnostic_warning_color);
        assert_eq!(info_color, style.diagnostic_info_color);
        assert_eq!(hint_color, style.diagnostic_hint_color);

        // Verify colors are different
        assert_ne!(error_color, warning_color);
        assert_ne!(error_color, info_color);
        assert_ne!(warning_color, hint_color);
    }
}
