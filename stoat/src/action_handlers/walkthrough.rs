use crate::{
    action_handlers::{self, lsp::HoverPopup, read_string_via_host},
    app::{Stoat, UpdateEffect},
    code_index::{build, nav},
    walkthrough::{self, run::WalkthroughRun, store},
};
use codegraph::{EdgeKind, SymbolKey};
use std::path::{Path, PathBuf};
use stoat_text::Rope;

/// One place a walkthrough sends the reader, being a stop's focus or one of its
/// annotations.
///
/// The two differ in the format but not in what the player does with them, so
/// the jump, the drift check, and the trail all take this instead.
struct Anchor {
    path: PathBuf,
    range: walkthrough::Range,
    snippet: String,
}

/// Load the walkthrough `slug` and jump to its first stop.
///
/// Replaces whatever tour the workspace already holds. A slug that does not
/// load, or one whose walkthrough has no stops, reports why and leaves the
/// previous tour in place.
///
/// Any trail goes. The first stop follows nothing, so there is no pair of stops
/// for a trail to connect yet.
pub(crate) fn open(stoat: &mut Stoat, slug: &str) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();

    let loaded = match store::load(stoat.fs_host.as_ref(), &git_root, slug) {
        Ok(walkthrough) => walkthrough,
        Err(error) => {
            stoat.set_status(format!("{error}"));
            return UpdateEffect::Redraw;
        },
    };

    let Some(run) = WalkthroughRun::new(loaded) else {
        stoat.set_status(format!("walkthrough '{slug}' has no stops"));
        return UpdateEffect::Redraw;
    };

    stoat.active_workspace_mut().walkthrough = Some(run);
    clear_trail(stoat);
    jump_to_stop(stoat, None)
}

/// Step forward to the next stop.
pub(crate) fn next(stoat: &mut Stoat) -> UpdateEffect {
    step(stoat, 1)
}

/// Step back to the previous stop.
pub(crate) fn prev(stoat: &mut Stoat) -> UpdateEffect {
    step(stoat, -1)
}

/// Step forward to the next annotation of the current stop.
pub(crate) fn next_annotation(stoat: &mut Stoat) -> UpdateEffect {
    step_annotation(stoat, 1)
}

/// Step back toward the stop's focus through its annotations.
pub(crate) fn prev_annotation(stoat: &mut Stoat) -> UpdateEffect {
    step_annotation(stoat, -1)
}

/// Raise the current stop's narration again after something dismissed it.
///
/// The narration shares the hover popup, so the next key press takes it down.
/// This puts it back without moving the reader off the stop.
pub(crate) fn show_narration_again(stoat: &mut Stoat) -> UpdateEffect {
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    };
    let point = current_anchor(run).range.start;

    let Some(offset) = focused_offset_of(stoat, point) else {
        return UpdateEffect::None;
    };
    show_narration(stoat, offset);

    if stoat.pending_hover.is_none() {
        stoat.set_status("this stop has no narration");
    }
    UpdateEffect::Redraw
}

/// End the walkthrough, leaving the reader wherever it put them.
///
/// The trail goes with it. It was laid between two stops of a tour that is
/// over, and nothing else put it there.
pub(crate) fn done(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().walkthrough.is_none() {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    }

    stoat.active_workspace_mut().walkthrough = None;
    clear_trail(stoat);
    stoat.set_status("walkthrough closed");
    UpdateEffect::Redraw
}

/// Move `delta` stops, lay the trail between the two, and jump to where that
/// lands.
///
/// A step off either end says the tour is over rather than jumping again, which
/// tells a clamped step apart from one that moved. A clamped step lays no
/// trail, since the reader has not moved between two stops.
fn step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(run) = stoat.active_workspace_mut().walkthrough.as_mut() else {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    };
    let from = current_anchor(run);

    if !run.step(delta) {
        let end = if delta < 0 { "first" } else { "last" };
        stoat.set_status(format!("already on the {end} stop"));
        return UpdateEffect::Redraw;
    }

    let to = current_anchor(run_of(stoat));
    let note = install_step_trail(stoat, &from, &to);
    jump_to_stop(stoat, note)
}

/// Move `delta` places along the current stop's annotations, lay the trail
/// between the two, and jump to where that lands.
///
/// The stop's own focus heads that walk, so a step back off the first
/// annotation returns to it and reads as a stop arrival again.
fn step_annotation(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(run) = stoat.active_workspace_mut().walkthrough.as_mut() else {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    };
    let from = current_anchor(run);

    if run.current_stop().annotations.is_empty() {
        stoat.set_status("this stop has no annotations");
        return UpdateEffect::Redraw;
    }

    if !run.step_annotation(delta) {
        let end = if delta < 0 { "stop" } else { "last annotation" };
        stoat.set_status(format!("already on the {end}"));
        return UpdateEffect::Redraw;
    }

    let to = current_anchor(run_of(stoat));
    let note = install_step_trail(stoat, &from, &to);

    match run_of(stoat).current_annotation().is_some() {
        true => jump_to_annotation(stoat, note),
        false => jump_to_stop(stoat, note),
    }
}

/// The active run, which every stepping path has already confirmed is there.
fn run_of(stoat: &Stoat) -> &WalkthroughRun {
    stoat
        .active_workspace()
        .walkthrough
        .as_ref()
        .expect("a walkthrough is playing")
}

/// Where the reader is, being the current annotation or the stop's own focus.
fn current_anchor(run: &WalkthroughRun) -> Anchor {
    match run.current_annotation() {
        Some(annotation) => Anchor {
            path: annotation
                .path
                .clone()
                .unwrap_or_else(|| run.current_stop().focus.path.clone()),
            range: annotation.range,
            snippet: annotation.snippet.clone(),
        },
        None => {
            let focus = &run.current_stop().focus;
            Anchor {
                path: focus.path.clone(),
                range: focus.range,
                snippet: focus.snippet.clone(),
            }
        },
    }
}

/// Jump to the current annotation and name it, with `note` appended when the
/// step has more to report.
///
/// The stop's narration comes back along with it. The reader is still on the
/// same slide, and the key press that stepped took the popup down.
fn jump_to_annotation(stoat: &mut Stoat, note: Option<String>) -> UpdateEffect {
    let run = run_of(stoat);
    let Some(annotation) = run.current_annotation() else {
        return UpdateEffect::None;
    };
    let (id, label) = (annotation.id.clone(), annotation.label.clone());
    let Some((at, count)) = run.annotation_progress() else {
        return UpdateEffect::None;
    };
    let anchor = current_anchor(run);

    let Some(landed) = jump_to_range(stoat, &anchor.path, anchor.range, &anchor.snippet) else {
        return UpdateEffect::Redraw;
    };
    show_narration(stoat, landed.offset);

    match (landed.drifted, note) {
        (true, _) => stoat.set_status(format!("{id} drifted from its capture")),
        (false, Some(note)) => stoat.set_status(format!("{id} {at}/{count}: {label} ({note})")),
        (false, None) => stoat.set_status(format!("{id} {at}/{count}: {label}")),
    }
    landed.effect
}

/// Open the current stop's file, put the cursor on its focus, and say where the
/// reader now is, with `note` appended when a step has more to report.
///
/// Drift is the whole of that report, since a stop pointing at the wrong code
/// outranks whatever a trail found between it and the last one.
fn jump_to_stop(stoat: &mut Stoat, note: Option<String>) -> UpdateEffect {
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        return UpdateEffect::None;
    };
    let stop = run.current_stop();
    let (focus, id) = (stop.focus.clone(), stop.id.clone());
    let (at, stops) = run.progress();
    let title = stop_title(run);

    let Some(landed) = jump_to_range(stoat, &focus.path, focus.range, &focus.snippet) else {
        return UpdateEffect::Redraw;
    };
    show_narration(stoat, landed.offset);

    match (landed.drifted, note) {
        (true, _) => stoat.set_status(format!("stop {id} drifted from its capture")),
        (false, Some(note)) => stoat.set_status(format!("{at}/{stops}: {title} ({note})")),
        (false, None) => stoat.set_status(format!("{at}/{stops}: {title}")),
    }
    landed.effect
}

/// Where a jump put the reader, and whether the code moved out from under it.
struct Landing {
    offset: usize,
    drifted: bool,
    effect: UpdateEffect,
}

/// Open `path`, put the cursor on `range`'s start, and compare what is there
/// against `captured`.
///
/// A range whose captured bytes no longer match still jumps. The range is the
/// best guide left to where the code went, and a refusal to move strands the
/// reader on the one place that most needs a look.
///
/// `None` when nothing was focused to jump within, which leaves the caller with
/// nothing to report either.
fn jump_to_range(
    stoat: &mut Stoat,
    path: &Path,
    range: walkthrough::Range,
    captured: &str,
) -> Option<Landing> {
    action_handlers::jump::push_jump(stoat);
    let target = stoat.active_workspace().panes.focus();
    action_handlers::file::open_file_in_pane(stoat, target, path);

    let offset = focused_offset_of(stoat, range.start)?;
    let effect = action_handlers::movement::jump_to_offset(stoat, offset);

    Some(Landing {
        offset,
        drifted: drifted(stoat, range, captured),
        effect,
    })
}

/// What to call the current stop, being its own title or the tour's.
///
/// A stop needs no title of its own, and the walkthrough's says more about
/// where the reader is than the narration's first paragraph does.
fn stop_title(run: &WalkthroughRun) -> String {
    run.current_stop()
        .title
        .clone()
        .unwrap_or_else(|| run.walkthrough.title.clone())
}

/// Put the current stop's narration in the hover popup, anchored at `offset`.
///
/// A stop with nothing to say takes the popup down rather than leaving the
/// previous stop's up, so what is on screen always describes where the reader
/// is. The popup itself is the one a hover raises, which is why the next key
/// press dismisses it and [`show_narration_again`] exists to bring it back.
fn show_narration(stoat: &mut Stoat, offset: usize) {
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        return;
    };
    let narration = run.current_stop().narration.clone();
    if narration.trim().is_empty() {
        stoat.pending_hover = None;
        return;
    }

    let (at, stops) = run.progress();
    let heading = format!("{} - {at}/{stops}", stop_title(run));

    let Some((editor_id, _)) = stoat.focused_editor_ids() else {
        return;
    };

    let mut lines = vec![vec![(heading, stoat.theme.get("syntax.markup.title"))]];
    lines.extend(crate::markdown::render_markdown(
        &narration,
        &stoat.theme,
        &stoat.language_registry,
    ));

    stoat.pending_hover = Some(HoverPopup::new(lines, offset, editor_id));
}

/// Lay the call-graph trail between the places `from` and `to`, and say how
/// long it is.
///
/// Two places that call each other, either way round, are worth walking
/// between, and the trail is what walks them. Places with no call relation
/// between them clear the trail rather than leave the last pair's up, since a
/// stale trail claims a connection these two do not have.
///
/// Runs before the jump, so `to`'s file is often still unopened. That is what
/// [`resolve_location_symbol`] reads through the fs host for.
fn install_step_trail(stoat: &mut Stoat, from: &Anchor, to: &Anchor) -> Option<String> {
    let path = {
        let (Some(a), Some(b)) = (
            resolve_location_symbol(stoat, &from.path, from.range.start),
            resolve_location_symbol(stoat, &to.path, to.range.start),
        ) else {
            clear_trail(stoat);
            return None;
        };
        stoat
            .active_workspace()
            .code_graph
            .path_relating(a, b, EdgeKind::Calls)
    };

    let Some(path) = path else {
        clear_trail(stoat);
        return None;
    };

    let stops = path.len();
    nav::install_trail(stoat, &path);
    Some(format!("trail: {stops} stops"))
}

/// Drop any trail, whether a walkthrough laid it or the reader marked it.
fn clear_trail(stoat: &mut Stoat) {
    stoat.active_workspace_mut().trail = None;
}

/// The indexed symbol whose definition encloses `point` of `path`.
///
/// Reads the open buffer when the workspace has one for the path, since that is
/// what the reader sees, and the file on disk when it does not. `None` when the
/// file is unreadable or the point lands outside every indexed definition,
/// which is ordinary for a stop over a comment or a config file.
fn resolve_location_symbol(
    stoat: &Stoat,
    path: &Path,
    point: walkthrough::Point,
) -> Option<SymbolKey> {
    let ws = stoat.active_workspace();
    let absolute = ws.git_root.join(path);
    let point = rope_point(point);

    let offset = match ws.buffers.id_for_path(&absolute) {
        Some(id) => {
            let shared = ws.buffers.get(id)?;
            let guard = shared.read().expect("buffer poisoned");
            guard.snapshot.visible_text.point_to_offset(point)
        },
        None => {
            let text = read_string_via_host(&*stoat.fs_host, &absolute).ok()?;
            Rope::from(text.as_str()).point_to_offset(point)
        },
    };

    let rel = build::relpath(&ws.git_root, &absolute)?;
    ws.code_graph.symbol_at(build::file_id(&rel), offset)
}

/// Byte offset of `point` in the focused buffer.
fn focused_offset_of(stoat: &mut Stoat, point: walkthrough::Point) -> Option<usize> {
    let editor = action_handlers::focused_editor_mut(stoat)?;
    let snapshot = editor.display_map.snapshot();
    Some(
        snapshot
            .buffer_snapshot()
            .rope()
            .point_to_offset(rope_point(point)),
    )
}

/// A stored point as the rope counts them.
///
/// The stored form counts lines and byte columns from one, where the rope
/// counts both from zero.
fn rope_point(point: walkthrough::Point) -> stoat_text::Point {
    stoat_text::Point::new(point.line.saturating_sub(1), point.col.saturating_sub(1))
}

/// Whether the focused buffer no longer holds the bytes `captured` over `range`.
///
/// Read through [`walkthrough::snippet_for`] rather than sliced here, so a stop
/// the player calls drifted is exactly one `stoat walkthrough check` reports.
fn drifted(stoat: &mut Stoat, range: walkthrough::Range, captured: &str) -> bool {
    let Some(editor) = action_handlers::focused_editor_mut(stoat) else {
        return false;
    };
    let content = editor
        .display_map
        .snapshot()
        .buffer_snapshot()
        .rope()
        .to_string();

    !walkthrough::snippet_for(&content, range).is_ok_and(|found| found == captured)
}

#[cfg(test)]
mod tests {
    use super::{done, next, next_annotation, open, prev, prev_annotation, show_narration_again};
    use crate::{
        action_handlers,
        app::Stoat,
        code_index::{build, nav},
        host::FakeFs,
        walkthrough::{Location, Point, Range, Walkthrough},
    };
    use codegraph::{Confidence, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey, Target};
    use std::{ops::Range as ByteRange, path::PathBuf, sync::Arc};
    use stoat_config::Settings;
    use stoat_language::SymbolKind;
    use stoat_scheduler::TestScheduler;

    const FIRST: &str = "fn one() {}\nfn two() {}\n";
    const SECOND: &str = "fn three() {}\n";
    /// Stop 1's narration. Stop 2 has none, so one tour covers both cases.
    const NARRATION: &str = "The **entry** point.";

    fn range_of(line: u32, cols: (u32, u32)) -> Range {
        Range {
            start: Point { line, col: cols.0 },
            end: Point { line, col: cols.1 },
        }
    }

    fn location(path: &str, line: u32, cols: (u32, u32), snippet: &str) -> Location {
        Location {
            path: PathBuf::from(path),
            range: range_of(line, cols),
            snippet: snippet.to_owned(),
        }
    }

    /// A workspace holding a two-stop tour over `a.rs` and `b.rs`, with `first`
    /// as the content of `a.rs`, which is how a test puts a stop out of date.
    fn stoat_with_tour(first: &str) -> Stoat {
        let scheduler = Arc::new(TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;

        let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
        walkthrough
            .add_stop(
                Some("first".to_owned()),
                NARRATION.to_owned(),
                location("a.rs", 2, (1, 11), "fn two() {}"),
                None,
            )
            .expect("append");
        walkthrough
            .add_stop(
                Some("second".to_owned()),
                String::new(),
                location("b.rs", 1, (1, 13), "fn three() {}"),
                None,
            )
            .expect("append");

        // Stop 1 calls out the callee it reaches in b.rs, then a neighbor in
        // its own file, so one walk covers a cross-file hop and a same-file one.
        walkthrough
            .add_annotation(
                "s1",
                Some(PathBuf::from("b.rs")),
                range_of(1, (1, 13)),
                "fn three() {}".to_owned(),
                "the callee".to_owned(),
            )
            .expect("s1 exists");
        walkthrough
            .add_annotation(
                "s1",
                None,
                range_of(1, (1, 11)),
                "fn one() {}".to_owned(),
                "the neighbor".to_owned(),
            )
            .expect("s1 exists");

        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/a.rs", first);
        fs.insert_file("/repo/b.rs", SECOND);
        fs.insert_file(
            "/repo/.stoat/walkthroughs/tour.json",
            serde_json::to_string(&walkthrough).expect("serialize"),
        );
        stoat.set_fs_host(fs);
        stoat
    }

    /// Where the cursor sits, as `(file, offset)`.
    fn cursor(stoat: &mut Stoat) -> (String, usize) {
        let (buffer_id, offset) = {
            let editor = action_handlers::focused_editor_mut(stoat).expect("a focused editor");
            let snapshot = editor.display_map.snapshot();
            let buffer = snapshot.buffer_snapshot();

            let selection = editor.selections.newest_anchor();
            let tail = buffer.resolve_anchor(&selection.tail());
            let head = buffer.resolve_anchor(&selection.head());

            (
                editor.buffer_id,
                stoat_text::cursor_offset(buffer.rope(), tail, head),
            )
        };

        let path = stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        (path, offset)
    }

    /// The two symbols the tour's stops focus on, `two` in `a.rs` calling
    /// `three` in `b.rs`.
    fn indexed_symbols() -> [(u8, FileId, &'static str, ByteRange<usize>); 2] {
        [
            (1, build::file_id("a.rs"), "two", 12..23),
            (2, build::file_id("b.rs"), "three", 0..13),
        ]
    }

    /// Index both stops' symbols, with the call edge between them only when
    /// `calls` is set, which is how a test picks a related or unrelated pair.
    fn index_the_tour(stoat: &mut Stoat, calls: bool) {
        let symbols = indexed_symbols();
        let keys: Vec<SymbolKey> = symbols
            .iter()
            .map(|(id, ..)| SymbolKey([*id; 16]))
            .collect();

        for (index, (id, file, name, def_range)) in symbols.into_iter().enumerate() {
            let edges = match calls && index == 0 {
                true => vec![Edge {
                    from: keys[0],
                    to: Target::Sym(keys[1]),
                    kind: EdgeKind::Calls,
                    site_range: def_range.clone(),
                    confidence: Confidence::Resolved,
                }],
                false => Vec::new(),
            };

            stoat
                .active_workspace_mut()
                .code_graph
                .insert_shard(FileShard {
                    content_hash: [0u8; 32],
                    symbols: vec![Symbol {
                        key: SymbolKey([id; 16]),
                        file,
                        name: name.to_owned(),
                        kind: SymbolKind::Function,
                        container: vec![],
                        def_range,
                        name_range: 0..1,
                        body_hash: [0u8; 32],
                    }],
                    edges,
                });
        }
    }

    /// Where the active trail sits and how long it is, or `None` for no trail.
    fn trail_progress(stoat: &Stoat) -> Option<(usize, usize)> {
        stoat
            .active_workspace()
            .trail
            .as_ref()
            .and_then(|trail| trail.progress())
    }

    /// The popup's text, one string per line, with the styling dropped.
    fn popup_lines(stoat: &Stoat) -> Vec<String> {
        stoat
            .pending_hover
            .as_ref()
            .map(|popup| {
                popup
                    .lines
                    .iter()
                    .map(|line| {
                        line.iter()
                            .map(|(text, _)| text.as_str())
                            .collect::<String>()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn open_lands_on_the_first_stops_focus() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");

        assert_eq!(
            cursor(&mut stoat),
            ("/repo/a.rs".to_owned(), 12),
            "stop 1 focuses line 2 of a.rs, which starts at byte 12",
        );
        assert_eq!(stoat.pending_message.as_deref(), Some("1/2: first"));
    }

    #[test]
    fn stepping_walks_the_stops_and_clamps() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");

        next(&mut stoat);
        assert_eq!(cursor(&mut stoat), ("/repo/b.rs".to_owned(), 0));
        assert_eq!(stoat.pending_message.as_deref(), Some("2/2: second"));

        next(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("already on the last stop"),
            "there is nothing past the end to jump to",
        );

        prev(&mut stoat);
        assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 12));

        prev(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("already on the first stop")
        );
    }

    #[test]
    fn arriving_shows_the_narration_under_a_title_line() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");

        assert_eq!(
            popup_lines(&stoat),
            ["first - 1/2", "The entry point."],
            "the title line names the stop and where it sits, then the markdown",
        );

        let popup = stoat.pending_hover.as_ref().expect("a popup");
        assert_eq!(
            popup.anchor_offset, 12,
            "the popup sits at the focus the cursor landed on",
        );
    }

    /// Every popup on screen describes the stop the reader is on, so a stop
    /// with nothing to say takes the previous one's down.
    #[test]
    fn a_stop_with_no_narration_shows_no_popup() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");
        next(&mut stoat);

        assert!(stoat.pending_hover.is_none());
    }

    #[test]
    fn the_narration_shows_again_after_a_dismissal() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");
        stoat.pending_hover = None;

        show_narration_again(&mut stoat);
        assert_eq!(popup_lines(&stoat), ["first - 1/2", "The entry point."]);

        next(&mut stoat);
        show_narration_again(&mut stoat);
        assert!(stoat.pending_hover.is_none());
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("this stop has no narration"),
            "a stop with nothing to re-show says so",
        );
    }

    #[test]
    fn stepping_between_related_stops_lays_the_trail() {
        let mut stoat = stoat_with_tour(FIRST);
        index_the_tour(&mut stoat, true);
        open(&mut stoat, "tour");

        assert_eq!(trail_progress(&stoat), None, "the first stop follows none");

        next(&mut stoat);
        assert_eq!(
            trail_progress(&stoat),
            Some((1, 2)),
            "the trail runs from the caller to the callee, sitting on the first",
        );
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("2/2: second (trail: 2 stops)"),
            "the stop the reader landed on comes first, then what connects it",
        );
    }

    /// A trail between the last pair of stops says nothing true about this
    /// pair, so an unrelated step takes it down rather than leaving it up.
    #[test]
    fn stepping_between_unrelated_stops_clears_the_trail() {
        let mut stoat = stoat_with_tour(FIRST);
        index_the_tour(&mut stoat, false);
        open(&mut stoat, "tour");
        nav::install_trail(&mut stoat, &[SymbolKey([9u8; 16]), SymbolKey([8u8; 16])]);

        next(&mut stoat);
        assert_eq!(trail_progress(&stoat), None);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("2/2: second"),
            "with no trail to report, the stop status stands alone",
        );
    }

    #[test]
    fn annotation_stepping_walks_out_from_the_focus_and_back() {
        let mut stoat = stoat_with_tour(FIRST);
        index_the_tour(&mut stoat, true);
        open(&mut stoat, "tour");

        prev_annotation(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("already on the stop"),
            "the focus heads the walk, so nothing precedes it",
        );

        next_annotation(&mut stoat);
        assert_eq!(
            cursor(&mut stoat),
            ("/repo/b.rs".to_owned(), 0),
            "the first annotation names a file of its own",
        );
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("a1 1/2: the callee (trail: 2 stops)"),
            "the cross-file hop lays the trail between the two symbols",
        );
        assert_eq!(trail_progress(&stoat), Some((1, 2)));

        next_annotation(&mut stoat);
        assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 0));
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("a2 2/2: the neighbor"),
            "an unindexed end lays no trail, so the status stands alone",
        );
        assert_eq!(trail_progress(&stoat), None);

        next_annotation(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("already on the last annotation")
        );

        prev_annotation(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("a1 1/2: the callee"),
            "stepping back off an unindexed annotation lays no trail either",
        );

        prev_annotation(&mut stoat);
        assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 12));
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("1/2: first (trail: 2 stops)"),
            "back past the first annotation reads as the stop, still related",
        );
    }

    /// The annotations belong to the stop, so leaving it leaves them.
    #[test]
    fn a_stop_step_returns_to_the_focus() {
        let mut stoat = stoat_with_tour(FIRST);
        index_the_tour(&mut stoat, true);
        open(&mut stoat, "tour");
        next_annotation(&mut stoat);

        next(&mut stoat);
        prev(&mut stoat);
        assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 12));
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("1/2: first (trail: 2 stops)"),
            "the stop reads as an arrival, not as the annotation it left from",
        );
    }

    #[test]
    fn a_stop_with_no_annotations_says_so() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");
        next(&mut stoat);

        next_annotation(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("this stop has no annotations")
        );
    }

    #[test]
    fn done_ends_the_walkthrough() {
        let mut stoat = stoat_with_tour(FIRST);
        index_the_tour(&mut stoat, true);
        open(&mut stoat, "tour");
        next(&mut stoat);
        done(&mut stoat);

        assert!(stoat.active_workspace().walkthrough.is_none());
        assert_eq!(
            trail_progress(&stoat),
            None,
            "the trail belonged to the tour that just ended",
        );
        assert_eq!(stoat.pending_message.as_deref(), Some("walkthrough closed"));

        next(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("no walkthrough is playing"),
            "the actions say so rather than doing nothing at all",
        );
    }

    #[test]
    fn an_unknown_slug_installs_nothing() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "missing");

        assert!(stoat.active_workspace().walkthrough.is_none());
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("no walkthrough 'missing'")
        );
    }

    /// The stop that drifted is the one most worth looking at, so the jump
    /// still happens and the report rides along with it.
    #[test]
    fn a_drifted_stop_still_jumps() {
        let mut stoat = stoat_with_tour("fn one() {}\nfn TWO() {}\n");
        open(&mut stoat, "tour");

        assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 12));
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("stop s1 drifted from its capture")
        );
    }
}
