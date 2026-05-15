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

/// Fallback for the workspace background fill when no theme
/// overrides `ui.background`. Neutral dark so the empty window
/// reads as an editor surface rather than GPUI's default black.
pub const DEFAULT_BACKGROUND_HEX: u32 = 0x1e1e1e;

/// Fallback for the inactive border / divider color when no
/// theme overrides `ui.border.inactive`. Muted enough to read as
/// a separator against the default backgrounds.
pub const DEFAULT_BORDER_INACTIVE_HEX: u32 = 0x404040;

/// Fallback for the focus-ring color when no theme overrides
/// `ui.border.focused`. Subtle accent blue so the focused pane
/// reads as the active surface without dominating the chrome.
pub const DEFAULT_BORDER_FOCUSED_HEX: u32 = 0x6090ff;

/// Fallback for the focused status-bar fill when no theme overrides
/// `ui.statusbar.focused`. Slightly lighter than the workspace-bg
/// fallback so the row reads as a distinct chrome strip even with
/// no theme installed.
pub const DEFAULT_STATUSBAR_FOCUSED_HEX: u32 = 0x2d2d2d;

/// Fallback for the focused status-bar text color when no theme
/// overrides `ui.statusbar.focused.fg`. Light gray so chrome labels
/// read against the status-bar fill without requiring a theme.
pub const DEFAULT_STATUSBAR_TEXT_HEX: u32 = 0xcccccc;

/// Fallback for the inactive tab fill when no theme overrides
/// `ui.tab.inactive`. Matches the workspace-bg fallback so unfocused
/// tabs read as part of the surrounding chrome.
pub const DEFAULT_TAB_INACTIVE_HEX: u32 = 0x1e1e1e;

/// Fallback for the active tab fill when no theme overrides
/// `ui.tab.active`. Slightly lighter than the inactive fallback so
/// the focused tab stands out against its siblings.
pub const DEFAULT_TAB_ACTIVE_HEX: u32 = 0x2d2d2d;

/// Fallback for the tab label color when no theme overrides
/// `ui.tab.label`. Light gray so labels read against both tab
/// fills without requiring a theme.
pub const DEFAULT_TAB_LABEL_HEX: u32 = 0xcccccc;

/// Fallback for the editor cursor cell fill when no theme overrides
/// `ui.cursor`. Light blue so the cursor reads against dark editor
/// backgrounds without a theme installed.
pub const DEFAULT_CURSOR_HEX: u32 = 0xc8d6ff;

/// Fallback for the active-line row highlight when no theme overrides
/// `ui.line_highlight`. Slightly lighter than the workspace background
/// so the cursor's row reads against neighboring rows without a theme
/// installed.
pub const DEFAULT_LINE_HIGHLIGHT_HEX: u32 = 0x2a2a2a;

/// Fallback for muted-text scopes (gutter line numbers, dimmed
/// chrome text) when no theme overrides `ui.text.muted`. Medium gray
/// so muted text reads against dark editor backgrounds without
/// requiring a theme.
pub const DEFAULT_MUTED_TEXT_HEX: u32 = 0x808080;

/// Fallback for the editor selection band when no theme overrides
/// `ui.selection.editor`. Returns a semi-transparent blue so a
/// selection over text remains legible without a theme installed.
/// Used in place of a packed-RGB constant because the alpha channel
/// matters for selection paint.
pub fn default_selection_color() -> Hsla {
    hsla(0.6, 0.5, 0.5, 0.3)
}

/// Resolve the workspace background fill from the active
/// [`Theme`] global, falling back to [`DEFAULT_BACKGROUND_HEX`]
/// when no theme is installed or the scope is unset.
pub fn background_color(cx: &App) -> Hsla {
    theme_fg_or(
        cx,
        stoat::theme::scope::UI_BACKGROUND,
        DEFAULT_BACKGROUND_HEX,
    )
}

/// Resolve the inactive border color from the active [`Theme`]
/// global, falling back to [`DEFAULT_BORDER_INACTIVE_HEX`] when
/// no theme is installed or the scope is unset.
pub fn border_inactive_color(cx: &App) -> Hsla {
    theme_fg_or(
        cx,
        stoat::theme::scope::UI_BORDER_INACTIVE,
        DEFAULT_BORDER_INACTIVE_HEX,
    )
}

/// Resolve the focus-ring color from the active [`Theme`] global,
/// falling back to [`DEFAULT_BORDER_FOCUSED_HEX`] when no theme is
/// installed or the scope is unset.
pub fn border_focused_color(cx: &App) -> Hsla {
    theme_fg_or(
        cx,
        stoat::theme::scope::UI_BORDER_FOCUSED,
        DEFAULT_BORDER_FOCUSED_HEX,
    )
}

/// Resolve the focused status-bar fill from the active [`Theme`]
/// global, falling back to [`DEFAULT_STATUSBAR_FOCUSED_HEX`] when no
/// theme is installed or the scope is unset. Reads the scope's
/// `bg` channel because chrome strips paint their fill from the
/// theme's background, not its foreground.
pub fn statusbar_focused_color(cx: &App) -> Hsla {
    theme_bg_or(
        cx,
        stoat::theme::scope::UI_STATUSBAR_FOCUSED,
        DEFAULT_STATUSBAR_FOCUSED_HEX,
    )
}

/// Resolve the focused status-bar text color from the active [`Theme`]
/// global, falling back to [`DEFAULT_STATUSBAR_TEXT_HEX`] when no
/// theme is installed or the scope is unset. Reads the scope's `fg`
/// channel so chrome labels paint over the bar's fill in the theme's
/// foreground color.
pub fn statusbar_text_color(cx: &App) -> Hsla {
    theme_fg_or(
        cx,
        stoat::theme::scope::UI_STATUSBAR_FOCUSED,
        DEFAULT_STATUSBAR_TEXT_HEX,
    )
}

/// Resolve the inactive tab fill from the active [`Theme`] global,
/// falling back to [`DEFAULT_TAB_INACTIVE_HEX`] when no theme is
/// installed or the scope is unset. Reads the scope's `bg` channel.
pub fn tab_inactive_color(cx: &App) -> Hsla {
    theme_bg_or(
        cx,
        stoat::theme::scope::UI_TAB_INACTIVE,
        DEFAULT_TAB_INACTIVE_HEX,
    )
}

/// Resolve the active tab fill from the active [`Theme`] global,
/// falling back to [`DEFAULT_TAB_ACTIVE_HEX`] when no theme is
/// installed or the scope is unset. Reads the scope's `bg` channel.
pub fn tab_active_color(cx: &App) -> Hsla {
    theme_bg_or(
        cx,
        stoat::theme::scope::UI_TAB_ACTIVE,
        DEFAULT_TAB_ACTIVE_HEX,
    )
}

/// Resolve the tab label color from the active [`Theme`] global,
/// falling back to [`DEFAULT_TAB_LABEL_HEX`] when no theme is
/// installed or the scope is unset. Reads the scope's `fg` channel.
pub fn tab_label_color(cx: &App) -> Hsla {
    theme_fg_or(cx, stoat::theme::scope::UI_TAB_LABEL, DEFAULT_TAB_LABEL_HEX)
}

/// Resolve the editor cursor cell fill from the active [`Theme`]
/// global, falling back to [`DEFAULT_CURSOR_HEX`] when no theme is
/// installed or the scope is unset. Reads the scope's `bg` channel
/// because the cursor paints as a highlighted cell behind the
/// underlying character.
pub fn cursor_color(cx: &App) -> Hsla {
    theme_bg_or(cx, stoat::theme::scope::UI_CURSOR, DEFAULT_CURSOR_HEX)
}

/// Resolve the active-line row highlight from the active [`Theme`]
/// global, falling back to [`DEFAULT_LINE_HIGHLIGHT_HEX`] when no
/// theme is installed or the scope is unset. Reads the scope's `bg`
/// channel because the highlight paints behind the row's characters.
pub fn active_line_color(cx: &App) -> Hsla {
    theme_bg_or(
        cx,
        stoat::theme::scope::UI_LINE_HIGHLIGHT,
        DEFAULT_LINE_HIGHLIGHT_HEX,
    )
}

/// Resolve the muted-text color (gutter line numbers, dimmed chrome
/// text) from the active [`Theme`] global, falling back to
/// [`DEFAULT_MUTED_TEXT_HEX`] when no theme is installed or the
/// scope is unset. Reads the scope's `fg` channel.
pub fn muted_text_color(cx: &App) -> Hsla {
    theme_fg_or(
        cx,
        stoat::theme::scope::UI_TEXT_MUTED,
        DEFAULT_MUTED_TEXT_HEX,
    )
}

/// Resolve the editor selection band fill from the active [`Theme`]
/// global, falling back to [`default_selection_color`] when no
/// theme is installed or the scope is unset. Reads the scope's
/// `bg` channel.
pub fn selection_color(cx: &App) -> Hsla {
    cx.try_global::<Theme>()
        .and_then(|t| t.0.try_get(stoat::theme::scope::UI_SELECTION_EDITOR))
        .and_then(|style| style.bg)
        .and_then(ratatui_color_to_hsla)
        .unwrap_or_else(default_selection_color)
}

fn theme_fg_or(cx: &App, scope: &str, fallback_hex: u32) -> Hsla {
    cx.try_global::<Theme>()
        .and_then(|t| t.0.try_get(scope))
        .and_then(|style| style.fg)
        .and_then(ratatui_color_to_hsla)
        .unwrap_or_else(|| rgb(fallback_hex).into())
}

fn theme_bg_or(cx: &App, scope: &str, fallback_hex: u32) -> Hsla {
    cx.try_global::<Theme>()
        .and_then(|t| t.0.try_get(scope))
        .and_then(|style| style.bg)
        .and_then(ratatui_color_to_hsla)
        .unwrap_or_else(|| rgb(fallback_hex).into())
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
    fn background_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| background_color(cx));
        assert_eq!(resolved, rgb(DEFAULT_BACKGROUND_HEX).into());
    }

    #[test]
    fn background_color_resolves_from_theme() {
        let cx = TestAppContext::single();
        let theme = Theme::load_from_source("theme custom { ui.background.fg = blue; }", "custom");
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            background_color(cx)
        });
        assert_ne!(resolved, rgb(DEFAULT_BACKGROUND_HEX).into());
    }

    #[test]
    fn statusbar_focused_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| statusbar_focused_color(cx));
        assert_eq!(resolved, rgb(DEFAULT_STATUSBAR_FOCUSED_HEX).into());
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
        assert_ne!(resolved, rgb(DEFAULT_STATUSBAR_FOCUSED_HEX).into());
    }

    #[test]
    fn statusbar_text_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| statusbar_text_color(cx));
        assert_eq!(resolved, rgb(DEFAULT_STATUSBAR_TEXT_HEX).into());
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
        assert_ne!(resolved, rgb(DEFAULT_STATUSBAR_TEXT_HEX).into());
    }

    #[test]
    fn tab_active_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| tab_active_color(cx));
        assert_eq!(resolved, rgb(DEFAULT_TAB_ACTIVE_HEX).into());
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
        assert_ne!(resolved, rgb(DEFAULT_TAB_ACTIVE_HEX).into());
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
        assert_eq!(resolved, rgb(DEFAULT_MUTED_TEXT_HEX).into());
    }

    #[test]
    fn muted_text_color_resolves_fg_from_theme() {
        let cx = TestAppContext::single();
        let theme = Theme::load_from_source("theme custom { ui.text.muted.fg = blue; }", "custom");
        let resolved = cx.update(|cx| {
            cx.set_global(theme);
            muted_text_color(cx)
        });
        assert_ne!(resolved, rgb(DEFAULT_MUTED_TEXT_HEX).into());
    }

    #[test]
    fn active_line_color_falls_back_when_theme_missing() {
        let cx = TestAppContext::single();
        let resolved = cx.update(|cx| active_line_color(cx));
        assert_eq!(resolved, rgb(DEFAULT_LINE_HIGHLIGHT_HEX).into());
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
        assert_ne!(resolved, rgb(DEFAULT_LINE_HIGHLIGHT_HEX).into());
    }
}
