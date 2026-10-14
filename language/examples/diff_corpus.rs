//! Measures the structural diff over a corpus of before and after fixtures
//! that carry human node mappings, for speed and for agreement.
//!
//! A diff change lands with numbers only when a run over real code shows how
//! its timings and its agreement with human judgment move. Each fixture is a
//! directory holding `before.<ext>.test`, `after.<ext>.test`, and
//! `human_mapping.json`. The mapping names what happened to each tree-sitter
//! node, and some fixtures add `text_mappings`, named human paintings of the
//! changed text.
//!
//! Run it from the workspace root:
//!
//! ```text
//! cargo run -p stoat_language --release --example diff_corpus -- \
//!     references/omnidiff/src/test/data/diffs \
//!     [--datasets small,full,stratified,defects4j,handmade] [--fixtures a,b] \
//!     [--repeats 3] [--csv path] [--baseline path]
//! ```
//!
//! The line metric projects the human mapping and Stoat's changes onto the
//! lines of each side, and counts the lines where the two disagree. The
//! painting metric counts the non-whitespace bytes where a painting and
//! Stoat's changes disagree, and keeps the closest painting. A fixture whose
//! language has no grammar here runs the line pass and gets timings only,
//! because no grammar resolves its node paths. A fixture with no human mapping
//! gets timings only too. The `unsrch` column counts the sections whose search
//! passed the vertex cap, where the changes are the preprocessing's guesses.
//!
//! `--baseline` defaults to the committed `benches/diff_corpus_baseline.csv`.
//! It reports every fixture whose agreement or fallback moved, and every
//! fixture whose time grew past twice the baseline.

use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    ops::Range,
    path::{Path, PathBuf},
    process::ExitCode,
    slice,
    sync::Arc,
    time::Instant,
};
use stoat_language::{
    structural_diff::{self, ChangeKind, DiffChange, DiffResult, Side},
    Language, LanguageRegistry, Node,
};

/// The datasets of the corpus, in the order the walk visits them.
const DATASETS: [&str; 5] = ["small", "full", "stratified", "defects4j", "handmade"];

/// The committed baseline that a run compares against when no `--baseline`
/// names one.
const DEFAULT_BASELINE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/diff_corpus_baseline.csv"
);

/// The CSV header, one column per field of a [`Row`] in order.
const CSV_HEADER: &str = "dataset,name,language,before_lines,after_lines,min_ms,fell_back,\
unresolved_paths,groups,line_mismatches,line_mismatches_no_moves,moved_changes,\
painting_bytes,painting_bytes_no_moves,painting,unsearched";

/// The smallest time, in milliseconds, that the baseline compare reports as
/// grown. Timer noise moves a run under it by more than twice.
const NOISE_FLOOR_MS: f64 = 1.0;

/// The command line, read by hand from [`env::args`].
struct Options {
    root: PathBuf,
    datasets: Vec<String>,
    fixtures: Option<Vec<String>>,
    repeats: usize,
    csv: Option<PathBuf>,
    baseline: Option<PathBuf>,
}

/// One fixture directory, and the extension that its two sides share.
struct Fixture {
    dataset: String,
    name: String,
    dir: PathBuf,
    ext: String,
}

/// One fixture's measurements, which make one line of the table and one row of
/// the CSV.
///
/// The agreement fields are `None` for a fixture whose language has no grammar
/// here, since nothing resolves its human node paths, and for a fixture with no
/// human mapping.
struct Row {
    dataset: String,
    name: String,
    language: String,
    before_lines: usize,
    after_lines: usize,
    min_ms: f64,
    fell_back: bool,
    unsearched: usize,
    unresolved_paths: Option<usize>,
    groups: Option<bool>,
    line_mismatches: Option<usize>,
    line_mismatches_no_moves: Option<usize>,
    moved_changes: usize,
    painting: Option<PaintingScore>,
}

/// The closest painting of a fixture that has `text_mappings`.
struct PaintingScore {
    name: String,
    bytes: usize,
    bytes_no_moves: usize,
}

/// One flag per line or per byte of each side.
#[derive(Debug, PartialEq)]
struct Sides {
    before: Vec<bool>,
    after: Vec<bool>,
}

/// The lines that a human mapping touches, and how many of its paths named no
/// node.
struct HumanProjection {
    touched: Sides,
    unresolved: usize,
}

/// The columns of a baseline row that the compare reads.
struct BaselineRow {
    min_ms: f64,
    fell_back: String,
    line_mismatches: String,
    painting_bytes: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("diff_corpus: {message}");
            ExitCode::FAILURE
        },
    }
}

fn run() -> Result<(), String> {
    let options = parse_options(env::args().skip(1))?;
    let (fixtures, skipped) = discover(&options)?;
    for dir in &skipped {
        println!("skipped {}: no before.<ext>.test", dir.display());
    }

    let registry = LanguageRegistry::standard();
    let mut rows = Vec::with_capacity(fixtures.len());
    println!(
        "{:<11} {:<70} {:<6} {:>6} {:>6} {:>9} {:>4} {:>6} {:>4} {:>6} {:>6} {:>6} {:>6} {:>6}",
        "dataset",
        "name",
        "lang",
        "before",
        "after",
        "min ms",
        "fall",
        "unsrch",
        "unres",
        "lines",
        "no-mv",
        "moved",
        "paint",
        "no-mv"
    );
    for fixture in &fixtures {
        let row = measure(fixture, &registry, options.repeats)?;
        print_row(&row);
        rows.push(row);
    }
    print_summary(&rows);

    // Read before the CSV is written, because a run that refreshes the
    // baseline names the same file for both.
    let baseline = match &options.baseline {
        Some(path) => Some(read_baseline(path)?),
        None => None,
    };
    if let Some(path) = &options.csv {
        write_csv(path, &rows)?;
    }
    if let Some(baseline) = baseline {
        print_baseline_changes(&rows, &baseline);
    }
    Ok(())
}

/// Read the corpus root and the flags from `args`.
///
/// `--baseline` falls back to [`DEFAULT_BASELINE`] when that file exists.
fn parse_options(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut root = None;
    let mut datasets = DATASETS.map(String::from).to_vec();
    let mut fixtures = None;
    let mut repeats = 3;
    let mut csv = None;
    let mut baseline = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--datasets" => datasets = split_list(&flag_value(&mut args, &arg)?),
            "--fixtures" => fixtures = Some(split_list(&flag_value(&mut args, &arg)?)),
            "--repeats" => {
                repeats = flag_value(&mut args, &arg)?
                    .parse()
                    .map_err(|_| "--repeats takes a whole number".to_string())?;
            },
            "--csv" => csv = Some(PathBuf::from(flag_value(&mut args, &arg)?)),
            "--baseline" => baseline = Some(PathBuf::from(flag_value(&mut args, &arg)?)),
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            _ => root = Some(PathBuf::from(arg)),
        }
    }

    let root =
        root.ok_or("give the corpus root, such as references/omnidiff/src/test/data/diffs")?;
    if repeats == 0 {
        return Err("--repeats takes at least 1".to_string());
    }
    let baseline = baseline.or_else(|| {
        let default = PathBuf::from(DEFAULT_BASELINE);
        default.exists().then_some(default)
    });
    Ok(Options {
        root,
        datasets,
        fixtures,
        repeats,
        csv,
        baseline,
    })
}

/// The value that follows `flag`.
fn flag_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} takes a value"))
}

fn split_list(list: &str) -> Vec<String> {
    list.split(',')
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

/// Every fixture under the chosen datasets, in sorted order, and the
/// directories that hold no `before.<ext>.test`.
fn discover(options: &Options) -> Result<(Vec<Fixture>, Vec<PathBuf>), String> {
    let mut fixtures = Vec::new();
    let mut skipped = Vec::new();
    for dataset in &options.datasets {
        let dataset_dir = options.root.join(dataset);
        let mut dirs: Vec<PathBuf> = fs::read_dir(&dataset_dir)
            .map_err(|err| format!("{}: {err}", dataset_dir.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();

        for dir in dirs {
            let name = dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if options
                .fixtures
                .as_ref()
                .is_some_and(|wanted| !wanted.contains(&name))
            {
                continue;
            }
            match before_ext(&dir) {
                Some(ext) => fixtures.push(Fixture {
                    dataset: dataset.clone(),
                    name,
                    dir,
                    ext,
                }),
                None => skipped.push(dir),
            }
        }
    }
    Ok((fixtures, skipped))
}

/// The `<ext>` of the `before.<ext>.test` in `dir`.
fn before_ext(dir: &Path) -> Option<String> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let ext = name.strip_prefix("before.")?.strip_suffix(".test")?;
            Some(ext.to_string())
        })
}

/// Diff one fixture, time it, and score it against its human mapping.
fn measure(fixture: &Fixture, registry: &LanguageRegistry, repeats: usize) -> Result<Row, String> {
    let read = |file: &str| {
        let path = fixture.dir.join(file);
        fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))
    };
    let before = read(&format!("before.{}.test", fixture.ext))?;
    let after = read(&format!("after.{}.test", fixture.ext))?;
    let language = registry.for_path(Path::new(&format!("x.{}", fixture.ext)));

    let (result, min_ms) = timed_diff(language.as_ref(), &before, &after, repeats);
    let mut row = Row {
        dataset: fixture.dataset.clone(),
        name: fixture.name.clone(),
        language: language
            .as_ref()
            .map_or_else(|| "lines".to_string(), |language| language.name.to_string()),
        before_lines: before.split('\n').count(),
        after_lines: after.split('\n').count(),
        min_ms,
        fell_back: result.fell_back_to_line_diff,
        unsearched: result.unsearched.len(),
        unresolved_paths: None,
        groups: None,
        line_mismatches: None,
        line_mismatches_no_moves: None,
        moved_changes: result
            .changes
            .iter()
            .filter(|change| change.kind == ChangeKind::Moved)
            .count(),
        painting: None,
    };
    // A fixture with no grammar here, or with no human mapping, has nothing to
    // score against, and keeps its timing alone.
    let Some(language) = language else {
        return Ok(row);
    };
    if !fixture.dir.join("human_mapping.json").exists() {
        return Ok(row);
    }

    let mapping: Value = serde_json::from_str(&read("human_mapping.json")?)
        .map_err(|err| format!("{}: {err}", fixture.dir.display()))?;
    let human = human_touched(&language, &mapping, &before, &after);
    let line_mismatches = |skip_moved| {
        mismatches(
            &human.touched,
            &stoat_touched(&result, &before, &after, skip_moved),
        )
    };
    row.unresolved_paths = Some(human.unresolved);
    row.groups = Some(
        mapping["groups"]
            .as_array()
            .is_some_and(|groups| !groups.is_empty()),
    );
    row.line_mismatches = Some(line_mismatches(false));
    row.line_mismatches_no_moves = Some(line_mismatches(true));
    row.painting = painting_score(&mapping, &before, &after, &result);
    Ok(row)
}

/// The diff of `before` against `after` and the fastest of `repeats` runs, in
/// milliseconds.
///
/// With a language the diff takes the structural path, which falls back to
/// lines on its own. Without one it is the line pass.
fn timed_diff(
    language: Option<&Arc<Language>>,
    before: &str,
    after: &str,
    repeats: usize,
) -> (DiffResult, f64) {
    let mut result = DiffResult::default();
    let mut min_ms = f64::INFINITY;
    for _ in 0..repeats {
        let start = Instant::now();
        result = match language {
            Some(language) => structural_diff::diff_with_language_or_lines(language, before, after),
            None => structural_diff::diff_lines(before, after),
        };
        min_ms = min_ms.min(start.elapsed().as_secs_f64() * 1000.0);
    }
    (result, min_ms)
}

/// The lines that the human `mapping` touches on each side.
///
/// A leaf with `delete` or `update` touches its range on the before side, and
/// a leaf with `insert` or `update` on the after side. A
/// `match_but_not_identical` pair touches its own content, the bytes that no
/// direct child covers, when that content differs once whitespace goes. That
/// keeps a quoted scalar's anonymous children and a reindent between a block's
/// children from reading as changes. A `delete_with_children` or
/// `insert_with_children` node touches its whole range. Identical entries and
/// group members touch nothing, so their paths are not resolved.
fn human_touched(
    language: &Language,
    mapping: &Value,
    before: &str,
    after: &str,
) -> HumanProjection {
    let (before_lines, after_lines) = (line_bounds(before), line_bounds(after));
    let mut touched = Sides {
        before: vec![false; before_lines.len()],
        after: vec![false; after_lines.len()],
    };
    let entries = mapping["entries"].as_array().map_or(&[][..], Vec::as_slice);
    let (Some(before_tree), Some(after_tree)) = (
        stoat_language::parse(language, before, None),
        stoat_language::parse(language, after, None),
    ) else {
        return HumanProjection {
            touched,
            unresolved: entries.len(),
        };
    };

    let mut unresolved = 0;
    for entry in entries {
        let operation = entry["operation"].as_str().unwrap_or_default();
        if matches!(operation, "identical" | "identical_with_children") {
            continue;
        }
        let before_node = entry
            .get("before_path")
            .map(|path| resolve(before_tree.root_node(), path));
        let after_node = entry
            .get("after_path")
            .map(|path| resolve(after_tree.root_node(), path));
        if matches!(before_node, Some(None)) || matches!(after_node, Some(None)) {
            unresolved += 1;
            continue;
        }

        match operation {
            "delete" | "update" | "insert" => {
                if let Some(node) = leaf(before_node).filter(|_| operation != "insert") {
                    touch(&mut touched.before, &before_lines, node.byte_range());
                }
                if let Some(node) = leaf(after_node).filter(|_| operation != "delete") {
                    touch(&mut touched.after, &after_lines, node.byte_range());
                }
            },
            "match_but_not_identical" => {
                let (Some(Some(lhs)), Some(Some(rhs))) = (before_node, after_node) else {
                    continue;
                };
                let (lhs_own, rhs_own) = (own_content(lhs), own_content(rhs));
                if squeeze(before, &lhs_own) != squeeze(after, &rhs_own) {
                    for range in lhs_own {
                        touch(&mut touched.before, &before_lines, range);
                    }
                    for range in rhs_own {
                        touch(&mut touched.after, &after_lines, range);
                    }
                }
            },
            "delete_with_children" => {
                if let Some(Some(node)) = before_node {
                    touch(&mut touched.before, &before_lines, node.byte_range());
                }
            },
            "insert_with_children" => {
                if let Some(Some(node)) = after_node {
                    touch(&mut touched.after, &after_lines, node.byte_range());
                }
            },
            _ => {},
        }
    }
    HumanProjection {
        touched,
        unresolved,
    }
}

/// The node that an entry's path named, when it has no children.
fn leaf(node: Option<Option<Node<'_>>>) -> Option<Node<'_>> {
    node.flatten().filter(|node| node.child_count() == 0)
}

/// The node that `path` names below `root`, or `None` when a segment names no
/// child.
///
/// A segment is `kind:n`, split at its last `:`, where `n` counts the
/// children of that kind from 1 among all children, anonymous ones included.
/// A segment with no `:` names the first child of its kind.
fn resolve<'t>(root: Node<'t>, path: &Value) -> Option<Node<'t>> {
    let mut node = root;
    for segment in path.as_array()? {
        let segment = segment.as_str()?;
        let (kind, occurrence) = match segment.rsplit_once(':') {
            Some((kind, index)) => (kind, index.parse::<usize>().ok()?.checked_sub(1)?),
            None => (segment, 0),
        };
        let mut cursor = node.walk();
        node = node
            .children(&mut cursor)
            .filter(|child| child.kind() == kind)
            .nth(occurrence)?;
    }
    Some(node)
}

/// The byte ranges of `node` that none of its direct children covers.
fn own_content(node: Node<'_>) -> Vec<Range<usize>> {
    let mut gaps = Vec::new();
    let mut at = node.start_byte();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() > at {
            gaps.push(at..child.start_byte());
        }
        at = at.max(child.end_byte());
    }
    if node.end_byte() > at {
        gaps.push(at..node.end_byte());
    }
    gaps
}

/// The text of `ranges` in `text`, joined, with every whitespace char gone.
fn squeeze(text: &str, ranges: &[Range<usize>]) -> String {
    ranges
        .iter()
        .flat_map(|range| text[range.clone()].chars())
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

/// The lines that Stoat's changes touch on each side, without the moved
/// changes when `skip_moved` is set.
///
/// The editor skips a moved span inside a modified hunk, so the count without
/// moves is the closer read of what it marks there.
fn stoat_touched(result: &DiffResult, before: &str, after: &str, skip_moved: bool) -> Sides {
    let (before_lines, after_lines) = (line_bounds(before), line_bounds(after));
    let mut touched = Sides {
        before: vec![false; before_lines.len()],
        after: vec![false; after_lines.len()],
    };
    for change in &result.changes {
        if skip_moved && change.kind == ChangeKind::Moved {
            continue;
        }
        let (lines, bounds) = match change.side {
            Side::Lhs => (&mut touched.before, &before_lines),
            Side::Rhs => (&mut touched.after, &after_lines),
        };
        for range in effective_ranges(change) {
            touch(lines, bounds, range.clone());
        }
    }
    touched
}

/// The ranges that `change` marks.
///
/// These are its `refined_spans` when the differ refined it, else its whole
/// `byte_range`, as the editor's diff map reads them.
fn effective_ranges(change: &DiffChange) -> &[Range<usize>] {
    match &change.refined_spans {
        Some(spans) => spans,
        None => slice::from_ref(&change.byte_range),
    }
}

/// Each line's byte range, its terminator left out, one per line of
/// `text.split('\n')`.
fn line_bounds(text: &str) -> Vec<Range<usize>> {
    let mut start = 0;
    text.split('\n')
        .map(|line| {
            let bounds = start..start + line.len();
            start = bounds.end + 1;
            bounds
        })
        .collect()
}

/// Mark each line of `bounds` that `range` intersects.
///
/// A range touches a line when it starts before the line's terminator and ends
/// after the line's start, so a range over the terminator alone, and a
/// zero-width range, touch nothing.
fn touch(lines: &mut [bool], bounds: &[Range<usize>], range: Range<usize>) {
    if range.is_empty() {
        return;
    }
    let first = bounds.partition_point(|line| line.end <= range.start);
    for (line, bound) in lines.iter_mut().zip(bounds).skip(first) {
        if bound.start >= range.end {
            break;
        }
        *line = true;
    }
}

/// The count of lines, over both sides, where `human` and `stoat` disagree.
fn mismatches(human: &Sides, stoat: &Sides) -> usize {
    let differ = |a: &[bool], b: &[bool]| a.iter().zip(b).filter(|(a, b)| a != b).count();
    differ(&human.before, &stoat.before) + differ(&human.after, &stoat.after)
}

/// The closest of the fixture's paintings, or `None` when it has none.
///
/// A painting scores the non-whitespace bytes, over both sides, whose touched
/// state differs from Stoat's changes. The score without moves treats the
/// painting's move matches and Stoat's moved changes as untouched, and takes
/// its own minimum over the paintings.
fn painting_score(
    mapping: &Value,
    before: &str,
    after: &str,
    result: &DiffResult,
) -> Option<PaintingScore> {
    let paintings = mapping.get("text_mappings")?.as_array()?;
    let (before_lines, after_lines) = (line_bounds(before), line_bounds(after));
    let stoat = stoat_bytes(result, before.len(), after.len(), false);
    let stoat_no_moves = stoat_bytes(result, before.len(), after.len(), true);

    let mut best: Option<PaintingScore> = None;
    let mut best_no_moves = usize::MAX;
    for painting in paintings {
        let marks = |skip_moves| {
            painting_bytes(
                painting,
                (before, &before_lines),
                (after, &after_lines),
                skip_moves,
            )
        };
        let bytes = differing_bytes(&marks(false), &stoat, before, after);
        best_no_moves = best_no_moves.min(differing_bytes(
            &marks(true),
            &stoat_no_moves,
            before,
            after,
        ));
        if best.as_ref().is_none_or(|best| bytes < best.bytes) {
            best = Some(PaintingScore {
                name: painting["name"].as_str().unwrap_or_default().to_string(),
                bytes,
                bytes_no_moves: 0,
            });
        }
    }
    best.map(|best| PaintingScore {
        bytes_no_moves: best_no_moves,
        ..best
    })
}

/// The bytes that `painting` touches on each side, with its move matches left
/// out when `skip_moves` is set.
///
/// An `Insert` touches its after spans, a `Delete` its before spans, and a
/// `Match` its spans on both sides.
fn painting_bytes(
    painting: &Value,
    (before, before_lines): (&str, &[Range<usize>]),
    (after, after_lines): (&str, &[Range<usize>]),
    skip_moves: bool,
) -> Sides {
    let mut marks = Sides {
        before: vec![false; before.len()],
        after: vec![false; after.len()],
    };
    for entry in painting["entries"].as_array().into_iter().flatten() {
        if skip_moves && is_move(entry, (before, before_lines), (after, after_lines)) {
            continue;
        }
        let operation = entry["operation"].as_str().unwrap_or_default();
        if operation != "Insert" {
            mark_spans(&mut marks.before, &entry["before"], before_lines);
        }
        if operation != "Delete" {
            mark_spans(&mut marks.after, &entry["after"], after_lines);
        }
    }
    marks
}

/// Whether `entry` is a `Match` whose spans all hold byte-identical text, on
/// both sides, which makes it a move rather than an update.
fn is_move(
    entry: &Value,
    (before, before_lines): (&str, &[Range<usize>]),
    (after, after_lines): (&str, &[Range<usize>]),
) -> bool {
    if entry["operation"].as_str() != Some("Match") {
        return false;
    }
    let texts: Option<Vec<&str>> = spans(&entry["before"])
        .map(|span| span_range(span, before_lines).and_then(|range| before.get(range)))
        .chain(
            spans(&entry["after"])
                .map(|span| span_range(span, after_lines).and_then(|range| after.get(range))),
        )
        .collect();
    texts.is_some_and(|texts| texts.windows(2).all(|pair| pair[0] == pair[1]))
}

/// Set the byte of `marks` under each span of `side`.
fn mark_spans(marks: &mut [bool], side: &Value, lines: &[Range<usize>]) {
    for range in spans(side).filter_map(|span| span_range(span, lines)) {
        let end = range.end.min(marks.len());
        for mark in &mut marks[range.start.min(end)..end] {
            *mark = true;
        }
    }
}

/// The spans of one side of a painting entry, which is one span object or a
/// list of them.
fn spans(side: &Value) -> impl Iterator<Item = &Value> {
    let (one, many) = match side {
        Value::Array(spans) => (None, spans.as_slice()),
        Value::Object(_) => (Some(side), &[][..]),
        _ => (None, &[][..]),
    };
    one.into_iter().chain(many)
}

/// The byte range of a span.
///
/// Each end is its row's start plus its byte column. An end column of 0 stops
/// at the start of the end row, so the span holds the newline of the row
/// before it and nothing of the end row.
fn span_range(span: &Value, lines: &[Range<usize>]) -> Option<Range<usize>> {
    let at = |row: &str, column: &str| {
        let row = usize::try_from(span[row].as_u64()?).ok()?;
        let column = usize::try_from(span[column].as_u64()?).ok()?;
        Some(lines.get(row)?.start + column)
    };
    Some(at("start_row", "start_column")?..at("end_row", "end_column")?)
}

/// Each byte that Stoat's changes touch on each side, without the moved
/// changes when `skip_moved` is set.
fn stoat_bytes(
    result: &DiffResult,
    before_len: usize,
    after_len: usize,
    skip_moved: bool,
) -> Sides {
    let mut marks = Sides {
        before: vec![false; before_len],
        after: vec![false; after_len],
    };
    for change in &result.changes {
        if skip_moved && change.kind == ChangeKind::Moved {
            continue;
        }
        let side = match change.side {
            Side::Lhs => &mut marks.before,
            Side::Rhs => &mut marks.after,
        };
        for range in effective_ranges(change) {
            let end = range.end.min(side.len());
            for mark in &mut side[range.start.min(end)..end] {
                *mark = true;
            }
        }
    }
    marks
}

/// The count of non-whitespace bytes, over both sides, where `painting` and
/// `stoat` disagree.
fn differing_bytes(painting: &Sides, stoat: &Sides, before: &str, after: &str) -> usize {
    let differ = |text: &str, a: &[bool], b: &[bool]| {
        text.bytes()
            .zip(a.iter().zip(b))
            .filter(|(byte, (a, b))| !byte.is_ascii_whitespace() && a != b)
            .count()
    };
    differ(before, &painting.before, &stoat.before) + differ(after, &painting.after, &stoat.after)
}

fn print_row(row: &Row) {
    let or_dash =
        |value: Option<usize>| value.map_or_else(|| "-".to_string(), |value| value.to_string());
    println!(
        concat!(
            "{:<11} {:<70} {:<6} {:>6} {:>6} {:>9.3} {:>4} {:>6} ",
            "{:>4} {:>6} {:>6} {:>6} {:>6} {:>6} {}",
        ),
        row.dataset,
        row.name,
        row.language,
        row.before_lines,
        row.after_lines,
        row.min_ms,
        if row.fell_back { "yes" } else { "no" },
        row.unsearched,
        or_dash(row.unresolved_paths),
        or_dash(row.line_mismatches),
        or_dash(row.line_mismatches_no_moves),
        row.moved_changes,
        or_dash(row.painting.as_ref().map(|painting| painting.bytes)),
        or_dash(
            row.painting
                .as_ref()
                .map(|painting| painting.bytes_no_moves)
        ),
        match (&row.painting, row.groups) {
            (Some(painting), _) => format!("painting {:?}", painting.name),
            (None, Some(true)) => "groups".to_string(),
            _ => String::new(),
        }
    );
}

/// Print each language's fixture count, perfect count on the line metric,
/// mismatched lines, and timing percentiles, then the totals.
fn print_summary(rows: &[Row]) {
    let mut by_language: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for row in rows {
        by_language.entry(&row.language).or_default().push(row);
    }

    println!();
    println!(
        "{:<10} {:>8} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "language", "fixtures", "perfect", "mismatched", "p50 ms", "p90 ms", "p99 ms", "max ms"
    );
    let all: Vec<&Row> = rows.iter().collect();
    for (language, rows) in by_language
        .iter()
        .map(|(language, rows)| (*language, rows.as_slice()))
        .chain([("total", all.as_slice())])
    {
        let scored: Vec<usize> = rows.iter().filter_map(|row| row.line_mismatches).collect();
        let (perfect, mismatched) = match scored.is_empty() {
            true => ("-".to_string(), "-".to_string()),
            false => (
                scored
                    .iter()
                    .filter(|&&lines| lines == 0)
                    .count()
                    .to_string(),
                scored.iter().sum::<usize>().to_string(),
            ),
        };
        let mut times: Vec<f64> = rows.iter().map(|row| row.min_ms).collect();
        times.sort_by(f64::total_cmp);
        println!(
            "{:<10} {:>8} {:>8} {:>10} {:>10.3} {:>10.3} {:>10.3} {:>10.3}",
            language,
            rows.len(),
            perfect,
            mismatched,
            percentile(&times, 0.50),
            percentile(&times, 0.90),
            percentile(&times, 0.99),
            percentile(&times, 1.0),
        );
    }
}

/// The `quantile` of `sorted`, read at the index nearest `(len - 1) * quantile`,
/// or 0 for an empty slice.
fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn write_csv(path: &Path, rows: &[Row]) -> Result<(), String> {
    let or_dash =
        |value: Option<usize>| value.map_or_else(|| "-".to_string(), |value| value.to_string());
    let mut csv = String::from(CSV_HEADER);
    csv.push('\n');
    for row in rows {
        let fields = [
            row.dataset.clone(),
            row.name.clone(),
            row.language.clone(),
            row.before_lines.to_string(),
            row.after_lines.to_string(),
            format!("{:.3}", row.min_ms),
            row.fell_back.to_string(),
            or_dash(row.unresolved_paths),
            row.groups
                .map_or_else(|| "-".to_string(), |groups| groups.to_string()),
            or_dash(row.line_mismatches),
            or_dash(row.line_mismatches_no_moves),
            row.moved_changes.to_string(),
            or_dash(row.painting.as_ref().map(|painting| painting.bytes)),
            or_dash(
                row.painting
                    .as_ref()
                    .map(|painting| painting.bytes_no_moves),
            ),
            // A painting name is free text, and a comma in it splits the row.
            row.painting
                .as_ref()
                .map(|painting| painting.name.replace(',', ";"))
                .unwrap_or_default(),
            row.unsearched.to_string(),
        ];
        csv.push_str(&fields.join(","));
        csv.push('\n');
    }
    fs::write(path, csv).map_err(|err| format!("{}: {err}", path.display()))
}

/// The baseline rows of the CSV at `path`, keyed by dataset and name.
fn read_baseline(path: &Path) -> Result<HashMap<(String, String), BaselineRow>, String> {
    let csv = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut rows = HashMap::new();
    for line in csv.lines().skip(1) {
        let fields: Vec<&str> = line.split(',').collect();
        let [dataset, name, _, _, _, min_ms, fell_back, _, _, lines, _, _, painting, ..] =
            fields.as_slice()
        else {
            return Err(format!("{}: short row {line:?}", path.display()));
        };
        rows.insert(
            (dataset.to_string(), name.to_string()),
            BaselineRow {
                min_ms: min_ms.parse().unwrap_or(f64::INFINITY),
                fell_back: fell_back.to_string(),
                line_mismatches: lines.to_string(),
                painting_bytes: painting.to_string(),
            },
        );
    }
    Ok(rows)
}

/// Print each fixture whose agreement or fallback differs from `baseline`, or
/// whose time grew past twice the baseline's and past [`NOISE_FLOOR_MS`].
fn print_baseline_changes(rows: &[Row], baseline: &HashMap<(String, String), BaselineRow>) {
    let or_dash =
        |value: Option<usize>| value.map_or_else(|| "-".to_string(), |value| value.to_string());
    println!();
    println!("changes against the baseline:");
    let mut changed = 0;
    for row in rows {
        let Some(old) = baseline.get(&(row.dataset.clone(), row.name.clone())) else {
            println!("  {}/{}: not in the baseline", row.dataset, row.name);
            changed += 1;
            continue;
        };
        let mut notes = Vec::new();
        let lines = or_dash(row.line_mismatches);
        if lines != old.line_mismatches {
            notes.push(format!(
                "line mismatches {} -> {lines}",
                old.line_mismatches
            ));
        }
        let painting = or_dash(row.painting.as_ref().map(|painting| painting.bytes));
        if painting != old.painting_bytes {
            notes.push(format!(
                "painting bytes {} -> {painting}",
                old.painting_bytes
            ));
        }
        if row.fell_back.to_string() != old.fell_back {
            notes.push(format!("fell back {} -> {}", old.fell_back, row.fell_back));
        }
        if row.min_ms > 2.0 * old.min_ms && row.min_ms >= NOISE_FLOOR_MS {
            notes.push(format!("min ms {:.3} -> {:.3}", old.min_ms, row.min_ms));
        }
        if !notes.is_empty() {
            println!("  {}/{}: {}", row.dataset, row.name, notes.join(", "));
            changed += 1;
        }
    }
    println!("{changed} of {} fixtures changed", rows.len());
}

#[cfg(test)]
mod tests {
    use super::{human_touched, is_move, line_bounds, resolve, span_range, Sides};
    use serde_json::json;
    use std::{path::Path, sync::Arc};
    use stoat_language::{Language, LanguageRegistry};

    fn rust() -> Arc<Language> {
        LanguageRegistry::standard()
            .for_path(Path::new("x.rs"))
            .expect("rust language")
    }

    #[test]
    fn a_path_resolves_by_kind_and_occurrence() {
        let text = "fn a() {}\nfn b() {}";
        let tree = stoat_language::parse(&rust(), text, None).expect("parses");

        let node = resolve(
            tree.root_node(),
            &json!(["function_item:2", "identifier:1"]),
        );

        assert_eq!(
            node.map(|node| &text[node.byte_range()]),
            Some("b"),
            "the second function item's first identifier",
        );
    }

    #[test]
    fn a_delete_with_children_touches_every_line_of_its_node() {
        let before = "fn a() {\n    x();\n}\nfn b() {}\n";
        let after = "fn b() {}\n";
        let mapping = json!({
            "entries": [{"operation": "delete_with_children", "before_path": ["function_item:1"]}],
        });

        let human = human_touched(&rust(), &mapping, before, after);

        assert_eq!(
            (human.touched, human.unresolved),
            (
                Sides {
                    before: vec![true, true, true, false, false],
                    after: vec![false, false],
                },
                0,
            ),
            "the deleted function's three lines and nothing else",
        );
    }

    #[test]
    fn a_painting_span_with_end_column_zero_stops_before_its_end_row() {
        let span = json!({"start_row": 0, "start_column": 0, "end_row": 1, "end_column": 0});

        assert_eq!(
            span_range(&span, &line_bounds("ab\ncd")),
            Some(0..3),
            "the first row and its newline, nothing of the second row",
        );
    }

    #[test]
    fn a_match_painting_over_identical_text_is_a_move() {
        let (before, after) = ("foo\nbar\n", "bar\nfoo\n");
        let (before_lines, after_lines) = (line_bounds(before), line_bounds(after));
        let span = |row: u64| {
            json!({
                "start_row": row,
                "start_column": 0,
                "end_row": row,
                "end_column": 3,
            })
        };
        let matched = |before_row, after_row| {
            json!({
                "operation": "Match",
                "before": span(before_row),
                "after": [span(after_row)],
            })
        };

        let verdicts = [matched(0, 1), matched(0, 0)]
            .map(|entry| is_move(&entry, (before, &before_lines), (after, &after_lines)));

        assert_eq!(
            verdicts,
            [true, false],
            "foo to foo is a move, and foo to bar is an update",
        );
    }
}
