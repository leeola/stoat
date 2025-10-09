//! File finder with async preview loading.
//!
//! Demonstrates the Context<Self> pattern - Stoat can spawn self-updating async tasks.

use std::path::Path;
use stoat_rope::TokenSnapshot;
use stoat_text::{Language, Parser};
use text::{Buffer, BufferId};

/// Preview data for file finder.
///
/// Enum supports progressive enhancement: show plain text immediately,
/// upgrade to syntax-highlighted version when ready.
#[derive(Clone)]
pub enum PreviewData {
    /// Plain text preview (fast, shown immediately)
    Plain(String),
    /// Syntax-highlighted preview (slower, shown after parsing)
    Highlighted { text: String, tokens: TokenSnapshot },
}

impl PreviewData {
    /// Get the text content of this preview
    pub fn text(&self) -> &str {
        match self {
            PreviewData::Plain(text) => text,
            PreviewData::Highlighted { text, .. } => text,
        }
    }

    /// Get the token snapshot if this is a highlighted preview
    pub fn tokens(&self) -> Option<&TokenSnapshot> {
        match self {
            PreviewData::Plain(_) => None,
            PreviewData::Highlighted { tokens, .. } => Some(tokens),
        }
    }
}

/// Load plain text preview without syntax highlighting.
///
/// Fast operation suitable for immediate display. Reads up to 100KB.
/// Uses `smol::unblock` to avoid blocking async executor.
pub async fn load_text_only(path: &Path) -> Option<String> {
    let path = path.to_path_buf();

    smol::unblock(move || {
        const MAX_BYTES: usize = 100 * 1024; // 100KB

        // Read only first MAX_BYTES
        let mut file = std::fs::File::open(&path).ok()?;
        let mut buffer = vec![0; MAX_BYTES];
        let bytes_read = std::io::Read::read(&mut file, &mut buffer).ok()?;
        buffer.truncate(bytes_read);

        // Check for binary content
        let check_size = buffer.len().min(1024);
        if buffer[..check_size].contains(&0) {
            return None; // Binary file
        }

        // Decode as UTF-8
        String::from_utf8(buffer).ok()
    })
    .await
}

/// Load syntax-highlighted file preview.
///
/// Reads file and parses for syntax highlighting. Both file I/O and parsing
/// run on thread pool via `smol::unblock` to avoid blocking executor.
pub async fn load_file_preview(path: &Path) -> Option<PreviewData> {
    let path = path.to_path_buf();

    // Phase 1: File I/O on thread pool
    let (text, language) = smol::unblock(move || {
        const MAX_BYTES: usize = 100 * 1024;

        let mut file = std::fs::File::open(&path).ok()?;
        let mut buffer = vec![0; MAX_BYTES];
        let bytes_read = std::io::Read::read(&mut file, &mut buffer).ok()?;
        buffer.truncate(bytes_read);

        let check_size = buffer.len().min(1024);
        if buffer[..check_size].contains(&0) {
            return None;
        }

        let text = String::from_utf8(buffer).ok()?;
        let language = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(Language::from_extension)
            .unwrap_or(Language::PlainText);

        Some((text, language))
    })
    .await?;

    // Phase 2: CPU-intensive parsing on thread pool
    smol::unblock(move || {
        let mut parser = Parser::new(language).ok()?;
        let buffer = Buffer::new(0, BufferId::new(1).ok()?, text.clone());
        let snapshot = buffer.snapshot();
        let parsed_tokens = parser.parse(&text, &snapshot).ok()?;

        // Build token snapshot
        let mut token_map = stoat_rope::TokenMap::new(&snapshot);
        token_map.replace_tokens(parsed_tokens, &snapshot);
        let tokens = token_map.snapshot();

        Some(PreviewData::Highlighted { text, tokens })
    })
    .await
}
