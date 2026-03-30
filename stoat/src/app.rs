use crate::{
    action_handlers,
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
use std::io;
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
}

impl Stoat {
    #[cfg(test)]
    pub fn test() -> crate::test_harness::TestHarness {
        crate::test_harness::TestHarness::default()
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

        Self {
            size: Rect::default(),
            panes: PaneTree::new(Rect::default()),
            mode: "normal".into(),
            executor,
            keymap,
        }
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

    pub(crate) fn render(&self) -> Buffer {
        let mut buf = Buffer::empty(self.size);
        let focused = self.panes.focus();
        for (id, pane) in self.panes.split_panes() {
            let is_focused = id == focused;
            render_pane(pane, is_focused, &mut buf);
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

fn arg_as_str(arg: &ResolvedArg) -> Option<String> {
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

fn render_pane(pane: &Pane, is_focused: bool, buf: &mut Buffer) {
    let label = match &pane.view {
        View::Label(s) => s.as_str(),
    };

    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(pane.area);
    block.render(pane.area, buf);

    let text_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Paragraph::new(Text::styled(label, text_style))
        .centered()
        .render(inner, buf);
}
