//! Command palette picker delegate.
//!
//! Lists every [`stoat_action::ActionDef`] whose
//! [`ActionDef::palette_visible`] returns true, fuzzy-ranks them
//! against the picker's query, and on confirm constructs the action
//! via [`registry::RegistryEntry::create`] and dispatches it through
//! [`Workspace::dispatch_action`]. Param-taking actions are listed
//! but no-op on confirm in v1; param collection lands in a follow-up.

use crate::{
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::statusbar_text_color,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, HighlightStyle, IntoElement, ParentElement, SharedString, Styled,
    StyledText, Task, WeakEntity, Window,
};
use stoat_action::registry::{self, RegistryEntry};

pub struct CommandPaletteDelegate {
    /// Every palette-visible entry, captured at construction time.
    entries: Vec<&'static RegistryEntry>,
    /// Index into [`Self::entries`] plus the matched character
    /// indices for the active query, ordered for display.
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    workspace: WeakEntity<Workspace>,
}

impl CommandPaletteDelegate {
    pub fn new(workspace: WeakEntity<Workspace>) -> Self {
        let entries: Vec<&'static RegistryEntry> = registry::all()
            .filter(|entry| entry.def.palette_visible())
            .collect();
        let mut delegate = Self {
            entries,
            matches: Vec::new(),
            selected: 0,
            workspace,
        };
        delegate.set_matches_for_empty_query();
        delegate
    }

    fn set_matches_for_empty_query(&mut self) {
        let mut indexed: Vec<usize> = (0..self.entries.len()).collect();
        indexed.sort_by_key(|&i| {
            let def = self.entries[i].def;
            (def.priority().ord(), def.name())
        });
        self.matches = indexed.into_iter().map(|i| (i, Vec::new())).collect();
    }

    fn selected_entry(&self) -> Option<&'static RegistryEntry> {
        let (idx, _) = self.matches.get(self.selected)?;
        self.entries.get(*idx).copied()
    }

    fn refilter(&mut self, query: &str) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            self.set_matches_for_empty_query();
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
            return;
        }

        let items = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| (i, entry.def.name().to_string()));
        let ranked = match rank_matches(trimmed, items) {
            Some(r) => r,
            None => {
                self.set_matches_for_empty_query();
                if self.selected >= self.matches.len() {
                    self.selected = self.matches.len().saturating_sub(1);
                }
                return;
            },
        };

        let mut tie_broken = ranked;
        tie_broken.sort_by(|a, b| {
            b.score.cmp(&a.score).then_with(|| {
                let a_def = self.entries[a.item].def;
                let b_def = self.entries[b.item].def;
                a_def
                    .priority()
                    .ord()
                    .cmp(&b_def.priority().ord())
                    .then_with(|| a_def.name().cmp(b_def.name()))
            })
        });

        self.matches = tie_broken
            .into_iter()
            .map(|m| (m.item, m.matched_indices))
            .collect();
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }
}

impl PickerDelegate for CommandPaletteDelegate {
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
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let action = match (entry.create)(&[]) {
            Ok(a) => a,
            Err(err) => {
                tracing::warn!(
                    target: "stoat_gui::command_palette",
                    action = entry.def.name(),
                    ?err,
                    "command palette cannot dispatch param-taking action yet",
                );
                return;
            },
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |ws, cx| ws.dispatch_action(action, window, cx));
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let Some((entry_idx, matched)) = self.matches.get(ix) else {
            return div().into_any_element();
        };
        let Some(entry) = self.entries.get(*entry_idx) else {
            return div().into_any_element();
        };
        let name = entry.def.name();
        let color = statusbar_text_color(cx);
        let runs = match_highlight_runs(
            name,
            matched,
            HighlightStyle {
                color: Some(gpui::white()),
                ..Default::default()
            },
        );
        let label = StyledText::new(SharedString::from(name)).with_highlights(runs);
        let mut row = div().px_2().text_color(color).child(label);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }
}

/// Open the command palette as a modal picker. Constructed in
/// `Workspace::dispatch_action` when `OpenCommandPalette` is dispatched.
pub fn open_command_palette(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let weak = cx.weak_entity();
    workspace.toggle_modal::<Picker<CommandPaletteDelegate>, _>(window, cx, move |window, cx| {
        let delegate = CommandPaletteDelegate::new(weak);
        Picker::new(delegate, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_action::ActionKind;

    fn new_delegate() -> CommandPaletteDelegate {
        CommandPaletteDelegate::new(WeakEntity::new_invalid())
    }

    fn matched_names(delegate: &CommandPaletteDelegate) -> Vec<&'static str> {
        delegate
            .matches
            .iter()
            .map(|(i, _)| delegate.entries[*i].def.name())
            .collect()
    }

    #[test]
    fn empty_query_lists_every_palette_visible_entry() {
        let delegate = new_delegate();
        let names = matched_names(&delegate);
        assert!(!names.is_empty());
        assert!(names.contains(&"Quit"));
        assert!(names.contains(&"OpenFile"));
        assert!(
            !names.contains(&"OpenCommandPalette"),
            "OpenCommandPalette is palette_visible=false",
        );
    }

    #[test]
    fn empty_query_orders_by_priority_then_alphabetical() {
        let delegate = new_delegate();
        let names = matched_names(&delegate);
        let pairs: Vec<(u8, &'static str)> = names
            .iter()
            .map(|n| {
                let prio = registry::all()
                    .find(|e| e.def.name() == *n)
                    .map(|e| e.def.priority().ord())
                    .expect("listed entry must be in registry");
                (prio, *n)
            })
            .collect();
        let mut sorted = pairs.clone();
        sorted.sort();
        assert_eq!(pairs, sorted, "not sorted by (priority, name)");
    }

    #[test]
    fn refilter_open_query_lists_open_actions() {
        let mut delegate = new_delegate();
        delegate.refilter("Open");

        let names = matched_names(&delegate);
        assert!(
            names.contains(&"OpenFile"),
            "OpenFile expected in {names:?}"
        );
        assert!(
            names.contains(&"OpenReview"),
            "OpenReview expected in {names:?}",
        );
    }

    #[test]
    fn whitespace_query_falls_back_to_full_list() {
        let mut delegate = new_delegate();
        delegate.refilter("   ");

        let names = matched_names(&delegate);
        assert!(names.contains(&"Quit"));
        assert!(names.contains(&"OpenFile"));
    }

    #[test]
    fn refilter_clamps_selected_when_results_shrink() {
        let mut delegate = new_delegate();
        delegate.selected = delegate.matches.len() - 1;
        delegate.refilter("Quit");

        assert!(!delegate.matches.is_empty());
        assert!(delegate.selected < delegate.matches.len());
    }

    #[test]
    fn refilter_quit_all_selects_quit_all() {
        let mut delegate = new_delegate();
        delegate.refilter("QuitAll");

        let entry = delegate.selected_entry().expect("selected entry");
        assert_eq!(entry.def.kind(), ActionKind::QuitAll);
    }

    #[test]
    fn refilter_non_matching_query_yields_empty_match_list() {
        let mut delegate = new_delegate();
        delegate.refilter("zzzzzzzzzzz");

        assert!(
            delegate.matches.is_empty(),
            "query with no matches should produce an empty list, got {:?}",
            matched_names(&delegate),
        );
    }
}
