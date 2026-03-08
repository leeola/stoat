use crate::claude::state::ChatMessage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct ConversationMeta {
    pub id: String,
    pub session_id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
    pub forked_from: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ConversationFile {
    meta: ConversationMeta,
    messages: Vec<ChatMessage>,
}

pub struct ConversationStore {
    dir: PathBuf,
}

impl ConversationStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn default_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("stoat/conversations")
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub fn save(
        &self,
        messages: &[ChatMessage],
        meta: &ConversationMeta,
    ) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(&self.dir)?;
        let file = ConversationFile {
            meta: meta.clone(),
            messages: messages.to_vec(),
        };
        let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        std::fs::write(self.path_for(&meta.id), json)?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<(ConversationMeta, Vec<ChatMessage>), std::io::Error> {
        let data = std::fs::read_to_string(self.path_for(id))?;
        let file: ConversationFile = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok((file.meta, file.messages))
    }

    pub fn list(&self) -> Result<Vec<ConversationMeta>, std::io::Error> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut metas = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(data) = std::fs::read_to_string(&path) {
                    if let Ok(file) = serde_json::from_str::<ConversationFile>(&data) {
                        metas.push(file.meta);
                    }
                }
            }
        }
        metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(metas)
    }

    pub fn delete(&self, id: &str) -> Result<(), std::io::Error> {
        let path = self.path_for(id);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn fork(
        &self,
        source_id: &str,
        fork_at_message_index: usize,
    ) -> Result<ConversationMeta, std::io::Error> {
        let (source_meta, messages) = self.load(source_id)?;
        let forked_messages: Vec<ChatMessage> =
            messages.into_iter().take(fork_at_message_index).collect();

        let title = format!("Fork of {}", source_meta.title);
        let first_char_count = title.chars().count().min(50);
        let truncated_title: String = title.chars().take(first_char_count).collect();

        let now = Utc::now();
        let new_meta = ConversationMeta {
            id: Uuid::new_v4().to_string(),
            session_id: String::new(),
            title: truncated_title,
            created_at: now,
            updated_at: now,
            message_count: forked_messages.len(),
            forked_from: Some(source_id.to_string()),
        };

        self.save(&forked_messages, &new_meta)?;
        Ok(new_meta)
    }
}

pub fn new_conversation_id() -> String {
    Uuid::new_v4().to_string()
}

pub fn auto_title(text: &str) -> String {
    let trimmed = text.trim();
    let first_char_count = trimmed.chars().count().min(50);
    trimmed.chars().take(first_char_count).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, ConversationStore) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = ConversationStore::new(dir.path().to_path_buf());
        (dir, store)
    }

    fn sample_meta(id: &str, title: &str) -> ConversationMeta {
        let now = Utc::now();
        ConversationMeta {
            id: id.to_string(),
            session_id: "sess1".to_string(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
            forked_from: None,
        }
    }

    fn sample_messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage::User {
                text: "hello".into(),
                session_id: "s1".into(),
            },
            ChatMessage::Assistant {
                blocks: vec![crate::claude::state::AssistantBlock::Text {
                    text: "hi there".into(),
                }],
                session_id: "s1".into(),
            },
        ]
    }

    #[test]
    fn save_and_load() {
        let (_dir, store) = temp_store();
        let mut meta = sample_meta("c1", "Test Chat");
        let msgs = sample_messages();
        meta.message_count = msgs.len();
        store.save(&msgs, &meta).expect("save failed");

        let (loaded_meta, loaded_msgs) = store.load("c1").expect("load failed");
        assert_eq!(loaded_meta.id, "c1");
        assert_eq!(loaded_meta.title, "Test Chat");
        assert_eq!(loaded_msgs.len(), 2);
    }

    #[test]
    fn list_conversations() {
        let (_dir, store) = temp_store();
        store.save(&[], &sample_meta("c1", "First")).expect("save");
        store.save(&[], &sample_meta("c2", "Second")).expect("save");

        let metas = store.list().expect("list");
        assert_eq!(metas.len(), 2);
    }

    #[test]
    fn delete_conversation() {
        let (_dir, store) = temp_store();
        store.save(&[], &sample_meta("c1", "Bye")).expect("save");
        store.delete("c1").expect("delete");
        assert!(store.load("c1").is_err());
        assert_eq!(store.list().expect("list").len(), 0);
    }

    #[test]
    fn fork_conversation() {
        let (_dir, store) = temp_store();
        let msgs = sample_messages();
        let mut meta = sample_meta("c1", "Original");
        meta.message_count = msgs.len();
        store.save(&msgs, &meta).expect("save");

        let forked = store.fork("c1", 1).expect("fork");
        assert_eq!(forked.forked_from, Some("c1".to_string()));
        assert!(forked.title.starts_with("Fork of"));

        let (_, forked_msgs) = store.load(&forked.id).expect("load fork");
        assert_eq!(forked_msgs.len(), 1);
    }

    #[test]
    fn list_empty_dir() {
        let (_dir, store) = temp_store();
        let metas = store.list().expect("list");
        assert!(metas.is_empty());
    }

    #[test]
    fn auto_title_truncation() {
        let short = auto_title("hello");
        assert_eq!(short, "hello");

        let long = auto_title(&"a".repeat(100));
        assert_eq!(long.len(), 50);
    }
}
