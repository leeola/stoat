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
            background: rgb(0x3b414d).into(),
            surface: rgb(0x2f343e).into(),
            border: rgb(0x464b57).into(),
            text: rgb(0xdce0e5).into(),
            text_muted: rgb(0xa9afbc).into(),
            accent: rgb(0x74ade8).into(),
            success: rgb(0xa1c181).into(),
            warning: rgb(0xdec184).into(),
            danger: rgb(0xd07277).into(),
            info: rgb(0x74ade8).into(),
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
    /// Fill for the main editor pane (`EditorMode::Full`). One Dark
    /// layers this as the darkest surface, below [`Self::background`]
    /// (the window frame), so the editor reads as the focal surface
    /// rather than blending into the frame.
    pub editor_background: Hsla,
    /// Elevated-surface fill for floating surfaces (popovers, modals,
    /// docks) that sit above the app background. Not yet read by any
    /// renderer; the surface-elevation styling helpers consume it.
    #[allow(dead_code)]
    pub elevated_surface: Hsla,
    pub border_inactive: Hsla,
    pub border_focused: Hsla,
    /// Deemphasized divider border, dimmer than [`Self::border_focused`]
    /// and [`Self::border_inactive`]. Not yet read by any renderer; the
    /// surface-elevation styling helpers consume it.
    #[allow(dead_code)]
    pub border_variant: Hsla,
    pub statusbar_focused: Hsla,
    pub statusbar_text: Hsla,
    pub tab_inactive: Hsla,
    pub tab_active: Hsla,
    pub tab_label: Hsla,
    pub breadcrumb_text: Hsla,
    pub breadcrumb_separator: Hsla,
    pub cursor: Hsla,
    /// Glyph foreground for the cursor cell. Pairs with [`Self::cursor`]
    /// as the cell background to produce a reverse-video block, so the
    /// character under the cursor stays legible.
    pub cursor_text: Hsla,
    pub line_highlight: Hsla,
    pub search_match: Hsla,
    pub muted_text: Hsla,
    /// Base foreground for editor buffer text. Unhighlighted spans --
    /// notably every glyph of a file with no registered language --
    /// render in this color; syntax and selection runs layer over it.
    pub editor_text: Hsla,
    /// Optional color override for end-of-line inline git blame. `None`
    /// falls back to [`Self::muted_text`].
    pub blame_inline: Option<Hsla>,
    /// Faint vertical indent-guide line color.
    pub indent_guide: Hsla,
    /// Indent-guide color for the cursor's active indent level.
    pub indent_guide_active: Hsla,
    /// Visible-whitespace glyph color: muted dots and arrows drawn for
    /// spaces and tabs.
    pub whitespace: Hsla,
    /// Background for the pinned sticky-scroll header at the viewport top.
    pub sticky_header_background: Hsla,
    pub diagnostic_error: Hsla,
    pub diagnostic_warning: Hsla,
    pub diagnostic_info: Hsla,
    pub diagnostic_hint: Hsla,
    /// File-tree VCS status markers: a saturated triad kept distinct from
    /// the softer `diff.*` text colors so file-tree decorations stand out.
    pub vcs_gutter_added: Hsla,
    pub vcs_gutter_modified: Hsla,
    pub vcs_gutter_deleted: Hsla,
    pub goto_word_label: Hsla,
    pub goto_word_prefix: Hsla,
    pub selection: Hsla,
    pub selection_editor: Hsla,
    pub modal_palette: Hsla,
    pub modal_picker: Hsla,
    pub modal_selection: Hsla,
    pub popup_text: Hsla,
    pub popup_selection_background: Hsla,
    pub popup_selection_text: Hsla,
    pub minimap_thumb: Hsla,
    pub minimap_thumb_border: Hsla,
    pub dock_minimized_background: Hsla,
    pub dock_minimized_border: Hsla,
    pub error: Hsla,
    pub success: Hsla,
    /// Foreground for the run-pane "running" status marker. Pairs with
    /// [`Self::success`]/[`Self::error`] (the finished-block markers) so
    /// the three command states read as distinct colors.
    pub badge_active: Hsla,
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
            editor_background: theme_bg_or(
                cx,
                stoat::theme::scope::UI_EDITOR_BACKGROUND,
                rgb(0x282c33).into(),
            ),
            elevated_surface: theme_bg_or(
                cx,
                stoat::theme::scope::UI_SURFACE_ELEVATED,
                palette.surface,
            ),
            border_inactive: theme_fg_or(
                cx,
                stoat::theme::scope::UI_BORDER_INACTIVE,
                palette.border,
            ),
            border_focused: theme_fg_or(cx, stoat::theme::scope::UI_BORDER_FOCUSED, palette.accent),
            border_variant: theme_fg_or(cx, stoat::theme::scope::UI_BORDER_VARIANT, palette.border),
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
            breadcrumb_text: theme_fg_or(
                cx,
                stoat::theme::scope::UI_TEXT_MUTED,
                palette.text_muted,
            ),
            breadcrumb_separator: theme_fg_or(
                cx,
                stoat::theme::scope::UI_BORDER_INACTIVE,
                palette.border,
            ),
            cursor: theme_bg_or(cx, stoat::theme::scope::UI_CURSOR, palette.accent),
            cursor_text: theme_fg_or(cx, stoat::theme::scope::UI_CURSOR, palette.background),
            line_highlight: theme_bg_or(
                cx,
                stoat::theme::scope::UI_LINE_HIGHLIGHT,
                palette.surface,
            ),
            search_match: theme_bg_or(cx, stoat::theme::scope::UI_SEARCH_MATCH, palette.warning),
            muted_text: theme_fg_or(cx, stoat::theme::scope::UI_TEXT_MUTED, palette.text_muted),
            editor_text: theme_fg_or(cx, stoat::theme::scope::UI_TEXT, palette.text),
            blame_inline: None,
            indent_guide: theme_fg_or(cx, stoat::theme::scope::UI_BORDER_INACTIVE, palette.border),
            indent_guide_active: theme_fg_or(
                cx,
                stoat::theme::scope::UI_TEXT_MUTED,
                palette.text_muted,
            ),
            whitespace: theme_fg_or(cx, stoat::theme::scope::UI_TEXT_MUTED, palette.text_muted)
                .opacity(0.3),
            sticky_header_background: theme_bg_or(
                cx,
                stoat::theme::scope::UI_TAB_INACTIVE,
                palette.surface,
            ),
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
            vcs_gutter_added: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_GUTTER_ADDED,
                rgb(0x27a657).into(),
            ),
            vcs_gutter_modified: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_GUTTER_MODIFIED,
                rgb(0xd3b020).into(),
            ),
            vcs_gutter_deleted: theme_fg_or(
                cx,
                stoat::theme::scope::VCS_GUTTER_DELETED,
                rgb(0xe06c76).into(),
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
            modal_selection: theme_bg_or(
                cx,
                stoat::theme::scope::UI_MODAL_SELECTION,
                palette.surface,
            ),
            popup_text: theme_fg_or(cx, stoat::theme::scope::UI_POPUP_TEXT, palette.text),
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
            success: palette.success,
            badge_active: theme_fg_or(cx, stoat::theme::scope::UI_BADGE_ACTIVE, palette.warning),
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

/// Replace the active [`Theme`] global. Triggers the
/// `observe_global::<Theme>` observers registered by the renderers,
/// so the UI repaints with the new theme's colors.
pub fn set_active_theme(cx: &mut App, theme: Theme) {
    cx.set_global(theme);
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
        assert_eq!(theme.elevated_surface, palette.surface);
        assert_eq!(theme.border_variant, palette.border);
        assert_eq!(theme.editor_text, palette.text);
        assert_eq!(theme.statusbar_focused, palette.surface);
        assert_eq!(theme.diagnostic_error, palette.danger);
        assert_eq!(theme.selection_editor, default_selection_color());
        assert_eq!(theme.selection, palette.accent);
        assert_eq!(theme.modal_palette, palette.accent);
        assert_eq!(theme.modal_picker, palette.accent);
        assert_eq!(theme.error, palette.danger);
        assert_eq!(theme.vcs_rebase_pick, palette.success);
        assert_eq!(theme.vcs_rebase_drop, palette.danger);
        assert_eq!(theme.vcs_commit_sha, palette.warning);
        assert_eq!(theme.vcs_commit_metadata, palette.text_muted);
        assert_eq!(theme.diff_context, palette.text_muted);
        assert_eq!(theme.diff_current_hunk, palette.info);
        assert_eq!(theme.vcs_conflict_ours, palette.success);
        assert_eq!(theme.vcs_conflict_theirs, palette.accent);
        assert_eq!(theme.ui_modal_help, palette.accent);
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
    fn cx_theme_resolves_elevation_scopes_from_installed_theme() {
        let cx = TestAppContext::single();
        let theme_src = Theme::load_from_source(
            r##"theme custom { ui.surface.elevated.bg = "#252529"; ui.border.variant.fg = "#353539"; }"##,
            "custom",
        );
        let palette = BasePalette::default_dark();
        let resolved = cx.update(|cx| {
            cx.set_global(theme_src);
            cx.theme()
        });
        assert_ne!(resolved.elevated_surface, palette.surface);
        assert_ne!(resolved.border_variant, palette.border);
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

    #[test]
    fn set_active_theme_swaps_resolved_colors() {
        let cx = TestAppContext::single();
        let first = Theme::load_from_source("theme a { ui.background.fg = blue; }", "a");
        let second = Theme::load_from_source("theme b { ui.background.fg = green; }", "b");
        let (after_first, after_second) = cx.update(|cx| {
            set_active_theme(cx, first);
            let after_first = cx.theme().background;
            set_active_theme(cx, second);
            (after_first, cx.theme().background)
        });
        assert_ne!(
            after_first, after_second,
            "set_active_theme swaps the global so resolved colors change"
        );
    }
}
