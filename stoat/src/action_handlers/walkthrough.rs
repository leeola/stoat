use crate::{
    action_handlers::{
        self, read_string_via_host,
        review_walk::{WalkEntry, WalkLandingKind},
    },
    app::{Stoat, UpdateEffect},
    buffer_registry::OpenOrigin,
    code_index::{build, nav},
    editor_state::EditorState,
    host::CommitInfo,
    render::{
        hover::{HoverFrame, HoverPopup},
        text::text_width,
        walkthrough::{SlideParts, EXIT_MS},
    },
    review_walk::{WalkOrigin, WalkSpan},
    walkthrough::{
        self,
        run::{part, WalkthroughRun},
        store,
    },
};
use codegraph::{EdgeKind, SymbolKey};
use ratatui::{layout::Rect, style::Style};
use std::{
    ops::RangeInclusive,
    path::{Path, PathBuf},
    time::Duration,
};
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
///
/// A tour whose stops name commits plays as a review walk over those commits,
/// and the first landing makes the jump. See [`open_commit_tour`].
pub fn open(stoat: &mut Stoat, slug: &str) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();

    let loaded = match store::load(stoat.fs_host.as_ref(), &git_root, slug) {
        Ok(walkthrough) => walkthrough,
        Err(error) => {
            stoat.set_status(format!("{error}"));
            return UpdateEffect::Redraw;
        },
    };

    stoat.walkthrough_runs_opened = stoat.walkthrough_runs_opened.wrapping_add(1);
    let Some(run) = WalkthroughRun::new(loaded, stoat.walkthrough_runs_opened) else {
        stoat.set_status(format!("walkthrough '{slug}' has no stops"));
        return UpdateEffect::Redraw;
    };

    if run.spans_commits() {
        return open_commit_tour(stoat, run, &git_root);
    }

    stoat.active_workspace_mut().walkthrough = Some(run);
    clear_trail(stoat);
    jump_to_stop(stoat, None)
}

/// Play `run`, whose stops name commits, as a review walk over those commits.
///
/// The walk takes each commit once, in the order the stops first name them,
/// which is the order a reader meets them. It checks the first one out, and
/// that landing jumps to the stop, so nothing jumps here.
///
/// A tour that plays over its own walk hands HEAD back through that walk's
/// return first, so the new walk records the ref the return restores. Any other
/// open walk is the reader's own and refuses the tour. So does a commit the
/// repository lacks. Each refusal keeps the tour that plays.
fn open_commit_tour(stoat: &mut Stoat, run: WalkthroughRun, git_root: &Path) -> UpdateEffect {
    let walking = stoat.active_workspace().review_walk.is_some();
    let tour_walks = stoat
        .active_workspace()
        .walkthrough
        .as_ref()
        .is_some_and(WalkthroughRun::spans_commits);
    if walking && !tour_walks {
        stoat.set_status("a review walk is open; :review-done first");
        return UpdateEffect::Redraw;
    }

    let Some((repo, workdir)) = stoat
        .git_host
        .discover(git_root)
        .and_then(|repo| repo.workdir().map(|workdir| (repo, workdir)))
    else {
        stoat.set_status("not in a git repository");
        return UpdateEffect::Redraw;
    };
    let mut commits: Vec<CommitInfo> = Vec::new();
    for sha in run
        .walkthrough
        .stops
        .iter()
        .filter_map(|stop| stop.commit.as_deref())
    {
        if commits.iter().any(|commit| commit.sha == sha) {
            continue;
        }
        let Some(commit) = repo.log_from(sha, 1).pop() else {
            stoat.set_status(format!("commit {sha:.7} is not in this repository"));
            return UpdateEffect::Redraw;
        };
        commits.push(commit);
    }

    if walking {
        super::review_walk::review_done(stoat);
        // A return still out refuses the end, and leaves this tour and its walk
        // where they are.
        if stoat.active_workspace().review_walk.is_some() {
            return UpdateEffect::Redraw;
        }
    }

    let first = commits[0].sha.clone();
    stoat.active_workspace_mut().walkthrough = Some(run);
    clear_trail(stoat);
    super::review_walk::queue_walk_start(
        stoat,
        workdir,
        commits,
        WalkLandingKind::Walkthrough,
        WalkSpan::Commits,
        WalkEntry::default(),
        WalkOrigin::Commits,
    );
    stoat.set_status(format!("checking out {first:.7}"));
    UpdateEffect::Redraw
}

/// Step forward `count` stops.
pub fn next(stoat: &mut Stoat, count: i32) -> UpdateEffect {
    step(stoat, count)
}

/// Step back `count` stops.
pub fn prev(stoat: &mut Stoat, count: i32) -> UpdateEffect {
    step(stoat, -count)
}

/// Step forward `count` annotations of the current stop.
pub fn next_annotation(stoat: &mut Stoat, count: i32) -> UpdateEffect {
    step_annotation(stoat, count)
}

/// Step back `count` places toward the stop's focus through its annotations.
pub fn prev_annotation(stoat: &mut Stoat, count: i32) -> UpdateEffect {
    step_annotation(stoat, -count)
}

/// Step forward `count` attention points of the tour, read as one sequence.
pub fn forward(stoat: &mut Stoat, count: i32) -> UpdateEffect {
    step_linear(stoat, count)
}

/// Step back `count` attention points of the tour, exactly inverting
/// [`forward`].
pub fn backward(stoat: &mut Stoat, count: i32) -> UpdateEffect {
    step_linear(stoat, -count)
}

/// Take the tour's card down, or raise the narration again.
///
/// The card is pinned, so it stays through everything but a deliberate
/// dismissal. That leaves one key to both hide it when it is in the way and
/// bring it back afterward, which is what this is.
pub fn show_narration_again(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().walkthrough.is_none() {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    }

    // A card that is up is the tour's, so the reader asking again wants it out
    // of the way rather than redrawn where it already is.
    if stoat
        .pending_hover
        .as_ref()
        .is_some_and(|popup| popup.pinned)
    {
        stoat.pending_hover = None;
        stoat.pending_hover_request.clear();
        return UpdateEffect::Redraw;
    }

    if !refresh_narration(stoat) {
        return UpdateEffect::None;
    }

    if stoat.pending_hover.is_none() {
        stoat.set_status("this stop has no narration");
    }
    UpdateEffect::Redraw
}

/// Raise the tour's description as a card headed by the tour's title.
///
/// A card that is up gives way rather than going down, since the reader asked
/// for a different card than the one on screen. The next step puts the
/// narration back.
pub fn show_description(stoat: &mut Stoat) -> UpdateEffect {
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    };

    let tour = &run.walkthrough;
    if tour.description.trim().is_empty() {
        let message = format!("walkthrough '{}' has no description", tour.slug);
        stoat.set_status(message);
        return UpdateEffect::Redraw;
    }
    let title = tour.title.clone();
    let description = tour.description.clone();
    let point = current_anchor(run).range.start;

    let Some(offset) =
        action_handlers::focused_editor_mut(stoat).map(|editor| offset_of(editor, point))
    else {
        return UpdateEffect::None;
    };
    raise_card(stoat, title, &description, offset);
    UpdateEffect::Redraw
}

/// End the walkthrough, leaving the reader wherever it put them.
///
/// The trail goes with it. It was laid between two stops of a tour that is
/// over, and nothing else put it there.
///
/// A tour over commits ends its walk as well, which checks the ref the walk
/// started from back out. The walk's end closes the tour.
pub fn done(stoat: &mut Stoat) -> UpdateEffect {
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    };
    if run.spans_commits() && stoat.active_workspace().review_walk.is_some() {
        return super::review_walk::review_done(stoat);
    }

    stoat.active_workspace_mut().walkthrough = None;
    clear_trail(stoat);
    stoat.set_status("walkthrough closed");
    UpdateEffect::Redraw
}

/// Put the reader on the current stop once the walk has checked `landed` out,
/// with the stop's file showing its diff against the commit's parent.
///
/// When a step taken during the checkout moved the tour onto another commit,
/// the walk's cursor follows the tour instead. The landing then queues the
/// checkout for the cursor, so the reader arrives once, on the commit the stop
/// names.
///
/// A stop that names no commit arrives on whatever the walk checked out.
pub(super) fn arrive_at_commit(stoat: &mut Stoat, landed: &str) {
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        return;
    };
    if run.current_commit().is_some_and(|commit| commit != landed) {
        if let Some(index) = commit_index(stoat)
            && let Some(walk) = stoat.active_workspace_mut().review_walk.as_mut()
        {
            walk.seek(index);
        }
        return;
    }

    match run.current_annotation().is_some() {
        true => jump_to_annotation(stoat, None),
        false => jump_to_stop(stoat, None),
    };
    // The jump's own latch check skips a buffer the pane already shows, and
    // the first landing finds the pane not yet latched.
    super::review::latch_diff_view(stoat);

    // The latch lays the diff out anew, so the landing frames its range again
    // against the rows the reader sees.
    let range = current_anchor(run_of(stoat)).range;
    frame_range(stoat, range, false);
}

/// Close a tour over commits whose walk ended or never started.
///
/// The slide, the trail, and the narration card go with it, since each
/// describes a stop of a tour that has ended.
pub(super) fn abandon_commit_tour(stoat: &mut Stoat) {
    retire_slide(stoat);
    clear_trail(stoat);
    stoat.pending_hover = None;
    stoat.active_workspace_mut().walkthrough = None;
    stoat.set_status("walkthrough closed");
}

/// Raise the current stop's narration, reporting whether the stop's location
/// resolved into the focused buffer.
///
/// A caller with no popup to show afterwards knows the stop is silent rather
/// than unreachable, which is the difference the returned flag carries.
///
/// The caller must have checked that a walkthrough is playing.
fn refresh_narration(stoat: &mut Stoat) -> bool {
    let point = current_anchor(run_of(stoat)).range.start;

    let Some(offset) =
        action_handlers::focused_editor_mut(stoat).map(|editor| offset_of(editor, point))
    else {
        return false;
    };
    show_narration(stoat, offset);
    true
}

/// Hand the stop the reader is leaving over to be un-drawn.
///
/// Its parts are re-declared with the exit phase for a moment, then dropped. A
/// slide that simply stopped being re-declared would vanish between two frames,
/// which reads as a glitch rather than as the reader moving on.
///
/// A step that stayed put retires nothing. The marks are the same marks, and
/// un-drawing them only to draw them again reads as a flicker.
fn retire_slide(stoat: &mut Stoat) {
    let parts = match stoat.active_workspace_mut().walkthrough.as_mut() {
        Some(run) => {
            // The arriving slide opens on its own schedule. When the parts of
            // the slide being left last went out says nothing about it.
            run.last_declared.clear();
            std::mem::take(&mut run.last_parts)
        },
        None => return,
    };
    start_exit(stoat, parts);
}

/// Start `parts` running back off with the exit stroke.
///
/// An exit still running is replaced, and its parts go at once. Empty parts
/// start nothing, so they never cut a running exit short.
fn start_exit(stoat: &mut Stoat, parts: SlideParts) {
    if parts.marks.is_empty() && parts.runs.is_empty() {
        return;
    }

    let now = stoat.executor.now();
    stoat.active_workspace_mut().walkthrough_exit = Some((parts, now));

    // An exit that ends while nothing else asks for a frame leaves the retired
    // parts on screen until the next key press, so a timer retires them on an
    // idle screen.
    let exit = Duration::from_millis(u64::from(EXIT_MS));
    let timer = stoat.executor.timer(exit);
    stoat.walkthrough_exit_timer = Some(stoat.spawn_woken(async move {
        timer.await;
    }));
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
    let from_commit = run.current_commit().map(str::to_owned);

    if !run.step(delta) {
        let end = if delta < 0 { "first" } else { "last" };
        stoat.set_status(format!("already on the {end} stop"));
        refresh_narration(stoat);
        return UpdateEffect::Redraw;
    }
    if let Some(effect) = cross_commit(stoat, from_commit.as_deref()) {
        return effect;
    }

    let to = current_anchor(run_of(stoat));
    retire_slide(stoat);
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
    let from_at = run.annotation_progress().map(|(at, _)| at - 1);

    if run.current_stop().annotations.is_empty() {
        stoat.set_status("this stop has no annotations");
        refresh_narration(stoat);
        return UpdateEffect::Redraw;
    }

    if !run.step_annotation(delta) {
        let end = if delta < 0 { "stop" } else { "last annotation" };
        stoat.set_status(format!("already on the {end}"));
        refresh_narration(stoat);
        return UpdateEffect::Redraw;
    }

    let to = current_anchor(run_of(stoat));
    let to_at = run_of(stoat).annotation_progress().map(|(at, _)| at - 1);
    // An annotation step within one file leaves the same marks on the same
    // code, so they stay: un-drawing them to draw them again reads as a
    // flicker. A step back takes the callout it leaves with it. A step into
    // another file leaves marks that no longer describe what is on screen.
    if to.path != from.path {
        retire_slide(stoat);
    } else {
        step_callouts(stoat, from_at, to_at);
    }
    let note = install_step_trail(stoat, &from, &to);

    match run_of(stoat).current_annotation().is_some() {
        true => jump_to_annotation(stoat, note),
        false => jump_to_stop(stoat, note),
    }
}

/// Move `delta` attention points along the whole tour, lay the trail between
/// the two, and jump to where that lands.
///
/// The tour reads as one sequence here rather than as a stop walk with an
/// in-stop walk beside it, which is what lets one gesture carry a reader
/// through every point of it in order.
fn step_linear(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(run) = stoat.active_workspace_mut().walkthrough.as_mut() else {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    };
    let from = current_anchor(run);
    let from_stop = run.current_stop().id.clone();
    let from_commit = run.current_commit().map(str::to_owned);
    let from_at = run.annotation_progress().map(|(at, _)| at - 1);

    if !run.step_linear(delta) {
        let end = if delta < 0 { "start" } else { "end" };
        stoat.set_status(format!("already at the {end} of the tour"));
        refresh_narration(stoat);
        return UpdateEffect::Redraw;
    }
    if let Some(effect) = cross_commit(stoat, from_commit.as_deref()) {
        return effect;
    }

    let to = current_anchor(run_of(stoat));
    let to_at = run_of(stoat).annotation_progress().map(|(at, _)| at - 1);
    // A point on the same stop and the same file leaves the same marks on the
    // same code, so they stay: un-drawing them to draw them again reads as a
    // flicker. A step back takes the callout it leaves with it. A new stop, or
    // another file, leaves marks that no longer describe what is on screen.
    if run_of(stoat).current_stop().id != from_stop || to.path != from.path {
        retire_slide(stoat);
    } else {
        step_callouts(stoat, from_at, to_at);
    }
    let note = install_step_trail(stoat, &from, &to);

    match run_of(stoat).current_annotation().is_some() {
        true => jump_to_annotation(stoat, note),
        false => jump_to_stop(stoat, note),
    }
}

/// Hand a step that crossed onto another commit to the walk, whose landing
/// jumps once that commit is checked out.
///
/// `None` when the step stayed on `from`'s commit, or landed on a stop that
/// reads the working tree, so the caller jumps as usual.
///
/// Before the walk starts, the step only moves the tour. The walk's first
/// landing then seeks the commit the tour stands on by that time.
fn cross_commit(stoat: &mut Stoat, from: Option<&str>) -> Option<UpdateEffect> {
    let commit = run_of(stoat).current_commit()?.to_owned();
    if from == Some(commit.as_str()) {
        return None;
    }

    retire_slide(stoat);
    clear_trail(stoat);
    stoat.set_status(format!("checking out {commit:.7}"));
    Some(match commit_index(stoat) {
        Some(index) => super::review_walk::walk_seek(stoat, index),
        None => UpdateEffect::Redraw,
    })
}

/// Where the walk holds the current stop's commit, or `None` when the stop
/// reads the working tree or no walk has started yet.
fn commit_index(stoat: &Stoat) -> Option<usize> {
    let ws = stoat.active_workspace();
    let commit = ws.walkthrough.as_ref()?.current_commit()?;
    ws.review_walk
        .as_ref()?
        .commits
        .iter()
        .position(|walked| walked.sha == commit)
}

/// Retire the callouts a step within one slide leaves behind, and take back
/// from a running exit the ones it reaches again.
///
/// A callout reached again before its exit ends draws under the same ids. Left
/// in the exit as well, every frame declares it once per phase, and the
/// terminal restarts a stroke whenever its phase changes.
fn step_callouts(stoat: &mut Stoat, from_at: Option<usize>, to_at: Option<usize>) {
    let past = |at: Option<usize>| at.map_or(0, |at| at + 1);
    match (from_at, to_at) {
        (Some(from), to) if to < from_at => retire_annotations(stoat, past(to)..=from),
        (from, Some(to)) if from < to_at => reclaim_annotations(stoat, past(from)..=to),
        _ => {},
    }
}

/// Run the callouts of annotations `keys` back off, and leave the rest of the
/// slide up.
///
/// Their ids leave the declared record as well, so a step that reaches them
/// again draws them as a reveal rather than as parts still on screen.
fn retire_annotations(stoat: &mut Stoat, keys: RangeInclusive<usize>) {
    let parts = {
        let Some(run) = stoat.active_workspace_mut().walkthrough.as_mut() else {
            return;
        };
        let ids = annotation_part_ids(run, keys);
        run.last_declared.retain(|id, _| !ids.contains(id));

        SlideParts {
            marks: run
                .last_parts
                .marks
                .extract_if(.., |mark| ids.contains(&mark.id))
                .collect(),
            runs: run
                .last_parts
                .runs
                .extract_if(.., |text| ids.contains(&text.follow))
                .collect(),
        }
    };
    start_exit(stoat, parts);
}

/// Take the callouts of annotations `keys` back out of a running exit.
fn reclaim_annotations(stoat: &mut Stoat, keys: RangeInclusive<usize>) {
    let ids = annotation_part_ids(run_of(stoat), keys);
    if let Some((parts, _)) = stoat.active_workspace_mut().walkthrough_exit.as_mut() {
        parts.marks.retain(|mark| !ids.contains(&mark.id));
        parts.runs.retain(|text| !ids.contains(&text.follow));
    }
}

/// The connector and label ids of annotations `keys`.
fn annotation_part_ids(run: &WalkthroughRun, keys: RangeInclusive<usize>) -> Vec<u32> {
    keys.flat_map(|key| {
        let (link, label) = run.annotation_ids(key);
        [link, label]
    })
    .collect()
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
    let commit = commit_suffix(run);

    let Some(landed) = jump_to_range(stoat, &anchor.path, anchor.range, &anchor.snippet) else {
        return UpdateEffect::Redraw;
    };
    show_narration(stoat, landed.offset);

    match (landed.drifted, note) {
        (true, _) => stoat.set_status(format!("{id} drifted from its capture")),
        (false, Some(note)) => {
            stoat.set_status(format!("{id} {at}/{count}: {label}{commit} ({note})"))
        },
        (false, None) => stoat.set_status(format!("{id} {at}/{count}: {label}{commit}")),
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
    // A tour over commits latches on its own landing, and a stop without the
    // flag leaves the view as the reader had it.
    let latch = stop.diff && !run.spans_commits();
    let (at, stops) = run.progress();
    let title = stop_title(run);
    let commit = commit_suffix(run);

    let Some(landed) = jump_to_range(stoat, &focus.path, focus.range, &focus.snippet) else {
        return UpdateEffect::Redraw;
    };
    if latch {
        super::review::latch_diff_view(stoat);
        // The latch lays the diff out anew, so the landing frames its range
        // again against the rows the reader sees.
        frame_range(stoat, focus.range, false);
    }
    show_narration(stoat, landed.offset);

    match (landed.drifted, note) {
        (true, _) => stoat.set_status(format!("stop {id} drifted from its capture")),
        (false, Some(note)) => stoat.set_status(format!("{at}/{stops}: {title}{commit} ({note})")),
        (false, None) => stoat.set_status(format!("{at}/{stops}: {title}{commit}")),
    }
    landed.effect
}

/// ` @ <short sha>` for a stop that reads against a commit, so the status says
/// which commit the reader is on. Empty for a working-tree stop.
fn commit_suffix(run: &WalkthroughRun) -> String {
    run.current_commit()
        .map(|sha| format!(" @ {sha:.7}"))
        .unwrap_or_default()
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
    let buffer_before = action_handlers::focused_editor_mut(stoat).map(|editor| editor.buffer_id);
    // A stop in another file focuses another editor, which carries its own
    // mode. The mode belongs to the reader rather than to whichever buffer a
    // jump puts under them, so a tour read in walkthrough mode keeps it.
    let mode = stoat.focused_mode().to_owned();
    crate::buffer_lifecycle::open_file_in_pane(stoat, target, path, OpenOrigin::Visited);
    stoat.set_focused_mode(mode);

    let offset =
        action_handlers::focused_editor_mut(stoat).map(|editor| offset_of(editor, range.start))?;
    let effect = action_handlers::movement::jump_to_offset(stoat, offset);
    // Landing in another file shows an editor with no earlier view to glide
    // from, so it snaps.
    let same_buffer =
        action_handlers::focused_editor_mut(stoat).map(|editor| editor.buffer_id) == buffer_before;
    frame_range(stoat, range, same_buffer);

    Some(Landing {
        offset,
        drifted: action_handlers::focused_editor_mut(stoat)
            .is_some_and(|editor| drifted(editor, range, captured)),
        effect,
    })
}

/// Frame `range`'s lines in the focused editor, so a stop or annotation lands
/// whole on screen when it fits.
fn frame_range(stoat: &mut Stoat, range: walkthrough::Range, glide: bool) {
    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    if let Some(editor) = action_handlers::focused_editor_mut(stoat) {
        frame_lines(editor, range, scrolloff, glide);
    }
}

/// Frame `range`'s lines in `editor`, as a jump to a declaration frames its
/// block.
///
/// The end column is one-based and inclusive, so the block runs to the start
/// of the line after the last and frames that last line whole. The frame
/// works from the editor's cursor, so a caller that moves the selection does
/// that first.
pub(crate) fn frame_lines(
    editor: &mut EditorState,
    range: walkthrough::Range,
    scrolloff: u32,
    glide: bool,
) {
    let block = {
        let snapshot = editor.display_map.snapshot();
        let rope = snapshot.buffer_snapshot().rope();
        let line_start = |row: u32| match row > rope.max_point().row {
            true => rope.len(),
            false => rope.point_to_offset(stoat_text::Point::new(row, 0)),
        };
        line_start(range.start.line.saturating_sub(1))..line_start(range.end.line)
    };
    action_handlers::view::frame_jump_on_block(editor, block, scrolloff, glide);
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

/// Narrowest and widest the card gets, in cells.
///
/// Below the floor a wrapped line is more break than text. The ceiling and the
/// half-pane bound below keep the card from becoming the screen.
pub const CARD_MIN_WIDTH: u16 = 24;
pub const CARD_MAX_WIDTH: u16 = 52;

/// The protocol version that decodes a sketch. An older stoatty ignores the
/// frames, so the card would draw no border at all.
const SKETCH_PROTOCOL: u32 = 3;

/// Put the narration for where the reader is in the hover popup, anchored at
/// `offset`.
///
/// A position with nothing to say takes the popup down rather than leaving the
/// last one's up, so what is on screen always describes where the reader is.
fn show_narration(stoat: &mut Stoat, offset: usize) {
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        return;
    };
    let Some((heading, narration)) = card_text(run) else {
        stoat.pending_hover = None;
        return;
    };

    raise_card(stoat, heading, &narration, offset);
}

/// Raise a card of `heading` over the rendered `markdown`, anchored at
/// `offset`.
///
/// The card takes the place of whatever popup is up. It is pinned, so an
/// incidental key press leaves it up, and it sits where the slide puts it
/// rather than beside the cursor.
fn raise_card(stoat: &mut Stoat, heading: String, markdown: &str, offset: usize) {
    let Some((editor_id, _)) = stoat.focused_editor_ids() else {
        return;
    };

    // The card is one part of the slide, so it takes its id from the same
    // scheme the marks do. A card re-declared on every frame of one stop must
    // not restart its stroke.
    let Some(card_id) = stoat
        .active_workspace()
        .walkthrough
        .as_ref()
        .map(|run| run.part_id(part::CARD))
    else {
        return;
    };

    let mut lines = vec![vec![(heading, stoat.theme.get("syntax.markup.title"))]];
    lines.extend(crate::markdown::render_markdown(
        markdown,
        &stoat.theme,
        &stoat.language_registry,
    ));

    // Wrapped to the card rather than clipped to it, since a narration is prose
    // and a clipped sentence loses its end. The width is settled first, because
    // the wrap needs it and the height falls out of the result.
    let frame = stoat.size();
    let width = card_width(&lines, frame.width);
    let inner_width = usize::from(width.saturating_sub(2));
    let wrapped: Vec<Vec<(String, Style)>> = lines
        .iter()
        .flat_map(|line| crate::render::text::wrap_styled(line, inner_width))
        .collect();
    let height = (wrapped.len() as u16 + 2)
        .min(frame.height.saturating_sub(2))
        .max(3);

    let mut popup = HoverPopup::new(wrapped, offset, editor_id);
    popup.pinned = true;
    popup.placement = Some(Rect {
        x: frame.x + frame.width.saturating_sub(width + 1),
        y: frame.y + 1,
        width,
        height,
    });
    popup.frame = card_frame(stoat, card_id);
    stoat.pending_hover = Some(popup);
}

/// The heading and the markdown the card shows for where the reader is, or
/// `None` when this position has nothing to say.
///
/// An annotation that narrates speaks for itself, since it is the sub-step the
/// reader stepped onto. One that does not leaves the stop's card standing, so a
/// tour written before annotations carried narration reads as it always did.
fn card_text(run: &WalkthroughRun) -> Option<(String, String)> {
    let narrated = run
        .current_annotation()
        .filter(|annotation| !annotation.narration.trim().is_empty());

    if let Some(annotation) = narrated {
        let (at, count) = run.annotation_progress()?;
        return Some((
            format!("{} - {at}/{count}", annotation.label),
            annotation.narration.clone(),
        ));
    }

    let narration = run.current_stop().narration.clone();
    if narration.trim().is_empty() {
        return None;
    }

    let (at, stops) = run.progress();
    Some((format!("{} - {at}/{stops}", stop_title(run)), narration))
}

/// The card's width, from the widest line it holds.
///
/// Bounded below so a short narration still reads as a card, and above by both
/// a fixed ceiling and half the frame, so the card never takes the screen it is
/// explaining.
fn card_width(lines: &[Vec<(String, Style)>], frame_width: u16) -> u16 {
    let widest = lines
        .iter()
        .map(|line| line.iter().map(|(text, _)| text_width(text)).sum::<usize>())
        .max()
        .unwrap_or(0);
    let ceiling = CARD_MAX_WIDTH.min((frame_width / 2).max(CARD_MIN_WIDTH));
    (widest as u16)
        .saturating_add(2)
        .clamp(CARD_MIN_WIDTH, ceiling)
}

/// The border the card draws.
///
/// The hand-drawn box needs a terminal that decodes one. Everywhere else the
/// card keeps the modal border, so the narration reads the same even where the
/// marks around it do not draw.
fn card_frame(stoat: &Stoat, id: u32) -> HoverFrame {
    if !stoat.stoatty || stoat.stoatty_protocol < SKETCH_PROTOCOL {
        return HoverFrame::Modal;
    }

    let card = stoat.theme.get(crate::theme::scope::UI_WALKTHROUGH_CARD);
    HoverFrame::Sketch {
        id,
        stroke: crate::render::paint::style_rgb(card.fg).unwrap_or([255, 255, 255]),
        fill: crate::render::paint::style_rgb(card.bg).unwrap_or([0, 0, 0]),
    }
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
    // A one-stop trail is ordinary rather than an edge. Two stops inside one
    // definition resolve to the same symbol, and the path is that symbol alone.
    let plural = if stops == 1 { "" } else { "s" };
    Some(format!("trail: {stops} stop{plural}"))
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

/// The byte offset of `point` in the buffer of `editor`.
fn offset_of(editor: &mut EditorState, point: walkthrough::Point) -> usize {
    let snapshot = editor.display_map.snapshot();
    snapshot
        .buffer_snapshot()
        .rope()
        .point_to_offset(rope_point(point))
}

/// A stored point as the rope counts them.
///
/// The stored form counts lines and byte columns from one, where the rope
/// counts both from zero.
fn rope_point(point: walkthrough::Point) -> stoat_text::Point {
    stoat_text::Point::new(point.line.saturating_sub(1), point.col.saturating_sub(1))
}

/// Whether the buffer of `editor` no longer holds the bytes `captured` over
/// `range`.
///
/// Read through [`walkthrough::snippet_for`] rather than sliced here, so a stop
/// the player calls drifted is exactly one `stoat walkthrough check` reports.
fn drifted(editor: &mut EditorState, range: walkthrough::Range, captured: &str) -> bool {
    let content = editor
        .display_map
        .snapshot()
        .buffer_snapshot()
        .rope()
        .to_string();

    !walkthrough::snippet_for(&content, range).is_ok_and(|found| found == captured)
}
