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
/// The `goto_word_*` and `selection_editor` fields keep
/// scope-specific defaults rather than derive from [`BasePalette`]:
/// `goto_word_*` have no theme scope today (TODO adds them) and
/// `selection_editor` carries the alpha channel that
/// [`default_selection_color`] supplies.
///
/// `selection` (the broader UI scope used by modal pickers) and
/// `selection_editor` (the editor's text-band) are intentionally
/// distinct: the default theme paints them with different roles
/// (accent vs muted) so highlighting a list row reads as a stronger
/// signal than highlighting text under the cursor.
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
    pub selection_editor: Hsla,
    pub modal_palette: Hsla,
    pub error: Hsla,
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
            goto_word_label: rgb(0xeeee00).into(),
            goto_word_prefix: rgb(0x666600).into(),
            selection: theme_bg_or(cx, stoat::theme::scope::UI_SELECTION, palette.accent),
            selection_editor: theme_bg_or(
                cx,
                stoat::theme::scope::UI_SELECTION_EDITOR,
                default_selection_color(),
            ),
            modal_palette: theme_fg_or(cx, stoat::theme::scope::UI_MODAL_PALETTE, palette.accent),
            error: theme_fg_or(cx, stoat::theme::scope::UI_ERROR, palette.danger),
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
    fn cx_theme_falls_back_to_palette_when_no_theme_installed() {
        let cx = TestAppContext::single();
        let palette = BasePalette::default_dark();
        let theme = cx.update(|cx| cx.theme());
        assert_eq!(theme.background, palette.background);
        assert_eq!(theme.statusbar_focused, palette.surface);
        assert_eq!(theme.diagnostic_error, palette.danger);
        assert_eq!(theme.selection_editor, default_selection_color());
        assert_eq!(theme.selection, palette.accent);
        assert_eq!(theme.modal_palette, palette.accent);
        assert_eq!(theme.error, palette.danger);
    }

    #[test]
    fn cx_theme_picks_up_theme_global_overrides() {
        let cx = TestAppContext::single();
        let theme_src = Theme::load_from_source(
            "theme custom { ui.background.fg = blue; ui.diagnostic.error.fg = green; }",
            "custom",
        );
        let palette = BasePalette::default_dark();
        let resolved = cx.update(|cx| {
            cx.set_global(theme_src);
            cx.theme()
        });
        assert_ne!(resolved.background, palette.background);
        assert_ne!(resolved.diagnostic_error, palette.danger);
    }
}
