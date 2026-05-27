use crate::{
    picker::{Picker, PickerDelegate, PickerSecondary},
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, IntoElement, ParentElement, Styled, Task, WeakEntity, Window,
};
use stoat::review_session::MoveRelationship;

/// Picker delegate that lists every cross-file move in the active
/// review session. The full list is captured at construction time
/// (relationships do not change while the picker is open);
/// [`Self::update_matches`] filters by substring against either
/// the source or target rel-path.
///
/// On confirm the delegate upgrades the workspace ref and calls
/// [`Workspace::navigate_to_move_relationship`], then emits
/// [`gpui::DismissEvent`] via the picker container so the modal
/// layer pops the picker.
pub struct MoveRelationshipPickerDelegate {
    all: Vec<MoveRelationship>,
    matches: Vec<usize>,
    selected: usize,
    workspace: WeakEntity<Workspace>,
}

impl MoveRelationshipPickerDelegate {
    pub fn new(relationships: Vec<MoveRelationship>, workspace: WeakEntity<Workspace>) -> Self {
        let matches: Vec<usize> = (0..relationships.len()).collect();
        Self {
            all: relationships,
            matches,
            selected: 0,
            workspace,
        }
    }

    pub fn selected_relationship(&self) -> Option<&MoveRelationship> {
        let idx = *self.matches.get(self.selected)?;
        self.all.get(idx)
    }
}

impl PickerDelegate for MoveRelationshipPickerDelegate {
    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        self.selected = ix;
    }

    fn update_matches(&mut self, query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        let needle = query.to_lowercase();
        self.matches = if needle.is_empty() {
            (0..self.all.len()).collect()
        } else {
            self.all
                .iter()
                .enumerate()
                .filter(|(_, rel)| {
                    rel.source.rel_path.to_lowercase().contains(&needle)
                        || rel.target.rel_path.to_lowercase().contains(&needle)
                })
                .map(|(i, _)| i)
                .collect()
        };
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let Some(rel) = self.selected_relationship().cloned() else {
            return;
        };
        let workspace = self.workspace.clone();
        // Defer past the keystroke observer's outer `Workspace::update`
        // lease so the re-entrant update does not panic.
        window.defer(cx, move |_window, cx| {
            let _ = workspace.update(cx, |ws, cx| ws.navigate_to_move_relationship(&rel, cx));
        });
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        let label = self
            .matches
            .get(ix)
            .and_then(|&i| self.all.get(i))
            .map(format_relationship)
            .unwrap_or_default();
        let color = cx.theme().statusbar_text;
        let mut row = div().px_2().text_color(color).child(label);
        if selected {
            row = row.bg(gpui::white().opacity(0.1));
        }
        row.into_any_element()
    }
}

fn format_relationship(rel: &MoveRelationship) -> String {
    format!(
        "{}:{} -> {}:{}",
        rel.source.rel_path,
        rel.source.line + 1,
        rel.target.rel_path,
        rel.target.line + 1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat::review::MoveProvenance;

    fn rel(source: (&str, u32), target: (&str, u32)) -> MoveRelationship {
        MoveRelationship {
            source: MoveProvenance {
                rel_path: source.0.to_string(),
                line: source.1,
            },
            target: MoveProvenance {
                rel_path: target.0.to_string(),
                line: target.1,
            },
        }
    }

    #[test]
    fn format_relationship_renders_one_based_lines() {
        let r = rel(("src/a.rs", 9), ("src/b.rs", 24));
        assert_eq!(format_relationship(&r), "src/a.rs:10 -> src/b.rs:25");
    }
}
