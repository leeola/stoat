use crate::{
    action_handlers::{self, read_string_via_host, review_walk::WalkLandingKind},
    app::{Stoat, UpdateEffect},
    code_index::{build, nav},
    host::CommitInfo,
    render::{
        hover::{HoverFrame, HoverPopup},
        text::text_width,
        walkthrough::{SlideParts, EXIT_MS},
    },
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
pub(crate) fn open(stoat: &mut Stoat, slug: &str) -> UpdateEffect {
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
    super::review_walk::queue_walk_start(stoat, workdir, commits, WalkLandingKind::Walkthrough);
    stoat.set_status(format!("checking out {first:.7}"));
    UpdateEffect::Redraw
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

/// Step forward one attention point of the tour, read as one sequence.
pub(crate) fn forward(stoat: &mut Stoat) -> UpdateEffect {
    step_linear(stoat, 1)
}

/// Step back one attention point of the tour, exactly inverting [`forward`].
pub(crate) fn backward(stoat: &mut Stoat) -> UpdateEffect {
    step_linear(stoat, -1)
}

/// Take the narration card down, or raise it again.
///
/// The card is pinned, so it stays through everything but a deliberate
/// dismissal. That leaves one key to both hide it when it is in the way and
/// bring it back afterward, which is what this is.
pub(crate) fn show_narration_again(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().walkthrough.is_none() {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    }

    // A card that is up came from this stop, so the reader asking again wants
    // it out of the way rather than redrawn where it already is.
    if stoat
        .pending_hover
        .as_ref()
        .is_some_and(|popup| popup.pinned)
    {
        stoat.pending_hover = None;
        stoat.pending_hover_request = None;
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

/// End the walkthrough, leaving the reader wherever it put them.
///
/// The trail goes with it. It was laid between two stops of a tour that is
/// over, and nothing else put it there.
///
/// A tour over commits ends its walk as well, which checks the ref the walk
/// started from back out. The walk's end closes the tour.
pub(crate) fn done(stoat: &mut Stoat) -> UpdateEffect {
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

    // A key press follows the cursor after its handler runs, and a landing
    // arrives from the git queue with no key behind it.
    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    if let Some(editor) = action_handlers::focused_editor_mut(stoat) {
        action_handlers::view::follow_jump(editor, scrolloff);
    }
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

    let Some(offset) = focused_offset_of(stoat, point) else {
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
    let (at, stops) = run.progress();
    let title = stop_title(run);
    let commit = commit_suffix(run);

    let Some(landed) = jump_to_range(stoat, &focus.path, focus.range, &focus.snippet) else {
        return UpdateEffect::Redraw;
    };
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
    // A stop in another file focuses another editor, which carries its own
    // mode. The mode belongs to the reader rather than to whichever buffer a
    // jump puts under them, so a tour read in walkthrough mode keeps it.
    let mode = stoat.focused_mode().to_owned();
    crate::buffer_lifecycle::open_file_in_pane(stoat, target, path);
    stoat.set_focused_mode(mode);

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

/// Narrowest and widest the card gets, in cells.
///
/// Below the floor a wrapped line is more break than text. The ceiling and the
/// half-pane bound below keep the card from becoming the screen.
const CARD_MIN_WIDTH: u16 = 24;
const CARD_MAX_WIDTH: u16 = 52;

/// The protocol version that decodes a sketch. An older stoatty ignores the
/// frames, so the card would draw no border at all.
const SKETCH_PROTOCOL: u32 = 3;

/// Put the narration for where the reader is in the hover popup, anchored at
/// `offset`.
///
/// A position with nothing to say takes the popup down rather than leaving the
/// last one's up, so what is on screen always describes where the reader is.
/// The popup itself is the one a hover raises, which is why the next key press
/// dismisses it and [`show_narration_again`] exists to bring it back.
fn show_narration(stoat: &mut Stoat, offset: usize) {
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        return;
    };
    let Some((heading, narration)) = card_text(run) else {
        stoat.pending_hover = None;
        return;
    };

    let Some((editor_id, _)) = stoat.focused_editor_ids() else {
        return;
    };

    // The card is one part of the slide, so it takes its id from the same
    // scheme the marks do. A card re-declared on every frame of one stop must
    // not restart its stroke.
    let card_id = run.part_id(part::CARD);

    let mut lines = vec![vec![(heading, stoat.theme.get("syntax.markup.title"))]];
    lines.extend(crate::markdown::render_markdown(
        &narration,
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
    use super::{
        backward, done, forward, next, next_annotation, open, prev, prev_annotation,
        show_narration_again, CARD_MAX_WIDTH, CARD_MIN_WIDTH,
    };
    use crate::{
        action_handlers,
        app::Stoat,
        badge::BadgeSource,
        code_index::{build, nav},
        git_jobs,
        host::FakeFs,
        render::hover::HoverFrame,
        review_walk::ReturnRef,
        test_harness::TestHarness,
        walkthrough::{Location, Point, Range, Walkthrough},
        workspace::diff::DiffBase,
    };
    use codegraph::{Confidence, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey, Target};
    use std::{
        ops::Range as ByteRange,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_config::Settings;
    use stoat_language::SymbolKind;
    use stoat_scheduler::TestScheduler;

    const FIRST: &str = "fn one() {}\nfn two() {}\n";
    const SECOND: &str = "fn three() {}\n";
    /// Stop 1's narration. Stop 2 has none, so one tour covers both cases.
    const NARRATION: &str = "The **entry** point.";
    /// Annotation a1's narration. a2 has none, so one walk covers both cases.
    const ANNOTATION_NARRATION: &str = "Where the call **lands**.";

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
                None,
            )
            .expect("append");
        walkthrough
            .add_stop(
                Some("second".to_owned()),
                String::new(),
                location("b.rs", 1, (1, 13), "fn three() {}"),
                None,
                None,
            )
            .expect("append");

        // Stop 1 calls out the callee it reaches in b.rs, then a neighbor in
        // its own file, so one walk covers a cross-file hop and a same-file one.
        // Only the callee narrates, so the walk covers both card branches too.
        walkthrough
            .add_annotation(
                "s1",
                Some(PathBuf::from("b.rs")),
                range_of(1, (1, 13)),
                "fn three() {}".to_owned(),
                "the callee".to_owned(),
                ANNOTATION_NARRATION.to_owned(),
            )
            .expect("s1 exists");
        walkthrough
            .add_annotation(
                "s1",
                None,
                range_of(1, (1, 11)),
                "fn one() {}".to_owned(),
                "the neighbor".to_owned(),
                String::new(),
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

    /// A hover goes on the next key press, which is what makes it a hover. A
    /// narration card is the point of the screen while a tour plays, so an
    /// incidental key leaves it up.
    #[test]
    fn an_ordinary_key_leaves_the_card_up() {
        let mut h = harness_with_tour();
        open(&mut h.stoat, "tour");
        assert!(
            h.stoat
                .pending_hover
                .as_ref()
                .is_some_and(|popup| popup.pinned),
            "the card is pinned",
        );

        h.type_keys("j");
        assert_eq!(
            popup_lines(&h.stoat),
            ["first - 1/2", "The entry point."],
            "and a movement key does not take it down",
        );
    }

    /// A card that cannot be dismissed is worse than one that vanishes, so the
    /// deliberate dismissals go on working.
    #[test]
    fn esc_takes_the_card_down() {
        let mut h = harness_with_tour();
        open(&mut h.stoat, "tour");

        h.type_keys("escape");
        assert!(h.stoat.pending_hover.is_none());
    }

    /// The card stays until it is dismissed, so one key both hides it when it
    /// is in the way and brings it back.
    /// Walkthrough mode binds Esc to an action, so it never reaches the plain
    /// key branch while that mode is active. Without the action path knowing,
    /// the card survives the one key meant to take it down.
    #[test]
    fn esc_takes_the_card_down_from_inside_walkthrough_mode() {
        let mut h = harness_with_tour();
        open(&mut h.stoat, "tour");

        h.type_keys("space W");
        assert_eq!(h.stoat.focused_mode(), "walkthrough", "the mode is entered");
        assert!(
            h.stoat.pending_hover.is_some(),
            "and the card survived getting there",
        );

        h.type_keys("escape");
        assert!(h.stoat.pending_hover.is_none(), "Esc takes it down");
    }

    #[test]
    fn asking_again_toggles_the_card() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");

        show_narration_again(&mut stoat);
        assert!(stoat.pending_hover.is_none(), "a card that is up goes down");

        show_narration_again(&mut stoat);
        assert_eq!(
            popup_lines(&stoat),
            ["first - 1/2", "The entry point."],
            "and asking once more brings it back",
        );
    }

    /// A plain terminal draws no marks, so the card keeps the modal border and
    /// the narration reads the same there.
    #[test]
    fn a_plain_terminal_keeps_the_modal_frame() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");

        assert_eq!(
            stoat.pending_hover.as_ref().map(|popup| popup.frame),
            Some(HoverFrame::Modal),
            "the harness leaves the protocol at zero, which is the older-terminal case",
        );
    }

    /// The card goes where the layout puts it rather than beside the cursor,
    /// which is what keeps it clear of the code the stop is about.
    #[test]
    fn the_card_carries_its_own_placement() {
        let mut h = harness_with_tour();
        open(&mut h.stoat, "tour");

        let frame = h.stoat.size();
        let popup = h.stoat.pending_hover.as_ref().expect("a popup");
        let placement = popup.placement.expect("the card places itself");

        // The floor wins on a narrow frame, so the ceiling is stated the way
        // the sizing states it rather than as a bare half-frame bound.
        let ceiling = CARD_MAX_WIDTH.min((frame.width / 2).max(CARD_MIN_WIDTH));
        assert!(
            (CARD_MIN_WIDTH..=ceiling).contains(&placement.width),
            "the card is between {CARD_MIN_WIDTH} and {ceiling}, got {placement:?}",
        );
        assert!(
            placement.x + placement.width <= frame.x + frame.width,
            "and stays inside the frame, got {placement:?} in {frame:?}",
        );
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

    /// Two stops inside one definition resolve to the same symbol, so the path
    /// between them is that symbol alone. That is ordinary rather than an
    /// edge, and "1 stops" tells the reader the count is generated and leaves
    /// them wondering what it miscounted.
    #[test]
    fn a_trail_within_one_definition_reads_as_one_stop() {
        let scheduler = Arc::new(TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            Settings::default(),
            PathBuf::from("/repo"),
        );
        stoat.persistence_disabled = true;

        // Both stops sit inside `fn two`, which is the whole point: they
        // resolve to one symbol and the walk between them has one stop.
        let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
        for (id, cols, snippet) in [("first", (1, 6), "fn two"), ("second", (7, 11), "() {}")] {
            walkthrough
                .add_stop(
                    Some(id.to_owned()),
                    String::new(),
                    location("a.rs", 2, cols, snippet),
                    None,
                    None,
                )
                .expect("append");
        }

        let fs = Arc::new(FakeFs::new());
        fs.insert_file("/repo/a.rs", FIRST);
        fs.insert_file(
            "/repo/.stoat/walkthroughs/tour.json",
            serde_json::to_string(&walkthrough).expect("serialize"),
        );
        stoat.set_fs_host(fs);

        stoat
            .active_workspace_mut()
            .code_graph
            .insert_shard(FileShard {
                content_hash: [0u8; 32],
                symbols: vec![Symbol {
                    key: SymbolKey([1u8; 16]),
                    file: build::file_id("a.rs"),
                    name: "two".to_owned(),
                    kind: SymbolKind::Function,
                    container: vec![],
                    def_range: 12..23,
                    name_range: 15..18,
                    body_hash: [0u8; 32],
                }],
                edges: Vec::new(),
            });
        stoat
            .active_workspace_mut()
            .file_paths
            .insert(build::file_id("a.rs"), PathBuf::from("a.rs"));

        open(&mut stoat, "tour");
        next(&mut stoat);

        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("2/2: second (trail: 1 stop)"),
            "one stop is one stop",
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

    /// An annotation is a sub-step with documentation of its own, so the card
    /// follows the reader down into it and back up when it has nothing to add.
    #[test]
    fn a_narrated_annotation_takes_the_card_and_an_unnarrated_one_leaves_it() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");

        next_annotation(&mut stoat);
        assert_eq!(
            popup_lines(&stoat),
            ["the callee - 1/2", "Where the call lands."],
            "the annotation speaks for itself, under its own heading",
        );

        next_annotation(&mut stoat);
        assert_eq!(
            popup_lines(&stoat),
            ["first - 1/2", "The entry point."],
            "an annotation with nothing to add leaves the stop's card up",
        );
    }

    #[test]
    fn asking_again_on_an_annotation_brings_its_own_card_back() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");
        next_annotation(&mut stoat);

        show_narration_again(&mut stoat);
        assert!(stoat.pending_hover.is_none(), "a card that is up goes down");

        show_narration_again(&mut stoat);
        assert_eq!(
            popup_lines(&stoat),
            ["the callee - 1/2", "Where the call lands."],
            "and asking once more brings the annotation's own card back",
        );
    }

    /// One gesture reads the tour as a single sequence, so it carries the
    /// reader off a focus, through the stop's annotations, and on to the next.
    #[test]
    fn the_linear_walk_runs_a_focus_its_annotations_then_the_next_stop() {
        let mut stoat = stoat_with_tour(FIRST);
        index_the_tour(&mut stoat, true);
        open(&mut stoat, "tour");

        backward(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("already at the start of the tour"),
        );

        forward(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("a1 1/2: the callee (trail: 2 stops)"),
            "the first point past a focus is the stop's first annotation",
        );

        forward(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("a2 2/2: the neighbor"),
        );

        forward(&mut stoat);
        assert_eq!(cursor(&mut stoat), ("/repo/b.rs".to_owned(), 0));
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("2/2: second"),
            "past the last annotation the walk reaches the next stop's focus",
        );

        forward(&mut stoat);
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("already at the end of the tour"),
        );
    }

    /// Backward inverts forward exactly, so a reader who overshoots gets the
    /// point they came from rather than the top of the stop they left.
    #[test]
    fn the_linear_walk_back_lands_on_the_previous_stops_last_annotation() {
        let mut stoat = stoat_with_tour(FIRST);
        index_the_tour(&mut stoat, true);
        open(&mut stoat, "tour");
        next(&mut stoat);

        backward(&mut stoat);
        assert_eq!(cursor(&mut stoat), ("/repo/a.rs".to_owned(), 0));
        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("a2 2/2: the neighbor"),
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

    /// The tour of [`stoat_with_tour`] over a [`TestHarness`], so a test can
    /// reach it the way a reader does, through the keymap rather than by
    /// calling the handler. Both stops narrate here, because the popup is what
    /// these tests watch.
    fn harness_with_tour() -> TestHarness {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");

        let mut walkthrough = Walkthrough::new("tour".to_owned(), "Tour".to_owned(), None);
        walkthrough
            .add_stop(
                Some("first".to_owned()),
                NARRATION.to_owned(),
                location("a.rs", 2, (1, 11), "fn two() {}"),
                None,
                None,
            )
            .expect("append");
        walkthrough
            .add_stop(
                Some("second".to_owned()),
                "The **exit**.".to_owned(),
                location("b.rs", 1, (1, 13), "fn three() {}"),
                None,
                None,
            )
            .expect("append");
        // Only stop 1 carries an annotation, so one tour reaches both annotation
        // branches. A step onto it clamps there, and stop 2 has none to step onto.
        walkthrough
            .add_annotation(
                "s1",
                None,
                range_of(1, (1, 11)),
                "fn one() {}".to_owned(),
                "the neighbor".to_owned(),
                String::new(),
            )
            .expect("s1 exists");

        h.fake_fs().insert_file("/repo/a.rs", FIRST);
        h.fake_fs().insert_file("/repo/b.rs", SECOND);
        h.fake_fs().insert_file(
            "/repo/.stoat/walkthroughs/tour.json",
            serde_json::to_string(&walkthrough).expect("serialize"),
        );
        h
    }

    /// The narration shares the hover popup, which any key takes down, and the
    /// chord that raises it is itself a key press. A popup this dispatch put up
    /// is the one case that clear must leave alone, or no reader ever sees it.
    #[test]
    fn the_chord_that_raises_the_narration_leaves_it_up() {
        let mut h = harness_with_tour();
        open(&mut h.stoat, "tour");
        h.stoat.pending_hover = None;

        h.type_keys("space W s");

        assert_eq!(popup_lines(&h.stoat), ["first - 1/2", "The entry point."]);
    }

    #[test]
    fn stepping_by_chord_shows_the_stop_it_lands_on() {
        let mut h = harness_with_tour();
        open(&mut h.stoat, "tour");

        h.type_keys("space W n");

        assert_eq!(popup_lines(&h.stoat), ["second - 2/2", "The exit."]);
    }

    /// A step off the end moves nobody, so nothing raises a popup and the
    /// reader would lose the narration of the stop they are still on.
    #[test]
    fn a_clamped_step_keeps_the_narration_of_the_stop_it_stays_on() {
        let mut h = harness_with_tour();
        open(&mut h.stoat, "tour");
        h.type_keys("space W n");

        h.type_keys("space W n");

        assert_eq!(popup_lines(&h.stoat), ["second - 2/2", "The exit."]);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("already on the last stop"),
        );
    }

    /// The same hold covers the annotation walk, whose two no-move branches
    /// leave the reader on a stop whose narration they still need.
    #[test]
    fn a_no_move_annotation_step_keeps_the_narration_up() {
        for (keys, status, popup) in [
            (
                "space W a space W a",
                "already on the last annotation",
                ["first - 1/2", "The entry point."],
            ),
            (
                "space W n space W a",
                "this stop has no annotations",
                ["second - 2/2", "The exit."],
            ),
        ] {
            let mut h = harness_with_tour();
            open(&mut h.stoat, "tour");

            h.type_keys(keys);

            assert_eq!(
                popup_lines(&h.stoat),
                popup,
                "{keys} leaves the reader on a narrated stop",
            );
            assert_eq!(h.stoat.pending_message.as_deref(), Some(status));
        }
    }

    /// A tour is read a step at a time, so the mode has to survive each step
    /// for the next one to take a bare key.
    #[test]
    fn the_walkthrough_mode_holds_until_escape() {
        let mut h = harness_with_tour();
        open(&mut h.stoat, "tour");

        h.type_keys("space W n");
        assert_eq!(h.stoat.focused_mode(), "walkthrough");
        assert_eq!(popup_lines(&h.stoat), ["second - 2/2", "The exit."]);

        h.type_keys("p");
        assert_eq!(
            popup_lines(&h.stoat),
            ["first - 1/2", "The entry point."],
            "a bare p walks back, taking no space W prefix",
        );

        h.type_keys("escape");
        assert_eq!(h.stoat.focused_mode(), "normal");
        assert!(
            h.stoat.active_workspace().walkthrough.is_some(),
            "escape leaves the mode, not the tour, so space W returns to it",
        );
    }

    /// A repository of two commits with a three-stop tour over them. `s1` reads
    /// `a1b2c3d4` over `a.rs` line 2, and `s2` and `s3` read `b2c3d4e5` over
    /// `b.rs` line 1 and `a.rs` line 1.
    ///
    /// The fake host moves HEAD on a checkout without writing the tree, so the
    /// tree is seeded once at the tip, where every stop reads what it captured.
    fn commit_tour_harness() -> TestHarness {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "feat: add a.rs", &[("a.rs", FIRST)]),
                (
                    "b2c3d4e5",
                    "feat: add b.rs",
                    &[("a.rs", FIRST), ("b.rs", SECOND)],
                ),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "b2c3d4e5")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_fs().insert_file("/repo/a.rs", FIRST);
        h.fake_fs().insert_file("/repo/b.rs", SECOND);

        store_tour(
            &h,
            "tour",
            &[
                (
                    "first",
                    Some("a1b2c3d4"),
                    location("a.rs", 2, (1, 11), "fn two() {}"),
                ),
                (
                    "second",
                    Some("b2c3d4e5"),
                    location("b.rs", 1, (1, 13), "fn three() {}"),
                ),
                (
                    "third",
                    Some("b2c3d4e5"),
                    location("a.rs", 1, (1, 11), "fn one() {}"),
                ),
            ],
        );
        h
    }

    /// Store the tour `slug`, whose stops are `(title, commit, focus)`.
    fn store_tour(h: &TestHarness, slug: &str, stops: &[(&str, Option<&str>, Location)]) {
        let mut walkthrough = Walkthrough::new(slug.to_owned(), "Tour".to_owned(), None);
        for (title, commit, focus) in stops {
            walkthrough
                .add_stop(
                    Some((*title).to_owned()),
                    String::new(),
                    focus.clone(),
                    commit.map(str::to_owned),
                    None,
                )
                .expect("append");
        }
        h.fake_fs().insert_file(
            format!("/repo/.stoat/walkthroughs/{slug}.json"),
            serde_json::to_string(&walkthrough).expect("serialize"),
        );
    }

    fn checkouts(h: &TestHarness) -> Vec<String> {
        h.fake_git().checkouts(Path::new("/repo"))
    }

    fn walk_shas(h: &TestHarness) -> Vec<String> {
        h.stoat
            .active_workspace()
            .review_walk
            .as_ref()
            .map(|walk| {
                walk.commits
                    .iter()
                    .map(|commit| commit.sha.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The sha a revision base names, or `None` for the working tree's own base.
    fn diff_base(h: &TestHarness) -> Option<Option<String>> {
        match h.stoat.active_workspace().diff_base() {
            Some(DiffBase::Rev { sha, .. }) => Some(sha.clone()),
            _ => None,
        }
    }

    fn latched(h: &TestHarness) -> bool {
        let panes = &h.stoat.active_workspace().panes;
        panes.pane(panes.focus()).diff_mode
    }

    fn open_path(h: &TestHarness) -> Option<PathBuf> {
        let id = h.stoat.focused_editor_ids()?.1;
        h.stoat
            .active_workspace()
            .buffers
            .path_for(id)
            .map(Path::to_path_buf)
    }

    fn status(h: &TestHarness) -> Option<&str> {
        h.stoat.pending_message.as_deref()
    }

    fn tour_slug(h: &TestHarness) -> Option<&str> {
        h.stoat
            .active_workspace()
            .walkthrough
            .as_ref()
            .map(|run| run.walkthrough.slug.as_str())
    }

    fn review_badge(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| badge.label.clone())
    }

    #[test]
    fn opening_a_commit_tour_checks_out_the_first_stops_commit() {
        let mut h = commit_tour_harness();
        open(&mut h.stoat, "tour");
        h.settle();

        assert_eq!(
            (checkouts(&h), walk_shas(&h), diff_base(&h)),
            (
                vec!["detached:a1b2c3d4".to_owned()],
                vec!["a1b2c3d4".to_owned(), "b2c3d4e5".to_owned()],
                Some(None),
            ),
            "one walk over the tour's two commits, standing on the first",
        );
        assert_eq!(
            (open_path(&h), latched(&h), status(&h)),
            (
                Some(PathBuf::from("/repo/a.rs")),
                true,
                Some("1/3: first @ a1b2c3d")
            ),
        );
    }

    /// The checkout runs off the loop, so the jump waits for its landing rather
    /// than showing the stop over the last commit's tree.
    #[test]
    fn a_step_onto_another_commit_lands_after_its_checkout() {
        let mut h = commit_tour_harness();
        open(&mut h.stoat, "tour");
        h.settle();

        next(&mut h.stoat);
        assert_eq!(
            (open_path(&h), status(&h)),
            (
                Some(PathBuf::from("/repo/a.rs")),
                Some("checking out b2c3d4e")
            ),
            "nothing jumps before the landing",
        );

        h.settle();
        assert_eq!(
            (
                checkouts(&h).last().cloned(),
                diff_base(&h),
                open_path(&h),
                status(&h)
            ),
            (
                Some("detached:b2c3d4e5".to_owned()),
                Some(Some("a1b2c3d4".to_owned())),
                Some(PathBuf::from("/repo/b.rs")),
                Some("2/3: second @ b2c3d4e"),
            ),
        );
    }

    /// A landing arrives with no key press behind it, so it follows the cursor
    /// itself, and a stop far down its file lands on screen.
    #[test]
    fn a_landing_scrolls_a_distant_stop_into_view() {
        let long: String = (1..=80).map(|n| format!("fn f{n}() {{}}\n")).collect();
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "feat: add long.rs", &[("long.rs", &long)]),
                (
                    "b2c3d4e5",
                    "feat: add b.rs",
                    &[("long.rs", &long), ("b.rs", SECOND)],
                ),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "b2c3d4e5")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_fs().insert_file("/repo/long.rs", &long);
        h.fake_fs().insert_file("/repo/b.rs", SECOND);
        store_tour(
            &h,
            "tour",
            &[
                (
                    "top",
                    Some("a1b2c3d4"),
                    location("long.rs", 1, (1, 10), "fn f1() {}"),
                ),
                (
                    "bottom",
                    Some("b2c3d4e5"),
                    location("long.rs", 70, (1, 11), "fn f70() {}"),
                ),
            ],
        );
        open(&mut h.stoat, "tour");
        h.settle();

        next(&mut h.stoat);
        h.settle();
        h.snapshot();
        assert!(
            h.rendered_text().contains("fn f70() {}"),
            "the landed stop is on screen, got:\n{}",
            h.rendered_text(),
        );
    }

    /// The point-by-point walk crosses commits the same way the stop walk does.
    #[test]
    fn a_linear_step_onto_another_commit_checks_it_out() {
        let mut h = commit_tour_harness();
        open(&mut h.stoat, "tour");
        h.settle();

        forward(&mut h.stoat);
        h.settle();
        assert_eq!(
            (checkouts(&h), open_path(&h), status(&h)),
            (
                vec![
                    "detached:a1b2c3d4".to_owned(),
                    "detached:b2c3d4e5".to_owned()
                ],
                Some(PathBuf::from("/repo/b.rs")),
                Some("2/3: second @ b2c3d4e"),
            ),
        );
    }

    #[test]
    fn a_step_within_one_commit_checks_out_nothing() {
        let mut h = commit_tour_harness();
        open(&mut h.stoat, "tour");
        h.settle();
        next(&mut h.stoat);
        h.settle();

        next(&mut h.stoat);
        h.settle();
        assert_eq!(
            (checkouts(&h), open_path(&h), status(&h)),
            (
                vec![
                    "detached:a1b2c3d4".to_owned(),
                    "detached:b2c3d4e5".to_owned()
                ],
                Some(PathBuf::from("/repo/a.rs")),
                Some("3/3: third @ b2c3d4e"),
            ),
        );
    }

    #[test]
    fn stepping_back_across_commits_checks_the_earlier_one_out_again() {
        let mut h = commit_tour_harness();
        open(&mut h.stoat, "tour");
        h.settle();
        next(&mut h.stoat);
        h.settle();

        prev(&mut h.stoat);
        h.settle();
        assert_eq!(
            (checkouts(&h), diff_base(&h), open_path(&h), status(&h)),
            (
                vec![
                    "detached:a1b2c3d4".to_owned(),
                    "detached:b2c3d4e5".to_owned(),
                    "detached:a1b2c3d4".to_owned(),
                ],
                Some(None),
                Some(PathBuf::from("/repo/a.rs")),
                Some("1/3: first @ a1b2c3d"),
            ),
        );
    }

    #[test]
    fn done_returns_head_and_closes_the_tour() {
        let mut h = commit_tour_harness();
        open(&mut h.stoat, "tour");
        h.settle();

        done(&mut h.stoat);
        h.settle();
        assert_eq!(
            (
                checkouts(&h).last().cloned(),
                walk_shas(&h),
                tour_slug(&h),
                diff_base(&h),
                latched(&h)
            ),
            (Some("ref:main".to_owned()), Vec::new(), None, None, false),
        );
    }

    #[test]
    fn review_done_closes_a_commit_tour() {
        let mut h = commit_tour_harness();
        open(&mut h.stoat, "tour");
        h.settle();

        action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();
        assert_eq!(
            (checkouts(&h).last().cloned(), tour_slug(&h), status(&h)),
            (
                Some("ref:main".to_owned()),
                None,
                Some("walkthrough closed")
            ),
        );
    }

    /// A refused walk sends no landing, so a tour left to wait for one never
    /// moves.
    #[test]
    fn a_dirty_tree_refuses_a_commit_tour() {
        let mut h = commit_tour_harness();
        h.fake_git()
            .add_repo("/repo")
            .modified("a.rs", FIRST, "fn edited() {}\n");

        open(&mut h.stoat, "tour");
        h.settle();
        assert_eq!(
            (checkouts(&h), tour_slug(&h), review_badge(&h)),
            (Vec::new(), None, Some("uncommitted changes".to_owned())),
        );
    }

    #[test]
    fn a_tour_naming_an_unknown_commit_keeps_the_playing_tour() {
        let mut h = commit_tour_harness();
        open(&mut h.stoat, "tour");
        h.settle();
        store_tour(
            &h,
            "ghost",
            &[(
                "gone",
                Some("deadbeef00"),
                location("a.rs", 1, (1, 11), "fn one() {}"),
            )],
        );

        open(&mut h.stoat, "ghost");
        h.settle();
        assert_eq!(
            (
                tour_slug(&h),
                walk_shas(&h).len(),
                checkouts(&h),
                status(&h)
            ),
            (
                Some("tour"),
                2,
                vec!["detached:a1b2c3d4".to_owned()],
                Some("commit deadbee is not in this repository"),
            ),
        );
    }

    #[test]
    fn a_plain_tour_starts_no_walk() {
        let mut h = commit_tour_harness();
        store_tour(
            &h,
            "plain",
            &[("here", None, location("a.rs", 2, (1, 11), "fn two() {}"))],
        );

        open(&mut h.stoat, "plain");
        h.settle();
        assert_eq!(
            (checkouts(&h), walk_shas(&h), open_path(&h), status(&h)),
            (
                Vec::new(),
                Vec::new(),
                Some(PathBuf::from("/repo/a.rs")),
                Some("1/1: here")
            ),
        );
    }

    /// The tour's walk waits behind another git job here, the way it waits
    /// behind a slow return, so the tour closes before the walk's turn.
    #[test]
    fn a_tour_closed_before_its_checkout_starts_no_walk() {
        let mut h = commit_tour_harness();
        git_jobs::enqueue(&mut h.stoat, git_jobs::idle_job(None));
        open(&mut h.stoat, "tour");
        done(&mut h.stoat);
        h.settle();

        assert_eq!(
            (checkouts(&h), walk_shas(&h), tour_slug(&h)),
            (Vec::new(), Vec::new(), None),
        );
    }

    /// A step taken before the walk starts has no walk to seek. The first
    /// landing finds the tour on another commit and follows it there, so the
    /// reader arrives once, on the stop's own commit.
    #[test]
    fn a_step_before_the_first_checkout_lands_on_the_stops_commit() {
        let mut h = commit_tour_harness();
        git_jobs::enqueue(&mut h.stoat, git_jobs::idle_job(None));
        open(&mut h.stoat, "tour");
        next(&mut h.stoat);
        h.settle();

        assert_eq!(
            (checkouts(&h), diff_base(&h), open_path(&h), status(&h)),
            (
                vec![
                    "detached:a1b2c3d4".to_owned(),
                    "detached:b2c3d4e5".to_owned()
                ],
                Some(Some("a1b2c3d4".to_owned())),
                Some(PathBuf::from("/repo/b.rs")),
                Some("2/3: second @ b2c3d4e"),
            ),
        );
    }

    /// A stop that names no commit reads whatever the walk has checked out, and
    /// a step from it back onto that commit still lands, with nothing to check
    /// out again.
    #[test]
    fn a_working_tree_stop_in_a_commit_tour_jumps_without_a_checkout() {
        let mut h = commit_tour_harness();
        store_tour(
            &h,
            "mixed",
            &[
                (
                    "first",
                    Some("a1b2c3d4"),
                    location("a.rs", 2, (1, 11), "fn two() {}"),
                ),
                (
                    "second",
                    None,
                    location("b.rs", 1, (1, 13), "fn three() {}"),
                ),
                (
                    "third",
                    Some("a1b2c3d4"),
                    location("a.rs", 1, (1, 11), "fn one() {}"),
                ),
            ],
        );
        open(&mut h.stoat, "mixed");
        h.settle();

        next(&mut h.stoat);
        let second = (open_path(&h), status(&h).map(str::to_owned));
        next(&mut h.stoat);
        h.settle();

        assert_eq!(
            (second, checkouts(&h), open_path(&h), status(&h)),
            (
                (
                    Some(PathBuf::from("/repo/b.rs")),
                    Some("2/3: second".to_owned())
                ),
                vec!["detached:a1b2c3d4".to_owned()],
                Some(PathBuf::from("/repo/a.rs")),
                Some("3/3: third @ a1b2c3d"),
            ),
        );
    }

    /// The first tour's walk hands HEAD back before the second starts, so the
    /// second records the branch rather than the first tour's commit.
    #[test]
    fn opening_a_commit_tour_over_another_returns_the_first() {
        let mut h = commit_tour_harness();
        open(&mut h.stoat, "tour");
        h.settle();
        store_tour(
            &h,
            "later",
            &[(
                "again",
                Some("a1b2c3d4"),
                location("a.rs", 1, (1, 11), "fn one() {}"),
            )],
        );

        open(&mut h.stoat, "later");
        h.settle();
        let return_ref = h
            .stoat
            .active_workspace()
            .review_walk
            .as_ref()
            .map(|walk| walk.return_ref.clone());
        assert_eq!(
            (checkouts(&h), tour_slug(&h), return_ref),
            (
                vec![
                    "detached:a1b2c3d4".to_owned(),
                    "ref:main".to_owned(),
                    "detached:a1b2c3d4".to_owned(),
                ],
                Some("later"),
                Some(ReturnRef::Branch("main".to_owned())),
            ),
        );
    }
}
