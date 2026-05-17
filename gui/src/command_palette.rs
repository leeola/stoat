//! Command palette picker delegate.
//!
//! Lists every [`stoat_action::ActionDef`] whose
//! [`ActionDef::palette_visible`] returns true, fuzzy-ranks them
//! against the picker's query, and on confirm either constructs the
//! action via [`registry::RegistryEntry::create`] and dispatches it
//! through [`Workspace::dispatch_action`], or transitions into a
//! [`PalettePhase::CollectArgs`] state that walks the user through
//! providing each parameter in sequence before dispatch.

use crate::{
    editor::Editor,
    picker::{match_highlight_runs, rank_matches, Picker, PickerDelegate, PickerSecondary},
    theme::statusbar_text_color,
    workspace::Workspace,
};
use gpui::{
    div, AnyElement, Context, Entity, HighlightStyle, IntoElement, ParentElement, SharedString,
    Styled, StyledText, Task, WeakEntity, Window,
};
use stoat_action::{
    registry::{self, RegistryEntry},
    ParamValue,
};

pub struct CommandPaletteDelegate {
    /// Every palette-visible entry, captured at construction time.
    entries: Vec<&'static RegistryEntry>,
    /// Index into [`Self::entries`] plus the matched character
    /// indices for the active query, ordered for display. Always
    /// empty while [`Self::phase`] is [`PalettePhase::CollectArgs`].
    matches: Vec<(usize, Vec<u32>)>,
    selected: usize,
    workspace: WeakEntity<Workspace>,
    phase: PalettePhase,
    /// Weak handle to the picker's query editor, captured in
    /// [`Self::on_attach`]. Used to clear the editor text on phase
    /// transitions inside [`Self::confirm`], which cannot reach the
    /// picker through its own context because the picker is being
    /// mutated while the delegate runs.
    query_editor: Option<WeakEntity<Editor>>,
}

/// Two-step state machine driving the palette's interaction model.
///
/// [`Filter`] is the standard fuzzy-filter view. Confirming a
/// zero-parameter action dispatches it immediately. Confirming a
/// param-taking action transitions into [`CollectArgs`].
///
/// [`CollectArgs`] walks the user through each parameter in
/// sequence. Each [`Self::confirm`] parses the query text against
/// the current parameter's [`stoat_action::ParamKind`], either
/// advancing to the next parameter (clearing the query editor) or
/// dispatching the action once every parameter has been collected.
/// A parse failure leaves the phase intact and surfaces the error
/// next to the prompt.
enum PalettePhase {
    Filter,
    CollectArgs {
        entry: &'static RegistryEntry,
        collected: Vec<ParamValue>,
        current: usize,
        error: Option<String>,
    },
}

/// Snapshot of the phase data needed to drive [`CommandPaletteDelegate::confirm`]
/// once the borrow on [`CommandPaletteDelegate::phase`] is released. Lifts
/// the per-arm decision out of the match so the body can mutate the phase
/// freely without conflicting borrows.
enum ConfirmStep {
    Filter,
    CollectArgs {
        entry: &'static RegistryEntry,
        current: usize,
    },
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
            phase: PalettePhase::Filter,
            query_editor: None,
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

    /// Replace the picker query editor's text. No-op when the editor
    /// has been dropped (the picker entity is gone) or
    /// [`on_attach`] hasn't run yet.
    fn clear_query_editor(&self, cx: &mut Context<'_, Picker<Self>>) {
        let Some(editor) = self.query_editor.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let buffer = editor.read(cx).multi_buffer().clone();
        let Some(singleton) = buffer.read(cx).as_singleton().cloned() else {
            return;
        };
        let len = singleton.read(cx).text().len();
        if len == 0 {
            return;
        }
        singleton.update(cx, |b, cx| b.edit(0..len, "", cx));
    }

    fn query_editor_text(&self, cx: &Context<'_, Picker<Self>>) -> String {
        let Some(editor) = self.query_editor.as_ref().and_then(WeakEntity::upgrade) else {
            return String::new();
        };
        let buffer = editor.read(cx).multi_buffer().clone();
        let Some(singleton) = buffer.read(cx).as_singleton().cloned() else {
            return String::new();
        };
        singleton.read(cx).text()
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
        match &self.phase {
            PalettePhase::Filter => self.matches.len(),
            PalettePhase::CollectArgs { .. } => 1,
        }
    }

    fn selected_index(&self) -> usize {
        match &self.phase {
            PalettePhase::Filter => self.selected,
            PalettePhase::CollectArgs { .. } => 0,
        }
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<'_, Picker<Self>>) {
        if matches!(self.phase, PalettePhase::CollectArgs { .. }) {
            return;
        }
        if ix < self.matches.len() {
            self.selected = ix;
        }
    }

    fn update_matches(&mut self, query: String, _cx: &mut Context<'_, Picker<Self>>) -> Task<()> {
        if matches!(self.phase, PalettePhase::CollectArgs { .. }) {
            return Task::ready(());
        }
        self.refilter(&query);
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: Option<PickerSecondary>,
        window: &mut Window,
        cx: &mut Context<'_, Picker<Self>>,
    ) {
        let step = match &self.phase {
            PalettePhase::Filter => ConfirmStep::Filter,
            PalettePhase::CollectArgs { entry, current, .. } => ConfirmStep::CollectArgs {
                entry,
                current: *current,
            },
        };
        match step {
            ConfirmStep::Filter => {
                let Some(entry) = self.selected_entry() else {
                    return;
                };
                if entry.def.params().is_empty() {
                    dispatch_action(entry, &[], &self.workspace, window, cx);
                    return;
                }
                self.phase = PalettePhase::CollectArgs {
                    entry,
                    collected: Vec::new(),
                    current: 0,
                    error: None,
                };
                self.clear_query_editor(cx);
                cx.notify();
            },
            ConfirmStep::CollectArgs { entry, current } => {
                let params = entry.def.params();
                let kind = params[current].kind;
                let text = self.query_editor_text(cx);
                match ParamValue::parse(kind, &text) {
                    Ok(value) => {
                        let PalettePhase::CollectArgs {
                            collected, error, ..
                        } = &mut self.phase
                        else {
                            return;
                        };
                        collected.push(value);
                        *error = None;
                        if collected.len() == params.len() {
                            let collected = std::mem::take(collected);
                            self.phase = PalettePhase::Filter;
                            self.clear_query_editor(cx);
                            dispatch_action(entry, &collected, &self.workspace, window, cx);
                        } else {
                            let next_current = collected.len();
                            let collected = std::mem::take(collected);
                            self.phase = PalettePhase::CollectArgs {
                                entry,
                                collected,
                                current: next_current,
                                error: None,
                            };
                            self.clear_query_editor(cx);
                            cx.notify();
                        }
                    },
                    Err(e) => {
                        let PalettePhase::CollectArgs { error, .. } = &mut self.phase else {
                            return;
                        };
                        *error = Some(e.to_string());
                        cx.notify();
                    },
                }
            },
        }
    }

    fn dismissed(&mut self, _cx: &mut Context<'_, Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut Context<'_, Picker<Self>>,
    ) -> AnyElement {
        match &self.phase {
            PalettePhase::Filter => self.render_filter_match(ix, selected, cx),
            PalettePhase::CollectArgs {
                entry,
                current,
                error,
                ..
            } => render_collect_args_prompt(entry, *current, error.as_deref(), cx),
        }
    }

    fn on_attach(&mut self, query_editor: &Entity<Editor>) {
        self.query_editor = Some(query_editor.downgrade());
    }
}

impl CommandPaletteDelegate {
    fn render_filter_match(
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

fn dispatch_action(
    entry: &'static RegistryEntry,
    args: &[ParamValue],
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<'_, Picker<CommandPaletteDelegate>>,
) {
    let action = match (entry.create)(args) {
        Ok(a) => a,
        Err(err) => {
            tracing::warn!(
                target: "stoat_gui::command_palette",
                action = entry.def.name(),
                ?err,
                "command palette could not build action from collected params",
            );
            return;
        },
    };
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    workspace.update(cx, |ws, cx| ws.dispatch_action(action, window, cx));
}

fn render_collect_args_prompt(
    entry: &'static RegistryEntry,
    current: usize,
    error: Option<&str>,
    cx: &mut Context<'_, Picker<CommandPaletteDelegate>>,
) -> AnyElement {
    let params = entry.def.params();
    let total = params.len();
    let Some(param) = params.get(current) else {
        return div().into_any_element();
    };
    let color = statusbar_text_color(cx);
    let header = format!(
        "[{}/{}] {} ({})",
        current + 1,
        total,
        param.name,
        param.kind,
    );
    let description = param.description;
    let mut block = div()
        .flex()
        .flex_col()
        .px_2()
        .text_color(color)
        .child(div().child(SharedString::from(header)))
        .child(div().child(SharedString::from(description)));
    if let Some(message) = error {
        block = block.child(
            div()
                .text_color(gpui::red())
                .child(SharedString::from(message.to_string())),
        );
    }
    block.into_any_element()
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

    mod param_collection {
        use super::*;
        use crate::globals::ExecutorGlobal;
        use gpui::{AppContext, TestAppContext, VisualTestContext};
        use std::sync::Arc;
        use stoat_action::{ActionKind, ParamKind};
        use stoat_scheduler::{Executor, TestScheduler};

        fn install_executor_global(cx: &mut TestAppContext) {
            let executor = Executor::new(Arc::new(TestScheduler::new()));
            cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
        }

        struct Harness<'a> {
            picker: Entity<Picker<CommandPaletteDelegate>>,
            vcx: &'a mut VisualTestContext,
        }

        fn new_harness(cx: &mut TestAppContext) -> Harness<'_> {
            install_executor_global(cx);
            let delegate = CommandPaletteDelegate::new(WeakEntity::new_invalid());
            let vcx = cx.add_empty_window();
            let picker = vcx.update(|window, cx| cx.new(|cx| Picker::new(delegate, window, cx)));
            Harness { picker, vcx }
        }

        fn select_entry_by_name(harness: &mut Harness<'_>, name: &str) -> &'static RegistryEntry {
            harness.picker.update(harness.vcx, |p, _cx| {
                let delegate = p.delegate_mut();
                let idx = delegate
                    .entries
                    .iter()
                    .position(|e| e.def.name() == name)
                    .unwrap_or_else(|| panic!("entry {name} missing from registry"));
                delegate.matches = vec![(idx, Vec::new())];
                delegate.selected = 0;
                delegate.entries[idx]
            })
        }

        fn type_query(harness: &mut Harness<'_>, text: &str) {
            let buffer = harness.picker.read_with(harness.vcx, |p, cx| {
                p.query_editor()
                    .read(cx)
                    .multi_buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("single-line editor has singleton buffer")
                    .clone()
            });
            buffer.update(harness.vcx, |b, cx| {
                let len = b.text().len();
                b.edit(0..len, text, cx);
            });
            harness.vcx.run_until_parked();
        }

        fn confirm(harness: &mut Harness<'_>) {
            let picker = harness.picker.clone();
            harness.vcx.update(|window, cx| {
                picker.update(cx, |p, cx| {
                    p.handle_action(&stoat_action::PickerConfirm, window, cx)
                });
            });
            harness.vcx.run_until_parked();
        }

        #[test]
        fn on_attach_captures_query_editor_weak_handle() {
            let mut cx = TestAppContext::single();
            let h = new_harness(&mut cx);
            let attached = h
                .picker
                .read_with(h.vcx, |p, _cx| p.delegate().query_editor.is_some());
            assert!(attached, "delegate did not capture query editor handle");
        }

        #[test]
        fn confirm_filter_zero_arg_stays_in_filter() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            let entry = select_entry_by_name(&mut h, "Quit");
            assert!(
                entry.def.params().is_empty(),
                "Quit is expected to be zero-arg",
            );
            confirm(&mut h);
            let in_filter = h.picker.read_with(h.vcx, |p, _cx| {
                matches!(p.delegate().phase, PalettePhase::Filter)
            });
            assert!(
                in_filter,
                "Filter phase must persist after zero-arg confirm"
            );
        }

        #[test]
        fn confirm_filter_param_action_enters_collect_args() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "OpenFile");
            confirm(&mut h);
            let snapshot = h
                .picker
                .read_with(h.vcx, |p, _cx| match &p.delegate().phase {
                    PalettePhase::CollectArgs {
                        entry,
                        current,
                        collected,
                        error,
                    } => Some((entry.def.kind(), *current, collected.len(), error.clone())),
                    PalettePhase::Filter => None,
                });
            assert_eq!(
                snapshot,
                Some((ActionKind::OpenFile, 0, 0, None)),
                "param-taking confirm must transition into CollectArgs",
            );
        }

        #[test]
        fn match_count_in_collect_args_returns_one() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "OpenFile");
            confirm(&mut h);
            let count = h
                .picker
                .read_with(h.vcx, |p, _cx| p.delegate().match_count());
            assert_eq!(count, 1);
        }

        #[test]
        fn selected_index_in_collect_args_pinned_to_zero() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "OpenFile");
            confirm(&mut h);
            h.picker.update(h.vcx, |p, cx| {
                p.delegate_mut().set_selected_index(5, cx);
            });
            let ix = h
                .picker
                .read_with(h.vcx, |p, _cx| p.delegate().selected_index());
            assert_eq!(ix, 0);
        }

        #[test]
        fn update_matches_in_collect_args_does_not_refilter() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "OpenFile");
            confirm(&mut h);
            type_query(&mut h, "anything goes");
            let in_collect = h.picker.read_with(h.vcx, |p, _cx| {
                matches!(p.delegate().phase, PalettePhase::CollectArgs { .. })
                    && p.delegate().match_count() == 1
            });
            assert!(
                in_collect,
                "typing must not pull the delegate out of CollectArgs"
            );
        }

        #[test]
        fn entering_collect_args_clears_query_editor() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            type_query(&mut h, "OpenF");
            select_entry_by_name(&mut h, "OpenFile");
            confirm(&mut h);
            let text = h.picker.read_with(h.vcx, |p, cx| {
                p.query_editor()
                    .read(cx)
                    .multi_buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("singleton")
                    .read(cx)
                    .text()
            });
            assert_eq!(text, "");
        }

        #[test]
        fn collect_args_with_valid_input_resets_to_filter_phase() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "OpenFile");
            confirm(&mut h);
            type_query(&mut h, "/tmp/example.rs");
            confirm(&mut h);
            let back_to_filter = h.picker.read_with(h.vcx, |p, _cx| {
                matches!(p.delegate().phase, PalettePhase::Filter)
            });
            assert!(
                back_to_filter,
                "single-param OpenFile must collect its arg and return to Filter",
            );
        }

        #[test]
        fn collect_args_clears_query_between_steps_when_advancing() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "OpenFile");
            confirm(&mut h);
            type_query(&mut h, "/tmp/a.rs");
            confirm(&mut h);
            let text = h.picker.read_with(h.vcx, |p, cx| {
                p.query_editor()
                    .read(cx)
                    .multi_buffer()
                    .read(cx)
                    .as_singleton()
                    .expect("singleton")
                    .read(cx)
                    .text()
            });
            assert_eq!(text, "");
        }

        #[test]
        fn render_collect_args_renders_param_prompt_at_ix_zero() {
            let mut cx = TestAppContext::single();
            let mut h = new_harness(&mut cx);
            select_entry_by_name(&mut h, "OpenFile");
            confirm(&mut h);
            let entry = h
                .picker
                .read_with(h.vcx, |p, _cx| match &p.delegate().phase {
                    PalettePhase::CollectArgs { entry, .. } => *entry,
                    PalettePhase::Filter => panic!("expected CollectArgs"),
                });
            assert_eq!(entry.def.params()[0].kind, ParamKind::String);
            assert_eq!(entry.def.params()[0].name, "path");
        }
    }
}
