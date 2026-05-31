use crate::buffer::{Buffer, BufferEvent};
use gpui::{Context, Entity, EventEmitter, Subscription};
use stoat::host::BlameLine;

/// Per-buffer cache of [`BlameLine`] data produced by
/// [`stoat::host::GitRepo::blame_path`]. The cache is producer-pushed
/// via [`BlameState::set_blame`] and cleared whenever the underlying
/// buffer emits [`BufferEvent::Edited`]: stale blame rows would mis-
/// attribute lines after the row offsets shift, so the strip blanks
/// until a coordinator refills it.
///
/// Subscribers observe [`BlameStateEvent::Changed`] -- emitted on
/// both fresh data and edit-driven invalidation -- to drive re-render.
pub struct BlameState {
    blame: Vec<BlameLine>,
    _subscription: Subscription,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlameStateEvent {
    Changed,
}

impl EventEmitter<BlameStateEvent> for BlameState {}

impl BlameState {
    pub fn new(buffer: Entity<Buffer>, cx: &mut Context<'_, Self>) -> Self {
        let subscription = cx.subscribe(&buffer, |this, _, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited | BufferEvent::Reloaded) {
                if this.blame.is_empty() {
                    return;
                }
                this.blame.clear();
                cx.emit(BlameStateEvent::Changed);
                cx.notify();
            }
        });
        Self {
            blame: Vec::new(),
            _subscription: subscription,
        }
    }

    pub fn blame(&self) -> &[BlameLine] {
        &self.blame
    }

    pub fn is_empty(&self) -> bool {
        self.blame.is_empty()
    }

    /// Replace the cached entries with `blame` and notify subscribers.
    /// A coordinator calls this after computing
    /// [`stoat::host::GitRepo::blame_path`].
    pub fn set_blame(&mut self, blame: Vec<BlameLine>, cx: &mut Context<'_, Self>) {
        self.blame = blame;
        cx.emit(BlameStateEvent::Changed);
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if self.blame.is_empty() {
            return;
        }
        self.blame.clear();
        cx.emit(BlameStateEvent::Changed);
        cx.notify();
    }
}

/// Format `commit_seconds` as a short relative age against `now_seconds`,
/// capped at 3 visible columns ("now", "5m", "2h", "3d", "4w", "6mo",
/// "2y"). Future-dated commits fold to "now" rather than reporting a
/// negative age.
pub fn format_age_short(commit_seconds: i64, now_seconds: i64) -> String {
    let delta = now_seconds.saturating_sub(commit_seconds).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    if delta < MINUTE {
        return "now".to_string();
    }
    if delta < HOUR {
        return format!("{}m", delta / MINUTE);
    }
    if delta < DAY {
        return format!("{}h", delta / HOUR);
    }
    if delta < WEEK {
        return format!("{}d", delta / DAY);
    }
    if delta < MONTH {
        return format!("{}w", delta / WEEK);
    }
    if delta < YEAR {
        return format!("{}mo", delta / MONTH);
    }
    format!("{}y", delta / YEAR)
}

/// First whitespace-delimited token of `author`, truncated to a
/// character boundary at `max_chars`. Empty input returns an empty
/// string; the renderer pads to the strip's column width.
pub fn author_first_name(author: &str, max_chars: usize) -> String {
    let first = author.split_whitespace().next().unwrap_or("");
    first.chars().take(max_chars).collect()
}

/// Maximum character width of the inline blame label appended at a
/// line's end; longer labels are truncated with a trailing `...`.
const INLINE_BLAME_MAX_CHARS: usize = 40;

/// Compose the end-of-line inline blame label for `entry` -- full
/// author name and verbose relative age (e.g. `Lee Olayvar, 3 days
/// ago`) -- truncated to [`INLINE_BLAME_MAX_CHARS`] characters so the
/// trailing text stays bounded regardless of line width.
pub fn inline_blame_text(entry: &BlameLine, now_seconds: i64) -> String {
    let raw = format!(
        "{}, {}",
        entry.author_name,
        format_relative(entry.time, now_seconds)
    );
    elide(&raw, INLINE_BLAME_MAX_CHARS)
}

/// Format `commit_seconds` as a verbose relative age against
/// `now_seconds` ("just now", "5 minutes ago", "3 days ago", "2 years
/// ago"). The count is singular at 1. Future-dated commits fold to
/// "just now" rather than reporting a negative age.
pub fn format_relative(commit_seconds: i64, now_seconds: i64) -> String {
    let delta = now_seconds.saturating_sub(commit_seconds).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    if delta < MINUTE {
        return "just now".to_string();
    }

    let (count, unit) = if delta < HOUR {
        (delta / MINUTE, "minute")
    } else if delta < DAY {
        (delta / HOUR, "hour")
    } else if delta < WEEK {
        (delta / DAY, "day")
    } else if delta < MONTH {
        (delta / WEEK, "week")
    } else if delta < YEAR {
        (delta / MONTH, "month")
    } else {
        (delta / YEAR, "year")
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
}

/// Truncate `s` to at most `max_chars` characters, ending with `...`
/// when truncation occurs. Operates on `char` boundaries so multi-byte
/// text is never split.
fn elide(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, Subscription, TestAppContext};
    use std::sync::{Arc, Mutex};
    use stoat::buffer::BufferId;

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            state: &Entity<BlameState>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<BlameStateEvent>>>) {
            let events: Arc<Mutex<Vec<BlameStateEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let state = state.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&state, move |_, _, event: &BlameStateEvent, _| {
                            sink.lock().expect("recorder mutex").push(event.clone());
                        });
                    Recorder {
                        _subscription: subscription,
                    }
                })
            });
            (recorder, events)
        }
    }

    fn drain(events: &Arc<Mutex<Vec<BlameStateEvent>>>) -> Vec<BlameStateEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn new_pair(cx: &mut TestAppContext, text: &str) -> (Entity<Buffer>, Entity<BlameState>) {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let state = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| BlameState::new(buffer, cx)))
        };
        (buffer, state)
    }

    fn sample_blame(line: u32) -> BlameLine {
        BlameLine {
            line,
            commit_sha: "abc1234deadbeef".to_string(),
            short_sha: "abc1234".to_string(),
            author_name: "Ada Lovelace".to_string(),
            time: 1_700_000_000,
        }
    }

    #[test]
    fn new_state_is_empty() {
        let mut cx = TestAppContext::single();
        let (_buffer, state) = new_pair(&mut cx, "hi");
        assert!(state.read_with(&cx, |s, _| s.is_empty()));
        assert_eq!(state.read_with(&cx, |s, _| s.blame().len()), 0);
    }

    #[test]
    fn set_blame_stores_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, state) = new_pair(&mut cx, "hi");
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        state.update(&mut cx, |s, cx| s.set_blame(vec![sample_blame(0)], cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BlameStateEvent::Changed]);
        assert_eq!(
            state.read_with(&cx, |s, _| s.blame().to_vec()),
            vec![sample_blame(0)]
        );
    }

    #[test]
    fn buffer_edit_clears_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let (buffer, state) = new_pair(&mut cx, "hi");
        state.update(&mut cx, |s, cx| s.set_blame(vec![sample_blame(0)], cx));
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        buffer.update(&mut cx, |b, cx| b.edit(2..2, "!", cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BlameStateEvent::Changed]);
        assert!(state.read_with(&cx, |s, _| s.is_empty()));
    }

    #[test]
    fn buffer_edit_on_empty_cache_does_not_emit() {
        let mut cx = TestAppContext::single();
        let (buffer, state) = new_pair(&mut cx, "hi");
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        buffer.update(&mut cx, |b, cx| b.edit(2..2, "!", cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<BlameStateEvent>::new());
    }

    #[test]
    fn buffer_reload_invalidates_cache() {
        let mut cx = TestAppContext::single();
        let (buffer, state) = new_pair(&mut cx, "hi");
        state.update(&mut cx, |s, cx| s.set_blame(vec![sample_blame(0)], cx));
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        buffer.update(&mut cx, |b, cx| b.reload(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BlameStateEvent::Changed]);
        assert!(state.read_with(&cx, |s, _| s.is_empty()));
    }

    #[test]
    fn buffer_save_does_not_invalidate_cache() {
        let mut cx = TestAppContext::single();
        let (buffer, state) = new_pair(&mut cx, "hi");
        state.update(&mut cx, |s, cx| s.set_blame(vec![sample_blame(0)], cx));
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        buffer.update(&mut cx, |b, cx| b.save(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<BlameStateEvent>::new());
        assert!(!state.read_with(&cx, |s, _| s.is_empty()));
    }

    #[test]
    fn clear_on_populated_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, state) = new_pair(&mut cx, "hi");
        state.update(&mut cx, |s, cx| s.set_blame(vec![sample_blame(0)], cx));
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        state.update(&mut cx, |s, cx| s.clear(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![BlameStateEvent::Changed]);
        assert!(state.read_with(&cx, |s, _| s.is_empty()));
    }

    #[test]
    fn clear_on_empty_does_not_emit() {
        let mut cx = TestAppContext::single();
        let (_buffer, state) = new_pair(&mut cx, "hi");
        let (_recorder, events) = Recorder::install(&mut cx, &state);

        state.update(&mut cx, |s, cx| s.clear(cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<BlameStateEvent>::new());
    }

    #[test]
    fn format_age_short_thresholds() {
        let now = 1_000_000_000i64;
        assert_eq!(format_age_short(now, now), "now");
        assert_eq!(format_age_short(now - 30, now), "now");
        assert_eq!(format_age_short(now - 60, now), "1m");
        assert_eq!(format_age_short(now - 59 * 60, now), "59m");
        assert_eq!(format_age_short(now - 60 * 60, now), "1h");
        assert_eq!(format_age_short(now - 23 * 60 * 60, now), "23h");
        assert_eq!(format_age_short(now - 24 * 60 * 60, now), "1d");
        assert_eq!(format_age_short(now - 6 * 24 * 60 * 60, now), "6d");
        assert_eq!(format_age_short(now - 7 * 24 * 60 * 60, now), "1w");
        assert_eq!(format_age_short(now - 29 * 24 * 60 * 60, now), "4w");
        assert_eq!(format_age_short(now - 30 * 24 * 60 * 60, now), "1mo");
        assert_eq!(format_age_short(now - 364 * 24 * 60 * 60, now), "12mo");
        assert_eq!(format_age_short(now - 365 * 24 * 60 * 60, now), "1y");
        assert_eq!(format_age_short(now - 730 * 24 * 60 * 60, now), "2y");
    }

    #[test]
    fn format_age_short_future_dated_folds_to_now() {
        let now = 1_000_000_000i64;
        assert_eq!(format_age_short(now + 999, now), "now");
    }

    #[test]
    fn author_first_name_extracts_first_token() {
        assert_eq!(author_first_name("Ada Lovelace", 8), "Ada");
        assert_eq!(author_first_name("Bjarne", 8), "Bjarne");
        assert_eq!(author_first_name("", 8), "");
        assert_eq!(author_first_name("   ", 8), "");
    }

    #[test]
    fn author_first_name_truncates_long_names() {
        assert_eq!(author_first_name("Octocatherine", 8), "Octocath");
        assert_eq!(author_first_name("Octocatherine Smith", 8), "Octocath");
    }

    #[test]
    fn author_first_name_respects_char_boundaries() {
        assert_eq!(author_first_name("Ångström", 4), "Ångs");
    }

    #[test]
    fn format_relative_verbose_thresholds() {
        let now = 1_000_000_000i64;
        assert_eq!(format_relative(now, now), "just now");
        assert_eq!(format_relative(now - 59, now), "just now");
        assert_eq!(format_relative(now - 60, now), "1 minute ago");
        assert_eq!(format_relative(now - 120, now), "2 minutes ago");
        assert_eq!(format_relative(now - 60 * 60, now), "1 hour ago");
        assert_eq!(format_relative(now - 24 * 60 * 60, now), "1 day ago");
        assert_eq!(format_relative(now - 3 * 24 * 60 * 60, now), "3 days ago");
        assert_eq!(format_relative(now - 7 * 24 * 60 * 60, now), "1 week ago");
        assert_eq!(format_relative(now - 30 * 24 * 60 * 60, now), "1 month ago");
        assert_eq!(format_relative(now - 365 * 24 * 60 * 60, now), "1 year ago");
        assert_eq!(
            format_relative(now - 730 * 24 * 60 * 60, now),
            "2 years ago"
        );
    }

    #[test]
    fn format_relative_future_folds_to_just_now() {
        let now = 1_000_000_000i64;
        assert_eq!(format_relative(now + 5000, now), "just now");
    }

    #[test]
    fn inline_blame_text_composes_author_and_age() {
        let now = 1_700_000_000i64 + 3 * 24 * 60 * 60;
        assert_eq!(
            inline_blame_text(&sample_blame(0), now),
            "Ada Lovelace, 3 days ago"
        );
    }

    #[test]
    fn inline_blame_text_elides_long_label() {
        let mut entry = sample_blame(0);
        entry.author_name = "A".repeat(60);
        let out = inline_blame_text(&entry, entry.time);
        assert_eq!(out.chars().count(), INLINE_BLAME_MAX_CHARS);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn elide_truncates_on_char_boundaries() {
        assert_eq!(elide("short", 40), "short");
        assert_eq!(elide("Ångström rocks", 8), "Ångst...");
    }
}
