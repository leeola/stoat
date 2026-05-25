use crate::editor::render::ratatui_color_to_hsla;
use gpui::{hsla, rgb, App, Global, Hsla};

/// Default monospace font family used by the editor pane when the
/// active [`Settings`] does not override it. Picked to resolve on
/// gpui's primary platform without requiring the user to install
/// extra fonts.
///
/// [`Settings`]: crate::settings::Settings
pub const DEFAULT_EDITOR_FONT_FAMILY: &str = "Menlo";

/// Default editor pane font size in logical pixels. Mirrors the
/// size most editors ship with so a fresh install reads correctly
/// at 1x scaling.
pub const DEFAULT_EDITOR_FONT_SIZE: f32 = 14.0;

/// Default proportional font family for chrome (status bar, tab
/// bar, modals, dock panels). `.SystemUIFont` is the platform's
/// resolved system UI face on the gpui-supported platforms.
pub const DEFAULT_UI_FONT_FAMILY: &str = ".SystemUIFont";

/// Default chrome font size in logical pixels.
pub const DEFAULT_UI_FONT_SIZE: f32 = 14.0;

/// Fallback for the goto-word label cell when no theme overrides
/// `ui.goto_word.label`. Bright yellow so label characters stand out
/// against editor text without requiring a theme. Kept as a
/// scope-specific constant pending the
/// `ui.goto_word.label`/`prefix` theme scopes (next item).
pub const DEFAULT_GOTO_WORD_LABEL_HEX: u32 = 0xeeee00;

/// Fallback for label characters already matched by the user's typed
/// prefix when no theme overrides `ui.goto_word.prefix`. Dim yellow
/// so the matched characters fade visually while the unmatched
/// remainder stays bright.
pub const DEFAULT_GOTO_WORD_PREFIX_HEX: u32 = 0x666600;

/// Fallback for the editor selection band when no theme overrides
/// `ui.selection.editor`. Returns a semi-transparent blue so a
/// selection over text remains legible without a theme installed.
/// Used in place of a packed-RGB constant because the alpha channel
/// matters for selection paint.
pub fn default_selection_color() -> Hsla {
    hsla(0.6, 0.5, 0.5, 0.3)
}

/// Coherent base palette the GUI's scope fallbacks derive from when
/// the active theme leaves a scope unset. Replaces the previous set
/// of independent per-scope `DEFAULT_*_HEX` constants so a partial or
/// absent theme renders with consistent relationships between
/// surfaces, text, and severity colors.
///
/// The 10 fields are the conventional Zed-style roles; surfaces in
/// [`ThemeColors`] map each scope to one of these members. Curating
/// these values is the next item's job
/// (`Curate the default_dark base palette`); the current
/// [`BasePalette::default_dark`] keeps the values that match the
/// existing constants so visual output is unchanged for the scopes
/// that already had a clean palette mapping.
pub(crate) struct BasePalette {
    pub(crate) background: Hsla,
    pub(crate) surface: Hsla,
    pub(crate) border: Hsla,
    pub(crate) text: Hsla,
    pub(crate) text_muted: Hsla,
    pub(crate) accent: Hsla,
    /// Part of the canonical 10-color palette spec. No
    /// [`ThemeColors`] scope maps to it yet; the chat-styling and
    /// vcs-conflict TODO items will consume it.
    #[allow(dead_code)]
    pub(crate) success: Hsla,
    pub(crate) warning: Hsla,
    pub(crate) danger: Hsla,
    pub(crate) info: Hsla,
}

impl BasePalette {
    pub(crate) fn default_dark() -> Self {
        Self {
            background: rgb(0x1e1e1e).into(),
            surface: rgb(0x2d2d2d).into(),
            border: rgb(0x404040).into(),
            text: rgb(0xcccccc).into(),
            text_muted: rgb(0x808080).into(),
            accent: rgb(0x6090ff).into(),
            success: rgb(0x55ff55).into(),
            warning: rgb(0xffaa00).into(),
            danger: rgb(0xff5555).into(),
            info: rgb(0x6090ff).into(),
        }
    }
}

/// Resolved colors for every GUI scope, built from the active
/// [`Theme`] global with palette-derived fallbacks. Construct via
/// [`ActiveTheme::theme`] (i.e. `cx.theme()`); the constructor
/// re-reads the [`Theme`] global on each call, so a newly installed
/// theme is visible to the next read.
///
/// The `goto_word_*` and `selection` fields keep scope-specific
/// defaults rather than derive from [`BasePalette`]: `goto_word_*`
/// have no theme scope today (TODO adds them) and `selection`
/// carries the alpha channel that [`default_selection_color`]
/// supplies.
pub struct ThemeColors {
    pub background: Hsla,
    pub border_inactive: Hsla,
    pub border_focused: Hsla,
    pub statusbar_focused: Hsla,
    pub statusbar_text: Hsla,
    pub tab_inactive: Hsla,
    pub tab_active: Hsla,
    pub tab_label: Hsla,
    pub cursor: Hsla,
    pub line_highlight: Hsla,
    pub search_match: Hsla,
    pub muted_text: Hsla,
    pub diagnostic_error: Hsla,
    pub diagnostic_warning: Hsla,
    pub diagnostic_info: Hsla,
    pub diagnostic_hint: Hsla,
    pub goto_word_label: Hsla,
    pub goto_word_prefix: Hsla,
    pub selection: Hsla,
}

impl ThemeColors {
    pub fn from_app(cx: &App) -> Self {
        let palette = BasePalette::default_dark();
        Self {
            background: theme_fg_or(cx, stoat::theme::scope::UI_BACKGROUND, palette.background),
            border_inactive: theme_fg_or(
                cx,
                stoat::theme::scope::UI_BORDER_INACTIVE,
                palette.border,
            ),
            border_focused: theme_fg_or(cx, stoat::theme::scope::UI_BORDER_FOCUSED, palette.accent),
            statusbar_focused: theme_bg_or(
                cx,
                stoat::theme::scope::UI_STATUSBAR_FOCUSED,
                palette.surface,
            ),
            statusbar_text: theme_fg_or(
                cx,
                stoat::theme::scope::UI_STATUSBAR_FOCUSED,
                palette.text,
            ),
            tab_inactive: theme_bg_or(cx, stoat::theme::scope::UI_TAB_INACTIVE, palette.background),
            tab_active: theme_bg_or(cx, stoat::theme::scope::UI_TAB_ACTIVE, palette.surface),
            tab_label: theme_fg_or(cx, stoat::theme::scope::UI_TAB_LABEL, palette.text),
            cursor: theme_bg_or(cx, stoat::theme::scope::UI_CURSOR, palette.accent),
            line_highlight: theme_bg_or(
                cx,
                stoat::theme::scope::UI_LINE_HIGHLIGHT,
                palette.surface,
            ),
            search_match: theme_bg_or(cx, stoat::theme::scope::UI_SEARCH_MATCH, palette.warning),
            muted_text: theme_fg_or(cx, stoat::theme::scope::UI_TEXT_MUTED, palette.text_muted),
            diagnostic_error: theme_fg_or(
                cx,
                stoat::theme::scope::UI_DIAGNOSTIC_ERROR,
                palette.danger,
            ),
            diagnostic_warning: theme_fg_or(
                cx,
                stoat::theme::scope::UI_DIAGNOSTIC_WARNING,
                palette.warning,
            ),
            diagnostic_info: theme_fg_or(cx, stoat::theme::scope::UI_DIAGNOSTIC_INFO, palette.info),
            diagnostic_hint: theme_fg_or(
                cx,
                stoat::theme::scope::UI_DIAGNOSTIC_HINT,
                palette.text_muted,
            ),
            goto_word_label: rgb(DEFAULT_GOTO_WORD_LABEL_HEX).into(),
            goto_word_prefix: rgb(DEFAULT_GOTO_WORD_PREFIX_HEX).into(),
            selection: theme_bg_or(
                cx,
                stoat::theme::scope::UI_SELECTION_EDITOR,
                default_selection_color(),
            ),
        }
    }
}

/// Accessor trait that exposes the resolved [`ThemeColors`] for the
/// active [`Theme`] global. Modeled on Zed's `ActiveTheme` shape so
/// call sites read theme colors as `cx.theme().<field>`.
pub trait ActiveTheme {
    fn theme(&self) -> ThemeColors;
}

impl ActiveTheme for App {
    fn theme(&self) -> ThemeColors {
        ThemeColors::from_app(self)
    }
}

/// Resolve the workspace background fill from the active [`Theme`]
/// global, falling back to the palette-derived value when no theme
/// is installed or the scope is unset.
pub fn background_color(cx: &App) -> Hsla {
    cx.theme().background
}

/// Resolve the inactive border color from the active [`Theme`]
/// global, palette-derived fallback otherwise.
pub fn border_inactive_color(cx: &App) -> Hsla {
    cx.theme().border_inactive
}

/// Resolve the focus-ring color from the active [`Theme`] global,
/// palette-derived fallback otherwise.
pub fn border_focused_color(cx: &App) -> Hsla {
    cx.theme().border_focused
}

/// Resolve the focused status-bar fill from the active [`Theme`]
/// global, palette-derived fallback otherwise. Reads the scope's
/// `bg` channel.
pub fn statusbar_focused_color(cx: &App) -> Hsla {
    cx.theme().statusbar_focused
}

/// Resolve the focused status-bar text color from the active [`Theme`]
/// global, palette-derived fallback otherwise. Reads the scope's `fg`
/// channel.
pub fn statusbar_text_color(cx: &App) -> Hsla {
    cx.theme().statusbar_text
}

/// Resolve the inactive tab fill from the active [`Theme`] global,
/// palette-derived fallback otherwise. Reads the scope's `bg` channel.
pub fn tab_inactive_color(cx: &App) -> Hsla {
    cx.theme().tab_inactive
}

/// Resolve the active tab fill from the active [`Theme`] global,
/// palette-derived fallback otherwise. Reads the scope's `bg` channel.
pub fn tab_active_color(cx: &App) -> Hsla {
    cx.theme().tab_active
}

/// Resolve the tab label color from the active [`Theme`] global,
/// palette-derived fallback otherwise. Reads the scope's `fg` channel.
pub fn tab_label_color(cx: &App) -> Hsla {
    cx.theme().tab_label
}

/// Resolve the editor cursor cell fill from the active [`Theme`]
/// global, palette-derived fallback otherwise. Reads the scope's `bg`
/// channel.
pub fn cursor_color(cx: &App) -> Hsla {
    cx.theme().cursor
}

/// Resolve the active-line row highlight from the active [`Theme`]
/// global, palette-derived fallback otherwise. Reads the scope's `bg`
/// channel.
pub fn active_line_color(cx: &App) -> Hsla {
    cx.theme().line_highlight
}

/// Resolve the search-match highlight from the active [`Theme`]
/// global, palette-derived fallback otherwise. Reads the scope's `bg`
/// channel.
pub fn search_match_color(cx: &App) -> Hsla {
    cx.theme().search_match
}

/// Resolve the goto-word label cell color. Returns a fixed yellow
/// per [`DEFAULT_GOTO_WORD_LABEL_HEX`] today -- the corresponding
/// theme scope is added in a follow-up.
pub fn goto_word_label_color(cx: &App) -> Hsla {
    cx.theme().goto_word_label
}

/// Resolve the goto-word matched-prefix color. Same direct-constant
/// path as [`goto_word_label_color`] using
/// [`DEFAULT_GOTO_WORD_PREFIX_HEX`].
pub fn goto_word_prefix_color(cx: &App) -> Hsla {
    cx.theme().goto_word_prefix
}

/// Resolve the muted-text color (gutter line numbers, dimmed chrome
/// text) from the active [`Theme`] global, palette-derived fallback
/// otherwise. Reads the scope's `fg` channel.
pub fn muted_text_color(cx: &App) -> Hsla {
    cx.theme().muted_text
}

/// Resolve the diagnostic-error foreground from the active [`Theme`]
/// global, palette-derived fallback otherwise. Reads the scope's `fg`
/// channel; consumed by the status-bar diagnostic badge.
pub fn diagnostic_error_color(cx: &App) -> Hsla {
    cx.theme().diagnostic_error
}

/// Resolve the diagnostic-warning foreground from the active
/// [`Theme`] global, palette-derived fallback otherwise. Reads the
/// scope's `fg` channel.
pub fn diagnostic_warning_color(cx: &App) -> Hsla {
    cx.theme().diagnostic_warning
}

/// Resolve the diagnostic-information foreground from the active
/// [`Theme`] global, palette-derived fallback otherwise. Reads the
/// scope's `fg` channel.
pub fn diagnostic_info_color(cx: &App) -> Hsla {
    cx.theme().diagnostic_info
}

/// Resolve the diagnostic-hint foreground from the active [`Theme`]
/// global, palette-derived fallback otherwise. Reads the scope's `fg`
/// channel.
pub fn diagnostic_hint_color(cx: &App) -> Hsla {
    cx.theme().diagnostic_hint
}

/// Resolve the editor selection band fill from the active [`Theme`]
/// global, falling back to [`default_selection_color`] when no
/// theme is installed or the scope is unset. Reads the scope's
/// `bg` channel.
pub fn selection_color(cx: &App) -> Hsla {
    cx.theme().selection
}

fn theme_fg_or(cx: &App, scope: &str, fallback: Hsla) -> Hsla {
    cx.try_global::<Theme>()
        .and_then(|t| t.0.try_get(scope))
        .and_then(|style| style.fg)
        .and_then(ratatui_color_to_hsla)
        .unwrap_or(fallback)
}

fn theme_bg_or(cx: &App, scope: &str, fallback: Hsla) -> Hsla {
    cx.try_global::<Theme>()
        .and_then(|t| t.0.try_get(scope))
        .and_then(|style| style.bg)
        .and_then(ratatui_color_to_hsla)
        .unwrap_or(fallback)
}

/// App-global wrapper around [`stoat::theme::Theme`]. Stored via
/// [`gpui::App::set_global`] and observed via
/// [`gpui::App::observe_global::<Theme>`]. The inner value is the
/// resolved theme struct produced by the existing
/// `stoat_config::parse` -> `stoat::theme::Theme::from_config`
/// pipeline.
pub struct Theme(pub stoat::theme::Theme);

impl Global for Theme {}

impl Theme {
    /// Construct an empty [`Theme`] -- every scope lookup returns
    /// `None`. Used as the parse-failure fallback so the GUI never
    /// refuses to render due to a bad config.
    pub fn empty() -> Self {
        Self(stoat::theme::Theme::empty())
    }

    /// Resolve the named theme block from an already-parsed
    /// [`stoat_config::Config`]. Logs a tracing error and falls
    /// back to [`Theme::empty`] when the block is absent or fails
    /// to resolve.
    pub fn from_config(config: &stoat_config::Config, name: &str) -> Self {
        match stoat::theme::Theme::from_config(config, name) {
            Ok(inner) => Self(inner),
            Err(e) => {
                tracing::error!("theme '{name}' load failed: {e}");
                Self::empty()
            },
        }
    }

    /// Parse stcfg source text and resolve the named theme block.
    /// Falls back to [`Theme::empty`] if the source fails to parse.
    pub fn load_from_source(source: &str, name: &str) -> Self {
        let (config, _errors) = stoat_config::parse(source);
        match config {
            Some(c) => Self::from_config(&c, name),
            None => Self::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn default_dark_palette_severity_colors_are_distinct() {
        let palette = BasePalette::default_dark();
        assert_ne!(palette.success, palette.warning);
        assert_ne!(palette.warning, palette.danger);
        assert_ne!(palette.success, palette.danger);
    }

    #[test]
    fn background_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| background_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().background);
    }

    #[test]
    fn background_color_resolves_from_theme() {
        let cx = TestAppContext::single();
        let theme = Theme::load_from_source("theme custom { ui.background.fg = blue; }", "custom");
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            background_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().background);
    }

    #[test]
    fn statusbar_focused_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| statusbar_focused_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().surface);
    }

    #[test]
    fn statusbar_focused_color_resolves_bg_from_theme() {
        let cx = TestAppContext::single();
        let theme = Theme::load_from_source(
            "theme custom { ui.statusbar.focused = { bg: blue }; }",
            "custom",
        );
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            statusbar_focused_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().surface);
    }

    #[test]
    fn statusbar_text_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| statusbar_text_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().text);
    }

    #[test]
    fn statusbar_text_color_resolves_fg_from_theme() {
        let cx = TestAppContext::single();
        let theme = Theme::load_from_source(
            "theme custom { ui.statusbar.focused = { fg: blue }; }",
            "custom",
        );
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            statusbar_text_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().text);
    }

    #[test]
    fn tab_active_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| tab_active_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().surface);
    }

    #[test]
    fn tab_active_color_resolves_bg_from_theme() {
        let cx = TestAppContext::single();
        let theme =
            Theme::load_from_source("theme custom { ui.tab.active = { bg: blue }; }", "custom");
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            tab_active_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().surface);
    }

    #[test]
    fn selection_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| selection_color(cx));
        assert_eq!(resolved, default_selection_color());
    }

    #[test]
    fn selection_color_resolves_bg_from_theme() {
        let cx = TestAppContext::single();
        let theme = Theme::load_from_source(
            "theme custom { ui.selection.editor = { bg: blue }; }",
            "custom",
        );
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            selection_color(cx)
        });
        assert_ne!(resolved, default_selection_color());
    }

    #[test]
    fn muted_text_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| muted_text_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().text_muted);
    }

    #[test]
    fn muted_text_color_resolves_fg_from_theme() {
        let cx = TestAppContext::single();
        let theme = Theme::load_from_source("theme custom { ui.text.muted.fg = blue; }", "custom");
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            muted_text_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().text_muted);
    }

    #[test]
    fn active_line_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| active_line_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().surface);
    }

    #[test]
    fn active_line_color_resolves_bg_from_theme() {
        let cx = TestAppContext::single();
        let theme = Theme::load_from_source(
            "theme custom { ui.line_highlight = { bg: blue }; }",
            "custom",
        );
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            active_line_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().surface);
    }

    #[test]
    fn search_match_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| search_match_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().warning);
    }

    #[test]
    fn search_match_color_resolves_bg_from_theme() {
        let cx = TestAppContext::single();
        let theme =
            Theme::load_from_source("theme custom { ui.search.match = { bg: blue }; }", "custom");
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            search_match_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().warning);
    }

    #[test]
    fn diagnostic_error_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| diagnostic_error_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().danger);
    }

    #[test]
    fn diagnostic_error_color_resolves_fg_from_theme() {
        let cx = TestAppContext::single();
        let theme =
            Theme::load_from_source("theme custom { ui.diagnostic.error.fg = blue; }", "custom");
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            diagnostic_error_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().danger);
    }

    #[test]
    fn diagnostic_warning_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| diagnostic_warning_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().warning);
    }

    #[test]
    fn diagnostic_warning_color_resolves_fg_from_theme() {
        let cx = TestAppContext::single();
        let theme = Theme::load_from_source(
            "theme custom { ui.diagnostic.warning.fg = blue; }",
            "custom",
        );
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            diagnostic_warning_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().warning);
    }

    #[test]
    fn diagnostic_info_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| diagnostic_info_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().info);
    }

    #[test]
    fn diagnostic_info_color_resolves_fg_from_theme() {
        let cx = TestAppContext::single();
        let theme =
            Theme::load_from_source("theme custom { ui.diagnostic.info.fg = blue; }", "custom");
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            diagnostic_info_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().info);
    }

    #[test]
    fn diagnostic_hint_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| diagnostic_hint_color(cx));
        assert_eq!(resolved, BasePalette::default_dark().text_muted);
    }

    #[test]
    fn diagnostic_hint_color_resolves_fg_from_theme() {
        let cx = TestAppContext::single();
        let theme =
            Theme::load_from_source("theme custom { ui.diagnostic.hint.fg = blue; }", "custom");
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            diagnostic_hint_color(cx)
        });
        assert_ne!(resolved, BasePalette::default_dark().text_muted);
    }
}
