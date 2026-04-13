use crate::{
    action_handlers,
    badge::{Anchor, BadgeState, BadgeTray, StackDirection},
    buffer::{BufferId, TextBufferSnapshot},
    buffer_registry::BufferRegistry,
    command_palette::{CommandPalette, PaletteOutcome},
    display_map::{highlights::SemanticTokenHighlight, syntax_theme::SyntaxStyles, BlockRowKind},
    editor_state::{EditorId, EditorState},
    host::{ClaudeCodeFactory, ClaudeCodeSessions},
    keymap::{Keymap, KeymapState, ResolvedAction, ResolvedArg, StateValue},
    pane::{Pane, View},
    review::ReviewRow,
    run::{PtyNotification, RunId, RunState},
    workspace::{Workspace, WorkspaceId},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Text,
    widgets::{Block, Borders, Paragraph, Widget},
};
use slotmap::SlotMap;
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_action::{Action, OpenFile, OpenReview};
use stoat_config::Settings;
use stoat_language::{self as language, Language, LanguageRegistry, SyntaxState};
use stoat_scheduler::Executor;
use stoat_text::Bias;
use tokio::sync::mpsc::{Receiver, Sender};

const DEFAULT_KEYMAP: &str = include_str!("../../config.stcfg");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateEffect {
    Redraw,
    Quit,
    None,
}

pub struct Stoat {
    size: Rect,
    pub mode: String,
    pub executor: Executor,
    keymap: Keymap,
    pub settings: Settings,
    pub(crate) command_palette: Option<CommandPalette>,
    pub(crate) language_registry: Arc<LanguageRegistry>,
    pub(crate) syntax_styles: SyntaxStyles,
    pub(crate) workspaces: SlotMap<WorkspaceId, Workspace>,
    pub(crate) active_workspace: WorkspaceId,
    claude_sessions: ClaudeCodeSessions,
    pub(crate) pty_tx: Sender<PtyNotification>,
    pty_rx: Receiver<PtyNotification>,
    pub(crate) modal_run: Option<RunId>,
}

/// Result of a successful background parse, ready to be installed on the
/// foreground thread.
pub(crate) struct ParseJobOutput {
    pub(crate) buffer_id: BufferId,
    pub(crate) syntax: SyntaxState,
    /// Multi-layer parse state from [`stoat_language::SyntaxMap::reparse`].
    /// Populated alongside [`Self::syntax`] so the legacy single-tree
    /// highlight path and the capture-merging path can run side by side
    /// while consumers migrate.
    pub(crate) syntax_map: stoat_language::SyntaxMap,
    pub(crate) tokens: Arc<[SemanticTokenHighlight]>,
}

impl Stoat {
    #[cfg(test)]
    pub fn test() -> crate::test_harness::TestHarness {
        crate::test_harness::TestHarness::default()
    }

    #[cfg(test)]
    pub(crate) fn active_keys_for_mode(
        &self,
        mode: &str,
    ) -> Vec<(&crate::keymap::CompiledKey, &[ResolvedAction])> {
        let state = StoatKeymapState::new(mode);
        self.keymap.active_keys(&state)
    }

    pub fn new(executor: Executor, cli_settings: Settings, initial_git_root: PathBuf) -> Self {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        if !errors.is_empty() {
            tracing::error!(
                "default keymap parse errors: {}",
                stoat_config::format_errors(DEFAULT_KEYMAP, &errors)
            );
        }
        let settings = config
            .as_ref()
            .map(Settings::from_config)
            .unwrap_or_default()
            .merge(cli_settings);
        let keymap = config
            .map(|c| Keymap::compile(&c))
            .unwrap_or_else(|| Keymap::compile(&stoat_config::Config { blocks: vec![] }));

        let syntax_styles = SyntaxStyles::standard();
        let language_registry = Arc::new(LanguageRegistry::standard());
        // Install a theme-driven HighlightMap on every loaded language.
        // Done at registry-init time because adding new languages later
        // would also need a fresh HighlightMap; today the registry is
        // static so this one-shot install is sufficient.
        let theme_keys = syntax_styles.theme_keys();
        for lang in language_registry.languages() {
            let map = stoat_language::HighlightMap::new(lang.highlight_capture_names(), theme_keys);
            lang.set_highlight_map(map);
        }

        let mut workspaces = SlotMap::with_key();
        let active_workspace = workspaces.insert(Workspace::new(initial_git_root, &executor));
        workspaces[active_workspace].id = active_workspace;

        let (pty_tx, pty_rx) = tokio::sync::mpsc::channel(256);

        Self {
            size: Rect::default(),
            mode: "normal".into(),
            executor,
            keymap,
            settings,
            command_palette: None,
            language_registry,
            syntax_styles,
            workspaces,
            active_workspace,
            claude_sessions: ClaudeCodeSessions::default(),
            pty_tx,
            pty_rx,
            modal_run: None,
        }
    }

    pub fn active_workspace(&self) -> &Workspace {
        &self.workspaces[self.active_workspace]
    }

    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active_workspace]
    }

    pub fn set_claude_code_factory(&mut self, factory: Arc<dyn ClaudeCodeFactory>) {
        self.claude_sessions.set_factory(factory);
    }

    pub fn claude_sessions(&self) -> &ClaudeCodeSessions {
        &self.claude_sessions
    }

    pub fn claude_sessions_mut(&mut self) -> &mut ClaudeCodeSessions {
        &mut self.claude_sessions
    }

    /// Convenience wrapper that dispatches the [`OpenFile`] action with `path`.
    ///
    /// The action handler reads the file, creates a buffer, and shows it in
    /// the focused pane. A missing file becomes an empty buffer with the path
    /// attached (vim-style); other IO errors are logged and ignored.
    pub fn open_file(&mut self, path: &Path) {
        let action = OpenFile {
            path: path.to_path_buf(),
        };
        action_handlers::dispatch(self, &action);
    }

    pub fn open_review(&mut self) {
        action_handlers::dispatch(self, &OpenReview);
    }

    pub async fn run(
        &mut self,
        mut events: Receiver<Event>,
        render: Sender<Buffer>,
    ) -> io::Result<()> {
        loop {
            let effect = tokio::select! {
                biased;
                event = events.recv() => {
                    let Some(event) = event else { break };
                    self.update(event)
                }
                notif = self.pty_rx.recv() => {
                    let Some(notif) = notif else { continue };
                    self.handle_pty_notification(notif)
                }
            };
            match effect {
                UpdateEffect::Redraw => {
                    if render.send(self.render()).await.is_err() {
                        break;
                    }
                },
                UpdateEffect::Quit => break,
                UpdateEffect::None => {},
            }
        }
        Ok(())
    }

    pub(crate) fn update(&mut self, event: Event) -> UpdateEffect {
        match event {
            Event::Resize(w, h) => {
                self.size = Rect::new(0, 0, w, h);
                let size = self.size;
                self.active_workspace_mut().panes.resize(size);
                UpdateEffect::Redraw
            },
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            _ => UpdateEffect::None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> UpdateEffect {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(run_id) = self.modal_run {
                let ws = self.active_workspace_mut();
                if let Some(run_state) = ws.runs.get_mut(run_id) {
                    if let Some(handle) = &mut run_state.shell_handle {
                        handle.kill();
                    }
                    if let Some(block) = run_state.active_block_mut() {
                        block.finished = true;
                    }
                }
                return UpdateEffect::Redraw;
            }
            if self.mode == "run" {
                return action_handlers::dispatch(self, &stoat_action::RunInterrupt);
            }
            return UpdateEffect::Quit;
        }

        let key = normalize_shift_letter(key);

        if let Some(run_id) = self.modal_run {
            let finished = self
                .active_workspace()
                .runs
                .get(run_id)
                .is_some_and(|r| !r.is_running());
            if finished && key.code == KeyCode::Esc {
                self.active_workspace_mut().runs.remove(run_id);
                self.modal_run = None;
                return UpdateEffect::Redraw;
            }
            return UpdateEffect::None;
        }

        if self.command_palette.is_some() {
            return self.dispatch_palette_key(key);
        }

        if self.mode == "run" {
            if let Some(effect) = self.handle_run_key(key) {
                return effect;
            }
        }

        let state = StoatKeymapState::new(&self.mode);
        let Some(actions) = self.keymap.lookup(&state, &key) else {
            return UpdateEffect::None;
        };
        let actions = actions.to_vec();

        let mut effect = UpdateEffect::None;
        for ra in &actions {
            if ra.name == "SetMode" {
                if let Some(mode_name) = ra.args.first().and_then(arg_as_str) {
                    self.mode = mode_name;
                    effect = UpdateEffect::Redraw;
                }
                continue;
            }
            if let Some(action) = resolve_action(&ra.name, &ra.args) {
                let e = action_handlers::dispatch(self, &*action);
                match e {
                    UpdateEffect::Quit => return UpdateEffect::Quit,
                    UpdateEffect::Redraw => effect = UpdateEffect::Redraw,
                    UpdateEffect::None => {},
                }
            }
        }
        effect
    }

    fn handle_run_key(&mut self, key: KeyEvent) -> Option<UpdateEffect> {
        let ws = self.active_workspace_mut();
        let focused = ws.panes.focus();
        let View::Run(id) = ws.panes.pane(focused).view else {
            return None;
        };
        let run_state = ws.runs.get_mut(id)?;

        match key.code {
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                run_state.input.insert_char(ch);
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Backspace => {
                run_state.input.delete_backward();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Delete => {
                run_state.input.delete_forward();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                run_state.input.move_word_left();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                run_state.input.move_word_right();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Left => {
                run_state.input.move_left();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Right => {
                run_state.input.move_right();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Home => {
                run_state.input.move_home();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::End => {
                run_state.input.move_end();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Up => {
                run_state.history_up();
                Some(UpdateEffect::Redraw)
            },
            KeyCode::Down => {
                run_state.history_down();
                Some(UpdateEffect::Redraw)
            },
            // Enter and Escape fall through to keymap dispatch
            _ => None,
        }
    }

    pub(crate) fn handle_pty_notification(&mut self, notif: PtyNotification) -> UpdateEffect {
        let ws = self.active_workspace_mut();
        match notif {
            PtyNotification::Output { run_id, data } => {
                let Some(run_state) = ws.runs.get_mut(run_id) else {
                    return UpdateEffect::None;
                };
                let Some(block) = run_state.active_block_mut() else {
                    return UpdateEffect::None;
                };
                block.feed(&data);
                if block.grid.alt_screen_detected {
                    block.error = Some("this command requires a full terminal".into());
                    block.finished = true;
                    block.grid.alt_screen_detected = false;
                    if let Some(handle) = &mut run_state.shell_handle {
                        handle.kill();
                    }
                    run_state.shell_handle = None;
                }
                UpdateEffect::Redraw
            },
            PtyNotification::CommandDone {
                run_id,
                exit_status,
            } => {
                let Some(run_state) = ws.runs.get_mut(run_id) else {
                    return UpdateEffect::None;
                };
                let Some(block) = run_state.active_block_mut() else {
                    return UpdateEffect::None;
                };
                if !block.finished {
                    block.finished = true;
                    block.exit_status = exit_status;
                }
                UpdateEffect::Redraw
            },
        }
    }

    /// Drive background parse jobs: poll any in-flight tasks for completion,
    /// install their results, then spawn new jobs for visible buffers whose
    /// stored syntax version is stale.
    ///
    /// At most one job per buffer is in flight at a time. If a buffer advances
    /// past the in-flight job's `target_version`, the new job is queued only
    /// after the old one completes. Anchors in the result are computed using
    /// the parsed snapshot, so they remain valid even if the buffer has been
    /// edited further while the parse was running.
    fn drive_parse_jobs(&mut self) {
        let Self {
            workspaces,
            active_workspace,
            executor,
            syntax_styles,
            ..
        } = self;
        workspaces[*active_workspace].drive_parse_jobs(executor, syntax_styles);
    }

    pub(crate) fn render(&mut self) -> Buffer {
        self.drive_parse_jobs();
        let mut buf = Buffer::empty(self.size);
        let ws = &mut self.workspaces[self.active_workspace];
        let focused = ws.panes.focus();
        for (id, pane) in ws.panes.split_panes() {
            let is_focused = id == focused;
            render_pane(
                pane,
                is_focused,
                &mut ws.editors,
                &ws.buffers,
                &ws.runs,
                &mut buf,
            );
        }
        render_badges(&ws.badges, self.size, &mut buf);
        if let Some(run_id) = self.modal_run {
            if let Some(run_state) = ws.runs.get(run_id) {
                render_modal_run(run_state, self.size, &mut buf);
            }
        } else if let Some(palette) = &self.command_palette {
            render_command_palette(palette, self.size, &mut buf);
        } else if !PRIMARY_MODES.contains(&self.mode.as_str()) {
            let state = StoatKeymapState::new(&self.mode);
            let raw = self.keymap.active_bindings(&state);
            let bindings: Vec<_> = raw
                .iter()
                .map(|(key, actions)| {
                    let desc = actions.first().map(action_display_desc).unwrap_or_default();
                    (key.as_str(), desc)
                })
                .collect();
            render_mini_help(&self.mode, &bindings, self.size, &mut buf);
        }
        buf
    }

    fn dispatch_palette_key(&mut self, key: KeyEvent) -> UpdateEffect {
        let outcome = match self.command_palette.as_mut() {
            Some(palette) => palette.handle_key(key),
            None => return UpdateEffect::None,
        };
        match outcome {
            PaletteOutcome::None => UpdateEffect::Redraw,
            PaletteOutcome::Close => {
                self.command_palette = None;
                UpdateEffect::Redraw
            },
            PaletteOutcome::Dispatch(entry, params) => {
                self.command_palette = None;
                match (entry.create)(&params) {
                    Ok(action) => action_handlers::dispatch(self, &*action),
                    Err(e) => {
                        tracing::warn!("palette dispatch `{}`: {e}", entry.def.name());
                        UpdateEffect::Redraw
                    },
                }
            },
        }
    }
}

struct StoatKeymapState {
    mode_value: StateValue,
}

impl StoatKeymapState {
    fn new(mode: &str) -> Self {
        Self {
            mode_value: StateValue::String(mode.into()),
        }
    }
}

impl KeymapState for StoatKeymapState {
    fn get(&self, field: &str) -> Option<&StateValue> {
        match field {
            "mode" => Some(&self.mode_value),
            _ => None,
        }
    }
}

/// Collapse Shift+letter events onto the bare uppercase form so keymap bindings
/// written as `A` or `S-a` both match what terminals emit.
///
/// Default crossterm without the kitty keyboard protocol reports Shift+a as
/// `(Char('A'), SHIFT)`, but a binding written as `A` compiles to
/// `(Char('A'), NONE)`, and modifier comparison is strict. Normalizing the
/// event up-front keeps bindings terminal-agnostic.
fn normalize_shift_letter(key: KeyEvent) -> KeyEvent {
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return key;
    }
    let KeyCode::Char(ch) = key.code else {
        return key;
    };
    if !ch.is_ascii_alphabetic() {
        return key;
    }
    let mut modifiers = key.modifiers;
    modifiers.remove(KeyModifiers::SHIFT);
    KeyEvent::new(KeyCode::Char(ch.to_ascii_uppercase()), modifiers)
}

/// Synchronous core of the parse pipeline. When `deadline` is `Some`, the
/// host parse aborts if it would exceed it and the function returns `None`,
/// signalling that the caller should fall back to the background path.
/// `None` is also returned for ordinary parse failures (unsupported
/// language, etc.); the difference does not matter for the call sites.
pub(crate) fn parse_buffer_step(
    buffer_id: BufferId,
    snapshot: TextBufferSnapshot,
    lang: &Arc<Language>,
    prior: &mut Option<SyntaxState>,
    prior_syntax_map: &mut Option<stoat_language::SyntaxMap>,
    styles: &SyntaxStyles,
    deadline: Option<std::time::Instant>,
) -> Option<ParseJobOutput> {
    let cur_version = snapshot.version;
    let new_rope = snapshot.visible_text.clone();

    // Edit a clone of the prior tree rather than mutating it in place. If
    // the parse aborts (deadline exceeded, etc.) the caller's prior must
    // remain valid for the next attempt; an in-place edit would leave the
    // registry holding a half-edited tree that would double-stamp position
    // offsets when re-edited next call.
    //
    // tree_sitter::Tree::clone is O(1) (refcount bump on the root subtree),
    // and tree.edit goes through ts_subtree_edit which is copy-on-write, so
    // editing the clone leaves the original untouched.
    let edited_tree = prior.as_ref().map(|prev| {
        let mut tree = prev.tree.clone();
        let edits = snapshot.edits_since(prev.version);
        language::edit_tree(&mut tree, edits.edits(), &prev.rope_snapshot, &new_rope);
        tree
    });

    let tree = match edited_tree.as_ref() {
        Some(old_tree) => match deadline {
            Some(dl) => language::parse_rope_within(lang, &new_rope, Some(old_tree), dl)?,
            None => language::parse_rope(lang, &new_rope, Some(old_tree))?,
        },
        None => match deadline {
            Some(dl) => language::parse_rope_within(lang, &new_rope, None, dl)?,
            None => language::parse_rope(lang, &new_rope, None)?,
        },
    };

    // Parse succeeded; from here on we consume the prior state.
    let prev_injection_trees = prior
        .take()
        .map(|prev| prev.injection_trees)
        .unwrap_or_default();

    let extracted =
        language::extract_highlights_rope_with_cache(lang, &tree, &new_rope, prev_injection_trees);
    // Theme-driven path: span.id is set to the theme key index by
    // collect_highlights_into via language.highlight_map(). Spans
    // whose id is DEFAULT (capture not in the active theme) are
    // skipped because they have no rendered style.
    let tokens: Arc<[SemanticTokenHighlight]> = extracted
        .spans
        .into_iter()
        .filter_map(|sp| {
            let style_id = styles.id_for_highlight(sp.id)?;
            Some(SemanticTokenHighlight {
                // Insertions at the start of a token attach to the
                // previous span, not this one; insertions at the end
                // attach to the next span. Keeps a typed character
                // from silently extending a keyword or string into
                // neighboring text.
                range: snapshot.anchor_at(sp.byte_range.start, Bias::Right)
                    ..snapshot.anchor_at(sp.byte_range.end, Bias::Left),
                style: style_id,
            })
        })
        .collect();

    // Drive the multi-layer SyntaxMap alongside the legacy
    // SyntaxState. We don't have an interpolation pass on the host
    // side yet (it would need anchored byte offsets), so each parse
    // produces a fresh SyntaxMap from scratch; the prior_syntax_map
    // is consumed but only its captured tree is reused via
    // SyntaxMap::reparse's internal `prior_injections` snapshot.
    let mut syntax_map = prior_syntax_map.take().unwrap_or_default();
    let _ = syntax_map.reparse(&new_rope, lang.clone(), cur_version);

    Some(ParseJobOutput {
        buffer_id,
        syntax: SyntaxState {
            tree,
            version: cur_version,
            rope_snapshot: new_rope,
            injection_trees: extracted.injection_trees,
        },
        syntax_map,
        tokens,
    })
}

/// Background parse worker. Owns all inputs by value so the future is `Send`
/// and can run on any executor thread.
pub(crate) async fn parse_buffer_async(
    buffer_id: BufferId,
    snapshot: TextBufferSnapshot,
    lang: Arc<Language>,
    mut prior: Option<SyntaxState>,
    mut prior_syntax_map: Option<stoat_language::SyntaxMap>,
    styles: SyntaxStyles,
) -> Option<ParseJobOutput> {
    parse_buffer_step(
        buffer_id,
        snapshot,
        &lang,
        &mut prior,
        &mut prior_syntax_map,
        &styles,
        None,
    )
}

pub(crate) fn arg_as_str(arg: &ResolvedArg) -> Option<String> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(s.clone()),
        stoat_config::Value::Ident(s) => Some(s.clone()),
        _ => None,
    }
}

fn arg_to_param_value(arg: &ResolvedArg) -> Option<stoat_action::ParamValue> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Ident(s) => Some(stoat_action::ParamValue::String(s.clone())),
        stoat_config::Value::Number(n) => Some(stoat_action::ParamValue::Number(*n)),
        stoat_config::Value::Bool(b) => Some(stoat_action::ParamValue::Bool(*b)),
        _ => None,
    }
}

const PRIMARY_MODES: &[&str] = &["normal", "insert", "run"];

fn action_display_desc(action: &ResolvedAction) -> String {
    if action.name == "SetMode" {
        let target = action.args.first().and_then(arg_as_str).unwrap_or_default();
        return format!("{target} mode");
    }
    stoat_action::registry::lookup(&action.name)
        .map(|e| e.def.short_desc().to_string())
        .unwrap_or_else(|| action.name.clone())
}

fn resolve_action(name: &str, args: &[ResolvedArg]) -> Option<Box<dyn Action>> {
    let entry = stoat_action::registry::lookup(name)?;
    let mut params = Vec::with_capacity(args.len());
    for arg in args {
        match arg_to_param_value(arg) {
            Some(value) => params.push(value),
            None => {
                tracing::warn!("action `{name}`: cannot convert arg {:?}", arg.value);
                return None;
            },
        }
    }
    match (entry.create)(&params) {
        Ok(action) => Some(action),
        Err(e) => {
            tracing::warn!("action `{name}`: {e}");
            None
        },
    }
}

fn render_mini_help(mode: &str, bindings: &[(&str, String)], area: Rect, buf: &mut Buffer) {
    if bindings.is_empty() || area.width < 10 || area.height < 4 {
        return;
    }

    let key_width = bindings.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let action_width = bindings.iter().map(|(_, a)| a.len()).max().unwrap_or(0);
    let gap = 3;
    let inner_width = key_width + gap + action_width;
    let border_pad = 2;
    let title_width = mode.len() + 4;
    let content_width = inner_width.max(title_width);
    let box_width = (content_width + border_pad) as u16;
    let box_height = (bindings.len() + border_pad) as u16;

    if box_width > area.width || box_height > area.height {
        return;
    }

    let x = area.x + area.width.saturating_sub(box_width);
    let y = area.y + area.height.saturating_sub(box_height);
    let help_area = Rect::new(x, y, box_width, box_height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(format!(" {mode} "))
        .title_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(help_area);
    block.render(help_area, buf);

    let key_style = Style::default().fg(Color::Cyan);
    let action_style = Style::default().fg(Color::White);

    for (i, (key, action)) in bindings.iter().enumerate() {
        let row = inner.y + i as u16;
        if row >= inner.y + inner.height {
            break;
        }
        let padded_key = format!("{key:>width$}", width = key_width);
        let line = format!("{padded_key}   {action}");

        for (j, ch) in line.chars().enumerate() {
            let col = inner.x + j as u16;
            if col >= inner.x + inner.width {
                break;
            }
            let style = if j < key_width {
                key_style
            } else {
                action_style
            };
            buf[(col, row)].set_char(ch).set_style(style);
        }
    }
}

fn render_command_palette(palette: &CommandPalette, area: Rect, buf: &mut Buffer) {
    match palette.phase() {
        crate::command_palette::PalettePhase::Filter {
            input,
            filtered,
            selected,
        } => render_palette_filter(input, filtered, *selected, area, buf),
        crate::command_palette::PalettePhase::CollectArgs {
            entry,
            collected,
            current,
            input,
            error,
        } => render_palette_collect_args(
            entry,
            collected,
            *current,
            input,
            error.as_deref(),
            area,
            buf,
        ),
    }
}

fn render_palette_filter(
    input: &str,
    filtered: &[&'static stoat_action::registry::RegistryEntry],
    selected: usize,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 30 || area.height < 10 {
        return;
    }

    let box_width = 80u16.min(area.width.saturating_sub(4));
    if box_width < 20 {
        return;
    }
    let inner_width = box_width.saturating_sub(2) as usize;
    let max_rows = 10u16;
    let row_count = (filtered.len() as u16).min(max_rows).max(1);

    let doc_lines: Vec<String> = filtered
        .get(selected)
        .map(|e| wrap_text(e.def.long_desc(), inner_width))
        .unwrap_or_default();
    let doc_height = doc_lines.len() as u16;
    let doc_section: u16 = if doc_height == 0 { 0 } else { doc_height + 1 };

    let box_height = 1 + 1 + 1 + row_count + doc_section + 1;
    if box_height > area.height {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let palette_area = Rect::new(x, y, box_width, box_height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(" command palette ")
        .title_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(palette_area);
    block.render(palette_area, buf);

    let prompt_style = Style::default().fg(Color::Yellow);
    let input_style = Style::default().fg(Color::White);
    let row_style = Style::default().fg(Color::White);
    let selected_style = Style::default().fg(Color::Black).bg(Color::Cyan);
    let desc_style = Style::default().fg(Color::DarkGray);
    let cursor_style = Style::default().fg(Color::Black).bg(Color::White);

    let input_row = inner.y;
    write_str(buf, inner.x, input_row, ":", prompt_style);
    write_str(buf, inner.x + 2, input_row, input, input_style);
    let cursor_col = inner.x + 2 + input.chars().count() as u16;
    if cursor_col < inner.x + inner.width {
        buf[(cursor_col, input_row)]
            .set_char(' ')
            .set_style(cursor_style);
    }

    let separator_row = inner.y + 1;
    for col in inner.x..inner.x + inner.width {
        buf[(col, separator_row)]
            .set_char('─')
            .set_style(Style::default().fg(Color::DarkGray));
    }

    let list_top = inner.y + 2;
    let name_col_width: usize = filtered
        .iter()
        .take(max_rows as usize)
        .map(|e| e.def.name().len())
        .max()
        .unwrap_or(0);

    for (i, entry) in filtered.iter().take(max_rows as usize).enumerate() {
        let row = list_top + i as u16;
        let is_selected = i == selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };

        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(style);
        }

        let name = entry.def.name();
        write_str(buf, inner.x + 1, row, name, style);
        let desc_col = inner.x + 1 + name_col_width as u16 + 2;
        if desc_col < inner.x + inner.width {
            let desc_style = if is_selected { style } else { desc_style };
            write_str(buf, desc_col, row, entry.def.short_desc(), desc_style);
        }
    }

    if doc_section > 0 {
        let doc_separator_row = list_top + row_count;
        for col in inner.x..inner.x + inner.width {
            buf[(col, doc_separator_row)]
                .set_char('─')
                .set_style(Style::default().fg(Color::DarkGray));
        }
        let doc_top = doc_separator_row + 1;
        for (i, line) in doc_lines.iter().enumerate() {
            write_str(
                buf,
                inner.x,
                doc_top + i as u16,
                line,
                Style::default().fg(Color::Gray),
            );
        }
    }
}

fn render_palette_collect_args(
    entry: &'static stoat_action::registry::RegistryEntry,
    collected: &[stoat_action::ParamValue],
    current: usize,
    input: &str,
    error: Option<&str>,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 30 || area.height < 10 {
        return;
    }

    let box_width = 80u16.min(area.width.saturating_sub(4));
    if box_width < 20 {
        return;
    }
    let inner_width = box_width.saturating_sub(2) as usize;

    let params = entry.def.params();
    let current_param = &params[current];
    let body_lines = wrap_text(current_param.description, inner_width);
    let body_height = body_lines.len() as u16;
    // header line + body lines
    let doc_height = 1 + body_height;

    let collected_lines = collected.len() as u16;
    let error_lines: u16 = if error.is_some() { 1 } else { 0 };
    // chrome: top + collected + input + (error?) + separator + doc + bottom
    let box_height = 1 + collected_lines + 1 + error_lines + 1 + doc_height + 1;
    if box_height > area.height {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let palette_area = Rect::new(x, y, box_width, box_height);

    let title = format!(" {} ", entry.def.name());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(title)
        .title_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(palette_area);
    block.render(palette_area, buf);

    let label_style = Style::default().fg(Color::Yellow);
    let value_style = Style::default().fg(Color::White);
    let cursor_style = Style::default().fg(Color::Black).bg(Color::White);
    let error_style = Style::default().fg(Color::Red);
    let muted_style = Style::default().fg(Color::DarkGray);

    let mut row = inner.y;

    for (i, value) in collected.iter().enumerate() {
        let label = format!("{}: ", params[i].name);
        write_str(buf, inner.x, row, &label, muted_style);
        let value_col = inner.x + label.chars().count() as u16;
        write_str(buf, value_col, row, &format_param_value(value), muted_style);
        row += 1;
    }

    let label = format!("{}: ", current_param.name);
    write_str(buf, inner.x, row, &label, label_style);
    let value_col = inner.x + label.chars().count() as u16;
    write_str(buf, value_col, row, input, value_style);
    let cursor_col = value_col + input.chars().count() as u16;
    if cursor_col < inner.x + inner.width {
        buf[(cursor_col, row)].set_char(' ').set_style(cursor_style);
    }
    row += 1;

    if let Some(msg) = error {
        write_str(buf, inner.x, row, msg, error_style);
        row += 1;
    }

    let separator_row = row;
    for col in inner.x..inner.x + inner.width {
        buf[(col, separator_row)]
            .set_char('─')
            .set_style(muted_style);
    }
    let doc_top = separator_row + 1;

    let header = format!(
        "{} ({}{})",
        current_param.name,
        current_param.kind,
        if current_param.required {
            ", required"
        } else {
            ""
        },
    );
    write_str(buf, inner.x, doc_top, &header, muted_style);

    let body_top = doc_top + 1;
    for (i, line) in body_lines.iter().enumerate() {
        write_str(
            buf,
            inner.x,
            body_top + i as u16,
            line,
            Style::default().fg(Color::Gray),
        );
    }
}

fn format_param_value(v: &stoat_action::ParamValue) -> String {
    match v {
        stoat_action::ParamValue::String(s) => s.clone(),
        stoat_action::ParamValue::Number(n) => n.to_string(),
        stoat_action::ParamValue::Bool(b) => b.to_string(),
    }
}

fn write_str(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) {
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        if y >= buf.area.y + buf.area.height {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn render_badges(badges: &BadgeTray, area: Rect, buf: &mut Buffer) {
    if badges.is_empty() {
        return;
    }

    for anchor in Anchor::ALL {
        let tray = badges.tray(anchor);
        let visible: Vec<_> = badges
            .at_anchor(anchor)
            .take(tray.max_visible as usize)
            .collect();
        if visible.is_empty() {
            continue;
        }

        let (mut x, mut y) = anchor_origin(anchor, area);
        let grows_left = matches!(
            anchor,
            Anchor::TopRight | Anchor::MidRight | Anchor::BottomRight
        );
        let grows_up = matches!(
            anchor,
            Anchor::BottomLeft | Anchor::BottomCenter | Anchor::BottomRight
        );

        for (_, badge) in &visible {
            let text = match &badge.detail {
                Some(d) => format!("[{} {}]", badge.label, d),
                None => format!("[{}]", badge.label),
            };
            let width = (text.len() as u16).clamp(3, 8);
            let draw_x = if grows_left {
                x.saturating_sub(width)
            } else {
                x
            };

            let style = badge_style(badge.state);
            write_str(buf, draw_x, y, &text, style);

            match tray.stack {
                StackDirection::Horizontal => {
                    if grows_left {
                        x = x.saturating_sub(width + 1);
                    } else {
                        x += width + 1;
                    }
                },
                StackDirection::Vertical => {
                    if grows_up {
                        y = y.saturating_sub(1);
                    } else {
                        y += 1;
                    }
                },
            }
        }
    }
}

fn anchor_origin(anchor: Anchor, area: Rect) -> (u16, u16) {
    let x = match anchor {
        Anchor::TopLeft | Anchor::MidLeft | Anchor::BottomLeft => area.x,
        Anchor::TopCenter | Anchor::BottomCenter => area.x + area.width / 2,
        Anchor::TopRight | Anchor::MidRight | Anchor::BottomRight => area.x + area.width,
    };
    let y = match anchor {
        Anchor::TopLeft | Anchor::TopCenter | Anchor::TopRight => area.y,
        Anchor::MidLeft | Anchor::MidRight => area.y + area.height / 2,
        Anchor::BottomLeft | Anchor::BottomCenter | Anchor::BottomRight => {
            area.y + area.height.saturating_sub(1)
        },
    };
    (x, y)
}

fn badge_style(state: BadgeState) -> Style {
    match state {
        BadgeState::Active => Style::default().fg(Color::Yellow),
        BadgeState::Complete => Style::default().fg(Color::Green),
        BadgeState::Error => Style::default().fg(Color::Red),
    }
}

fn render_pane(
    pane: &Pane,
    is_focused: bool,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    runs: &SlotMap<RunId, RunState>,
    buf: &mut Buffer,
) {
    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let text_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(pane.area);
    block.render(pane.area, buf);

    match &pane.view {
        View::Label(label) => {
            Paragraph::new(Text::styled(label.clone(), text_style))
                .centered()
                .render(inner, buf);
        },
        View::Editor(editor_id) => {
            if let Some(editor) = editors.get_mut(*editor_id) {
                render_editor(editor, inner, text_style, buf, is_focused);
            }
            let _ = buffers;
        },
        View::Run(run_id) => {
            if let Some(run_state) = runs.get(*run_id) {
                render_run_pane(run_state, inner, is_focused, buf);
            }
        },
    }
}

fn render_run_pane(run_state: &RunState, area: Rect, is_focused: bool, buf: &mut Buffer) {
    if area.height < 2 || area.width < 4 {
        return;
    }

    let input_row = area.y + area.height - 1;
    let output_height = area.height.saturating_sub(1);

    // Collect all output lines from blocks (command headers + grid rows)
    let mut output_lines: Vec<OutputLine<'_>> = Vec::new();
    for block in &run_state.blocks {
        output_lines.push(OutputLine::CommandHeader(block.command.as_str()));
        for row_idx in 0..block.grid.line_count() {
            output_lines.push(OutputLine::GridRow(&block.grid, row_idx));
        }
        if let Some(err) = &block.error {
            output_lines.push(OutputLine::Error(err.as_str()));
        }
        if block.finished {
            let status = block.exit_status.unwrap_or(-1);
            output_lines.push(OutputLine::Status(status));
        }
        output_lines.push(OutputLine::Blank);
    }

    // Render output lines (bottom-aligned: show most recent output)
    let total = output_lines.len();
    let visible = output_height as usize;
    let start = total.saturating_sub(visible + run_state.scroll_offset);
    for (i, line) in output_lines.iter().skip(start).take(visible).enumerate() {
        let y = area.y + i as u16;
        match line {
            OutputLine::CommandHeader(cmd) => {
                write_str(buf, area.x, y, "$ ", Style::default().fg(Color::Green));
                let max_w = (area.width as usize).saturating_sub(2);
                let display: String = cmd.chars().take(max_w).collect();
                write_str(
                    buf,
                    area.x + 2,
                    y,
                    &display,
                    Style::default().fg(Color::Green),
                );
            },
            OutputLine::GridRow(grid, row_idx) => {
                let row = grid.row(*row_idx);
                let w = (area.width as usize).min(grid.width() as usize);
                for (col, cell) in row.iter().enumerate().take(w) {
                    if cell.ch == ' '
                        && cell.fg.is_none()
                        && cell.bg.is_none()
                        && cell.modifiers.is_empty()
                    {
                        continue;
                    }
                    let mut style = Style::default();
                    if let Some(fg) = cell.fg {
                        style = style.fg(fg);
                    }
                    if let Some(bg) = cell.bg {
                        style = style.bg(bg);
                    }
                    style = style.add_modifier(cell.modifiers);
                    let x = area.x + col as u16;
                    if x < area.x + area.width {
                        buf[(x, y)].set_char(cell.ch).set_style(style);
                    }
                }
            },
            OutputLine::Error(msg) => {
                let max_w = area.width as usize;
                let display: String = msg.chars().take(max_w).collect();
                write_str(buf, area.x, y, &display, Style::default().fg(Color::Red));
            },
            OutputLine::Status(code) => {
                let label = if *code == 0 {
                    String::new()
                } else {
                    format!("[exit {}]", code)
                };
                if !label.is_empty() {
                    write_str(buf, area.x, y, &label, Style::default().fg(Color::DarkGray));
                }
            },
            OutputLine::Blank => {},
        }
    }

    // Render input line
    let prompt_style = Style::default().fg(Color::Cyan);
    let input_style = Style::default().fg(Color::White);
    let cursor_style = Style::default().fg(Color::Black).bg(Color::White);

    write_str(buf, area.x, input_row, "$ ", prompt_style);
    let input_text = run_state.input.as_str();
    let max_input = (area.width as usize).saturating_sub(2);
    let display_input: String = input_text.chars().take(max_input).collect();
    write_str(buf, area.x + 2, input_row, &display_input, input_style);

    if is_focused {
        let cursor_col = run_state.input.cursor_column();
        let cx = area.x + 2 + cursor_col as u16;
        if cx < area.x + area.width {
            buf[(cx, input_row)].set_style(cursor_style);
        }
    }
}

enum OutputLine<'a> {
    CommandHeader(&'a str),
    GridRow(&'a crate::run::VtermGrid, usize),
    Error(&'a str),
    Status(i32),
    Blank,
}

fn render_modal_run(run_state: &RunState, area: Rect, buf: &mut Buffer) {
    if area.width < 20 || area.height < 8 {
        return;
    }

    let box_width = (area.width * 7 / 10).min(area.width.saturating_sub(4));
    let box_height = (area.height * 8 / 10).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal_area = Rect::new(x, y, box_width, box_height);

    let title = {
        let raw = run_state
            .title
            .as_deref()
            .or_else(|| run_state.active_block().map(|b| b.command.as_str()))
            .unwrap_or("run");
        let max = (box_width as usize).saturating_sub(4);
        let display: String = raw.chars().take(max).collect();
        format!(" {display} ")
    };
    let border = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(title)
        .title_style(Style::default().fg(Color::Yellow));
    let inner = border.inner(modal_area);
    border.render(modal_area, buf);

    let Some(active) = run_state.active_block() else {
        return;
    };

    let grid = &active.grid;
    let visible_rows = (inner.height as usize).saturating_sub(1);
    let total = grid.line_count();
    let start = total.saturating_sub(visible_rows + run_state.scroll_offset);
    let w = (inner.width as usize).min(grid.width() as usize);

    for (i, row_idx) in (start..total).take(visible_rows).enumerate() {
        let y = inner.y + i as u16;
        let row = grid.row(row_idx);
        for (col, cell) in row.iter().enumerate().take(w) {
            if cell.ch == ' ' && cell.fg.is_none() && cell.bg.is_none() && cell.modifiers.is_empty()
            {
                continue;
            }
            let mut style = Style::default();
            if let Some(fg) = cell.fg {
                style = style.fg(fg);
            }
            if let Some(bg) = cell.bg {
                style = style.bg(bg);
            }
            style = style.add_modifier(cell.modifiers);
            let cx = inner.x + col as u16;
            if cx < inner.x + inner.width {
                buf[(cx, y)].set_char(cell.ch).set_style(style);
            }
        }
    }

    let status_row = inner.y + inner.height.saturating_sub(1);
    let status = if active.finished {
        let code = active.exit_status.unwrap_or(-1);
        if code == 0 {
            "done -- press Escape to dismiss".to_owned()
        } else {
            format!("exited {} -- press Escape to dismiss", code)
        }
    } else {
        "running...".to_owned()
    };
    let status_style = if active.finished {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Yellow)
    };
    write_str(buf, inner.x, status_row, &status, status_style);
}

fn render_editor(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    buf: &mut Buffer,
    is_focused: bool,
) {
    if editor.review_rows.is_some() {
        render_review(editor, inner, fallback_style, buf);
        return;
    }

    let snapshot = editor.display_map.snapshot();
    let visible_rows = inner.height as u32;
    let total_rows = snapshot.line_count();
    let end_row = (editor.scroll_row + visible_rows).min(total_rows);
    if end_row <= editor.scroll_row {
        return;
    }

    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;

    {
        let mut x = inner.x;
        let mut y = inner.y;
        'chunks: for chunk in snapshot.highlighted_chunks(editor.scroll_row..end_row) {
            let style = chunk
                .highlight_style
                .as_ref()
                .map(|hs| hs.to_ratatui_style())
                .unwrap_or(fallback_style);
            for ch in chunk.text.chars() {
                if ch == '\n' {
                    y += 1;
                    x = inner.x;
                    if y >= bottom {
                        break 'chunks;
                    }
                    continue;
                }
                if x >= right {
                    continue;
                }
                buf[(x, y)].set_char(ch).set_style(style);
                x += 1;
            }
        }
    }

    if !is_focused {
        return;
    }

    let buffer_snapshot = snapshot.buffer_snapshot();
    let selection_style = Style::default().bg(Color::DarkGray);
    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
    for selection in editor.selections.all_anchors() {
        let start_offset = buffer_snapshot.resolve_anchor(&selection.start);
        let end_offset = buffer_snapshot.resolve_anchor(&selection.end);
        let head_offset = buffer_snapshot.resolve_anchor(&selection.head());
        let rope = buffer_snapshot.rope();

        if start_offset != end_offset {
            let mut offset = start_offset;
            let mut chars = rope.chars_at(offset);
            while offset < end_offset {
                let Some(ch) = chars.next() else {
                    break;
                };
                if ch != '\n' && offset != head_offset {
                    let point = rope.offset_to_point(offset);
                    let display = snapshot.buffer_to_display(point);
                    if display.row >= editor.scroll_row && display.row < end_row {
                        let y = inner.y + (display.row - editor.scroll_row) as u16;
                        let x = inner.x + display.column as u16;
                        if x < right && y < bottom {
                            let cell = &mut buf[(x, y)];
                            cell.set_style(selection_style);
                        }
                    }
                }
                offset += ch.len_utf8();
            }
        }

        let head_point = buffer_snapshot.point_for_anchor(&selection.head());
        let display = snapshot.buffer_to_display(head_point);
        if display.row >= editor.scroll_row && display.row < end_row {
            let y = inner.y + (display.row - editor.scroll_row) as u16;
            let x = inner.x + display.column as u16;
            if x < right && y < bottom {
                let cell = &mut buf[(x, y)];
                let existing_char = cell.symbol().chars().next().unwrap_or(' ');
                let char_to_paint = if existing_char == '\0' {
                    ' '
                } else {
                    existing_char
                };
                cell.set_char(char_to_paint);
                cell.set_style(cursor_style);
            }
        }
    }
}

/// Side-by-side review renderer.
///
/// Layout per row:
/// ```text
///  NNN  <left content>           │  NNN  <right content>
/// ```
///
/// Changed tokens within a line are highlighted; the rest of the line
/// is rendered in the default style so only the structural diff is
/// visually emphasised (matching difftastic behaviour).
fn render_review(editor: &mut EditorState, inner: Rect, fallback_style: Style, buf: &mut Buffer) {
    let snapshot = editor.display_map.snapshot();
    let rows = match editor.review_rows.as_ref() {
        Some(r) => r,
        None => return,
    };
    let total_rows = snapshot.line_count();
    let visible = inner.height as u32;
    let end_row = (editor.scroll_row + visible).min(total_rows);
    if end_row <= editor.scroll_row {
        return;
    }

    let full_w = inner.width as usize;
    // Line-number gutter: " NNN " = 5 chars
    let num_w: usize = 5;
    // Separator column (1 char)
    let sep: usize = 1;
    // Each half = (full_w - sep) / 2, left half gets the extra column on odd widths.
    let half_w = (full_w.saturating_sub(sep)) / 2;
    let left_content_w = half_w.saturating_sub(num_w);
    let right_start = inner.x + half_w as u16 + sep as u16;
    let right_content_w = (full_w - half_w - sep).saturating_sub(num_w);

    let dim_style = Style::default().fg(Color::DarkGray);
    let del_hl = Style::default().fg(Color::Red);
    let add_hl = Style::default().fg(Color::Green);

    for display_row in editor.scroll_row..end_row {
        let y = inner.y + (display_row - editor.scroll_row) as u16;
        if y >= inner.y + inner.height {
            break;
        }

        // Render separator column
        let sep_x = inner.x + half_w as u16;
        if sep_x < inner.x + inner.width {
            buf[(sep_x, y)].set_char('│').set_style(dim_style);
        }

        match snapshot.classify_row(display_row) {
            BlockRowKind::BufferRow { buffer_row } => {
                let Some(row) = rows.get(buffer_row as usize) else {
                    continue;
                };
                match row {
                    ReviewRow::Context { left, right } => {
                        render_side_num(buf, inner.x, y, left.line_num, dim_style);
                        render_side_text(
                            buf,
                            inner.x + num_w as u16,
                            y,
                            &left.text,
                            left_content_w,
                            fallback_style,
                            &[],
                            fallback_style,
                        );
                        render_side_num(buf, right_start, y, right.line_num, dim_style);
                        render_side_text(
                            buf,
                            right_start + num_w as u16,
                            y,
                            &right.text,
                            right_content_w,
                            fallback_style,
                            &[],
                            fallback_style,
                        );
                    },
                    ReviewRow::Changed { left, right } => {
                        if let Some(l) = left {
                            render_side_num(buf, inner.x, y, l.line_num, dim_style);
                            render_side_text(
                                buf,
                                inner.x + num_w as u16,
                                y,
                                &l.text,
                                left_content_w,
                                fallback_style,
                                &l.change_spans,
                                del_hl,
                            );
                        } else {
                            render_empty_num(buf, inner.x, y, dim_style);
                        }
                        if let Some(r) = right {
                            render_side_num(buf, right_start, y, r.line_num, dim_style);
                            render_side_text(
                                buf,
                                right_start + num_w as u16,
                                y,
                                &r.text,
                                right_content_w,
                                fallback_style,
                                &r.change_spans,
                                add_hl,
                            );
                        } else {
                            render_empty_num(buf, right_start, y, dim_style);
                        }
                    },
                }
            },
            BlockRowKind::Block { block, line_index } => {
                let line = block.get_line(line_index);
                for (i, ch) in line.chars().enumerate() {
                    let x = inner.x + i as u16;
                    if x >= inner.x + inner.width {
                        break;
                    }
                    buf[(x, y)]
                        .set_char(ch)
                        .set_style(Style::default().fg(Color::Yellow));
                }
            },
        }
    }
}

fn render_side_num(buf: &mut Buffer, x: u16, y: u16, num: u32, style: Style) {
    let s = format!("{num:>4} ");
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

fn render_empty_num(buf: &mut Buffer, x: u16, y: u16, style: Style) {
    for i in 0..5u16 {
        let col = x + i;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        buf[(col, y)].set_char('.').set_style(style);
    }
}

/// Render text with sub-line change span highlighting. Characters within
/// any `spans` range get `highlight_style`; the rest get `base_style`.
#[allow(clippy::too_many_arguments)]
fn render_side_text(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    max_cols: usize,
    base_style: Style,
    spans: &[std::ops::Range<usize>],
    highlight_style: Style,
) {
    for (col, (byte_idx, ch)) in text.char_indices().enumerate() {
        if col >= max_cols {
            break;
        }
        let x = start_x + col as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }
        let in_span = spans
            .iter()
            .any(|s| byte_idx >= s.start && byte_idx < s.end);
        let style = if in_span { highlight_style } else { base_style };
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TextBuffer;
    use std::path::Path;

    /// When `parse_buffer_step` aborts on the deadline, the prior state
    /// passed via `&mut Option<_>` must remain populated so the caller
    /// can hand it to a follow-up parse without losing incrementality.
    #[test]
    fn parse_buffer_step_preserves_prior_on_deadline_abort() {
        let lang = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .unwrap();
        let styles = SyntaxStyles::standard();
        let buffer_id = BufferId::new(1);

        // Large enough that tree-sitter's progress callback fires at least
        // once during parsing. ~100k bytes of valid rust.
        let text = "fn a() {}\n".repeat(10_000);
        let mut buf = TextBuffer::with_text(buffer_id, &text);
        let snap1 = buf.snapshot.clone();

        // Successful parse with no deadline.
        let mut prior: Option<SyntaxState> = None;
        let mut prior_map: Option<stoat_language::SyntaxMap> = None;
        let out = parse_buffer_step(
            buffer_id,
            snap1,
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            None,
        )
        .expect("first parse should succeed");
        let initial_version = out.syntax.version;

        // Reinstall the parse output as the prior, then reparse against a
        // bumped snapshot with an already-expired deadline.
        let mut prior: Option<SyntaxState> = Some(out.syntax);
        let mut prior_map: Option<stoat_language::SyntaxMap> = Some(out.syntax_map);
        buf.edit(0..0, "// edit\n");
        let snap2 = buf.snapshot.clone();

        let result = parse_buffer_step(
            buffer_id,
            snap2.clone(),
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            Some(std::time::Instant::now()),
        );
        assert!(result.is_none(), "expected deadline abort to return None");
        let prior_state = prior
            .as_ref()
            .expect("prior must survive deadline abort, was consumed");
        assert_eq!(
            prior_state.version, initial_version,
            "prior version must be unchanged",
        );
        assert!(
            prior_map.is_some(),
            "prior_syntax_map must survive deadline abort",
        );

        // The surviving prior must still be usable for a successful reparse.
        // If the prior tree had been mutated by edit_tree on the failed
        // attempt, this call would double-stamp the input edit.
        let recovery = parse_buffer_step(
            buffer_id,
            snap2,
            &lang,
            &mut prior,
            &mut prior_map,
            &styles,
            None,
        )
        .expect("recovery parse should succeed");
        assert!(recovery.syntax.version > initial_version);
        assert!(prior.is_none(), "successful parse must consume the prior");
        assert!(prior_map.is_none());
    }
}
