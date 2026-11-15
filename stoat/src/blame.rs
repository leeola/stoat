use crate::{
    buffer::{BufferId, TextBufferSnapshot},
    buffer_registry::BufferRegistry,
    editor_state::EditorId,
    host::{BlameCommit, FileBlame, GitHost, GitRepo},
    wake,
    workspace::diff::{self, DIFF_SETTLE},
};
use std::{
    collections::HashMap,
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};
use stoat_language::structural_diff;
use stoat_scheduler::{Executor, Task};
use stoat_text::{Anchor, Bias, Point};
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::Notify;

/// The age past which a blame date shows its year in place of its month and
/// day.
const YEAR_SECONDS: i64 = 365 * 86_400;

/// The most commits whose Blame Card details [`BlameState`] keeps.
const DETAILS_CAPACITY: usize = 64;

/// The commit of each line of every blamed buffer in a workspace.
///
/// The blame reads HEAD. A buffer line that differs from HEAD's text, through
/// a staged, unstaged, or unsaved edit, is uncommitted. The data lives per
/// buffer, so two panes of one file share it.
///
/// A blame runs on the blocking pool and maps each line against the buffer
/// text it was given. Its runs ride anchors from then on, so they follow typing
/// until the next run. That run waits out [`DIFF_SETTLE`] after the typing
/// stops, as a diff does.
#[derive(Default)]
pub(crate) struct BlameState {
    files: HashMap<BufferId, BufferBlame>,
    jobs: HashMap<BufferId, BlameJob>,
    /// HEAD's blame of each path a job resolved, so a re-map after typing reads
    /// no git. `None` for a path HEAD does not hold.
    base: HashMap<PathBuf, Option<Arc<BlameBase>>>,
    /// The version of each buffer whose settle window is open, and when it
    /// opened.
    settle: HashMap<BufferId, (u64, Instant)>,
    settle_timers: HashMap<BufferId, Task<()>>,
    /// The count of blames that landed. A landed blame changes the runs of a
    /// blamed pane while its buffer stays the same.
    pub(crate) epoch: u64,
    /// The Blame Card details of each commit read so far, by full sha. A
    /// commit's message and change never move, so no git write stales them.
    details: HashMap<String, Arc<CommitDetails>>,
}

/// The commit of one line of a blamed buffer.
pub(crate) enum LineBlame<'a> {
    /// The line differs from HEAD's text, or HEAD does not hold the file.
    Uncommitted,
    /// The commit that last changed the line, and its index in the file's
    /// blame, which every run of the commit shares.
    ///
    /// `commit_line` is the 0-based line the line has in the commit's own
    /// version of the file, and `commit_path` is that version's repo-relative
    /// path. A patch of the commit counts its lines there, so a view of the
    /// patch finds the line at that place.
    Commit {
        index: u32,
        commit: &'a BlameCommit,
        commit_line: u32,
        commit_path: &'a Path,
    },
}

/// HEAD's text of one file and the commit that last changed each of its lines.
///
/// The text and the blame read one commit, so the lines the blame counts are
/// the lines of the text.
pub(crate) struct BlameBase {
    pub(crate) text: String,
    pub(crate) blame: FileBlame,
    /// The age bucket of each commit of `blame`, by its index, as
    /// [`age_buckets`] ranks them.
    pub(crate) ages: Vec<u8>,
}

/// A run of consecutive buffer lines that keep consecutive lines of one blame
/// run, or of uncommitted lines, pinned to the buffer text that the blame
/// mapped.
struct BlameSpan {
    anchors: Range<Anchor>,
    /// Where the span's lines sit in HEAD's blame, or `None` for uncommitted
    /// lines.
    origin: Option<SpanOrigin>,
}

/// Where the lines of a span sit in HEAD's blame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpanOrigin {
    /// The index of the commit in [`FileBlame::commits`].
    commit: u32,
    /// The index in [`FileBlame::runs`] of the run that holds the span's lines.
    run: u32,
    /// The HEAD line of the span's first line. The span's later lines follow it
    /// in order.
    base_line: u32,
}

/// The spans of one buffer as the rows they cover in one version of its text.
///
/// Each run is one visual run of the Blame Column, which is a maximal group of
/// consecutive rows with one commit, or of uncommitted rows.
pub(crate) struct BlameRows {
    /// The rows in order, with no gap between two runs. Two adjacent runs name
    /// different commits.
    pub(crate) runs: Vec<(Range<u32>, Option<u32>)>,
    /// The rows of each span in order, with the span's origin. A run of the
    /// column joins the adjacent spans of one commit.
    pub(crate) spans: Vec<(Range<u32>, Option<SpanOrigin>)>,
}

/// The body of a commit's message and the size of its change, which the Blame
/// Card reads beyond the commit's blame entry.
pub(crate) struct CommitDetails {
    /// The first paragraph of the message after the subject, its lines joined
    /// with spaces, or `None` for a message of one paragraph.
    pub(crate) body: Option<String>,
    pub(crate) files: usize,
    pub(crate) added: u32,
    pub(crate) removed: u32,
}

/// The card that names the commit of the cursor's line in a blamed pane.
///
/// The card holds its commit, so a blame that lands under it does not change
/// what it shows. The card closes when the cursor leaves its row.
pub(crate) struct BlameCard {
    pub(crate) editor: EditorId,
    /// The buffer row the card opened on.
    pub(crate) row: u32,
    pub(crate) commit: BlameCommit,
    /// The body and the change size, `None` until they load.
    pub(crate) details: Option<Arc<CommitDetails>>,
    /// The key and the label of each blame binding the card lists, in keymap
    /// order.
    pub(crate) hints: Vec<(String, String)>,
    /// The read of the details on the blocking pool. It answers `None` when it
    /// finds no repository.
    load: Option<Task<Option<CommitDetails>>>,
}

/// The blame of one buffer.
struct BufferBlame {
    /// The buffer version the spans were mapped against.
    version: u64,
    /// Whether the git state moved after the blame read it, which owes the
    /// buffer a run at any version.
    stale: bool,
    base: Option<Arc<BlameBase>>,
    /// The runs of the buffer, in line order. Empty for a file outside the
    /// repository, whose lines have no blame.
    spans: Vec<BlameSpan>,
    /// The spans resolved against the buffer text, with the version they were
    /// resolved at.
    rows: Option<(u64, Arc<BlameRows>)>,
}

struct BlameJob {
    task: Task<BlameJobOutput>,
    /// Whether the git state moved while the job ran. Its answer then reads the
    /// state from before the move, or from after it, and nothing tells which.
    superseded: bool,
}

struct BlameJobOutput {
    buffer_id: BufferId,
    path: PathBuf,
    version: u64,
    /// The base the job read. `None` for a path outside the repository.
    base: Option<Option<Arc<BlameBase>>>,
    spans: Vec<BlameSpan>,
}

impl BlameState {
    /// Stale the blame of every buffer, after the git state moved under the
    /// editor.
    ///
    /// A blame in flight still lands, and its buffer runs again after it. A
    /// buffer that is not blamed runs again when a pane blames it.
    pub(crate) fn invalidate_all(&mut self) {
        self.base.clear();
        for file in self.files.values_mut() {
            file.stale = true;
        }
        for job in self.jobs.values_mut() {
            job.superseded = true;
        }
    }

    /// Land the blames that finished, and start one for each buffer in
    /// `wanted` whose blame is behind its text or the git state.
    ///
    /// `wanted` holds the buffers that a blamed pane shows. A buffer whose blame
    /// read an older version of its text waits out [`DIFF_SETTLE`] first, so a
    /// typing burst costs one run. A buffer with no blame yet, or with a stale
    /// one, starts at once. A buffer waits for its blame in flight to land.
    ///
    /// Answers whether a blame landed or started.
    pub(crate) fn pump(
        &mut self,
        buffers: &BufferRegistry,
        git_root: &Path,
        wanted: &[BufferId],
        executor: &Executor,
        git_host: &Arc<dyn GitHost>,
        redraw: &Arc<Notify>,
    ) -> bool {
        let landed = self.land(redraw);
        self.files.retain(|id, _| buffers.get(*id).is_some());
        self.settle.retain(|id, _| buffers.get(*id).is_some());
        self.settle_timers
            .retain(|id, _| buffers.get(*id).is_some());

        let mut started = false;
        for &id in wanted {
            let Some(shared) = buffers.get(id) else {
                continue;
            };
            let version = shared.read().expect("buffer poisoned").snapshot.version;
            if !self.owes_run(executor, redraw, id, version) || self.jobs.contains_key(&id) {
                continue;
            }
            let Some(path) = buffers.path_for(id).map(Path::to_path_buf) else {
                continue;
            };

            let snapshot = shared.read().expect("buffer poisoned").snapshot.clone();
            let cached = self.base.get(&path).cloned();
            let task = executor.spawn_blocking({
                let git_host = git_host.clone();
                let git_root = git_root.to_path_buf();
                let redraw = redraw.clone();
                move || {
                    let base = cached.or_else(|| resolve_base(&*git_host, &git_root, &path));
                    let spans = base
                        .as_ref()
                        .map_or_else(Vec::new, |base| map_spans(base.as_deref(), &snapshot));
                    redraw.notify_one();
                    BlameJobOutput {
                        buffer_id: id,
                        path,
                        version: snapshot.version,
                        base,
                        spans,
                    }
                }
            });
            self.jobs.insert(
                id,
                BlameJob {
                    task,
                    superseded: false,
                },
            );
            started = true;
        }
        landed || started
    }

    /// Resolve the spans of each buffer in `wanted` to the rows they cover in
    /// its text now.
    ///
    /// A typed line moves the rows below it before the next blame runs, so a
    /// paint resolves them first. A buffer whose rows match its version costs
    /// one lock.
    pub(crate) fn refresh_rows(&mut self, buffers: &BufferRegistry, wanted: &[BufferId]) {
        for &id in wanted {
            let (Some(file), Some(shared)) = (self.files.get_mut(&id), buffers.get(id)) else {
                continue;
            };
            let buffer = shared.read().expect("buffer poisoned");
            let version = buffer.snapshot.version;
            if file.rows.as_ref().is_some_and(|(at, _)| *at == version) {
                continue;
            }
            let rows = resolve_rows(&file.spans, &buffer.snapshot);
            file.rows = Some((version, Arc::new(rows)));
        }
    }

    /// The commit of row `row` of buffer `id`, with the row's place in that
    /// commit's version of the file, as [`Self::refresh_rows`] last resolved
    /// the rows.
    ///
    /// `None` before a blame of the buffer lands, for a file outside the
    /// repository, and for a row past the last line.
    pub(crate) fn line(&self, id: BufferId, row: u32) -> Option<LineBlame<'_>> {
        let file = self.files.get(&id)?;
        let (rows, origin) = file.rows.as_ref()?.1.span(row)?;
        let Some(origin) = origin else {
            return Some(LineBlame::Uncommitted);
        };
        let blame = &file.base.as_ref()?.blame;
        let run = blame.runs.get(origin.run as usize)?;
        let commit = blame.commits.get(origin.commit as usize)?;

        // A span's lines are consecutive lines of its run, so a row's offset
        // into the span carries into the run. Typing inside the span since the
        // blame mapped it carries a row past the run's end, which clamps.
        let offset = origin.base_line.saturating_sub(run.start_line) + (row - rows.start);
        Some(LineBlame::Commit {
            index: origin.commit,
            commit,
            commit_line: run.commit_line + offset.min(run.lines.saturating_sub(1)),
            commit_path: &run.commit_path,
        })
    }

    /// The index of the commit of row `row` of buffer `id`, which every run of
    /// that commit shares, or `None` for an uncommitted row and before a blame
    /// lands.
    pub(crate) fn commit_index(&self, id: BufferId, row: u32) -> Option<u32> {
        match self.line(id, row)? {
            LineBlame::Commit { index, .. } => Some(index),
            LineBlame::Uncommitted => None,
        }
    }

    /// The runs of buffer `id` as [`Self::refresh_rows`] last resolved them,
    /// or `None` before a blame of the buffer lands.
    pub(crate) fn rows(&self, id: BufferId) -> Option<&Arc<BlameRows>> {
        self.files.get(&id)?.rows.as_ref().map(|(_, rows)| rows)
    }

    /// HEAD's text and blame of buffer `id`, or `None` before a blame lands and
    /// for a file HEAD does not hold.
    pub(crate) fn base(&self, id: BufferId) -> Option<&Arc<BlameBase>> {
        self.files.get(&id)?.base.as_ref()
    }

    /// The Blame Card details of commit `sha`, or `None` before a card read
    /// them.
    pub(crate) fn details(&self, sha: &str) -> Option<&Arc<CommitDetails>> {
        self.details.get(sha)
    }

    /// Keep the Blame Card details of commit `sha`.
    ///
    /// The cache holds at most [`DETAILS_CAPACITY`] commits. A full cache
    /// empties before it takes the next, so a long walk through the history
    /// costs no more than that many entries.
    pub(crate) fn file_details(&mut self, sha: String, details: Arc<CommitDetails>) {
        if self.details.len() >= DETAILS_CAPACITY {
            self.details.clear();
        }
        self.details.insert(sha, details);
    }

    /// Whether buffer `id` at `version` is owed a blame run on this pump.
    fn owes_run(
        &mut self,
        executor: &Executor,
        redraw: &Arc<Notify>,
        id: BufferId,
        version: u64,
    ) -> bool {
        let held = self.files.get(&id).map(|file| (file.version, file.stale));
        match held {
            None => true,
            Some((held, stale)) if held == version => stale,
            Some(_) => self.settled(executor, redraw, id, version),
        }
    }

    /// Whether buffer `id` has held `version` for [`DIFF_SETTLE`].
    ///
    /// A version seen for the first time opens the window and arms a redraw
    /// timer, so the frame that closes the window is drawn even when nothing
    /// else asks for one.
    fn settled(
        &mut self,
        executor: &Executor,
        redraw: &Arc<Notify>,
        id: BufferId,
        version: u64,
    ) -> bool {
        let now = executor.now();
        match self.settle.get(&id) {
            Some((settling, since)) if *settling == version => {
                now.duration_since(*since) >= DIFF_SETTLE
            },
            _ => {
                self.settle.insert(id, (version, now));
                self.settle_timers
                    .insert(id, diff::settle_timer(executor, redraw));
                false
            },
        }
    }

    /// Install the blame of each job that finished, and answer whether one
    /// landed.
    fn land(&mut self, redraw: &Arc<Notify>) -> bool {
        if self.jobs.is_empty() {
            return false;
        }

        let waker = wake::notify_waker(redraw);
        let mut cx = Context::from_waker(&waker);
        let mut landed = Vec::new();
        self.jobs
            .retain(|_, job| match Pin::new(&mut job.task).poll(&mut cx) {
                Poll::Ready(out) => {
                    landed.push((out, job.superseded));
                    false
                },
                Poll::Pending => true,
            });

        let any = !landed.is_empty();
        for (out, superseded) in landed {
            // The base of a superseded job is possibly from before the move,
            // so the cache does not keep it, and its buffer owes another run.
            if let Some(base) = &out.base
                && !superseded
            {
                self.base.insert(out.path, base.clone());
            }
            self.files.insert(
                out.buffer_id,
                BufferBlame {
                    version: out.version,
                    stale: superseded,
                    base: out.base.flatten(),
                    spans: out.spans,
                    rows: None,
                },
            );
            self.epoch += 1;
        }
        any
    }
}

impl BlameRows {
    /// The run that holds row `row`, as its rows and its commit, or `None` for
    /// a row past the last run.
    pub(crate) fn run(&self, row: u32) -> Option<&(Range<u32>, Option<u32>)> {
        let after = self.runs.partition_point(|(rows, _)| rows.end <= row);
        self.runs.get(after).filter(|(rows, _)| rows.contains(&row))
    }

    /// The span that holds row `row`, as its rows and its origin, or `None` for
    /// a row past the last span.
    fn span(&self, row: u32) -> Option<&(Range<u32>, Option<SpanOrigin>)> {
        let after = self.spans.partition_point(|(rows, _)| rows.end <= row);
        self.spans
            .get(after)
            .filter(|(rows, _)| rows.contains(&row))
    }
}

impl BlameCard {
    /// A card that names `commit` on row `row` of the buffer that `editor`
    /// shows, with its `details` when a cache holds them and the `load` that
    /// reads them otherwise.
    pub(crate) fn new(
        editor: EditorId,
        row: u32,
        commit: BlameCommit,
        details: Option<Arc<CommitDetails>>,
        load: Option<Task<Option<CommitDetails>>>,
    ) -> Self {
        Self {
            editor,
            row,
            commit,
            details,
            hints: Vec::new(),
            load,
        }
    }

    /// The details once their load lands, or `None` while it runs.
    ///
    /// A card with no load, or whose load landed, answers `None`, and so does
    /// a load that found no repository. A load still pending wakes `redraw`
    /// when it lands.
    pub(crate) fn poll_details(&mut self, redraw: &Arc<Notify>) -> Option<CommitDetails> {
        let task = self.load.as_mut()?;
        let waker = wake::notify_waker(redraw);
        let landed = match Pin::new(task).poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(details) => details,
            Poll::Pending => return None,
        };
        self.load = None;
        landed
    }
}

/// For each line of `new`, the line of `base` that it keeps, or `None` for a
/// line in a region that the line diff from `base` changed.
///
/// A buffer line keeps the commit of the HEAD line it matches, so the blame of
/// a buffer maps its lines with this. The fake host's blame follows a line back
/// through history the same way. A line keeps an older commit for as long as
/// each diff leaves it alone.
pub(crate) fn line_origins(base: &str, new: &str) -> Vec<Option<u32>> {
    let new_lines = new.split_inclusive('\n').count();
    let mut origins: Vec<Option<u32>> = Vec::with_capacity(new_lines);
    let mut base_line = 0;
    for (base_bytes, new_bytes) in structural_diff::line_hunks(base, new) {
        let kept = boundary_line(new, new_bytes.start) as usize - origins.len();
        origins.extend((base_line..).take(kept).map(Some));
        origins.resize(boundary_line(new, new_bytes.end) as usize, None);
        base_line = boundary_line(base, base_bytes.end);
    }

    let kept = new_lines - origins.len();
    origins.extend((base_line..).take(kept).map(Some));
    origins
}

/// The details of commit `sha` in `repo`, as the Blame Card shows them.
///
/// Reads the commit's message and its change against its first parent, so a
/// caller runs it on the blocking pool. An unknown sha reads no body and no
/// change.
pub(crate) fn commit_details(repo: &dyn GitRepo, sha: &str) -> CommitDetails {
    let changes = repo.commit_file_changes(sha);
    CommitDetails {
        body: repo.commit_message(sha).as_deref().and_then(message_body),
        files: changes.len(),
        added: changes.iter().map(|change| change.additions).sum(),
        removed: changes.iter().map(|change| change.deletions).sum(),
    }
}

/// The first paragraph of commit message `message` after its subject, its
/// lines trimmed and joined with spaces, or `None` for a one-paragraph
/// message.
fn message_body(message: &str) -> Option<String> {
    let mut lines = message.lines().map(str::trim);
    lines
        .by_ref()
        .take_while(|line| !line.is_empty())
        .for_each(drop);
    let body: Vec<&str> = lines
        .skip_while(|line| line.is_empty())
        .take_while(|line| !line.is_empty())
        .collect();
    (!body.is_empty()).then(|| body.join(" "))
}

/// The first word of author name `name`, which names the author where the
/// room is short.
///
/// A name with no word in it answers whole.
pub(crate) fn author_word(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// The date of commit time `time` in the zone `utc_offset_minutes` east of
/// UTC, short enough for a status row.
///
/// A commit of the 365 days before `now_epoch` shows its `MM/DD`, and an older
/// one shows its four-digit year. A time or an offset that no date holds shows
/// `--/--`.
pub(crate) fn short_date(time: i64, utc_offset_minutes: i32, now_epoch: i64) -> String {
    let Some(date) = zoned(time, utc_offset_minutes) else {
        return "--/--".to_owned();
    };

    if now_epoch.saturating_sub(time) <= YEAR_SECONDS {
        format!("{:02}/{:02}", u8::from(date.month()), date.day())
    } else {
        date.year().to_string()
    }
}

/// The date and the time of commit time `time` in the zone
/// `utc_offset_minutes` east of UTC, as the Blame Card's meta line reads them.
///
/// A commit of the 365 days before `now_epoch` shows `MM/DD HH:MM`, and an
/// older one leads with its year, as `YYYY/MM/DD HH:MM`. A time or an offset
/// that no date holds shows `--/-- --:--`.
pub(crate) fn date_time(time: i64, utc_offset_minutes: i32, now_epoch: i64) -> String {
    let Some(date) = zoned(time, utc_offset_minutes) else {
        return "--/-- --:--".to_owned();
    };

    let (month, day) = (u8::from(date.month()), date.day());
    let (hour, minute) = (date.hour(), date.minute());
    if now_epoch.saturating_sub(time) <= YEAR_SECONDS {
        format!("{month:02}/{day:02} {hour:02}:{minute:02}")
    } else {
        format!("{}/{month:02}/{day:02} {hour:02}:{minute:02}", date.year())
    }
}

/// Commit time `time` in the zone `utc_offset_minutes` east of UTC, or `None`
/// when no date holds the time or no zone the offset.
fn zoned(time: i64, utc_offset_minutes: i32) -> Option<OffsetDateTime> {
    let offset = UtcOffset::from_whole_seconds(utc_offset_minutes.checked_mul(60)?).ok()?;
    OffsetDateTime::from_unix_timestamp(time)
        .ok()?
        .checked_to_offset(offset)
}

/// The age bucket of each of `commits`, by its index, from 0 for the oldest to
/// 3 for the newest.
///
/// The bucket is a rank among the file's distinct commit times, not an
/// absolute age, so a file of four commits draws four colors however close
/// their dates lie. A lone commit takes the newest bucket.
pub(crate) fn age_buckets(commits: &[BlameCommit]) -> Vec<u8> {
    let mut times: Vec<i64> = commits.iter().map(|commit| commit.info.time).collect();
    times.sort_unstable();
    times.dedup();

    let top = times.len().saturating_sub(1);
    commits
        .iter()
        .map(|commit| match top {
            0 => 3,
            _ => (times.partition_point(|&time| time < commit.info.time) * 3 / top) as u8,
        })
        .collect()
}

/// HEAD's blame of `path` in the repository at `git_root`.
///
/// `None` for a path outside the workdir, which has no blame. `Some(None)` for
/// a path HEAD does not hold and for a repository with no commit.
fn resolve_base(
    git_host: &dyn GitHost,
    git_root: &Path,
    path: &Path,
) -> Option<Option<Arc<BlameBase>>> {
    let repo = git_host.discover(git_root)?;
    if !repo
        .workdir()
        .is_some_and(|workdir| path.starts_with(workdir))
    {
        return None;
    }

    // HEAD resolves once, so the text and the blame read the same commit even
    // when a commit lands between the two reads.
    let base = repo.resolve_rev("HEAD").and_then(|head| {
        let text = repo.content_at(&head, path)?;
        let blame = repo.blame(path, Some(&head))?;
        let ages = age_buckets(&blame.commits);
        Some(Arc::new(BlameBase { text, blame, ages }))
    });
    Some(base)
}

/// Group the lines of `snapshot` into spans, each anchored in `snapshot`.
///
/// A line takes the place in HEAD's blame of the HEAD line it keeps under the
/// line diff from `base`'s text. A span holds consecutive lines of one blame
/// run, so a line that keeps another run, or a HEAD line past a gap, starts
/// the next span. A line in a changed region is uncommitted, and so is every
/// line when HEAD does not hold the file.
fn map_spans(base: Option<&BlameBase>, snapshot: &TextBufferSnapshot) -> Vec<BlameSpan> {
    let text = snapshot.visible_text.to_string();
    let origins: Vec<Option<SpanOrigin>> = match base {
        Some(base) => line_origins(&base.text, &text)
            .into_iter()
            .map(|origin| origin.and_then(|line| line_origin(&base.blame, line)))
            .collect(),
        None => vec![None; text.split_inclusive('\n').count()],
    };

    let mut groups: Vec<(Range<u32>, Option<SpanOrigin>)> = Vec::new();
    for (line, origin) in (0u32..).zip(origins) {
        match groups.last_mut() {
            Some((lines, last)) if continues(*last, origin, lines.end - lines.start) => {
                lines.end += 1
            },
            _ => groups.push((line..line + 1, origin)),
        }
    }

    // A row past the last one answers the rope's length, which is how the last
    // span's end reaches past a final line with no newline.
    let rope = &snapshot.visible_text;
    let row_start = |row: u32| rope.point_to_offset(Point::new(row, 0));
    let starts: Vec<usize> = groups
        .iter()
        .map(|(lines, _)| row_start(lines.start))
        .collect();
    let ends: Vec<usize> = groups
        .iter()
        .map(|(lines, _)| row_start(lines.end))
        .collect();
    let starts = snapshot.anchors_at_batch(&starts, Bias::Left);
    let ends = snapshot.anchors_at_batch(&ends, Bias::Right);

    groups
        .into_iter()
        .zip(starts.into_iter().zip(ends))
        .map(|((_, origin), (start, end))| BlameSpan {
            anchors: start..end,
            origin,
        })
        .collect()
}

/// Whether a line of `origin` continues a span of `len` lines that starts at
/// `first`, which holds when both are uncommitted or when the line keeps the
/// next HEAD line of the same run.
fn continues(first: Option<SpanOrigin>, origin: Option<SpanOrigin>, len: u32) -> bool {
    match (first, origin) {
        (None, None) => true,
        (Some(first), Some(origin)) => {
            origin.run == first.run && origin.base_line == first.base_line + len
        },
        _ => false,
    }
}

/// Where line `line` of the blamed text sits in `blame`, from the run that
/// covers it.
fn line_origin(blame: &FileBlame, line: u32) -> Option<SpanOrigin> {
    let run = blame
        .runs
        .partition_point(|run| run.start_line <= line)
        .checked_sub(1)?;
    let held = &blame.runs[run];
    (line < held.start_line + held.lines).then_some(SpanOrigin {
        commit: held.commit,
        run: run as u32,
        base_line: line,
    })
}

/// The rows that `spans` cover in `snapshot`.
///
/// A row belongs to the span that holds its first byte. Text typed where two
/// spans meet falls inside both, since a start anchors left and an end right,
/// so each span starts no earlier than the span before it ends. Each span
/// keeps its own rows, and a run of the column joins the adjacent spans of one
/// commit.
fn resolve_rows(spans: &[BlameSpan], snapshot: &TextBufferSnapshot) -> BlameRows {
    let anchors: Vec<Anchor> = spans
        .iter()
        .flat_map(|span| [span.anchors.start, span.anchors.end])
        .collect();
    let offsets = snapshot.resolve_anchors_batch(&anchors);

    // The count of row starts before `offset`, which is the first row whose
    // first byte sits at or after it.
    let rope = &snapshot.visible_text;
    let rows_before = |offset: usize| {
        let point = rope.offset_to_point(offset);
        point.row + u32::from(point.column > 0)
    };

    let mut runs: Vec<(Range<u32>, Option<u32>)> = Vec::with_capacity(spans.len());
    let mut span_rows = Vec::with_capacity(spans.len());
    let mut end = 0;
    let (pairs, _) = offsets.as_chunks::<2>();
    for (span, &[start_offset, end_offset]) in spans.iter().zip(pairs) {
        let start = rows_before(start_offset).max(end);
        end = rows_before(end_offset).max(start);
        if start == end {
            continue;
        }
        span_rows.push((start..end, span.origin));
        let commit = span.origin.map(|origin| origin.commit);
        match runs.last_mut() {
            Some((rows, last)) if *last == commit => rows.end = end,
            _ => runs.push((start..end, commit)),
        }
    }
    BlameRows {
        runs,
        spans: span_rows,
    }
}

/// The line that a region boundary at byte `offset` of `text` comes before.
///
/// A region covers whole lines, so a boundary is the start of a line or the
/// end of the text. The end comes after the last line even when that line has
/// no newline. A count of the newlines before the end misses that line.
fn boundary_line(text: &str, offset: usize) -> u32 {
    if offset == text.len() {
        return text.split_inclusive('\n').count() as u32;
    }
    text.as_bytes()[..offset]
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::{
        age_buckets, date_time, line_origins, message_body, short_date, BlameState, CommitDetails,
        LineBlame, DETAILS_CAPACITY,
    };
    use crate::{
        app::Stoat,
        host::{BlameCommit, CommitInfo},
        test_harness::{CommitSpec, TestHarness},
        workspace::diff::DIFF_SETTLE,
    };
    use std::{ops::Range, path::Path, sync::Arc, time::Duration};

    /// 2026-11-11T14:02:00Z, in unix epoch seconds.
    const NOV_11: i64 = 1_794_405_720;
    const DAY: i64 = 86_400;

    /// The history every blamed file below sits over: c1 writes `one` to `a.rs`,
    /// and c2 adds `two`.
    const HISTORY: &[CommitSpec<'static>] = &[
        ("c1", "one", &[("a.rs", "one\n")]),
        ("c2", "two", &[("a.rs", "one\ntwo\n")]),
    ];

    #[test]
    fn line_origins_map_each_kept_line_to_its_base_line() {
        assert_eq!(
            [
                line_origins("a\nb\n", "a\nb\n"),
                line_origins("a\nb\n", "a\nx\nb\n"),
                line_origins("a\nb\nc\n", "a\nx\nc\n"),
                line_origins("a\nb\nc\n", "a\nc\n"),
            ],
            [
                vec![Some(0), Some(1)],
                vec![Some(0), None, Some(1)],
                vec![Some(0), None, Some(2)],
                vec![Some(0), Some(2)],
            ],
            "equal texts, an inserted line, a replaced line, a deleted line",
        );
    }

    /// A date reads its month and day in the commit's own zone for a year,
    /// and its year after that.
    #[test]
    fn short_date_reads_the_day_in_the_commit_zone_and_the_year_past_a_year() {
        assert_eq!(
            [
                short_date(NOV_11, 0, NOV_11 + 3 * DAY),
                short_date(NOV_11 - 9 * 3_600, -480, NOV_11),
                short_date(NOV_11 - 365 * DAY, 0, NOV_11),
                short_date(NOV_11 - 400 * DAY, 0, NOV_11),
                short_date(i64::MAX, 0, NOV_11),
            ],
            ["11/11", "11/10", "11/11", "2025", "--/--"],
            "three days old, eight hours west of UTC, 365 days old, 400 days old, past every date",
        );
    }

    /// A date and time read the minute in the commit's own zone, and lead with
    /// the year after a year.
    #[test]
    fn date_time_reads_the_minute_in_the_commit_zone_and_the_year_past_a_year() {
        assert_eq!(
            [
                date_time(NOV_11, 0, NOV_11 + 3 * DAY),
                date_time(NOV_11 - 9 * 3_600, -480, NOV_11),
                date_time(NOV_11 - 400 * DAY, 0, NOV_11),
                date_time(i64::MAX, 0, NOV_11),
            ],
            [
                "11/11 14:02",
                "11/10 21:02",
                "2025/10/07 14:02",
                "--/-- --:--"
            ],
            "three days old, eight hours west of UTC, 400 days old, past every date",
        );
    }

    /// A full details cache empties before it takes the next commit, so that
    /// commit stands alone in it.
    #[test]
    fn a_full_details_cache_empties_before_it_takes_the_next() {
        let mut state = BlameState::default();
        let details = || {
            Arc::new(CommitDetails {
                body: None,
                files: 0,
                added: 0,
                removed: 0,
            })
        };
        for index in 0..DETAILS_CAPACITY {
            state.file_details(format!("sha{index}"), details());
        }
        let full = state
            .details(&format!("sha{}", DETAILS_CAPACITY - 1))
            .is_some();
        state.file_details("next".to_owned(), details());
        assert_eq!(
            (
                full,
                state
                    .details(&format!("sha{}", DETAILS_CAPACITY - 1))
                    .is_some(),
                state.details("next").is_some(),
            ),
            (true, false, true),
        );
    }

    /// A message body is the first paragraph after the subject with its lines
    /// joined, and a message of one paragraph has none.
    #[test]
    fn message_body_joins_the_first_paragraph_after_the_subject() {
        assert_eq!(
            [
                message_body("subject"),
                message_body("subject\n\nfirst line\nsecond line\n\nlater paragraph\n"),
                message_body("subject\nwrapped subject\n\n\n  indented body  \n"),
            ],
            [
                None,
                Some("first line second line".to_owned()),
                Some("indented body".to_owned())
            ],
        );
    }

    /// The buckets rank the distinct commit times, oldest 0 and newest 3, and a
    /// lone commit takes the newest.
    #[test]
    fn age_buckets_rank_the_commit_times_into_four_buckets() {
        let buckets = |times: &[i64]| {
            let commits: Vec<BlameCommit> = times
                .iter()
                .map(|&time| BlameCommit {
                    info: CommitInfo {
                        sha: String::new(),
                        short_sha: String::new(),
                        summary: String::new(),
                        author_name: String::new(),
                        author_email: String::new(),
                        time,
                        parents: Vec::new(),
                    },
                    utc_offset_minutes: 0,
                })
                .collect();
            age_buckets(&commits)
        };
        assert_eq!(
            [
                buckets(&[5]),
                buckets(&[9, 5]),
                buckets(&[4, 1, 3, 2]),
                buckets(&[50, 10, 40, 20, 30]),
            ],
            [vec![3], vec![3, 0], vec![3, 0, 2, 1], vec![3, 0, 2, 0, 1]],
            "one, two, four, and five commit times",
        );
    }

    /// A committed line names its commit, the index every run of that commit
    /// shares, and its line and path in the commit's own version of the file. A
    /// later commit that adds a line above it and drops one between its
    /// neighbors moves it in HEAD, and a buffer that adds and drops lines moves
    /// it again, so its commit line differs from its HEAD line and from its
    /// buffer row. Its commit index differs from the index of its blame run.
    #[test]
    fn a_line_names_its_place_in_its_commit() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("c1", "one", &[("a.rs", "a\nb\nc\nd\ne\n")]),
                ("c2", "two", &[("a.rs", "x\na\nc\nd\ne\n")]),
            ],
        );
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h.fake_fs().insert_file("/repo/a.rs", "new\nx\na\nc\ne\n");
        h.open_file(Path::new("/repo/a.rs"));
        h.type_keys("space h a");
        h.settle();

        let at = |place: &str| Some(place.to_string());
        assert_eq!(
            commit_lines(&mut h, 0..5),
            [
                None,
                at("c2/0 a.rs:0"),
                at("c1/1 a.rs:0"),
                at("c1/1 a.rs:2"),
                at("c1/1 a.rs:4"),
            ],
        );
    }

    /// A line typed inside a span before the next blame run keeps the span's
    /// commit, and its commit line stays inside the span's run.
    #[test]
    fn a_line_typed_into_a_span_keeps_its_place_inside_the_run() {
        let mut h = blamed("/repo/a.rs", "one\ntwo\n");
        h.type_keys("j o");
        let at = Some("c2/1 a.rs:1".to_string());
        assert_eq!(commit_lines(&mut h, 1..3), [at.clone(), at]);
    }

    /// Each line names the commit that last changed it. A line that differs
    /// from HEAD, and every line of a file HEAD does not hold, is uncommitted.
    /// A file outside the repository has no blame.
    #[test]
    fn a_blamed_buffer_names_the_commit_of_each_line() {
        let mut clean = blamed("/repo/a.rs", "one\ntwo\n");
        let mut edited = blamed("/repo/a.rs", "one\nTWO\nthree\n");
        let mut untracked = blamed("/repo/b.rs", "one\ntwo\n");
        let mut outside = blamed("/elsewhere/a.rs", "one\ntwo\n");
        assert_eq!(
            [
                blame_runs(&mut clean),
                blame_runs(&mut edited),
                blame_runs(&mut untracked),
                blame_runs(&mut outside),
            ],
            [
                shas(&[(0..1, Some("c1")), (1..2, Some("c2"))]),
                shas(&[(0..1, Some("c1")), (1..3, None)]),
                shas(&[(0..2, None)]),
                shas(&[]),
            ],
        );
    }

    /// A line typed where two runs meet joins the run above it at once, and
    /// the run after the settle window marks it uncommitted.
    #[test]
    fn a_typed_line_turns_uncommitted_once_the_typing_settles() {
        let mut h = blamed("/repo/a.rs", "one\ntwo\n");
        h.edit_focused(4..4, "new\n");
        let typed = blame_runs(&mut h);
        h.advance_clock(DIFF_SETTLE - Duration::from_millis(1));
        let settling = blame_runs(&mut h);
        h.advance_clock(Duration::from_millis(2));
        assert_eq!(
            [typed, settling, blame_runs(&mut h)],
            [
                shas(&[(0..2, Some("c1")), (2..3, Some("c2"))]),
                shas(&[(0..2, Some("c1")), (2..3, Some("c2"))]),
                shas(&[(0..1, Some("c1")), (1..2, None), (2..3, Some("c2"))]),
            ],
            "the anchors carry the runs until the window closes, and the run after it maps the lines",
        );
    }

    /// A commit under the editor leaves the blame as it was until the git
    /// state stales the diffs, and then the committed line names its commit.
    #[test]
    fn a_git_write_blames_the_buffer_again() {
        let mut h = blamed("/repo/a.rs", "one\ntwo\nthree\n");
        commit_three(&h);
        h.settle();
        let before = blame_runs(&mut h);
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.settle();
        assert_eq!(
            [before, blame_runs(&mut h)],
            [
                shas(&[(0..1, Some("c1")), (1..2, Some("c2")), (2..3, None)]),
                shas(&[(0..1, Some("c1")), (1..2, Some("c2")), (2..3, Some("c3"))]),
            ],
        );
    }

    /// A blame that read the git state before a write lands, and its buffer
    /// runs again against the state after it.
    #[test]
    fn a_blame_that_ran_across_a_git_write_runs_again() {
        let mut h = blamed("/repo/a.rs", "one\ntwo\nthree\n");
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.stoat.drive_pumps();
        h.run_until_parked();
        commit_three(&h);
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.settle();
        assert_eq!(
            (blame_runs(&mut h), h.stoat.active_workspace().blame.epoch),
            (
                shas(&[(0..1, Some("c1")), (1..2, Some("c2")), (2..3, Some("c3"))]),
                3
            ),
            "the run in flight read c2, so a third run reads c3",
        );
    }

    /// The file at `path` in a blamed pane, holding `content`, in a workspace
    /// at `/repo` over [`HISTORY`].
    fn blamed(path: &str, content: &str) -> TestHarness {
        let mut h = Stoat::test();
        h.seed_linear_history("/repo", HISTORY);
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h.fake_fs().insert_file(path, content);
        h.open_file(Path::new(path));
        h.type_keys("space h a");
        h.settle();
        h
    }

    /// Commit c3, which adds `three` to `a.rs`, and move HEAD to it.
    fn commit_three(h: &TestHarness) {
        h.fake_git().add_repo("/repo").commit_with_parent(
            "c3",
            "c2",
            &[("a.rs", "one\ntwo\nthree\n")],
        );
    }

    /// The blame runs of the focused buffer as its rows resolve now, each
    /// commit named by its sha.
    fn blame_runs(h: &mut TestHarness) -> Vec<(Range<u32>, Option<String>)> {
        let ws = h.stoat.active_workspace_mut();
        ws.refresh_blame_rows();
        let (_, id) = ws.focused_editor().expect("an editor holds the focus");
        let file = ws.blame.files.get(&id).expect("a blame landed");
        let (_, rows) = file.rows.as_ref().expect("the rows resolved");
        rows.runs
            .iter()
            .map(|(rows, commit)| {
                let sha = commit.map(|index| {
                    let base = file.base.as_ref().expect("a committed run has a base");
                    base.blame.commits[index as usize].info.sha.clone()
                });
                (rows.clone(), sha)
            })
            .collect()
    }

    /// The place in its commit of each row in `rows` of the focused buffer, as
    /// `<sha>/<commit index> <commit path>:<commit line>`, or `None` for an
    /// uncommitted row.
    fn commit_lines(h: &mut TestHarness, rows: Range<u32>) -> Vec<Option<String>> {
        let ws = h.stoat.active_workspace_mut();
        ws.refresh_blame_rows();
        let (_, id) = ws.focused_editor().expect("an editor holds the focus");
        rows.map(|row| match ws.blame.line(id, row) {
            Some(LineBlame::Commit {
                index,
                commit,
                commit_line,
                commit_path,
            }) => Some(format!(
                "{}/{index} {}:{commit_line}",
                commit.info.sha,
                commit_path.display(),
            )),
            _ => None,
        })
        .collect()
    }

    fn shas(runs: &[(Range<u32>, Option<&str>)]) -> Vec<(Range<u32>, Option<String>)> {
        runs.iter()
            .map(|(rows, sha)| (rows.clone(), sha.map(str::to_owned)))
            .collect()
    }
}
