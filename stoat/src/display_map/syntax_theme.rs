use crate::display_map::highlights::{HighlightStyle, HighlightStyleId, HighlightStyleInterner};
use ratatui::style::Color;
use std::sync::Arc;
use stoat_language::HighlightId;

/// Canonical theme key list. Each entry is a dot-separated capture
/// name pattern. Tree-sitter capture names matched against this list
/// via longest-prefix matching produce a [`HighlightId`] that indexes
/// directly back into [`SyntaxStyles::theme_table`].
///
/// Adding a new entry: append it to [`THEME_KEYS`] and return its
/// [`HighlightStyle`] from [`style_for_theme_key`]. New, finer-grained
/// captures (e.g. distinct entries for `string.regex` vs
/// `string.escape`) only require touching this file.
const THEME_KEYS: &[&str] = &[
    "keyword",
    "keyword.control",
    "string",
    "string.escape",
    "comment",
    "comment.doc",
    "function",
    "function.method",
    "function.special",
    "type",
    "type.builtin",
    "type.interface",
    "constant",
    "constant.builtin",
    "boolean",
    "number",
    "operator",
    "punctuation.bracket",
    "punctuation.delimiter",
    "property",
    "attribute",
    "variable",
    "variable.parameter",
    "variable.special",
    "lifetime",
    "title.markup",
    "link_text.markup",
    "link_uri.markup",
    "emphasis.markup",
    "emphasis.strong.markup",
    "text.literal.markup",
    "strikethrough.markup",
];

#[derive(Clone)]
pub struct SyntaxStyles {
    pub interner: Arc<HighlightStyleInterner>,
    /// Indexed by [`HighlightId`] (which is itself an index into
    /// [`THEME_KEYS`]). The host populates each language's
    /// [`stoat_language::Language::highlight_map`] using
    /// [`SyntaxStyles::theme_keys`] so per-buffer extraction can
    /// resolve captures directly to entries in this table.
    theme_table: Vec<HighlightStyleId>,
}

impl SyntaxStyles {
    pub fn standard() -> Self {
        let mut interner = HighlightStyleInterner::default();
        let theme_table: Vec<HighlightStyleId> = THEME_KEYS
            .iter()
            .map(|key| interner.intern(style_for_theme_key(key)))
            .collect();
        Self {
            interner: Arc::new(interner),
            theme_table,
        }
    }

    /// Theme keys this style table was built against. Pass to
    /// [`stoat_language::HighlightMap::new`] to build a per-language
    /// capture-index lookup table.
    pub fn theme_keys(&self) -> &'static [&'static str] {
        THEME_KEYS
    }

    /// Resolve a theme-driven [`HighlightId`] to the corresponding
    /// interned [`HighlightStyleId`]. Returns `None` for
    /// [`HighlightId::DEFAULT`] (capture had no theme entry); the
    /// renderer leaves such spans unstyled.
    pub fn id_for_highlight(&self, id: HighlightId) -> Option<HighlightStyleId> {
        if id.is_default() {
            None
        } else {
            self.theme_table.get(id.0 as usize).copied()
        }
    }
}

fn style_for_theme_key(key: &str) -> HighlightStyle {
    let mut s = HighlightStyle::default();
    match key {
        "keyword" | "keyword.control" => {
            s.foreground = Some(Color::Blue);
            s.bold = Some(true);
        },
        "string" => {
            s.foreground = Some(Color::Green);
        },
        "string.escape" => {
            s.foreground = Some(Color::LightGreen);
            s.bold = Some(true);
        },
        "comment" => {
            s.foreground = Some(Color::DarkGray);
            s.italic = Some(true);
        },
        "comment.doc" => {
            s.foreground = Some(Color::Gray);
            s.italic = Some(true);
        },
        "function" | "function.method" => {
            s.foreground = Some(Color::Yellow);
        },
        "function.special" => {
            s.foreground = Some(Color::LightYellow);
            s.bold = Some(true);
        },
        "type" | "type.builtin" | "type.interface" => {
            s.foreground = Some(Color::Cyan);
        },
        "constant" | "constant.builtin" | "boolean" | "number" => {
            s.foreground = Some(Color::Magenta);
        },
        "operator" => {
            s.foreground = Some(Color::LightCyan);
        },
        "punctuation.bracket" | "punctuation.delimiter" => {
            s.foreground = Some(Color::Gray);
        },
        "property" => {
            s.foreground = Some(Color::LightBlue);
        },
        "attribute" => {
            s.foreground = Some(Color::LightMagenta);
        },
        "variable" | "variable.parameter" => {
            s.foreground = Some(Color::White);
        },
        "variable.special" => {
            s.foreground = Some(Color::LightRed);
            s.italic = Some(true);
        },
        "lifetime" => {
            s.foreground = Some(Color::LightYellow);
            s.italic = Some(true);
        },
        "title.markup" => {
            s.foreground = Some(Color::LightCyan);
            s.bold = Some(true);
        },
        "link_text.markup" => {
            s.foreground = Some(Color::LightBlue);
        },
        "link_uri.markup" => {
            s.foreground = Some(Color::Blue);
            s.underline = Some(true);
        },
        "emphasis.markup" => {
            s.italic = Some(true);
        },
        "emphasis.strong.markup" => {
            s.bold = Some(true);
        },
        "text.literal.markup" => {
            s.foreground = Some(Color::LightYellow);
        },
        "strikethrough.markup" => {
            s.strikethrough = Some(true);
        },
        _ => {},
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{SyntaxStyles, THEME_KEYS};
    use stoat_language::HighlightId;

    #[test]
    fn id_for_highlight_resolves_every_theme_key() {
        let styles = SyntaxStyles::standard();
        for idx in 0..THEME_KEYS.len() {
            let id = HighlightId(idx as u32);
            let style_id = styles
                .id_for_highlight(id)
                .expect("every theme key must resolve");
            // The interned style must be valid.
            let _style = &styles.interner[style_id];
        }
    }

    #[test]
    fn id_for_highlight_returns_none_for_default() {
        let styles = SyntaxStyles::standard();
        assert!(styles.id_for_highlight(HighlightId::DEFAULT).is_none());
    }

    #[test]
    fn distinct_theme_keys_get_distinct_styles() {
        let styles = SyntaxStyles::standard();
        let keyword_idx = THEME_KEYS.iter().position(|k| *k == "keyword").unwrap() as u32;
        let string_idx = THEME_KEYS.iter().position(|k| *k == "string").unwrap() as u32;
        let kw = styles.id_for_highlight(HighlightId(keyword_idx)).unwrap();
        let st = styles.id_for_highlight(HighlightId(string_idx)).unwrap();
        assert_ne!(
            styles.interner[kw], styles.interner[st],
            "Keyword and String should produce visually distinct styles"
        );
    }
}
