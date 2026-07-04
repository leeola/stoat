use crate::{
    display_map::highlights::{HighlightStyle, HighlightStyleId, HighlightStyleInterner},
    host::DiffStatus,
    theme::Theme,
};
use ratatui::style::{Color, Modifier, Style};
use std::sync::Arc;
use stoat_language::HighlightId;

/// Canonical list of syntax scope stems this build recognizes. Each entry
/// is the tree-sitter capture-name suffix; the full theme scope is
/// `syntax.<entry>`. Tree-sitter capture names match longest-prefix
/// against this list to produce a [`HighlightId`] indexing into
/// [`SyntaxStyles::theme_table`]. Adding a new capture requires adding
/// the stem here and its style to the active theme.
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

/// Translate a flat [`THEME_KEYS`] stem into the hierarchical scope used
/// by the [`Theme`]. Tree-sitter captures like `string.escape` become
/// `syntax.string.escape`; legacy suffix form `title.markup` becomes
/// `syntax.markup.title`. Markup-family captures use an inverted order
/// in tree-sitter queries to preserve longest-prefix matching on
/// `.markup`; the theme reorders them so the hierarchy rooted at
/// `syntax.markup` can fall back naturally.
fn theme_scope_for_key(key: &str) -> String {
    if let Some(rest) = key.strip_suffix(".markup") {
        // e.g. "emphasis.strong.markup" → "syntax.markup.emphasis.strong"
        return format!("syntax.markup.{rest}");
    }
    match key {
        "boolean" => "syntax.constant.boolean".to_string(),
        "number" => "syntax.constant.numeric".to_string(),
        "lifetime" => "syntax.special.lifetime".to_string(),
        _ => format!("syntax.{key}"),
    }
}

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
    /// Build syntax styles from the active [`Theme`]. Each [`THEME_KEYS`]
    /// stem is translated to a theme scope via [`theme_scope_for_key`]
    /// and the resulting [`Style`] is decomposed into a [`HighlightStyle`]
    /// for the merge-friendly display pipeline.
    pub fn from_theme(theme: &Theme) -> Self {
        let mut interner = HighlightStyleInterner::default();
        let theme_table: Vec<HighlightStyleId> = THEME_KEYS
            .iter()
            .map(|key| {
                let scope = theme_scope_for_key(key);
                let style = theme.get(&scope);
                interner.intern(style_to_highlight_style(&style))
            })
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

pub(crate) fn style_to_highlight_style(s: &Style) -> HighlightStyle {
    let mods = s.add_modifier;
    HighlightStyle {
        foreground: s.fg,
        background: s.bg,
        bold: mods.contains(Modifier::BOLD).then_some(true),
        italic: mods.contains(Modifier::ITALIC).then_some(true),
        underline: mods.contains(Modifier::UNDERLINED).then_some(true),
        strikethrough: mods.contains(Modifier::CROSSED_OUT).then_some(true),
    }
}

/// Central theme map for diff gutter / highlight colors. One entry
/// per [`DiffStatus`] plus a default for unchanged lines. Built from
/// `diff.*` scopes on the active [`Theme`]; any missing scope falls
/// back to the built-in defaults so the UI always renders something
/// reasonable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffTheme {
    pub added: Color,
    pub deleted: Color,
    pub modified: Color,
    /// Color for byte-for-byte relocated content. Deliberately distinct
    /// from added/deleted/modified: moves are not gains or losses, they
    /// are relocations. Muted cyan reads as "neither green nor red"
    /// and matches the convention used by difftastic and IDE move
    /// detectors.
    pub moved: Color,
}

impl Default for DiffTheme {
    fn default() -> Self {
        Self {
            added: Color::Green,
            deleted: Color::Red,
            modified: Color::Yellow,
            moved: Color::Cyan,
        }
    }
}

impl DiffTheme {
    /// Build a [`DiffTheme`] from `diff.*` scopes on the active theme.
    /// Missing `fg` values fall back to the built-in default colors so
    /// themes can omit any entry without the UI breaking.
    pub fn from_theme(theme: &Theme) -> Self {
        let default = Self::default();
        Self {
            added: theme.get("diff.added").fg.unwrap_or(default.added),
            deleted: theme.get("diff.deleted").fg.unwrap_or(default.deleted),
            modified: theme.get("diff.modified").fg.unwrap_or(default.modified),
            moved: theme.get("diff.moved").fg.unwrap_or(default.moved),
        }
    }

    /// Resolve a [`DiffStatus`] to its themed color. Returns `None`
    /// for unchanged lines so callers can leave them unstyled.
    pub fn color_for(&self, status: DiffStatus) -> Option<Color> {
        match status {
            DiffStatus::Unchanged => None,
            DiffStatus::Added => Some(self.added),
            DiffStatus::Modified => Some(self.modified),
            DiffStatus::Moved => Some(self.moved),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{theme_scope_for_key, DiffTheme, SyntaxStyles, THEME_KEYS};
    use crate::theme::Theme;
    use stoat_config::parse;
    use stoat_language::HighlightId;

    fn theme_from(src: &str) -> Theme {
        let (config, errors) = parse(src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        Theme::from_config(&config.unwrap(), "t").unwrap()
    }

    #[test]
    fn id_for_highlight_returns_none_for_default() {
        let styles = SyntaxStyles::from_theme(&Theme::empty());
        assert!(styles.id_for_highlight(HighlightId::DEFAULT).is_none());
    }

    #[test]
    fn empty_theme_builds_empty_styles() {
        let styles = SyntaxStyles::from_theme(&Theme::empty());
        for idx in 0..THEME_KEYS.len() {
            let id = HighlightId(idx as u32);
            let style_id = styles
                .id_for_highlight(id)
                .expect("every theme key must resolve");
            let style = &styles.interner[style_id];
            assert_eq!(style.foreground, None);
            assert_eq!(style.background, None);
        }
    }

    #[test]
    fn distinct_theme_keys_get_distinct_styles() {
        let theme = theme_from(
            r##"theme t {
                syntax.keyword = { fg: blue, modifiers: [bold] };
                syntax.string.fg = green;
            }"##,
        );
        let styles = SyntaxStyles::from_theme(&theme);
        let keyword_idx = THEME_KEYS.iter().position(|k| *k == "keyword").unwrap() as u32;
        let string_idx = THEME_KEYS.iter().position(|k| *k == "string").unwrap() as u32;
        let kw = styles.id_for_highlight(HighlightId(keyword_idx)).unwrap();
        let st = styles.id_for_highlight(HighlightId(string_idx)).unwrap();
        assert_ne!(styles.interner[kw], styles.interner[st]);
    }

    #[test]
    fn markup_keys_route_to_syntax_markup_scope() {
        assert_eq!(theme_scope_for_key("title.markup"), "syntax.markup.title");
        assert_eq!(
            theme_scope_for_key("emphasis.markup"),
            "syntax.markup.emphasis"
        );
        assert_eq!(
            theme_scope_for_key("emphasis.strong.markup"),
            "syntax.markup.emphasis.strong"
        );
        assert_eq!(
            theme_scope_for_key("strikethrough.markup"),
            "syntax.markup.strikethrough"
        );
        assert_eq!(
            theme_scope_for_key("link_uri.markup"),
            "syntax.markup.link_uri"
        );
    }

    #[test]
    fn constant_suffix_keys_reroute() {
        assert_eq!(theme_scope_for_key("boolean"), "syntax.constant.boolean");
        assert_eq!(theme_scope_for_key("number"), "syntax.constant.numeric");
        assert_eq!(theme_scope_for_key("lifetime"), "syntax.special.lifetime");
    }

    #[test]
    fn plain_keys_get_syntax_prefix() {
        assert_eq!(theme_scope_for_key("keyword"), "syntax.keyword");
        assert_eq!(theme_scope_for_key("string.escape"), "syntax.string.escape");
    }

    #[test]
    fn diff_theme_covers_every_status() {
        use crate::host::DiffStatus;
        let theme = DiffTheme::default();
        assert!(theme.color_for(DiffStatus::Unchanged).is_none());
        let colors = [
            theme.color_for(DiffStatus::Added).unwrap(),
            theme.color_for(DiffStatus::Modified).unwrap(),
            theme.color_for(DiffStatus::Moved).unwrap(),
        ];
        assert_eq!(
            colors
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3,
            "Added/Modified/Moved must be visually distinct"
        );
    }

    #[test]
    fn diff_theme_from_theme_reads_scopes() {
        let theme = theme_from(
            r##"theme t {
                diff.added.fg = green;
                diff.deleted.fg = red;
                diff.modified.fg = yellow;
                diff.moved.fg = cyan;
            }"##,
        );
        let dt = DiffTheme::from_theme(&theme);
        assert_eq!(dt.added, ratatui::style::Color::Green);
        assert_eq!(dt.deleted, ratatui::style::Color::Red);
        assert_eq!(dt.modified, ratatui::style::Color::Yellow);
        assert_eq!(dt.moved, ratatui::style::Color::Cyan);
    }

    #[test]
    fn diff_theme_from_empty_theme_uses_defaults() {
        let dt = DiffTheme::from_theme(&Theme::empty());
        assert_eq!(dt, DiffTheme::default());
    }
}
