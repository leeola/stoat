use crate::{
    input_view::{InputView, SubmitTarget},
    workspace::Workspace,
};
use stoat_action::{registry, ParamValue};
use stoat_scheduler::Executor;

pub struct CommandPalette {
    pub(crate) phase: PalettePhase,
    /// Mode to restore when the palette closes. Saved at `new()` time so
    /// the palette can transition [`crate::app::Stoat::mode`] back to whatever
    /// the user was in before `:` was pressed.
    pub(crate) previous_mode: String,
}

pub(crate) enum PalettePhase {
    /// Filtering the action list. The user is typing to narrow candidates and
    /// using Up/Down (or Ctrl-P/N) to navigate.
    Filter {
        input: InputView,
        filtered: Vec<&'static registry::RegistryEntry>,
        selected: usize,
    },
    /// A param-taking action has been chosen and the palette is walking the
    /// user through providing each parameter in sequence. Each parameter
    /// step owns its own [`InputView`]; disposed and replaced when the step
    /// advances so each param has independent edit/undo history.
    CollectArgs {
        entry: &'static registry::RegistryEntry,
        collected: Vec<ParamValue>,
        current: usize,
        input: InputView,
        error: Option<String>,
    },
}

pub(crate) enum PaletteOutcome {
    /// Re-render but keep the palette open.
    None,
    /// User cancelled. Currently unused because `CancelPromptInput` closes
    /// the palette directly via `close_palette`; retained as a shape that
    /// future submit paths may want when a per-phase cancel becomes distinct
    /// from a global cancel (e.g. "back up one arg step" vs "close palette").
    #[allow(dead_code)]
    Close,
    /// User selected an action with all required parameters collected.
    Dispatch(&'static registry::RegistryEntry, Vec<ParamValue>),
}

impl CommandPalette {
    pub fn new(ws: &mut Workspace, executor: Executor, previous_mode: String) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::PaletteFilter, "", "prompt", 1);
        let mut phase = PalettePhase::Filter {
            input,
            filtered: Vec::new(),
            selected: 0,
        };
        if let PalettePhase::Filter {
            filtered, selected, ..
        } = &mut phase
        {
            refilter("", filtered, selected);
        }
        Self {
            phase,
            previous_mode,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn phase(&self) -> &PalettePhase {
        &self.phase
    }

    /// Returns the palette's focused [`InputView`], which is always present
    /// since every palette phase is backed by an [`InputView`]. Used by the
    /// focus-resolution path in `Stoat::focused_editor_ids` so keymap-routed
    /// typing hits the correct scratch buffer.
    pub(crate) fn focused_input(&self) -> Option<&InputView> {
        match &self.phase {
            PalettePhase::Filter { input, .. } => Some(input),
            PalettePhase::CollectArgs { input, .. } => Some(input),
        }
    }

    /// Tear down all editor slots owned by the palette. Called on any palette
    /// close path (`CancelPromptInput`, `Ctrl-C`, or post-`Dispatch` cleanup)
    /// so the scratch editor for the current phase doesn't linger in the
    /// workspace's slotmap.
    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        match &self.phase {
            PalettePhase::Filter { input, .. } => input.dispose(ws),
            PalettePhase::CollectArgs { input, .. } => input.dispose(ws),
        }
    }

    /// Refilter the action list against the current filter text. `ws` is
    /// required to read the [`InputView`]'s current rope contents. Called
    /// every frame from the renderer so mutations picked up by
    /// `handle_insert_key` (typing / backspace / cursor motion) are reflected
    /// without a dedicated sync hook.
    pub(crate) fn refilter_from_input(&mut self, ws: &Workspace) {
        if let PalettePhase::Filter {
            input,
            filtered,
            selected,
        } = &mut self.phase
        {
            let text = input.text(ws);
            refilter(&text, filtered, selected);
        }
    }

    /// Invoke the effective "submit" step for the palette's current phase.
    /// In [`PalettePhase::Filter`] this either dispatches a zero-arg action
    /// or transitions to [`PalettePhase::CollectArgs`] when the chosen action
    /// takes parameters. In [`PalettePhase::CollectArgs`] it parses the
    /// current parameter value and either advances to the next parameter or
    /// dispatches with the fully collected argument list. Called from the
    /// `SubmitPromptInput` action handler while the palette is open.
    pub(crate) fn handle_submit(
        &mut self,
        ws: &mut Workspace,
        executor: Executor,
    ) -> PaletteOutcome {
        match &mut self.phase {
            PalettePhase::Filter {
                input,
                filtered,
                selected,
            } => {
                let picked = filtered.get(*selected).copied();
                match picked {
                    Some(entry) if entry.def.params().is_empty() => {
                        input.dispose(ws);
                        PaletteOutcome::Dispatch(entry, Vec::new())
                    },
                    Some(entry) => {
                        input.dispose(ws);
                        let arg_input = InputView::create(
                            ws,
                            executor,
                            SubmitTarget::PaletteArg,
                            "",
                            "prompt",
                            1,
                        );
                        self.phase = PalettePhase::CollectArgs {
                            entry,
                            collected: Vec::new(),
                            current: 0,
                            input: arg_input,
                            error: None,
                        };
                        PaletteOutcome::None
                    },
                    None => PaletteOutcome::None,
                }
            },
            PalettePhase::CollectArgs {
                entry,
                collected,
                current,
                input,
                error,
            } => {
                let params = entry.def.params();
                let kind = params[*current].kind;
                let text = input.text(ws);
                match ParamValue::parse(kind, &text) {
                    Ok(value) => {
                        collected.push(value);
                        *current += 1;
                        if *current == params.len() {
                            input.dispose(ws);
                            let entry = *entry;
                            let collected = std::mem::take(collected);
                            return PaletteOutcome::Dispatch(entry, collected);
                        }
                        input.dispose(ws);
                        *input = InputView::create(
                            ws,
                            executor,
                            SubmitTarget::PaletteArg,
                            "",
                            "prompt",
                            1,
                        );
                        *error = None;
                        PaletteOutcome::None
                    },
                    Err(e) => {
                        *error = Some(e.to_string());
                        PaletteOutcome::None
                    },
                }
            },
        }
    }
}

pub(crate) fn refilter(
    input: &str,
    filtered: &mut Vec<&'static registry::RegistryEntry>,
    selected: &mut usize,
) {
    let needle = input.to_lowercase();
    let mut prefix: Vec<&'static registry::RegistryEntry> = Vec::new();
    let mut substring: Vec<&'static registry::RegistryEntry> = Vec::new();

    for entry in registry::all() {
        if !entry.def.palette_visible() {
            continue;
        }
        if needle.is_empty() {
            prefix.push(entry);
            continue;
        }
        let name_lc = entry.def.name().to_lowercase();
        if name_lc.starts_with(&needle) {
            prefix.push(entry);
        } else if name_lc.contains(&needle) {
            substring.push(entry);
        }
    }

    prefix.sort_by_key(|e| e.def.name());
    substring.sort_by_key(|e| e.def.name());
    prefix.extend(substring);
    *filtered = prefix;

    if *selected >= filtered.len() {
        *selected = filtered.len().saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_for(text: &str) -> Vec<&'static str> {
        let mut filtered = Vec::new();
        let mut selected = 0;
        refilter(text, &mut filtered, &mut selected);
        filtered.iter().map(|e| e.def.name()).collect()
    }

    #[test]
    fn empty_filter_lists_visible_actions_alphabetically() {
        let listed = names_for("");
        assert!(listed.contains(&"Quit"));
        assert!(listed.contains(&"OpenFile"));
        assert!(!listed.contains(&"OpenCommandPalette"));
        let mut sorted = listed.clone();
        sorted.sort();
        assert_eq!(listed, sorted);
    }

    #[test]
    fn prefix_filter_ranks_first() {
        let listed = names_for("Foc");
        assert!(listed.iter().all(|n| n.starts_with("Focus")));
        assert!(listed.contains(&"FocusLeft"));
    }

    #[test]
    fn substring_filter_after_prefix() {
        let listed = names_for("Pane");
        // ClosePane has "Pane" as a substring but not as a prefix.
        assert!(listed.contains(&"ClosePane"));
    }

    #[test]
    fn case_insensitive_filter() {
        assert_eq!(names_for("quit"), vec!["Quit"]);
    }

    #[test]
    fn refilter_clamps_selected_when_results_shrink() {
        let mut filtered = Vec::new();
        let mut selected = 7;
        refilter("quit", &mut filtered, &mut selected);
        assert_eq!(filtered.len(), 1);
        assert_eq!(selected, 0);
    }

    #[test]
    fn command_palette_opens_file_end_to_end() {
        let mut h = crate::Stoat::test();
        let path = crate::test_harness::write_file(&h, "palette_target.txt", "loaded via palette");
        let path_str = path.to_str().expect("utf8 path");

        h.type_text(":OpenFile");
        h.type_keys("enter");
        h.type_text(path_str);
        h.type_keys("enter");
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        assert!(
            frame.content.contains("loaded via palette"),
            "buffer not visible in frame:\n{}",
            frame.content
        );
    }

    #[test]
    fn command_palette_escape_cancels() {
        let mut h = crate::Stoat::test();
        h.type_text(":Open");
        h.type_keys("escape");
        let frame = h.snapshot();
        assert_eq!(frame.mode, "normal");
    }

    #[test]
    fn command_palette_filter_narrows_on_typing() {
        let mut h = crate::Stoat::test();
        h.type_text(":quit");
        h.type_keys("enter");
        let frame = h.snapshot();
        assert_eq!(frame.mode, "normal");
    }

    #[test]
    fn command_palette_down_then_enter_dispatches_selection() {
        let mut h = crate::Stoat::test();
        h.type_text(":Focus");
        h.type_keys("down enter");
        assert!(h.stoat.command_palette.is_none());
    }

    #[test]
    fn snapshot_command_palette_filter_empty() {
        let mut h = crate::Stoat::test();
        h.type_text(":");
        h.assert_snapshot("command_palette_filter_empty");
    }

    #[test]
    fn snapshot_command_palette_filter_typing() {
        let mut h = crate::Stoat::test();
        h.type_text(":Foc");
        h.assert_snapshot("command_palette_filter_typing");
    }

    #[test]
    fn snapshot_command_palette_filter_narrows_to_one() {
        let mut h = crate::Stoat::test();
        h.type_text(":quit");
        h.assert_snapshot("command_palette_filter_narrows_to_one");
    }

    #[test]
    fn snapshot_command_palette_collect_args_empty() {
        let mut h = crate::Stoat::test();
        h.type_text(":OpenFile");
        h.type_keys("enter");
        h.assert_snapshot("command_palette_collect_args_empty");
    }

    #[test]
    fn snapshot_command_palette_collect_args_typing() {
        let mut h = crate::Stoat::test();
        h.type_text(":OpenFile");
        h.type_keys("enter");
        h.type_text("/tmp/example.rs");
        h.assert_snapshot("command_palette_collect_args_typing");
    }
}
