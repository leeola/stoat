//! GUI help modal.
//!
//! Mirrors the TUI [`stoat::help::Help`] surface: lists every
//! palette-visible action (Active scope shows only keybindings live
//! in the current mode; All scope shows every registry entry),
//! supports search, scope toggle, list navigation, detail scroll,
//! and submit-to-dispatch. Construction takes a snapshot of the
//! workspace's mode and active keymap bindings so the modal stays
//! a frozen view -- toggling scope only swaps between Active and
//! All over the same captured snapshot.
//!
//! Wired into the modal layer by
//! [`crate::workspace::Workspace::dispatch_action`] handling
//! `ActionKind::OpenHelp`.

use crate::{
    buffer::Buffer,
    editor::{Editor, EditorEvent},
    modal_layer::ModalView,
    theme::ActiveTheme,
    workspace::Workspace,
};
use gpui::{
    div, App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    WeakEntity, Window,
};
use stoat::keymap::{ResolvedAction, ResolvedArg};
use stoat_action::{
    registry::{self, RegistryEntry},
    ActionDef, ActionKind, ParamValue,
};
use stoat_config::Value;

/// Listing mode for the help modal. `Active` lists only entries for
/// keybindings that were active in the captured mode; `All` lists
/// every palette-visible action regardless of binding state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpScope {
    Active,
    All,
}

/// One row in the help modal: a registry-backed action plus
/// optional binding metadata. `key_label` is `Some` when the entry
/// came from an active keybinding, `None` for All-scope entries.
pub struct HelpEntry {
    pub def: &'static dyn ActionDef,
    pub key_label: Option<String>,
    pub bound_args: Vec<ResolvedArg>,
}

pub struct HelpModal {
    input: Entity<Editor>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    active: Vec<(String, Vec<ResolvedAction>)>,
    snapshot_mode: String,
    entries: Vec<HelpEntry>,
    filtered: Vec<usize>,
    selected: usize,
    scope: HelpScope,
    detail_scroll: u16,
    _input_subscription: Subscription,
}

impl HelpModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        snapshot_mode: String,
        active: Vec<(String, Vec<ResolvedAction>)>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::auto_height(1, 24, window, cx));
        let subscription = cx.subscribe(&input, |this, _editor, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Changed) {
                this.refilter(cx);
            }
        });
        let entries = build_active_entries(&active);
        let filtered: Vec<usize> = (0..entries.len()).collect();
        Self {
            input,
            focus_handle: cx.focus_handle(),
            workspace,
            active,
            snapshot_mode,
            entries,
            filtered,
            selected: 0,
            scope: HelpScope::Active,
            detail_scroll: 0,
            _input_subscription: subscription,
        }
    }

    fn selected_entry(&self) -> Option<&HelpEntry> {
        let idx = *self.filtered.get(self.selected)?;
        self.entries.get(idx)
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.filtered.len() - 1) as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
        self.detail_scroll = 0;
    }

    fn jump_selection(&mut self, target: usize) {
        self.selected = target.min(self.filtered.len().saturating_sub(1));
        self.detail_scroll = 0;
    }

    fn scroll_detail(&mut self, delta: i32) {
        if delta < 0 {
            self.detail_scroll = self.detail_scroll.saturating_sub((-delta) as u16);
        } else {
            self.detail_scroll = self.detail_scroll.saturating_add(delta as u16);
        }
    }

    fn toggle_scope(&mut self, cx: &Context<'_, Self>) {
        self.scope = match self.scope {
            HelpScope::Active => HelpScope::All,
            HelpScope::All => HelpScope::Active,
        };
        self.entries = match self.scope {
            HelpScope::Active => build_active_entries(&self.active),
            HelpScope::All => build_all_entries(),
        };
        let needle = current_needle(&self.input, cx);
        self.filtered = refilter(&self.entries, &needle);
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        self.detail_scroll = 0;
    }

    fn refilter(&mut self, cx: &Context<'_, Self>) {
        let needle = current_needle(&self.input, cx);
        self.filtered = refilter(&self.entries, &needle);
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        self.detail_scroll = 0;
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let Some(entry) = self.selected_entry() else {
            cx.emit(DismissEvent);
            return true;
        };
        let registry_entry = match registry::lookup(entry.def.name()) {
            Some(e) => e,
            None => {
                cx.emit(DismissEvent);
                return true;
            },
        };
        let params = entry.def.params();
        let values = if params.is_empty() {
            Vec::new()
        } else if entry.bound_args.len() < params.len() {
            cx.emit(DismissEvent);
            return true;
        } else {
            let mut values = Vec::with_capacity(params.len());
            for arg in &entry.bound_args {
                match arg_to_param_value(arg) {
                    Some(v) => values.push(v),
                    None => {
                        cx.emit(DismissEvent);
                        return true;
                    },
                }
            }
            values
        };
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            let Some(ws) = workspace.upgrade() else {
                return;
            };
            let Ok(action) = (registry_entry.create)(&values) else {
                tracing::warn!(
                    target: "stoat_gui::help",
                    action = registry_entry.def.name(),
                    "help confirm could not build action",
                );
                return;
            };
            ws.update(cx, |ws, cx| ws.dispatch_action(action, window, cx));
        });
        cx.emit(DismissEvent);
        true
    }

    fn close(&mut self, cx: &mut Context<'_, Self>) -> bool {
        cx.emit(DismissEvent);
        true
    }
}

fn current_needle(input: &Entity<Editor>, cx: &App) -> String {
    let editor = input.read(cx);
    let multi_buffer = editor.multi_buffer().read(cx);
    multi_buffer
        .as_singleton()
        .map(|b: &Entity<Buffer>| b.read(cx).text())
        .unwrap_or_default()
        .to_lowercase()
}

fn build_active_entries(active: &[(String, Vec<ResolvedAction>)]) -> Vec<HelpEntry> {
    let mut entries = Vec::new();
    for (label, actions) in active {
        let Some(first) = actions.first() else {
            continue;
        };
        let def: &'static dyn ActionDef = match registry::lookup(&first.name) {
            Some(reg) => reg.def,
            None => continue,
        };
        entries.push(HelpEntry {
            def,
            key_label: Some(label.clone()),
            bound_args: first.args.clone(),
        });
    }
    entries.sort_by(|a, b| {
        a.key_label
            .as_deref()
            .unwrap_or("")
            .cmp(b.key_label.as_deref().unwrap_or(""))
            .then_with(|| a.def.name().cmp(b.def.name()))
    });
    entries
}

fn build_all_entries() -> Vec<HelpEntry> {
    let mut entries: Vec<HelpEntry> = registry::all()
        .filter(|e: &&RegistryEntry| e.def.palette_visible())
        .map(|e| HelpEntry {
            def: e.def,
            key_label: None,
            bound_args: Vec::new(),
        })
        .collect();
    entries.sort_by(|a, b| a.def.name().cmp(b.def.name()));
    entries
}

fn refilter(entries: &[HelpEntry], needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return (0..entries.len()).collect();
    }

    let mut prefix_name: Vec<usize> = Vec::new();
    let mut substring_name: Vec<usize> = Vec::new();
    let mut key_match: Vec<usize> = Vec::new();
    let mut short_match: Vec<usize> = Vec::new();
    let mut long_match: Vec<usize> = Vec::new();

    for (i, entry) in entries.iter().enumerate() {
        let name_lc = entry.def.name().to_lowercase();
        if name_lc.starts_with(needle) {
            prefix_name.push(i);
            continue;
        }
        if name_lc.contains(needle) {
            substring_name.push(i);
            continue;
        }
        if let Some(label) = entry.key_label.as_deref() {
            if label.to_lowercase().contains(needle) {
                key_match.push(i);
                continue;
            }
        }
        if entry.def.short_desc().to_lowercase().contains(needle) {
            short_match.push(i);
            continue;
        }
        if entry.def.long_desc().to_lowercase().contains(needle) {
            long_match.push(i);
        }
    }

    let sort = |v: &mut Vec<usize>, entries: &[HelpEntry]| {
        v.sort_by(|&a, &b| entries[a].def.name().cmp(entries[b].def.name()));
    };
    sort(&mut prefix_name, entries);
    sort(&mut substring_name, entries);
    sort(&mut key_match, entries);
    sort(&mut short_match, entries);
    sort(&mut long_match, entries);

    let mut filtered = prefix_name;
    filtered.extend(substring_name);
    filtered.extend(key_match);
    filtered.extend(short_match);
    filtered.extend(long_match);
    filtered
}

fn arg_to_param_value(arg: &ResolvedArg) -> Option<ParamValue> {
    match &arg.value {
        Value::String(s) => Some(ParamValue::String(s.clone())),
        Value::Ident(s) => Some(ParamValue::String(s.clone())),
        Value::Number(n) => Some(ParamValue::Number(*n)),
        Value::Bool(b) => Some(ParamValue::Bool(*b)),
        _ => None,
    }
}

fn format_arg(arg: &ResolvedArg) -> Option<String> {
    match &arg.value {
        Value::String(s) => Some(format!("\"{s}\"")),
        Value::Ident(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn format_example(entry: &HelpEntry) -> String {
    let name = entry.def.name();
    let params = entry.def.params();
    if params.is_empty() {
        return format!("{name}()");
    }
    if !entry.bound_args.is_empty() {
        let args: Vec<String> = entry.bound_args.iter().filter_map(format_arg).collect();
        return format!("{name}({})", args.join(", "));
    }
    let placeholders: Vec<String> = params.iter().map(|p| p.name.to_string()).collect();
    format!("{name}({})", placeholders.join(", "))
}

impl Render for HelpModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let title = match self.scope {
            HelpScope::Active => SharedString::from(format!(
                "help: active ({})",
                if self.snapshot_mode.is_empty() {
                    "normal"
                } else {
                    &self.snapshot_mode
                },
            )),
            HelpScope::All => SharedString::from("help: all actions"),
        };

        let selection_bg = theme.selection;
        let list_rows: Vec<gpui::AnyElement> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(row, &entry_idx)| {
                let entry = &self.entries[entry_idx];
                let is_selected = row == self.selected;
                let key_text = entry.key_label.as_deref().unwrap_or("");
                let row_div = div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().min_w(gpui::px(40.0)).child(key_text.to_string()))
                    .child(div().child(entry.def.name().to_string()))
                    .child(div().child(entry.def.short_desc().to_string()));
                if is_selected {
                    row_div.bg(selection_bg).into_any_element()
                } else {
                    row_div.into_any_element()
                }
            })
            .collect();

        let detail_lines = build_detail_lines(self.selected_entry());
        let scroll = self.detail_scroll as usize;
        let detail_rows: Vec<gpui::AnyElement> = detail_lines
            .into_iter()
            .skip(scroll)
            .map(|line| div().child(line).into_any_element())
            .collect();

        div()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(div().text_color(theme.ui_modal_help).child(title))
            .child(self.input.clone())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_grow()
                    .child(div().flex_grow().children(list_rows))
                    .child(div().flex_grow().children(detail_rows)),
            )
    }
}

fn build_detail_lines(entry: Option<&HelpEntry>) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(entry) = entry else {
        return lines;
    };
    lines.push(entry.def.name().to_string());
    if let Some(label) = entry.key_label.as_deref() {
        lines.push(format!("bound: {label}"));
    } else {
        lines.push("(unbound)".to_string());
    }
    lines.push(entry.def.short_desc().to_string());
    lines.push(String::new());
    for wrapped in wrap_text(entry.def.long_desc(), 80) {
        lines.push(wrapped);
    }
    let params = entry.def.params();
    if !params.is_empty() {
        lines.push(String::new());
        lines.push("Parameters:".to_string());
        for p in params {
            let required = if p.required { "*" } else { "" };
            lines.push(format!(
                "  {}{}: {}: {}",
                p.name, required, p.kind, p.description
            ));
        }
    }
    lines.push(String::new());
    lines.push("Example:".to_string());
    lines.push(format!("  {}", format_example(entry)));
    lines
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            let needs_space = !current.is_empty();
            let projected = current.chars().count() + word.chars().count() + needs_space as usize;
            if projected > width && !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            if needs_space && !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out
}

impl Focusable for HelpModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for HelpModal {}

/// Open the help modal as a workspace modal seeded with the current
/// mode's active bindings. Constructed in `Workspace::dispatch_action`
/// when `OpenHelp` is dispatched.
pub fn open_help(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<'_, Workspace>) {
    let weak = cx.weak_entity();
    let (mode, active) = {
        let sm = workspace.input_state_machine().read(cx);
        let mode = sm.mode().to_string();
        let bindings: Vec<(String, Vec<ResolvedAction>)> = sm
            .keymap()
            .active_bindings(sm)
            .into_iter()
            .map(|(label, actions)| (label, actions.to_vec()))
            .collect();
        (mode, bindings)
    };
    workspace.toggle_modal::<HelpModal, _>(window, cx, move |window, cx| {
        HelpModal::new(weak, mode, active, window, cx)
    });
}

impl ModalView for HelpModal {
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match action.kind() {
            ActionKind::HelpSelectPrev => {
                self.move_selection(-1);
                cx.notify();
                true
            },
            ActionKind::HelpSelectNext => {
                self.move_selection(1);
                cx.notify();
                true
            },
            ActionKind::HelpScopeToggle => {
                self.toggle_scope(cx);
                cx.notify();
                true
            },
            ActionKind::HelpScrollDetailUp => {
                self.scroll_detail(-5);
                cx.notify();
                true
            },
            ActionKind::HelpScrollDetailDown => {
                self.scroll_detail(5);
                cx.notify();
                true
            },
            ActionKind::HelpJumpFirst => {
                self.jump_selection(0);
                cx.notify();
                true
            },
            ActionKind::HelpJumpLast => {
                let last = self.filtered.len().saturating_sub(1);
                self.jump_selection(last);
                cx.notify();
                true
            },
            ActionKind::CloseHelp | ActionKind::DismissModal => self.close(cx),
            _ => false,
        }
    }

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.confirm(window, cx)
    }

    fn cancel_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{AppContext, TestAppContext, VisualTestContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn new_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx))
    }

    fn binding(label: &str, action_name: &str) -> (String, Vec<ResolvedAction>) {
        (
            label.to_owned(),
            vec![ResolvedAction {
                name: action_name.to_owned(),
                args: Vec::new(),
            }],
        )
    }

    fn sample_active() -> Vec<(String, Vec<ResolvedAction>)> {
        vec![
            binding("q", "Quit"),
            binding("h", "MoveLeft"),
            binding("k", "MoveUp"),
        ]
    }

    fn open_modal(
        vcx: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
        active: Vec<(String, Vec<ResolvedAction>)>,
        mode: &str,
    ) -> Entity<HelpModal> {
        let weak = workspace.downgrade();
        let mode = mode.to_string();
        vcx.update(|window, cx| cx.new(|cx| HelpModal::new(weak, mode, active, window, cx)))
    }

    fn type_into(input: &Entity<Editor>, vcx: &mut VisualTestContext, text: &str) {
        let buffer = input.read_with(vcx, |ed, cx| {
            ed.multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("auto-height singleton")
                .clone()
        });
        buffer.update(vcx, |b, cx| b.edit(0..0, text, cx));
    }

    #[test]
    fn new_lists_active_bindings_as_entries() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (workspace, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &workspace, sample_active(), "normal");
        vcx.run_until_parked();

        let names: Vec<String> = modal.read_with(vcx, |m, _| {
            m.entries.iter().map(|e| e.def.name().to_string()).collect()
        });
        assert_eq!(names, vec!["MoveLeft", "MoveUp", "Quit"]);
    }

    #[test]
    fn toggle_scope_swaps_entries_between_active_and_all() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (workspace, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &workspace, sample_active(), "normal");
        vcx.run_until_parked();

        let active_count = modal.read_with(vcx, |m, _| m.entries.len());
        assert_eq!(active_count, 3);

        modal.update_in(vcx, |m, _window, cx| {
            m.toggle_scope(cx);
        });
        let (all_count, scope) = modal.read_with(vcx, |m, _| (m.entries.len(), m.scope));
        assert_eq!(scope, HelpScope::All);
        assert!(
            all_count > active_count,
            "All scope must list more entries than Active"
        );
    }

    #[test]
    fn move_selection_clamps_at_bounds() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (workspace, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &workspace, sample_active(), "normal");

        modal.update(vcx, |m, _| m.move_selection(-1));
        assert_eq!(modal.read_with(vcx, |m, _| m.selected), 0);

        modal.update(vcx, |m, _| m.move_selection(99));
        assert_eq!(modal.read_with(vcx, |m, _| m.selected), 2);
    }

    #[test]
    fn scroll_detail_clamps_at_zero() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (workspace, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &workspace, sample_active(), "normal");

        modal.update(vcx, |m, _| m.scroll_detail(-5));
        assert_eq!(modal.read_with(vcx, |m, _| m.detail_scroll), 0);

        modal.update(vcx, |m, _| m.scroll_detail(7));
        assert_eq!(modal.read_with(vcx, |m, _| m.detail_scroll), 7);

        modal.update(vcx, |m, _| m.scroll_detail(-3));
        assert_eq!(modal.read_with(vcx, |m, _| m.detail_scroll), 4);
    }

    #[test]
    fn refilter_prefix_match_wins() {
        let entries = vec![
            HelpEntry {
                def: stoat_action::MoveLeft::DEF,
                key_label: Some("h".to_string()),
                bound_args: Vec::new(),
            },
            HelpEntry {
                def: stoat_action::Quit::DEF,
                key_label: Some("q".to_string()),
                bound_args: Vec::new(),
            },
        ];
        let filtered = refilter(&entries, "qu");
        assert_eq!(filtered, vec![1]);
    }

    #[test]
    fn refilter_on_input_change_updates_filtered() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (workspace, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &workspace, sample_active(), "normal");
        vcx.run_until_parked();

        let input = modal.read_with(vcx, |m, _| m.input.clone());
        type_into(&input, vcx, "Quit");
        vcx.run_until_parked();

        modal.read_with(vcx, |m, _| {
            let names: Vec<&str> = m
                .filtered
                .iter()
                .map(|&i| m.entries[i].def.name())
                .collect();
            assert_eq!(names, vec!["Quit"]);
        });
    }

    #[test]
    fn close_help_emits_dismiss() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (workspace, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &workspace, sample_active(), "normal");

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::CloseHelp, window, cx)
        });
        assert!(handled, "CloseHelp must be handled");
    }

    #[test]
    fn dismiss_modal_emits_dismiss() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (workspace, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &workspace, sample_active(), "normal");

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::DismissModal, window, cx)
        });
        assert!(handled, "DismissModal must be handled");
    }

    #[test]
    fn unknown_action_falls_through() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (workspace, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &workspace, sample_active(), "normal");

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::OpenHelp, window, cx)
        });
        assert!(!handled, "Unrelated actions must not be intercepted");
    }
}
