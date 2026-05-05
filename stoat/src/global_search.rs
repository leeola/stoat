use crate::{host::FsHost, input_view::InputView};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::Regex;
use std::path::{Path, PathBuf};

/// Active state while the user is typing a global-search regex into
/// the input modal. Disposed by `global_search_submit` /
/// `global_search_cancel`.
pub(crate) struct GlobalSearchInputState {
    pub(crate) input: InputView,
    pub(crate) previous_mode: String,
}

/// One match site surfaced by [`perform_search`].
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub path: PathBuf,
    pub offset: usize,
    pub line: u32,
    pub column: u32,
    pub snippet: String,
}

/// Modal showing the result list after a regex submit.
pub struct GlobalSearchPicker {
    matches: Vec<SearchMatch>,
    selected: usize,
    query: String,
    pub previous_mode: String,
}

pub enum PickerOutcome {
    /// Re-render but keep the modal open.
    None,
    /// User cancelled; caller should drop the modal.
    Close,
    /// User selected match index `usize`; caller should open and jump.
    Select(usize),
}

const SNIPPET_MAX_CHARS: usize = 80;

impl GlobalSearchPicker {
    pub fn new(matches: Vec<SearchMatch>, query: String, previous_mode: String) -> Self {
        Self {
            matches,
            selected: 0,
            query,
            previous_mode,
        }
    }

    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn hint_bindings(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Enter", "open".to_string()),
            ("Esc", "cancel".to_string()),
            ("Ctrl-N", "next".to_string()),
            ("Ctrl-P", "prev".to_string()),
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerOutcome {
        match key.code {
            KeyCode::Esc => PickerOutcome::Close,
            KeyCode::Enter => match self.matches.get(self.selected) {
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
        if self.matches.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.matches.len() - 1) as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
    }
}

/// Walk every workspace file via `fs_host`, scan for `pattern`, and
/// return one [`SearchMatch`] per match site. Skips files whose
/// contents are not valid UTF-8. Returns the compiled regex error
/// when `pattern` does not parse.
pub fn perform_search(
    fs_host: &dyn FsHost,
    git_root: &Path,
    pattern: &str,
) -> Result<Vec<SearchMatch>, regex::Error> {
    let regex = Regex::new(pattern)?;
    let mut matches = Vec::new();
    for path in fs_host.walk_workspace_files(git_root) {
        let mut buf = Vec::new();
        if fs_host.read(&path, &mut buf).is_err() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&buf) else {
            continue;
        };
        for m in regex.find_iter(text) {
            let start = m.start();
            let (line, column) = offset_to_line_column(text, start);
            let snippet = line_snippet(text, start);
            matches.push(SearchMatch {
                path: path.clone(),
                offset: start,
                line,
                column,
                snippet,
            });
        }
    }
    Ok(matches)
}

/// Convert a byte offset into a `(line, column)` pair, both 1-based,
/// counting characters (not bytes) for the column. Out-of-range
/// `offset` clamps to the text length.
fn offset_to_line_column(text: &str, offset: usize) -> (u32, u32) {
    let clipped = offset.min(text.len());
    let preceding = &text[..clipped];
    let line = preceding.bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let line_start = preceding.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = text[line_start..clipped].chars().count() as u32 + 1;
    (line, column)
}

/// Extract the line containing `offset`, trim leading whitespace, and
/// cap at [`SNIPPET_MAX_CHARS`] for compact display.
fn line_snippet(text: &str, offset: usize) -> String {
    let clipped = offset.min(text.len());
    let line_start = text[..clipped].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[clipped..]
        .find('\n')
        .map(|i| clipped + i)
        .unwrap_or(text.len());
    let raw = &text[line_start..line_end];
    raw.trim_start().chars().take(SNIPPET_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{host::FakeFs, test_harness::keys};

    fn fake_with(files: &[(&str, &str)]) -> (FakeFs, PathBuf) {
        let fs = FakeFs::new();
        let root = PathBuf::from("/repo");
        fs.insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        (fs, root)
    }

    #[test]
    fn offset_to_line_column_first_line() {
        assert_eq!(offset_to_line_column("hello\nworld\n", 0), (1, 1));
        assert_eq!(offset_to_line_column("hello\nworld\n", 3), (1, 4));
    }

    #[test]
    fn offset_to_line_column_second_line() {
        assert_eq!(offset_to_line_column("hello\nworld\n", 6), (2, 1));
        assert_eq!(offset_to_line_column("hello\nworld\n", 8), (2, 3));
    }

    #[test]
    fn offset_to_line_column_counts_characters_for_column() {
        // "café" has c-a-f-é where é is 2 bytes (\xc3\xa9). Offset 5 is
        // after é, which is the 4th character on the line.
        assert_eq!(offset_to_line_column("café\n", 5), (1, 5));
    }

    #[test]
    fn line_snippet_returns_full_line_trimmed() {
        assert_eq!(line_snippet("    hello\nbeta\n", 4), "hello");
        assert_eq!(line_snippet("alpha\n  beta\n", 8), "beta");
    }

    #[test]
    fn perform_search_finds_matches_across_files() {
        let (fs, root) = fake_with(&[
            ("a.rs", "fn alpha() {}\nfn beta() {}\n"),
            ("b.rs", "fn alpha() {}\n"),
        ]);
        let matches = perform_search(&fs, &root, "alpha").unwrap();
        assert_eq!(matches.len(), 2);
        let paths: Vec<&str> = matches
            .iter()
            .map(|m| m.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(paths.contains(&"a.rs"));
        assert!(paths.contains(&"b.rs"));
        for m in &matches {
            assert_eq!(m.line, 1);
            assert_eq!(m.column, 4);
            assert_eq!(m.snippet, "fn alpha() {}");
        }
    }

    #[test]
    fn perform_search_with_invalid_regex_returns_err() {
        let (fs, root) = fake_with(&[("a.rs", "x")]);
        assert!(perform_search(&fs, &root, "[unclosed").is_err());
    }

    #[test]
    fn perform_search_skips_non_utf8_files() {
        let fs = FakeFs::new();
        let root = PathBuf::from("/repo");
        fs.insert_file(root.join("good.rs"), b"hello");
        fs.insert_file(root.join("bad.bin"), [0xff, 0xfe, 0xfd]);
        let matches = perform_search(&fs, &root, "hello").unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].path.ends_with("good.rs"));
    }

    #[test]
    fn perform_search_empty_pattern_compiles_and_matches_every_position() {
        let (fs, root) = fake_with(&[("a.rs", "ab")]);
        let matches = perform_search(&fs, &root, "").unwrap();
        assert!(!matches.is_empty(), "empty regex should match");
    }

    #[test]
    fn picker_enter_returns_select() {
        let mut picker = GlobalSearchPicker::new(
            vec![SearchMatch {
                path: PathBuf::from("/r/a.rs"),
                offset: 0,
                line: 1,
                column: 1,
                snippet: "hi".to_string(),
            }],
            "hi".into(),
            "normal".into(),
        );
        assert!(matches!(
            picker.handle_key(keys::key(KeyCode::Enter)),
            PickerOutcome::Select(0)
        ));
    }

    #[test]
    fn picker_esc_returns_close() {
        let mut picker = GlobalSearchPicker::new(Vec::new(), "x".into(), "normal".into());
        assert!(matches!(
            picker.handle_key(keys::key(KeyCode::Esc)),
            PickerOutcome::Close
        ));
    }

    #[test]
    fn picker_navigation_clamps_at_ends() {
        let mut picker = GlobalSearchPicker::new(
            (0..3)
                .map(|i| SearchMatch {
                    path: PathBuf::from("/r/a.rs"),
                    offset: i,
                    line: 1,
                    column: 1,
                    snippet: "x".to_string(),
                })
                .collect(),
            "x".into(),
            "normal".into(),
        );
        picker.handle_key(keys::key(KeyCode::Up));
        assert_eq!(picker.selected(), 0);
        picker.handle_key(keys::key(KeyCode::Down));
        picker.handle_key(keys::key(KeyCode::Down));
        picker.handle_key(keys::key(KeyCode::Down));
        assert_eq!(picker.selected(), 2);
    }

    #[test]
    fn snapshot_global_search_picker_listing() {
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/repo");
        h.fake_fs()
            .insert_file(root.join("a.rs"), b"fn alpha() { hello }\nfn beta() {}\n");
        h.fake_fs()
            .insert_file(root.join("sub/b.rs"), b"fn alpha_again() { hello }\n");
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenGlobalSearch);
        h.type_text("alpha");
        h.stoat
            .update(crossterm::event::Event::Key(keys::key(KeyCode::Enter)));
        h.assert_snapshot("global_search_picker_listing");
    }
}
