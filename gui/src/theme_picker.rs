//! Theme picker modal delegate.
//!
//! Lists every theme declared in the active config and applies the
//! highlighted theme live as the selection moves, so the user sees a
//! previewed theme before committing. Confirm keeps the previewed
//! theme; dismiss restores the theme that was active when the picker
//! opened.

use crate::{
    globals::FsHostGlobal,
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    settings::Settings,
    theme::{set_active_theme, ActiveTheme, Theme},
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, DismissEvent, HighlightStyle, IntoElement, ParentElement,
    SharedString, Styled, StyledText, Task, Window,
};
use std::path::Path;
use stoat::host::FsHost;

pub struct ThemePickerDelegate {
    /// Every theme name declared in the config, in source order.
    themes: Vec<String>,
    /// Index into [`Self::themes`] plus the matched character indices
    /// for the active query, ordered for display.
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    /// The theme active when the picker opened. Restored on dismiss so
    /// arrowing through previews leaves no trace when the user cancels.
    prior_theme: stoat::theme::Theme,
    /// Set by [`Self::confirm`] so the shared dismiss path keeps the chosen
    /// theme instead of reverting to [`Self::prior_theme`]. Confirm runs
    /// through the same `dismissed` teardown as cancel.
    confirmed: bool,
}

impl ThemePickerDelegate {
    /// Build a delegate over `themes`, restoring `prior_theme` on
    /// dismiss. The row matching `prior_theme`'s name starts selected
    /// so the highlight reflects the active theme.
    pub fn new(themes: Vec<String>, prior_theme: stoat::theme::Theme) -> Self {
        let selected = themes
            .iter()
            .position(|name| *name == prior_theme.name)
            .unwrap_or(0);
        let mut delegate = Self {
            themes,
            matches: Vec::new(),
            selected,
            prior_theme,
            confirmed: false,
        };
        delegate.set_matches_for_empty_query();
        delegate
    }

    fn set_matches_for_empty_query(&mut self) {
        self.matches = (0..self.themes.len()).map(|i| (i, Vec::new())).collect();
    }

    fn refilter(&mut self, query: &str) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            self.set_matches_for_empty_query();
        } else {
            let items = self
                .themes
                .iter()
                .enumerate()
                .map(|(i, name)| (i, name.clone()));
            match rank_matches(trimmed, items) {
                Some(ranked) => {
                    self.matches = ranked
                        .into_iter()
                        .map(|m| (m.item, m.matched_indices))
                        .collect();
                },
                None => self.set_matches_for_empty_query(),
            }
        }

        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn selected_theme(&self) -> Option<&str> {
        let (idx, _) = self.matches.get(self.selected)?;
        self.themes.get(*idx).map(String::as_str)
    }

    /// Resolve the highlighted theme from the active config and install
    /// it as the global theme, repainting the UI. No-op when no theme
    /// is selected.
    fn apply_selected(&self, cx: &mut Context<'_, Picker<Self>>) {
        let Some(name) = self.selected_theme() else {
            return;
        };
        let theme = {
            let config = &cx.global::<Settings>().config;
            Theme::from_config(config, name)
        };
        set_active_theme(cx, theme);
    }
}

impl PickerDelegate for ThemePickerDelegate {
    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if ix < self.matches.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        self.refilter(&query);
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        _window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        self.apply_selected(cx);
        self.confirmed = true;
        if let Some(name) = self.selected_theme() {
            if let Some(fs) = cx.try_global::<FsHostGlobal>().map(|g| g.0.clone()) {
                persist_theme(fs.as_ref(), name);
            }
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, cx: &mut Context<'_, Picker<Self>>) {
        if self.confirmed {
            return;
        }
        set_active_theme(cx, Theme(self.prior_theme.clone()));
    }

    fn render_match(&self, ix: usize, cx: &mut Context<'_, Picker<Self>>) -> AnyElement {
        let Some((theme_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(name) = self.themes.get(*theme_idx) else {
            return div().into_any_element();
        };
        let color = cx.theme().modal_picker;
        let runs = match_highlight_runs(
            name,
            matched,
            HighlightStyle {
                color: Some(cx.theme().text_accent),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(name.clone())).with_highlights(runs);
        div()
            .px_2()
            .text_color(color)
            .child(label)
            .into_any_element()
    }

    fn selection_changed(&mut self, cx: &mut Context<'_, Picker<Self>>) {
        self.apply_selected(cx);
    }
}

/// Persist `name` as the active theme in the user's stcfg config so the
/// next launch restores it. Best-effort: a failure to resolve the path
/// is logged and otherwise ignored, since the theme is already applied
/// for the running session.
fn persist_theme(fs: &dyn FsHost, name: &str) {
    let Some(path) = stoat_config::user_config_path() else {
        tracing::warn!("could not resolve user config path; theme not persisted");
        return;
    };
    write_theme_at(fs, &path, name);
}

/// Write `theme = "<name>"` into the stcfg file at `path`, creating it
/// (and its parent directory) when absent and updating the key in place
/// when present. Best-effort and non-clobbering: a present-but-unreadable
/// file, a directory-creation failure, or a write failure is logged and
/// the file is left as-is.
fn write_theme_at(fs: &dyn FsHost, path: &Path, name: &str) {
    let existing = if fs.exists(path) {
        let mut buf = Vec::new();
        if let Err(err) = fs.read(path, &mut buf) {
            tracing::warn!(path = %path.display(), ?err, "could not read user config; theme not persisted");
            return;
        }
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        String::new()
    };

    let updated = stoat_config::set_theme(&existing, name);

    if let Some(parent) = path.parent() {
        if let Err(err) = fs.create_dir_all(parent) {
            tracing::warn!(path = %parent.display(), ?err, "could not create config dir; theme not persisted");
            return;
        }
    }
    if let Err(err) = fs.write(path, updated.as_bytes()) {
        tracing::warn!(path = %path.display(), ?err, "could not write user config; theme not persisted");
    }
}

/// Open the theme picker as a modal. Constructed in
/// [`Workspace::dispatch_action`] when `OpenThemePicker` is dispatched.
pub fn open_theme_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let themes = stoat::theme::list_themes(&cx.global::<Settings>().config);
    let prior_theme = cx.global::<Theme>().0.clone();
    workspace.toggle_modal::<Picker<ThemePickerDelegate>, _>(window, cx, move |window, cx| {
        Picker::new(ThemePickerDelegate::new(themes, prior_theme), window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat_scheduler::{Executor, TestScheduler};

    const TWO_THEMES: &str = "theme alpha { ui.cursor.fg = red; } \
                              theme beta { ui.cursor.fg = blue; }";

    fn matched_names(delegate: &ThemePickerDelegate) -> Vec<String> {
        delegate
            .matches
            .iter()
            .map(|(i, _)| delegate.themes[*i].clone())
            .collect()
    }

    fn delegate(themes: &[&str], current: &str) -> ThemePickerDelegate {
        let prior = Theme::load_from_source(
            &format!("theme {current} {{ ui.cursor.fg = red; }}"),
            current,
        )
        .0;
        ThemePickerDelegate::new(themes.iter().map(|s| s.to_string()).collect(), prior)
    }

    #[test]
    fn lists_every_theme_in_source_order() {
        let d = delegate(&["alpha", "beta", "gamma"], "alpha");
        assert_eq!(d.match_count(), 3);
        assert_eq!(matched_names(&d), vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn new_preselects_the_active_theme_row() {
        let d = delegate(&["alpha", "beta", "gamma"], "beta");
        assert_eq!(d.selected_index(), 1);
    }

    #[test]
    fn refilter_narrows_to_query_matches() {
        let mut d = delegate(&["solarized", "dracula", "gruvbox"], "solarized");
        d.refilter("dra");
        assert_eq!(matched_names(&d), vec!["dracula"]);
    }

    fn install_globals(cx: &mut TestAppContext, active: &str) {
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
            cx.set_global(Settings::load_from_source(TWO_THEMES));
            let theme = Theme::load_from_source(TWO_THEMES, active);
            set_active_theme(cx, theme);
        });
    }

    fn active_theme_name(vcx: &mut VisualTestContext) -> String {
        vcx.update(|_, cx| cx.global::<Theme>().0.name.clone())
    }

    fn build_picker(
        cx: &mut TestAppContext,
    ) -> (Entity<Picker<ThemePickerDelegate>>, &mut VisualTestContext) {
        cx.add_window_view(|window, cx| {
            let themes = stoat::theme::list_themes(&cx.global::<Settings>().config);
            let prior = cx.global::<Theme>().0.clone();
            Picker::new(ThemePickerDelegate::new(themes, prior), window, cx)
        })
    }

    #[test]
    fn selection_change_previews_the_highlighted_theme() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, "alpha");
        let (picker, vcx) = build_picker(&mut cx);
        vcx.run_until_parked();

        picker.update(vcx, |p, cx| p.set_selected_index(1, cx));

        assert_eq!(active_theme_name(vcx), "beta");
    }

    #[test]
    fn dismiss_restores_the_theme_active_at_open() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, "alpha");
        let (picker, vcx) = build_picker(&mut cx);
        vcx.run_until_parked();

        picker.update(vcx, |p, cx| p.set_selected_index(1, cx));
        assert_eq!(active_theme_name(vcx), "beta");

        vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::DismissModal, window, cx);
            });
        });

        assert_eq!(active_theme_name(vcx), "alpha");
    }

    #[test]
    fn confirm_keeps_the_previewed_theme() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx, "alpha");
        let (picker, vcx) = build_picker(&mut cx);
        vcx.run_until_parked();

        picker.update(vcx, |p, cx| p.set_selected_index(1, cx));

        vcx.update(|window, cx| {
            picker.update(cx, |p, cx| {
                p.handle_action(&stoat_action::PickerConfirm, window, cx);
            });
        });

        assert_eq!(active_theme_name(vcx), "beta");
    }

    #[test]
    fn write_theme_at_creates_then_updates_in_place() {
        let fs = stoat::host::FakeFs::new();
        let path = Path::new("/cfg/stoat/config.stcfg");

        write_theme_at(&fs, path, "alpha");
        let created = read_back(&fs, path);
        assert!(
            created.contains("theme = \"alpha\";"),
            "absent file should be created with the theme: {created}"
        );

        write_theme_at(&fs, path, "beta");
        let updated = read_back(&fs, path);
        assert!(
            updated.contains("theme = \"beta\";"),
            "value should update: {updated}"
        );
        assert!(
            !updated.contains("alpha"),
            "old value should be replaced: {updated}"
        );
    }

    fn read_back(fs: &stoat::host::FakeFs, path: &Path) -> String {
        let mut buf = Vec::new();
        fs.read(path, &mut buf).expect("read written config");
        String::from_utf8(buf).expect("utf8 config")
    }
}
