//! Multi-location goto picker.
//!
//! Opened by [`crate::action_handlers::lsp::pump_lsp_jumps`] when a goto
//! request resolves to more than one location. Presents the candidates as
//! `path:line:col  target-line` rows over a preview of the file each names,
//! filtered by the prompt on top. Selecting a row jumps through the same apply
//! path a single-location goto uses.

use crate::{
    action_handlers,
    host::{FsHost, OffsetEncoding},
    lsp::util,
    picker::{PreviewSource, TargetPicker},
};
use lsp_types::Position;
use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
};
use stoat_text::{LineEnding, Rope};

/// The most entries that one run of tool output opens, so a search that
/// matches every line of a large tree keeps the picker responsive.
const MAX_LOCATIONS: usize = 5_000;

/// One resolved goto candidate. Carries the byte offset to jump to plus
/// the 1-based line/column and the target line's text for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocationEntry {
    pub(crate) path: PathBuf,
    pub(crate) offset: usize,
    pub(crate) line: u32,
    pub(crate) column: u32,
    pub(crate) text: String,
    /// The byte range of the whole declaration the server named for the
    /// target, docs included, which the jump frames in the pane.
    ///
    /// `None` when the server sent a bare location, which names no block.
    pub(crate) block: Option<Range<usize>>,
}

/// One location that a line of tool output names, before its file resolves
/// it to an offset.
///
/// A search tool and a compiler print a location as `path:line:col: text`.
/// The line and the column count from 1.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ParsedLocation {
    pub(crate) path: String,
    pub(crate) line: u32,
    pub(crate) column: u32,
    pub(crate) text: String,
}

/// Modal chooser over the candidates of a multi-location goto.
pub(crate) type LocationPicker = TargetPicker<LocationEntry>;

/// The location that one line of tool output names, or `None` for a line
/// that names no location.
///
/// A line reads `path:line:col: text`, `path:line: text`, or
/// `path:line:text`, or ends after its line number or its column. The path
/// ends at the first `:` after the first byte that a number follows. A `:` or
/// the end of the line follows that number, which is the line and starts at
/// 1.
///
/// A second number that a `:` or the end follows is the column, and 0 reads
/// as 1. A line that names no column takes the column 1. The text is the rest
/// of the line after the last number's `:`, trimmed.
pub(crate) fn parse_location_line(line: &str) -> Option<ParsedLocation> {
    let (colon, line_end) = line
        .bytes()
        .enumerate()
        .skip(1)
        .filter(|&(_, byte)| byte == b':')
        .find_map(|(at, _)| {
            let end = at + 1 + digits_at(&line[at + 1..]);
            let ends_number = end == line.len() || line.as_bytes()[end] == b':';
            (end > at + 1 && ends_number).then_some((at, end))
        })?;
    let number: u32 = line[colon + 1..line_end].parse().ok()?;
    if number == 0 {
        return None;
    }

    let rest = line.get(line_end + 1..).unwrap_or("");
    let column_end = digits_at(rest);
    let (column, text) =
        match column_end > 0 && (column_end == rest.len() || rest.as_bytes()[column_end] == b':') {
            true => (
                rest[..column_end].parse::<u32>().ok()?.max(1),
                rest.get(column_end + 1..).unwrap_or(""),
            ),
            false => (1, rest),
        };
    Some(ParsedLocation {
        path: line[..colon].to_owned(),
        line: number,
        column,
        text: text.trim().to_owned(),
    })
}

/// The entries that `parsed` names, each at its byte offset in its file's
/// text.
///
/// A relative path joins `root`. An open file resolves against its buffer's
/// rope in `open`, because the reader sees that text. Any other file reads
/// through `fs`, one read for each path, and a file that does not read drops
/// its entries. An entry with no text takes the text of the line it names. At
/// most [`MAX_LOCATIONS`] entries return.
///
/// Reads each file it names, so a caller runs it on a worker.
pub(crate) fn resolve_locations(
    parsed: Vec<ParsedLocation>,
    root: &Path,
    open: &HashMap<PathBuf, Rope>,
    fs: &dyn FsHost,
) -> Vec<LocationEntry> {
    let mut read: HashMap<PathBuf, Option<Rope>> = HashMap::new();
    let mut entries = Vec::new();
    for location in parsed.into_iter().take(MAX_LOCATIONS) {
        let path = root.join(&location.path);
        let rope = match open.get(&path) {
            Some(rope) => rope,
            None => {
                let file = read.entry(path.clone()).or_insert_with(|| {
                    let text = action_handlers::read_string_via_host(fs, &path).ok()?;
                    Some(Rope::from(LineEnding::normalize(&text).as_ref()))
                });
                let Some(rope) = file else {
                    continue;
                };
                rope
            },
        };

        let row = location.line - 1;
        let position = Position::new(row, location.column - 1);
        let text = match location.text.is_empty() {
            true => action_handlers::lsp::line_text(rope, row),
            false => location.text,
        };
        entries.push(LocationEntry {
            offset: util::lsp_pos_to_byte_offset(rope, position, OffsetEncoding::Utf8),
            path,
            line: location.line,
            column: location.column,
            text,
            block: None,
        });
    }
    entries
}

/// The count of ASCII digits at the start of `text`.
fn digits_at(text: &str) -> usize {
    text.bytes().take_while(u8::is_ascii_digit).count()
}

/// What a query matches a candidate against: where it points and what is
/// there, which is what the row shows.
pub(crate) fn location_haystack(entry: &LocationEntry) -> String {
    format!(
        "{}:{} {}",
        entry.path.display(),
        entry.line,
        entry.text.trim()
    )
}

/// Where a candidate's preview reads from and which 0-based line it centres on.
///
/// An open buffer wins over the file, so a candidate in edited text previews
/// what the reader is looking at rather than what is on disk.
pub(crate) fn location_target(
    ws: &crate::workspace::Workspace,
    entry: &LocationEntry,
) -> Option<(PreviewSource, u32)> {
    let source = match ws.buffers.id_for_path(&entry.path) {
        Some(id) => PreviewSource::Buffer(id),
        None => PreviewSource::File(entry.path.clone()),
    };
    Some((source, entry.line.saturating_sub(1)))
}

#[cfg(test)]
mod tests {
    use super::{parse_location_line, resolve_locations, LocationEntry, ParsedLocation};
    use crate::host::FakeFs;
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
    };
    use stoat_text::Rope;

    fn parsed(path: &str, line: u32, column: u32, text: &str) -> ParsedLocation {
        ParsedLocation {
            path: path.to_owned(),
            line,
            column,
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_location_line_reads_its_path_line_column_and_text() {
        let lines = [
            "src/a.rs:2:3: hit",
            "src/b.rs:1: top",
            "src/b.rs:7:fn x()",
            "src/b.rs:7",
            "src/b.rs:7:4",
            "src/b.rs:7:0: zero",
            "my file.txt:3: x",
        ];
        assert_eq!(
            lines.map(parse_location_line),
            [
                Some(parsed("src/a.rs", 2, 3, "hit")),
                Some(parsed("src/b.rs", 1, 1, "top")),
                Some(parsed("src/b.rs", 7, 1, "fn x()")),
                Some(parsed("src/b.rs", 7, 1, "")),
                Some(parsed("src/b.rs", 7, 4, "")),
                Some(parsed("src/b.rs", 7, 1, "zero")),
                Some(parsed("my file.txt", 3, 1, "x")),
            ],
        );
    }

    #[test]
    fn a_line_that_names_no_location_gives_none() {
        assert_eq!(
            ["noise", "a:b:c", ":3: x", "http://x:80/", "src/a.rs:0: x"].map(parse_location_line),
            [None, None, None, None, None],
        );
    }

    /// A relative path joins the root. An open rope wins over the file, a
    /// path that does not read gives no entry, and an empty text takes the
    /// line's own text.
    #[test]
    fn resolve_locations_joins_the_root_and_finds_the_offset() {
        let fs = FakeFs::new();
        fs.insert_file("/repo/src/a.rs", "one\ntwo three\n");
        let none = HashMap::new();
        let open = HashMap::from([(
            PathBuf::from("/repo/src/a.rs"),
            Rope::from("xx\nyyyyy two\n"),
        )]);
        let resolve = |location: ParsedLocation, open: &HashMap<PathBuf, Rope>| {
            resolve_locations(vec![location], Path::new("/repo"), open, &fs)
        };
        let entry = |offset, text: &str| LocationEntry {
            path: PathBuf::from("/repo/src/a.rs"),
            offset,
            line: 2,
            column: 5,
            text: text.to_owned(),
            block: None,
        };

        assert_eq!(
            [
                resolve(parsed("src/a.rs", 2, 5, "x"), &none),
                resolve(parsed("src/a.rs", 2, 5, "x"), &open),
                resolve(parsed("src/gone.rs", 2, 5, "x"), &none),
                resolve(parsed("src/a.rs", 2, 5, ""), &none),
            ],
            [
                vec![entry(8, "x")],
                vec![entry(7, "x")],
                Vec::new(),
                vec![entry(8, "two three")],
            ],
        );
    }

    #[test]
    fn resolve_locations_stops_at_the_cap() {
        let fs = FakeFs::new();
        fs.insert_file("/repo/a.rs", "x\n");
        let many = vec![parsed("a.rs", 1, 1, "x"); super::MAX_LOCATIONS + 1];
        assert_eq!(
            resolve_locations(many, Path::new("/repo"), &HashMap::new(), &fs).len(),
            super::MAX_LOCATIONS,
        );
    }
}
