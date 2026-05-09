use crate::claude_chat::{ChatMessage, ChatMessageContent, ChatRole};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Modal listing every user-message checkpoint in the active claude
/// chat. Each entry pairs the prompt label with the stash sha
/// captured at submit time. Selecting an entry routes the sha to
/// [`crate::host::GitRepo::restore_tree`] so the working tree rolls
/// back to the captured state.
pub struct CheckpointPicker {
    entries: Vec<CheckpointEntry>,
    selected: usize,
}

#[derive(Clone, Debug)]
pub struct CheckpointEntry {
    pub label: String,
    pub sha: String,
}

pub enum PickerOutcome {
    /// Re-render but keep the modal open.
    None,
    /// User cancelled; caller should drop the modal.
    Close,
    /// User selected entry index `usize`; caller should restore and
    /// drop the modal.
    Select(usize),
}

const LABEL_MAX_CHARS: usize = 80;

impl CheckpointPicker {
    /// Build a picker from a chat's `messages`, retaining only user
    /// messages whose `checkpoint_sha` is set, in their original
    /// chronological order. The `selected` index defaults to the
    /// most recent entry. Empty input produces an empty picker;
    /// callers should treat that as a no-op rather than open the
    /// modal.
    pub fn new(messages: &[ChatMessage]) -> Self {
        let entries: Vec<CheckpointEntry> = messages
            .iter()
            .filter_map(|msg| {
                if !matches!(msg.role, ChatRole::User) {
                    return None;
                }
                let sha = msg.checkpoint_sha.clone()?;
                let text = match &msg.content {
                    ChatMessageContent::Text(t) => t.as_str(),
                    _ => return None,
                };
                let trimmed = text.trim();
                let label: String = trimmed.chars().take(LABEL_MAX_CHARS).collect();
                Some(CheckpointEntry { label, sha })
            })
            .collect();
        let selected = entries.len().saturating_sub(1);
        Self { entries, selected }
    }

    pub fn entries(&self) -> &[CheckpointEntry] {
        &self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn hint_bindings(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Enter", "restore".to_string()),
            ("Esc", "cancel".to_string()),
            ("Ctrl-N", "next".to_string()),
            ("Ctrl-P", "prev".to_string()),
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        match key.code {
            KeyCode::Esc => PickerOutcome::Close,
            KeyCode::Enter => match self.entries.get(self.selected) {
                Some(_) => PickerOutcome::Select(self.selected),
                None => PickerOutcome::Close,
            },
            KeyCode::Up => {
                self.move_selection(-1);
                PickerOutcome::None
            },
            KeyCode::Down => {
                self.move_selection(1);
                PickerOutcome::None
            },
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-1);
                PickerOutcome::None
            },
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(1);
                PickerOutcome::None
            },
            _ => PickerOutcome::None,
        }
    }

    fn move_selection(&mut self, delta: i32) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.entries.len() - 1) as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_harness::keys;

    fn user(text: &str, sha: Option<&str>) -> ChatMessage {
        ChatMessage {
            role: ChatRole::User,
            content: ChatMessageContent::Text(text.to_string()),
            checkpoint_sha: sha.map(String::from),
        }
    }

    fn assistant_text(text: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Assistant,
            content: ChatMessageContent::Text(text.to_string()),
            checkpoint_sha: None,
        }
    }

    #[test]
    fn new_lists_user_messages_with_checkpoint() {
        let messages = vec![
            user("first", Some("sha1")),
            assistant_text("response"),
            user("second", Some("sha2")),
        ];
        let picker = CheckpointPicker::new(&messages);
        let entries = picker.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sha, "sha1");
        assert_eq!(entries[0].label, "first");
        assert_eq!(entries[1].sha, "sha2");
        assert_eq!(entries[1].label, "second");
    }

    #[test]
    fn new_skips_user_messages_without_sha() {
        let messages = vec![
            user("no checkpoint", None),
            user("with checkpoint", Some("s")),
        ];
        let picker = CheckpointPicker::new(&messages);
        let entries = picker.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "with checkpoint");
    }

    #[test]
    fn new_skips_assistant_messages_even_with_sha() {
        let mut a = assistant_text("oddly checkpointed");
        a.checkpoint_sha = Some("ignored".into());
        let messages = vec![a];
        let picker = CheckpointPicker::new(&messages);
        assert!(picker.entries().is_empty());
    }

    #[test]
    fn default_selection_is_latest() {
        let messages = vec![
            user("first", Some("s1")),
            user("second", Some("s2")),
            user("third", Some("s3")),
        ];
        let picker = CheckpointPicker::new(&messages);
        assert_eq!(picker.selected(), 2);
    }

    #[test]
    fn label_truncates_to_max_chars() {
        let long = "x".repeat(LABEL_MAX_CHARS + 50);
        let messages = vec![user(&long, Some("s"))];
        let picker = CheckpointPicker::new(&messages);
        assert_eq!(picker.entries()[0].label.chars().count(), LABEL_MAX_CHARS);
    }

    #[test]
    fn enter_returns_select() {
        let messages = vec![user("a", Some("s"))];
        let mut picker = CheckpointPicker::new(&messages);
        assert!(matches!(
            picker.handle_key(keys::key(KeyCode::Enter)),
            PickerOutcome::Select(0)
        ));
    }

    #[test]
    fn esc_returns_close() {
        let messages = vec![user("a", Some("s"))];
        let mut picker = CheckpointPicker::new(&messages);
        assert!(matches!(
            picker.handle_key(keys::key(KeyCode::Esc)),
            PickerOutcome::Close
        ));
    }

    #[test]
    fn down_and_up_clamp_at_ends() {
        let messages = vec![
            user("a", Some("s1")),
            user("b", Some("s2")),
            user("c", Some("s3")),
        ];
        let mut picker = CheckpointPicker::new(&messages);
        picker.handle_key(keys::key(KeyCode::Down));
        picker.handle_key(keys::key(KeyCode::Down));
        assert_eq!(picker.selected(), 2);
        picker.handle_key(keys::key(KeyCode::Up));
        picker.handle_key(keys::key(KeyCode::Up));
        picker.handle_key(keys::key(KeyCode::Up));
        assert_eq!(picker.selected(), 0);
    }
}
