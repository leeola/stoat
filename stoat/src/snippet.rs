//! User-defined snippet store.
//!
//! Snippets live in `$XDG_CONFIG_HOME/stoat/snippets/<language>.toml`,
//! one file per language. Each file is a table of snippet entries keyed
//! by trigger prefix:
//!
//! ```toml
//! [snippet.fn]
//! body = "fn $1() { $0 }"
//! description = "function definition"
//! ```
//!
//! `body` is LSP snippet syntax; it is stored verbatim here and parsed
//! by [`crate::completion::snippet::parse`] when a snippet is inserted.

use etcetera::{base_strategy::Xdg, BaseStrategy};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};

/// A user-defined snippet: its trigger `prefix`, the LSP-syntax `body`
/// inserted on acceptance, and an optional human-readable `description`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSnippet {
    pub prefix: String,
    pub body: String,
    pub description: Option<String>,
}

/// Load every user snippet, keyed by language name (the snippet file's
/// stem). Reads `$XDG_CONFIG_HOME/stoat/snippets/<language>.toml`,
/// blocking on filesystem IO. An absent directory yields an empty map;
/// an unreadable or malformed `<language>.toml` is logged and skipped so
/// one bad file never suppresses the rest. Within each language the
/// snippets are ordered by prefix.
pub fn load_user_snippets() -> HashMap<String, Vec<UserSnippet>> {
    match snippets_dir() {
        Some(dir) => load_snippets_dir(&dir),
        None => HashMap::new(),
    }
}

fn snippets_dir() -> Option<PathBuf> {
    Xdg::new()
        .ok()
        .map(|xdg| xdg.config_dir().join("stoat").join("snippets"))
}

fn load_snippets_dir(dir: &Path) -> HashMap<String, Vec<UserSnippet>> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let Some(language) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };

        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                tracing::warn!(target: "stoat::snippet", ?err, path = %path.display(), "failed to read snippet file");
                continue;
            },
        };
        match parse_snippets_toml(&content) {
            Ok(snippets) => {
                out.insert(language.to_string(), snippets);
            },
            Err(err) => {
                tracing::warn!(target: "stoat::snippet", ?err, path = %path.display(), "failed to parse snippet file");
            },
        }
    }
    out
}

fn parse_snippets_toml(content: &str) -> Result<Vec<UserSnippet>, toml::de::Error> {
    let file: SnippetsFile = toml::from_str(content)?;
    Ok(file
        .snippet
        .into_iter()
        .map(|(prefix, entry)| UserSnippet {
            prefix,
            body: entry.body,
            description: entry.description,
        })
        .collect())
}

#[derive(Deserialize)]
struct SnippetsFile {
    #[serde(default)]
    snippet: BTreeMap<String, SnippetEntry>,
}

#[derive(Deserialize)]
struct SnippetEntry {
    body: String,
    description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn snippet(prefix: &str, body: &str, description: Option<&str>) -> UserSnippet {
        UserSnippet {
            prefix: prefix.to_string(),
            body: body.to_string(),
            description: description.map(str::to_string),
        }
    }

    #[test]
    fn parses_entries_ordered_by_prefix() {
        let toml = r#"
[snippet.let]
body = "let $1 = $0;"

[snippet.fn]
body = "fn $1() { $0 }"
description = "function definition"
"#;
        assert_eq!(
            parse_snippets_toml(toml).unwrap(),
            vec![
                snippet("fn", "fn $1() { $0 }", Some("function definition")),
                snippet("let", "let $1 = $0;", None),
            ]
        );
    }

    #[test]
    fn empty_file_yields_no_snippets() {
        assert_eq!(parse_snippets_toml("").unwrap(), Vec::new());
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(parse_snippets_toml("= = =").is_err());
    }

    #[test]
    fn entry_without_body_is_an_error() {
        assert!(parse_snippets_toml("[snippet.fn]\ndescription = \"x\"").is_err());
    }

    #[test]
    fn loads_each_language_file_keyed_by_stem() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("rust.toml"), "[snippet.fn]\nbody = \"fn\"").unwrap();
        fs::write(
            dir.path().join("python.toml"),
            "[snippet.def]\nbody = \"def\"",
        )
        .unwrap();
        fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let loaded = load_snippets_dir(dir.path());
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["rust"], vec![snippet("fn", "fn", None)]);
        assert_eq!(loaded["python"], vec![snippet("def", "def", None)]);
    }

    #[test]
    fn absent_directory_yields_empty_map() {
        let dir = TempDir::new().unwrap();
        assert!(load_snippets_dir(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn malformed_file_is_skipped_others_load() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("rust.toml"), "[snippet.fn]\nbody = \"fn\"").unwrap();
        fs::write(dir.path().join("broken.toml"), "= = =").unwrap();

        let loaded = load_snippets_dir(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["rust"], vec![snippet("fn", "fn", None)]);
    }
}
