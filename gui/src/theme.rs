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
/// [`ThemeColors`] map each scope to one of these members. The
/// values in [`BasePalette::default_dark`] mirror the `let`-bindings
/// in `default_dark`'s `config.stcfg` block, so a theme-parse failure
/// still degrades to the intended palette rather than a divergent
/// fallback.
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
            accent: rgb(0x4fc1ff).into(),
            success: rgb(0x4ec9b0).into(),
            warning: rgb(0xdcdcaa).into(),
            danger: rgb(0xf44747).into(),
            info: rgb(0x569cd6).into(),
        }
    }
}

/// Resolved colors for every GUI scope, built from the active
/// [`Theme`] global with palette-derived fallbacks. Construct via
/// [`ActiveTheme::theme`] (i.e. `cx.theme()`); the constructor
/// re-reads the [`Theme`] global on each call, so a newly installed
/// theme is visible to the next read.
///
/// The `selection_editor` field keeps a scope-specific default
/// rather than deriving from [`BasePalette`]:
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
    pub modal_picker: Hsla,
    pub popup_background: Hsla,
    pub popup_text: Hsla,
    pub popup_border: Hsla,
    pub popup_selection_background: Hsla,
    pub popup_selection_text: Hsla,
    pub minimap_thumb: Hsla,
    pub minimap_thumb_border: Hsla,
    pub dock_minimized_background: Hsla,
    pub dock_minimized_border: Hsla,
    pub error: Hsla,
    pub chat_user: Hsla,
    pub chat_text: Hsla,
    pub chat_meta: Hsla,
    pub chat_time: Hsla,
    pub chat_thinking: Hsla,
    pub chat_tool_header: Hsla,
    pub chat_tool_body: Hsla,
    pub chat_tool_focused: Hsla,
    pub chat_tool_status_running: Hsla,
    pub chat_tool_status_done: Hsla,
    pub chat_tool_status_failed: Hsla,
    pub chat_tool_status_cancelled: Hsla,
    pub chat_error: Hsla,
    /// No rendering site yet (chat view doesn't draw visible
    /// separator lines today). Kept on the struct so the chat
    /// palette stays comprehensive.
    #[allow(dead_code)]
    pub chat_separator: Hsla,
    /// Same as `chat_separator` -- the throbber animation isn't
    /// drawn yet but the scope is part of the chat palette spec.
    #[allow(dead_code)]
    pub chat_throbber: Hsla,
    pub vcs_rebase_pick: Hsla,
    pub vcs_rebase_squash: Hsla,
    pub vcs_rebase_fixup: Hsla,
    pub vcs_rebase_reword: Hsla,
    pub vcs_rebase_edit: Hsla,
    pub vcs_rebase_drop: Hsla,
    pub vcs_commit_sha: Hsla,
    pub vcs_commit_summary: Hsla,
    pub vcs_commit_metadata: Hsla,
    pub diff_context: Hsla,
    pub diff_current_hunk: Hsla,
    pub vcs_conflict_header: Hsla,
    pub vcs_conflict_ours: Hsla,
    pub vcs_conflict_theirs: Hsla,
    pub ui_modal_help: Hsla,
    pub ui_modal_run: Hsla,
    pub dev_stats_text: Hsla,
    pub dev_stats_background: Hsla,
    pub dev_stats_border: Hsla,
    pub dev_stats_bar_good: Hsla,
    pub dev_stats_bar_warn: Hsla,
    pub dev_stats_bar_bad: Hsla,
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
            goto_word_label: theme_fg_or(
                cx,
                stoat::theme::scope::UI_GOTO_WORD_LABEL,
                rgb(0xeeee00).into(),
            ),
            goto_word_prefix: theme_fg_or(
                cx,
                stoat::theme::scope::UI_GOTO_WORD_PREFIX,
                rgb(0x666600).into(),
            ),
            selection: theme_bg_or(cx, stoat::theme::scope::UI_SELECTION, palette.accent),
            selection_editor: theme_bg_or(
                cx,
                stoat::theme::scope::UI_SELECTION_EDITOR,
                default_selection_color(),
            ),
            modal_palette: theme_fg_or(cx, stoat::theme::scope::UI_MODAL_PALETTE, palette.accent),
            modal_picker: theme_fg_or(cx, stoat::theme::scope::UI_MODAL_PICKER, palette.accent),
            popup_background: theme_bg_or(
                cx,
                stoat::theme::scope::UI_POPUP_BACKGROUND,
                palette.surface,
            ),
            popup_text: theme_fg_or(cx, stoat::theme::scope::UI_POPUP_TEXT, palette.text),
            popup_border: theme_fg_or(cx, stoat::theme::scope::UI_POPUP_BORDER, palette.border),
            popup_selection_background: theme_bg_or(
                cx,
                stoat::theme::scope::UI_POPUP_SELECTION_BACKGROUND,
                palette.accent,
            ),
            popup_selection_text: theme_fg_or(
                cx,
                stoat::theme::scope::UI_POPUP_SELECTION_TEXT,
                palette.text,
            ),
            minimap_thumb: theme_bg_or(
                cx,
                stoat::theme::scope::UI_MINIMAP_THUMB,
                hsla(0.0, 0.0, 1.0, 0.15),
            ),
            minimap_thumb_border: theme_fg_or(
                cx,
                stoat::theme::scope::UI_MINIMAP_THUMB_BORDER,
                hsla(0.0, 0.0, 1.0, 0.25),
            ),
            dock_minimized_background: theme_bg_or(
                cx,
                stoat::theme::scope::UI_DOCK_MINIMIZED_BACKGROUND,
                palette.background,
            ),
            dock_minimized_border: theme_fg_or(
                cx,
                stoat::theme::scope::UI_DOCK_MINIMIZED_BORDER,
                palette.border,
            ),
            error: theme_fg_or(cx, stoat::theme::scope::UI_ERROR, palette.danger),
            chat_user: theme_fg_or(cx, stoat::theme::scope::CHAT_USER, palette.success),
            chat_text: theme_fg_or(cx, stoat::theme::scope::CHAT_TEXT, palette.text),
            chat_meta: theme_fg_or(cx, stoat::theme::scope::CHAT_META, palette.text_muted),
            chat_time: theme_fg_or(cx, stoat::theme::scope::CHAT_TIME, palette.text_muted),
            chat_thinking: theme_fg_or(cx, stoat::theme::scope::CHAT_THINKING, palette.text_muted),
            chat_tool_header: theme_fg_or(
                cx,
                stoat::theme::scope::CHAT_TOOL_HEADER,
                palette.accent,
            ),
            chat_tool_body: theme_fg_or(
                cx,
                stoat::theme::scope::CHAT_TOOL_BODY,
                palette.text_muted,
            ),
            chat_tool_focused: theme_fg_or(
                cx,
                stoat::theme::scope::CHAT_TOOL_FOCUSED,
                palette.accent,
            ),
            chat_tool_status_running: theme_fg_or(
                cx,
                stoat::theme::scope::CHAT_TOOL_STATUS_RUNNING,
                palette.accent,
            ),
            chat_tool_status_done: theme_fg_or(
                cx,
                stoat::theme::scope::CHAT_TOOL_STATUS_DONE,
                palette.success,
            ),
            chat_tool_status_failed: theme_fg_or(
                cx,
                stoat::theme::scope::CHAT_TOOL_STATUS_FAILED,
                palette.danger,
            ),
            chat_tool_status_cancelled: theme_fg_or(
                cx,
                stoat::theme::scope::CHAT_TOOL_STATUS_CANCELLED,
                palette.text_muted,
            ),
            chat_error: theme_fg_or(cx, stoat::theme::scope::CHAT_ERROR, palette.danger),
            chat_separator: theme_fg_or(
                cx,
                stoat::theme::scope::CHAT_SEPARATOR,
                palette.text_muted,
            ),
            chat_throbber: theme_fg_or(cx, stoat::theme::scope::CHAT_THROBBER, palette.accent),
            vcs_rebase_pick: theme_fg_or(cx, stoat::theme::scope::VCS_REBASE_PICK, palette.success),
            vcs_rebase_squash: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_REBASE_SQUASH,
                palette.warning,
            ),
            vcs_rebase_fixup: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_REBASE_FIXUP,
                palette.warning,
            ),
            vcs_rebase_reword: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_REBASE_REWORD,
                palette.accent,
            ),
            vcs_rebase_edit: theme_fg_or(cx, stoat::theme::scope::VCS_REBASE_EDIT, palette.accent),
            vcs_rebase_drop: theme_fg_or(cx, stoat::theme::scope::VCS_REBASE_DROP, palette.danger),
            vcs_commit_sha: theme_fg_or(cx, stoat::theme::scope::VCS_COMMIT_SHA, palette.warning),
            vcs_commit_summary: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_COMMIT_SUMMARY,
                palette.text,
            ),
            vcs_commit_metadata: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_COMMIT_METADATA,
                palette.text_muted,
            ),
            diff_context: theme_fg_or(cx, stoat::theme::scope::DIFF_CONTEXT, palette.text_muted),
            diff_current_hunk: theme_fg_or(
                cx,
                stoat::theme::scope::DIFF_CURRENT_HUNK,
                palette.info,
            ),
            vcs_conflict_header: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_CONFLICT_HEADER,
                palette.danger,
            ),
            vcs_conflict_ours: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_CONFLICT_OURS,
                palette.success,
            ),
            vcs_conflict_theirs: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_CONFLICT_THEIRS,
                palette.accent,
            ),
            ui_modal_help: theme_fg_or(cx, stoat::theme::scope::UI_MODAL_HELP, palette.accent),
            ui_modal_run: theme_fg_or(cx, stoat::theme::scope::UI_MODAL_RUN, palette.warning),
            dev_stats_text: theme_fg_or(
                cx,
                stoat::theme::scope::UI_DEV_STATS_TEXT,
                hsla(0.0, 0.0, 0.9, 1.0),
            ),
            dev_stats_background: theme_bg_or(
                cx,
                stoat::theme::scope::UI_DEV_STATS_BACKGROUND,
                hsla(0.0, 0.0, 0.1, 0.8),
            ),
            dev_stats_border: theme_fg_or(
                cx,
                stoat::theme::scope::UI_DEV_STATS_BORDER,
                hsla(0.0, 0.0, 0.3, 0.8),
            ),
            dev_stats_bar_good: theme_bg_or(
                cx,
                stoat::theme::scope::UI_DEV_STATS_BAR_GOOD,
                hsla(0.333, 0.8, 0.5, 0.9),
            ),
            dev_stats_bar_warn: theme_bg_or(
                cx,
                stoat::theme::scope::UI_DEV_STATS_BAR_WARN,
                hsla(0.166, 0.8, 0.5, 0.9),
            ),
            dev_stats_bar_bad: theme_bg_or(
                cx,
                stoat::theme::scope::UI_DEV_STATS_BAR_BAD,
                hsla(0.0, 0.8, 0.5, 0.9),
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

fn theme_fg_or(cx: &App, scope: &str, fallback: Hsla) -> Hsla {
    let Some(theme) = cx.try_global::<Theme>() else {
        return fallback;
    };
    let Some(color) = theme.0.try_get(scope).and_then(|style| style.fg) else {
        return fallback;
    };
    let Some(mut hsla) = ratatui_color_to_hsla(color) else {
        return fallback;
    };
    if let Some(alpha) = theme.0.fg_alpha(scope) {
        hsla.a = f32::from(alpha) / 255.0;
    }
    hsla
}

fn theme_bg_or(cx: &App, scope: &str, fallback: Hsla) -> Hsla {
    let Some(theme) = cx.try_global::<Theme>() else {
        return fallback;
    };
    let Some(color) = theme.0.try_get(scope).and_then(|style| style.bg) else {
        return fallback;
    };
    let Some(mut hsla) = ratatui_color_to_hsla(color) else {
        return fallback;
    };
    if let Some(alpha) = theme.0.bg_alpha(scope) {
        hsla.a = f32::from(alpha) / 255.0;
    }
    hsla
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
        assert_eq!(theme.modal_picker, palette.accent);
        assert_eq!(theme.error, palette.danger);
        assert_eq!(theme.chat_user, palette.success);
        assert_eq!(theme.chat_tool_status_failed, palette.danger);
        assert_eq!(theme.chat_separator, palette.text_muted);
        assert_eq!(theme.vcs_rebase_pick, palette.success);
        assert_eq!(theme.vcs_rebase_drop, palette.danger);
        assert_eq!(theme.vcs_commit_sha, palette.warning);
        assert_eq!(theme.vcs_commit_metadata, palette.text_muted);
        assert_eq!(theme.diff_context, palette.text_muted);
        assert_eq!(theme.diff_current_hunk, palette.info);
        assert_eq!(theme.vcs_conflict_ours, palette.success);
        assert_eq!(theme.vcs_conflict_theirs, palette.accent);
        assert_eq!(theme.ui_modal_help, palette.accent);
        assert_eq!(theme.ui_modal_run, palette.warning);
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

    #[test]
    fn cx_theme_applies_alpha_from_eight_digit_hex() {
        let cx = TestAppContext::single();
        let theme_src = Theme::load_from_source(
            r##"theme custom { ui.selection.editor.bg = "#80808040"; }"##,
            "custom",
        );
        let resolved = cx.update(|cx| {
            cx.set_global(theme_src);
            cx.theme()
        });
        let expected = f32::from(0x40u8) / 255.0;
        assert!(
            (resolved.selection_editor.a - expected).abs() < 1e-6,
            "expected alpha {expected}, got {}",
            resolved.selection_editor.a
        );
    }
}
