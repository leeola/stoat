use crate::{
    action_handlers,
    buffer::{BufferId, TextBufferSnapshot},
    buffer_registry::BufferRegistry,
    command_palette::{CommandPalette, PaletteOutcome},
    display_map::{highlights::SemanticTokenHighlight, syntax_theme::SyntaxStyles},
    editor_state::{EditorId, EditorState},
    keymap::{Keymap, KeymapState, ResolvedAction, ResolvedArg, StateValue},
    pane::{Pane, PaneTree, View},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::Text,
    widgets::{Block, Borders, Paragraph, Widget},
};
use slotmap::SlotMap;
use std::{
    collections::HashMap,
    future::Future,
    io,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use stoat_action::{Action, OpenFile, OpenReview};
use stoat_language::{self as language, Language, LanguageRegistry, SyntaxState};
use stoat_scheduler::{Executor, Task};
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
    pub panes: PaneTree,
    pub mode: String,
    pub executor: Executor,
    keymap: Keymap,
    pub(crate) buffers: BufferRegistry,
    pub(crate) editors: SlotMap<EditorId, EditorState>,
    pub(crate) command_palette: Option<CommandPalette>,
    pub(crate) language_registry: Arc<LanguageRegistry>,
    pub(crate) syntax_styles: SyntaxStyles,
    parse_jobs: HashMap<BufferId, ParseJob>,
}

/// In-flight tree-sitter parse for a buffer.
///
/// `target_version` is the buffer version the task is parsing. While a job is
/// in flight for a given buffer, no new job is spawned for the same version;
/// once it completes, [`Stoat::drive_parse_jobs`] schedules a follow-up parse
/// if the buffer has advanced.
struct ParseJob {
    target_version: u64,
    task: Task<Option<ParseJobOutput>>,
}

/// Result of a successful background parse, ready to be installed on the
/// foreground thread.
struct ParseJobOutput {
    buffer_id: BufferId,
    syntax: SyntaxState,
    /// Multi-layer parse state from [`stoat_language::SyntaxMap::reparse`].
    /// Populated alongside [`Self::syntax`] so the legacy single-tree
    /// highlight path and the capture-merging path can run side by side
    /// while consumers migrate.
    syntax_map: stoat_language::SyntaxMap,
    tokens: Arc<[SemanticTokenHighlight]>,
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

    pub fn new(executor: Executor) -> Self {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        if !errors.is_empty() {
            tracing::error!(
                "default keymap parse errors: {}",
                stoat_config::format_errors(DEFAULT_KEYMAP, &errors)
            );
        }
        let keymap = config
            .map(|c| Keymap::compile(&c))
            .unwrap_or_else(|| Keymap::compile(&stoat_config::Config { blocks: vec![] }));

        let mut buffers = BufferRegistry::new();
        let (buffer_id, buffer) = buffers.new_scratch();
        let mut editors = SlotMap::with_key();
        let editor_id = editors.insert(EditorState::new(buffer_id, buffer, executor.clone()));
        let mut panes = PaneTree::new(Rect::default());
        panes.pane_mut(panes.focus()).view = View::Editor(editor_id);

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

        Self {
            size: Rect::default(),
            panes,
            mode: "normal".into(),
            executor,
            keymap,
            buffers,
            editors,
            command_palette: None,
            language_registry,
            syntax_styles,
            parse_jobs: HashMap::new(),
        }
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
        while let Some(event) = events.recv().await {
            match self.update(event) {
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
                self.panes.resize(self.size);
                UpdateEffect::Redraw
            },
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            _ => UpdateEffect::None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> UpdateEffect {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return UpdateEffect::Quit;
        }

        if self.command_palette.is_some() {
            return self.dispatch_palette_key(key);
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
        // 1. Poll completed tasks and harvest their outputs.
        let waker = futures::task::noop_waker();
        let mut completed: Vec<ParseJobOutput> = Vec::new();
        self.parse_jobs.retain(|_, job| {
            let mut cx = Context::from_waker(&waker);
            match Pin::new(&mut job.task).poll(&mut cx) {
                Poll::Ready(Some(out)) => {
                    completed.push(out);
                    false
                },
                Poll::Ready(None) => false,
                Poll::Pending => true,
            }
        });
        for out in completed {
            self.buffers.store_syntax(out.buffer_id, out.syntax);
            self.buffers.store_syntax_map(out.buffer_id, out.syntax_map);
            for editor in self.editors.values_mut() {
                if editor.buffer_id == out.buffer_id {
                    editor.display_map.set_semantic_token_highlights(
                        out.buffer_id,
                        out.tokens.clone(),
                        self.syntax_styles.interner.clone(),
                    );
                }
            }
        }

        // 2. Spawn new jobs for visible buffers needing parse.
        let mut visible: Vec<BufferId> = Vec::new();
        for (_, pane) in self.panes.split_panes() {
            if let View::Editor(editor_id) = pane.view {
                if let Some(editor) = self.editors.get(editor_id) {
                    if !visible.contains(&editor.buffer_id) {
                        visible.push(editor.buffer_id);
                    }
                }
            }
        }

        for buffer_id in visible {
            let Some(lang) = self.buffers.language_for(buffer_id) else {
                continue;
            };
            let Some(shared) = self.buffers.get(buffer_id) else {
                continue;
            };
            let snapshot = {
                let guard = shared.read().expect("buffer poisoned");
                guard.snapshot.clone()
            };
            let cur_version = snapshot.version;

            if self.buffers.syntax_version(buffer_id) == Some(cur_version) {
                continue;
            }
            if let Some(job) = self.parse_jobs.get(&buffer_id) {
                if job.target_version == cur_version {
                    continue;
                }
                // An older job is still in flight; let it finish before
                // queuing a new one. Skip for now -- next call will retry.
                continue;
            }

            // Take prior state out so the parse pipeline owns it. On a sync
            // miss the locals stay populated and flow into the background
            // spawn; on parse failure the next call falls back to a full
            // reparse.
            let mut prior = self.buffers.take_syntax(buffer_id);
            let mut prior_map = self.buffers.take_syntax_map(buffer_id);

            // Sync fast path: small edits typically finish in well under a
            // millisecond. Try to parse inline before paying the executor
            // round-trip; if the deadline is exceeded the parser aborts and
            // we fall through to the background spawn with `prior` /
            // `prior_map` still intact.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1);
            if let Some(out) = parse_buffer_step(
                buffer_id,
                snapshot.clone(),
                &lang,
                &mut prior,
                &mut prior_map,
                &self.syntax_styles,
                Some(deadline),
            ) {
                self.buffers.store_syntax(out.buffer_id, out.syntax);
                self.buffers.store_syntax_map(out.buffer_id, out.syntax_map);
                for editor in self.editors.values_mut() {
                    if editor.buffer_id == out.buffer_id {
                        editor.display_map.set_semantic_token_highlights(
                            out.buffer_id,
                            out.tokens.clone(),
                            self.syntax_styles.interner.clone(),
                        );
                    }
                }
                continue;
            }

            // Sync attempt aborted. `prior` / `prior_map` were not consumed
            // because the failure short-circuited before parse_buffer_step
            // reached its take points; hand them to the background path so
            // it can still reparse incrementally.
            let styles = self.syntax_styles.clone();
            let task = self.executor.spawn(parse_buffer_async(
                buffer_id, snapshot, lang, prior, prior_map, styles,
            ));
            self.parse_jobs.insert(
                buffer_id,
                ParseJob {
                    target_version: cur_version,
                    task,
                },
            );
        }
    }

    pub(crate) fn render(&mut self) -> Buffer {
        self.drive_parse_jobs();
        let mut buf = Buffer::empty(self.size);
        let focused = self.panes.focus();
        for (id, pane) in self.panes.split_panes() {
            let is_focused = id == focused;
            render_pane(pane, is_focused, &mut self.editors, &self.buffers, &mut buf);
        }
        if let Some(palette) = &self.command_palette {
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

/// Synchronous core of the parse pipeline. When `deadline` is `Some`, the
/// host parse aborts if it would exceed it and the function returns `None`,
/// signalling that the caller should fall back to the background path.
/// `None` is also returned for ordinary parse failures (unsupported
/// language, etc.); the difference does not matter for the call sites.
fn parse_buffer_step(
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
async fn parse_buffer_async(
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

const PRIMARY_MODES: &[&str] = &["normal", "insert"];

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

fn render_pane(
    pane: &Pane,
    is_focused: bool,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
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
                render_editor(editor, inner, text_style, buf);
            }
            let _ = buffers;
        },
    }
}

fn render_editor(editor: &mut EditorState, inner: Rect, fallback_style: Style, buf: &mut Buffer) {
    let snapshot = editor.display_map.snapshot();
    let visible_rows = inner.height as u32;
    let total_rows = snapshot.line_count();
    let end_row = (editor.scroll_row + visible_rows).min(total_rows);
    if end_row <= editor.scroll_row {
        return;
    }

    let mut x = inner.x;
    let mut y = inner.y;
    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;

    for chunk in snapshot.highlighted_chunks(editor.scroll_row..end_row) {
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
                    return;
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
