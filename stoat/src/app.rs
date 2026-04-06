use crate::{
    action_handlers,
    buffer_registry::BufferRegistry,
    editor_state::{EditorId, EditorState},
    keymap::{Keymap, KeymapState, ResolvedAction, ResolvedArg, StateValue},
    pane::{Axis, Pane, PaneTree, View},
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
use std::{io, path::Path};
use stoat_action::Action;
use stoat_scheduler::Executor;
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

        Self {
            size: Rect::default(),
            panes,
            mode: "normal".into(),
            executor,
            keymap,
            buffers,
            editors,
        }
    }

    /// Reads `path` from disk and assigns its contents to a pane.
    ///
    /// If the focused pane currently shows the untouched initial scratch buffer,
    /// the new editor replaces it in place. Otherwise the focused pane is split
    /// vertically and the new editor goes into the new pane.
    ///
    /// A missing file is treated as a new, empty buffer with the path attached
    /// (vim-style "edit a file that doesn't exist yet"). Other IO errors are
    /// returned to the caller.
    pub fn open_file(&mut self, path: &Path) -> io::Result<()> {
        let absolute = std::fs::canonicalize(path)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(path));
        let content = match std::fs::read_to_string(&absolute) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };

        let (buffer_id, buffer) = self.buffers.open(&absolute, &content);
        let new_editor_id =
            self.editors
                .insert(EditorState::new(buffer_id, buffer, self.executor.clone()));

        let focused = self.panes.focus();
        let replace = match self.panes.pane(focused).view {
            View::Editor(eid) => self
                .editors
                .get(eid)
                .is_some_and(|e| self.buffers.is_empty_scratch(e.buffer_id)),
            View::Label(_) => true,
        };

        if replace {
            let old = match self.panes.pane(focused).view {
                View::Editor(eid) => Some(eid),
                View::Label(_) => None,
            };
            self.panes.pane_mut(focused).view = View::Editor(new_editor_id);
            if let Some(old_id) = old {
                self.editors.remove(old_id);
            }
        } else {
            let new_pane_id = self.panes.split(Axis::Vertical);
            self.panes.pane_mut(new_pane_id).view = View::Editor(new_editor_id);
        }
        Ok(())
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
            if let Some(action) = resolve_action(&ra.name) {
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

    pub(crate) fn render(&mut self) -> Buffer {
        let mut buf = Buffer::empty(self.size);
        let focused = self.panes.focus();
        for (id, pane) in self.panes.split_panes() {
            let is_focused = id == focused;
            render_pane(pane, is_focused, &mut self.editors, &self.buffers, &mut buf);
        }
        if !PRIMARY_MODES.contains(&self.mode.as_str()) {
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

pub(crate) fn arg_as_str(arg: &ResolvedArg) -> Option<String> {
    match &arg.value {
        stoat_config::Value::String(s) => Some(s.clone()),
        stoat_config::Value::Ident(s) => Some(s.clone()),
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

fn resolve_action(name: &str) -> Option<Box<dyn Action>> {
    let entry = stoat_action::registry::lookup(name);
    if entry.is_none() {
        tracing::warn!("unknown action: {name}");
    }
    entry.map(|e| (e.create)())
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
                let snapshot = editor.display_map.snapshot();
                let visible_rows = inner.height as u32;
                let end_row = (editor.scroll_row + visible_rows).min(snapshot.line_count());
                for (i, line) in snapshot
                    .display_lines(editor.scroll_row..end_row)
                    .enumerate()
                {
                    let y = inner.y + i as u16;
                    for (j, ch) in line.chars().enumerate() {
                        let x = inner.x + j as u16;
                        if x >= inner.x + inner.width {
                            break;
                        }
                        buf[(x, y)].set_char(ch).set_style(text_style);
                    }
                }
            }
        },
    }
}
