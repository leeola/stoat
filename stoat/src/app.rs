use crate::{
    action_handlers,
    keymap::{Keymap, KeymapState, StateValue},
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
use stoat_action::{Action, Quit};
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
    keymap: Keymap,
}

impl Default for Stoat {
    fn default() -> Self {
        Self::new()
    }
}

impl Stoat {
    pub fn new() -> Self {
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

    fn update(&mut self, event: Event) -> UpdateEffect {
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
        // Ctrl-C is a hardcoded escape hatch
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return UpdateEffect::Quit;
        }

        let state = StoatKeymapState::new(&self.mode);
        let Some(actions) = self.keymap.lookup(&state, &key) else {
            return UpdateEffect::None;
        };

        let resolved: Vec<_> = actions
            .iter()
            .filter_map(|ra| resolve_action(&ra.name))
            .collect();

        let mut effect = UpdateEffect::None;
        for action in &resolved {
            let e = action_handlers::dispatch(self, &**action);
            match e {
                UpdateEffect::Quit => return UpdateEffect::Quit,
                UpdateEffect::Redraw => effect = UpdateEffect::Redraw,
                UpdateEffect::None => {},
            }
        }
        effect
    }

    fn render(&self) -> Buffer {
        let mut buf = Buffer::empty(self.size);
        let focused = self.panes.focus();
        for (id, pane) in self.panes.split_panes() {
            let is_focused = id == focused;
            render_pane(pane, is_focused, &mut buf);
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

fn resolve_action(name: &str) -> Option<Box<dyn Action>> {
    match name {
        "Quit" => Some(Box::new(Quit)),
        _ => {
            tracing::warn!("unknown action: {name}");
            None
        },
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
