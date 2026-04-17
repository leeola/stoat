use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use stoat_action::{registry, ParamValue};

pub struct CommandPalette {
    phase: PalettePhase,
}

pub enum PalettePhase {
    /// Filtering the action list. The user is typing to narrow candidates and
    /// using Up/Down (or Ctrl-P/N) to navigate.
    Filter {
        input: String,
        filtered: Vec<&'static registry::RegistryEntry>,
        selected: usize,
    },
    /// A param-taking action has been chosen and the palette is walking the
    /// user through providing each parameter in sequence.
    CollectArgs {
        entry: &'static registry::RegistryEntry,
        collected: Vec<ParamValue>,
        current: usize,
        input: String,
        error: Option<String>,
    },
}

pub enum PaletteOutcome {
    /// Re-render but keep the palette open.
    None,
    /// User cancelled.
    Close,
    /// User selected an action with all required parameters collected.
    Dispatch(&'static registry::RegistryEntry, Vec<ParamValue>),
}

impl CommandPalette {
    pub fn new() -> Self {
        let mut phase = PalettePhase::Filter {
            input: String::new(),
            filtered: Vec::new(),
            selected: 0,
        };
        if let PalettePhase::Filter {
            input,
            filtered,
            selected,
        } = &mut phase
        {
            refilter(input, filtered, selected);
        }
        Self { phase }
    }

    pub fn phase(&self) -> &PalettePhase {
        &self.phase
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PaletteOutcome {
        match &mut self.phase {
            PalettePhase::Filter { .. } => self.handle_filter_key(key),
            PalettePhase::CollectArgs { .. } => self.handle_collect_args_key(key),
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> PaletteOutcome {
        let PalettePhase::Filter {
            input,
            filtered,
            selected,
        } = &mut self.phase
        else {
            unreachable!()
        };

        match key.code {
            KeyCode::Esc => PaletteOutcome::Close,

            KeyCode::Enter => match filtered.get(*selected).copied() {
                Some(entry) if entry.def.params().is_empty() => {
                    PaletteOutcome::Dispatch(entry, Vec::new())
                },
                Some(entry) => {
                    self.phase = PalettePhase::CollectArgs {
                        entry,
                        collected: Vec::new(),
                        current: 0,
                        input: String::new(),
                        error: None,
                    };
                    PaletteOutcome::None
                },
                None => PaletteOutcome::None,
            },

            KeyCode::Up => {
                move_selection(filtered.len(), selected, -1);
                PaletteOutcome::None
            },
            KeyCode::Down => {
                move_selection(filtered.len(), selected, 1);
                PaletteOutcome::None
            },

            KeyCode::Backspace => {
                input.pop();
                refilter(input, filtered, selected);
                PaletteOutcome::None
            },

            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                move_selection(filtered.len(), selected, -1);
                PaletteOutcome::None
            },
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                move_selection(filtered.len(), selected, 1);
                PaletteOutcome::None
            },

            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                input.push(c);
                refilter(input, filtered, selected);
                PaletteOutcome::None
            },

            _ => PaletteOutcome::None,
        }
    }

    fn handle_collect_args_key(&mut self, key: KeyEvent) -> PaletteOutcome {
        let PalettePhase::CollectArgs {
            entry,
            collected,
            current,
            input,
            error,
        } = &mut self.phase
        else {
            unreachable!()
        };

        match key.code {
            KeyCode::Esc => PaletteOutcome::Close,

            KeyCode::Enter => {
                let params = entry.def.params();
                let kind = params[*current].kind;
                match ParamValue::parse(kind, input) {
                    Ok(value) => {
                        collected.push(value);
                        *current += 1;
                        if *current == params.len() {
                            let entry = *entry;
                            let collected = std::mem::take(collected);
                            return PaletteOutcome::Dispatch(entry, collected);
                        }
                        input.clear();
                        *error = None;
                        PaletteOutcome::None
                    },
                    Err(e) => {
                        *error = Some(e.to_string());
                        PaletteOutcome::None
                    },
                }
            },

            KeyCode::Backspace => {
                input.pop();
                *error = None;
                PaletteOutcome::None
            },

            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                input.push(c);
                *error = None;
                PaletteOutcome::None
            },

            _ => PaletteOutcome::None,
        }
    }
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

fn move_selection(len: usize, selected: &mut usize, delta: i32) {
    if len == 0 {
        *selected = 0;
        return;
    }
    let max = (len - 1) as i32;
    let next = (*selected as i32 + delta).clamp(0, max);
    *selected = next as usize;
}

fn refilter(
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn filter_state(
        palette: &CommandPalette,
    ) -> (&str, &[&'static registry::RegistryEntry], usize) {
        match &palette.phase {
            PalettePhase::Filter {
                input,
                filtered,
                selected,
            } => (input.as_str(), filtered.as_slice(), *selected),
            _ => panic!("expected Filter phase"),
        }
    }

    fn collect_state(
        palette: &CommandPalette,
    ) -> (
        &'static registry::RegistryEntry,
        &[ParamValue],
        usize,
        &str,
        Option<&str>,
    ) {
        match &palette.phase {
            PalettePhase::CollectArgs {
                entry,
                collected,
                current,
                input,
                error,
            } => (
                *entry,
                collected.as_slice(),
                *current,
                input.as_str(),
                error.as_deref(),
            ),
            _ => panic!("expected CollectArgs phase"),
        }
    }

    fn names(palette: &CommandPalette) -> Vec<&'static str> {
        let (_, filtered, _) = filter_state(palette);
        filtered.iter().map(|e| e.def.name()).collect()
    }

    fn type_str(palette: &mut CommandPalette, s: &str) {
        for ch in s.chars() {
            palette.handle_key(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn new_lists_visible_actions_alphabetically() {
        let palette = CommandPalette::new();
        let listed = names(&palette);
        assert!(listed.contains(&"Quit"));
        assert!(listed.contains(&"OpenFile"));
        assert!(!listed.contains(&"OpenCommandPalette"));
        let mut sorted = listed.clone();
        sorted.sort();
        assert_eq!(listed, sorted);
    }

    #[test]
    fn prefix_filter_ranks_first() {
        let mut palette = CommandPalette::new();
        type_str(&mut palette, "Foc");
        let listed = names(&palette);
        assert!(listed.iter().all(|n| n.starts_with("Focus")));
        assert!(listed.contains(&"FocusLeft"));
    }

    #[test]
    fn substring_filter_after_prefix() {
        let mut palette = CommandPalette::new();
        type_str(&mut palette, "Pane");
        let listed = names(&palette);
        // ClosePane has "Pane" as a substring but not as a prefix.
        assert!(listed.contains(&"ClosePane"));
    }

    #[test]
    fn case_insensitive_filter() {
        let mut palette = CommandPalette::new();
        type_str(&mut palette, "quit");
        assert_eq!(names(&palette), vec!["Quit"]);
    }

    #[test]
    fn backspace_pops_char_and_refilters() {
        let mut palette = CommandPalette::new();
        type_str(&mut palette, "Foc");
        let narrow = filter_state(&palette).1.len();
        palette.handle_key(key(KeyCode::Backspace));
        let (input, filtered, _) = filter_state(&palette);
        assert!(filtered.len() >= narrow);
        assert_eq!(input, "Fo");
    }

    #[test]
    fn down_clamps_to_last() {
        let mut palette = CommandPalette::new();
        let last = filter_state(&palette).1.len() - 1;
        for _ in 0..(last + 5) {
            palette.handle_key(key(KeyCode::Down));
        }
        assert_eq!(filter_state(&palette).2, last);
    }

    #[test]
    fn up_clamps_to_first() {
        let mut palette = CommandPalette::new();
        for _ in 0..3 {
            palette.handle_key(key(KeyCode::Down));
        }
        for _ in 0..10 {
            palette.handle_key(key(KeyCode::Up));
        }
        assert_eq!(filter_state(&palette).2, 0);
    }

    #[test]
    fn ctrl_n_moves_down_ctrl_p_moves_up() {
        let mut palette = CommandPalette::new();
        palette.handle_key(ctrl('n'));
        assert_eq!(filter_state(&palette).2, 1);
        palette.handle_key(ctrl('p'));
        assert_eq!(filter_state(&palette).2, 0);
    }

    #[test]
    fn enter_zero_arg_returns_dispatch() {
        let mut palette = CommandPalette::new();
        type_str(&mut palette, "Quit");
        let outcome = palette.handle_key(key(KeyCode::Enter));
        match outcome {
            PaletteOutcome::Dispatch(entry, params) => {
                assert_eq!(entry.def.name(), "Quit");
                assert!(params.is_empty());
            },
            _ => panic!("expected Dispatch"),
        }
    }

    #[test]
    fn esc_returns_close() {
        let mut palette = CommandPalette::new();
        assert!(matches!(
            palette.handle_key(key(KeyCode::Esc)),
            PaletteOutcome::Close
        ));
    }

    #[test]
    fn utf8_input_safe() {
        let mut palette = CommandPalette::new();
        palette.handle_key(key(KeyCode::Char('é')));
        assert_eq!(filter_state(&palette).0, "é");
        palette.handle_key(key(KeyCode::Backspace));
        assert_eq!(filter_state(&palette).0, "");
    }

    #[test]
    fn enter_param_action_transitions_to_collect_args() {
        let mut palette = CommandPalette::new();
        type_str(&mut palette, "OpenFile");
        let outcome = palette.handle_key(key(KeyCode::Enter));
        assert!(matches!(outcome, PaletteOutcome::None));
        let (entry, collected, current, input, error) = collect_state(&palette);
        assert_eq!(entry.def.name(), "OpenFile");
        assert!(collected.is_empty());
        assert_eq!(current, 0);
        assert_eq!(input, "");
        assert_eq!(error, None);
    }

    #[test]
    fn collect_args_collects_string() {
        let mut palette = CommandPalette::new();
        type_str(&mut palette, "OpenFile");
        palette.handle_key(key(KeyCode::Enter));
        type_str(&mut palette, "/tmp/x.rs");
        let outcome = palette.handle_key(key(KeyCode::Enter));
        match outcome {
            PaletteOutcome::Dispatch(entry, params) => {
                assert_eq!(entry.def.name(), "OpenFile");
                assert_eq!(params, vec![ParamValue::String("/tmp/x.rs".into())]);
            },
            _ => panic!("expected Dispatch"),
        }
    }

    #[test]
    fn collect_args_esc_closes() {
        let mut palette = CommandPalette::new();
        type_str(&mut palette, "OpenFile");
        palette.handle_key(key(KeyCode::Enter));
        type_str(&mut palette, "partial");
        assert!(matches!(
            palette.handle_key(key(KeyCode::Esc)),
            PaletteOutcome::Close
        ));
    }

    #[test]
    fn collect_args_backspace_safe() {
        let mut palette = CommandPalette::new();
        type_str(&mut palette, "OpenFile");
        palette.handle_key(key(KeyCode::Enter));
        palette.handle_key(key(KeyCode::Char('é')));
        palette.handle_key(key(KeyCode::Backspace));
        assert_eq!(collect_state(&palette).3, "");
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
