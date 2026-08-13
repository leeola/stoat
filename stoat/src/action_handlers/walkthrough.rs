use crate::{
    action_handlers,
    app::{Stoat, UpdateEffect},
    walkthrough::{self, run::WalkthroughRun, store},
};

/// Load the walkthrough `slug` and jump to its first stop.
///
/// Replaces whatever tour the workspace already holds. A slug that does not
/// load, or one whose walkthrough has no stops, reports why and leaves the
/// previous tour in place.
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
    jump_to_stop(stoat)
}

/// Step forward to the next stop.
pub(crate) fn next(stoat: &mut Stoat) -> UpdateEffect {
    step(stoat, 1)
}

/// Step back to the previous stop.
pub(crate) fn prev(stoat: &mut Stoat) -> UpdateEffect {
    step(stoat, -1)
}

/// End the walkthrough, leaving the reader wherever it put them.
pub(crate) fn done(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().walkthrough.is_none() {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    }

    stoat.active_workspace_mut().walkthrough = None;
    stoat.set_status("walkthrough closed");
    UpdateEffect::Redraw
}

/// Move `delta` stops and jump to where that lands.
///
/// A step off either end says the tour is over rather than jumping again, which
/// tells a clamped step apart from one that moved.
fn step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(run) = stoat.active_workspace_mut().walkthrough.as_mut() else {
        stoat.set_status("no walkthrough is playing");
        return UpdateEffect::Redraw;
    };

    if !run.step(delta) {
        let end = if delta < 0 { "first" } else { "last" };
        stoat.set_status(format!("already on the {end} stop"));
        return UpdateEffect::Redraw;
    }

    jump_to_stop(stoat)
}

/// Open the current stop's file, put the cursor on its focus, and say where the
/// reader now is.
///
/// A stop whose captured bytes no longer match the file still jumps. The range
/// is the best guide left to where the code went, and a refusal to move strands
/// the reader on the one stop that most needs a look.
fn jump_to_stop(stoat: &mut Stoat) -> UpdateEffect {
    let Some(run) = stoat.active_workspace().walkthrough.as_ref() else {
        return UpdateEffect::None;
    };
    let stop = run.current_stop();
    let (path, focus, id) = (stop.focus.path.clone(), stop.focus.clone(), stop.id.clone());
    let (at, stops) = run.progress();
    let title = stop.title.clone().unwrap_or_else(|| stop.narration.clone());

    action_handlers::jump::push_jump(stoat);
    let target = stoat.active_workspace().panes.focus();
    action_handlers::file::open_file_in_pane(stoat, target, &path);

    let Some(offset) = focus_offset(stoat, &focus) else {
        return UpdateEffect::Redraw;
    };
    let effect = action_handlers::movement::jump_to_offset(stoat, offset);

    match drifted(stoat, &focus) {
        true => stoat.set_status(format!("stop {id} drifted from its capture")),
        false => stoat.set_status(format!("{at}/{stops}: {title}")),
    }
    effect
}

/// Byte offset of `focus`'s start in the focused buffer.
///
/// The stored point counts lines and byte columns from one, where the rope
/// counts both from zero.
fn focus_offset(stoat: &mut Stoat, focus: &walkthrough::Location) -> Option<usize> {
    let editor = action_handlers::focused_editor_mut(stoat)?;
    let snapshot = editor.display_map.snapshot();
    let point = stoat_text::Point::new(
        focus.range.start.line.saturating_sub(1),
        focus.range.start.col.saturating_sub(1),
    );
    Some(snapshot.buffer_snapshot().rope().point_to_offset(point))
}

/// Whether the focused buffer no longer holds the bytes `focus` captured.
///
/// Read through [`walkthrough::snippet_for`] rather than sliced here, so a stop
/// the player calls drifted is exactly one `stoat walkthrough check` reports.
fn drifted(stoat: &mut Stoat, focus: &walkthrough::Location) -> bool {
    let Some(editor) = action_handlers::focused_editor_mut(stoat) else {
        return false;
    };
    let content = editor
        .display_map
        .snapshot()
        .buffer_snapshot()
        .rope()
        .to_string();

    !walkthrough::snippet_for(&content, focus.range).is_ok_and(|found| found == focus.snippet)
}

#[cfg(test)]
mod tests {
    use super::{done, next, open, prev};
    use crate::{
        action_handlers,
        app::Stoat,
        host::FakeFs,
        walkthrough::{Location, Point, Range, Walkthrough},
    };
    use std::{path::PathBuf, sync::Arc};
    use stoat_config::Settings;
    use stoat_scheduler::TestScheduler;

    const FIRST: &str = "fn one() {}\nfn two() {}\n";
    const SECOND: &str = "fn three() {}\n";

    fn location(path: &str, line: u32, cols: (u32, u32), snippet: &str) -> Location {
        Location {
            path: PathBuf::from(path),
            range: Range {
                start: Point { line, col: cols.0 },
                end: Point { line, col: cols.1 },
            },
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
                "n".to_owned(),
                location("a.rs", 2, (1, 11), "fn two() {}"),
                None,
            )
            .expect("append");
        walkthrough
            .add_stop(
                Some("second".to_owned()),
                "n".to_owned(),
                location("b.rs", 1, (1, 13), "fn three() {}"),
                None,
            )
            .expect("append");

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
    fn done_ends_the_walkthrough() {
        let mut stoat = stoat_with_tour(FIRST);
        open(&mut stoat, "tour");
        done(&mut stoat);

        assert!(stoat.active_workspace().walkthrough.is_none());
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
