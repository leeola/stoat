//! The git status panel's data, read off the repository in one pass.
//!
//! The panel answers what `git status` answers, laid out as the VcsStatus
//! board draws it. A header names the branch and how far it has moved from
//! the branch it follows. A banner names a merge or a rebase in progress. Then
//! the changed files sit in the four sections that git status uses, which are
//! conflicted, staged, unstaged, and untracked.
//!
//! [`GitStatusModel::read`] gathers all of it from the host and touches no
//! editor state, so the panel runs it on the blocking pool.

use crate::{
    host::{
        git::MergeState, ChangedFile, ConflictedFile, FsHost, GitRepo, RebaseTodoOp, StagedKind,
    },
    merge_view::MergeDoc,
    rebase::{ActiveRebase, ConflictResolution, RebasePause},
    theme::scope,
    workspace::diff,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use stoat_language::LanguageRegistry;

/// The branch a branch with no upstream measures against.
const MAIN: &str = "main";

/// How many of a rebase's next entries the banner's detail names.
const NEXT_SHOWN: usize = 4;

/// What the git status panel shows of a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitStatusModel {
    /// The repository's working tree, which every row's path lies under, and
    /// which the panel names the rows' paths relative to.
    pub(crate) root: PathBuf,
    /// The branch HEAD is attached to, or `None` when HEAD is detached.
    pub(crate) branch: Option<String>,
    pub(crate) divergence: Divergence,
    /// The branch the divergence measures against, as the right end of the
    /// header names it.
    ///
    /// That is the upstream, such as `origin/main`, or `vs main` for a branch
    /// with no upstream. Empty when there is nothing to measure against.
    pub(crate) upstream: String,
    pub(crate) op: Option<OpBanner>,
    /// The sections that hold a file, in the order conflicted, staged,
    /// unstaged, untracked. Empty for a clean working tree.
    pub(crate) sections: Vec<Section>,
}

/// How the header measures HEAD against the branch it follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Divergence {
    /// HEAD and the named branch hold the same commits.
    Equal(String),
    /// The commits HEAD holds that the branch lacks, then the commits the
    /// branch holds that HEAD lacks.
    AheadBehind(usize, usize),
    /// HEAD is detached at this short sha.
    Detached(String),
    /// A rebase that stoat runs, as the entries applied, the plan's length,
    /// and the name of the commit the plan stacks onto.
    Rebase {
        done: usize,
        total: usize,
        onto: String,
    },
    /// No branch to measure against. The branch has no upstream and no `main`
    /// to fall back on, or HEAD has no commit yet.
    Unknown,
}

/// The banner that names a merge or a rebase in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpBanner {
    pub(crate) kind: OpKind,
    /// What the operation joins or where it stands, such as `feature → main`
    /// or `3/7 · 3e9f0aa`.
    pub(crate) head: String,
    pub(crate) message: String,
    pub(crate) detail: String,
    /// The keys the operation takes, each as the key, its label, and the
    /// scope its key paints in.
    pub(crate) keys: Vec<(&'static str, &'static str, &'static str)>,
}

/// Which operation a banner names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpKind {
    Merge,
    Rebase,
}

/// One section of changed files, such as the staged ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Section {
    pub(crate) kind: SectionKind,
    /// The section's files in path order. Never empty, since an empty section
    /// is left out.
    pub(crate) rows: Vec<FileRow>,
}

/// The four sections git status sorts a changed file into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SectionKind {
    Conflicted,
    Staged,
    Unstaged,
    Untracked,
}

/// One changed file in a section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileRow {
    /// Absolute path, which the panel's keys act on.
    pub(crate) path: PathBuf,
    pub(crate) kind: RowKind,
    pub(crate) note: Note,
    /// Whether the row lists a file that a rebase's conflict pause holds,
    /// rather than a file of the index or the working tree.
    ///
    /// The pause keeps its files in memory, so the panel's keys act on such a
    /// row through the pause and not through the index.
    pub(crate) from_pause: bool,
}

/// What a row's change does to its file, as its kind letter names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowKind {
    Unmerged,
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

/// The note at the right end of a file row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Note {
    /// A conflicted file's chunks that still need a pick, of all its chunks.
    Open {
        open: usize,
        total: usize,
    },
    /// A file that the conflict view resolved and staged during the merge in
    /// progress.
    Resolved,
    /// A conflicted file the panel resolved whole to our side, then staged.
    TookOurs,
    /// A conflicted file the panel resolved whole to their side, then staged.
    TookTheirs,
    /// The lines the row's side of the index adds and deletes.
    Lines {
        added: u32,
        deleted: u32,
    },
    None,
}

/// What the status panel shows of a rebase that stoat runs, copied off the
/// workspace's [`ActiveRebase`].
///
/// The copy exists because the model reads on the blocking pool, and the
/// rebase stays behind with the workspace on the loop.
#[derive(Clone, Debug)]
pub(crate) struct RebaseProgress {
    pub(crate) done: usize,
    pub(crate) total: usize,
    /// The commit the plan stacks onto.
    pub(crate) onto: String,
    /// The tip of the rebase so far.
    pub(crate) head: String,
    /// The message of the last commit the rebase made, if it made one.
    pub(crate) message: Option<String>,
    /// The first entries left to apply, each as its op and its commit's sha.
    pub(crate) next: Vec<(RebaseTodoOp, String)>,
    /// The files a pause on a conflict holds, each with the note of the side
    /// its resolution takes, or `None` while it waits for one. Empty unless
    /// the rebase is paused on a conflict.
    ///
    /// The pause keeps these in memory rather than in the index, so
    /// [`GitRepo::conflicted_paths`] never lists them.
    pub(crate) conflicts: Vec<(ConflictedFile, Option<Note>)>,
}

/// The git status panel while it is open.
#[derive(Debug, Default)]
pub(crate) struct GitStatusPanel {
    /// The model the last read produced, or `None` until the first read
    /// lands.
    pub(crate) model: Option<GitStatusModel>,
    /// The selected row, as an index into [`Self::rows`].
    pub(crate) selected: usize,
    /// The first row the body shows, as an index into [`Self::rows`], which
    /// [`Self::scroll_into_view`] moves to keep the selected row in view.
    pub(crate) top: usize,
}

/// One line of the panel's body, which lists each section's head over its
/// rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyLine<'a> {
    Head(&'a Section),
    /// A file row, with its index among the rows of every section.
    Row(&'a Section, usize, &'a FileRow),
}

impl GitStatusPanel {
    /// Every file row of every section, in the order the panel lists them,
    /// which is the order the selection walks.
    pub(crate) fn rows(&self) -> impl Iterator<Item = &FileRow> {
        self.model
            .iter()
            .flat_map(|model| &model.sections)
            .flat_map(|section| &section.rows)
    }

    /// The selected row and the section it sits in, which decides what a key
    /// does to it. `None` before the first read lands and for a clean tree.
    pub(crate) fn selected_row(&self) -> Option<(SectionKind, &FileRow)> {
        self.model
            .iter()
            .flat_map(|model| &model.sections)
            .flat_map(|section| section.rows.iter().map(|row| (section.kind, row)))
            .nth(self.selected)
    }

    /// Move the selection `delta` rows, stopping at the first and the last
    /// row.
    pub(crate) fn step(&mut self, delta: isize) {
        let last = self.rows().count().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    /// Show `model`, with the selection held at its index unless the rows end
    /// before it.
    pub(crate) fn land(&mut self, model: GitStatusModel) {
        self.model = Some(model);
        self.step(0);
    }

    /// The body's lines from the row at [`Self::top`], in the order the panel
    /// paints them.
    ///
    /// The section of the first row keeps its head above it, so a scrolled
    /// body still names the section its first rows belong to. The sections
    /// before it show nothing.
    pub(crate) fn body_lines(&self) -> impl Iterator<Item = BodyLine<'_>> {
        let top = self.top;
        let mut start = 0;
        self.model
            .iter()
            .flat_map(|model| &model.sections)
            .flat_map(move |section| {
                let offset = start;
                start += section.rows.len();
                let skipped = top.saturating_sub(offset).min(section.rows.len());
                let head = (skipped < section.rows.len()).then_some(BodyLine::Head(section));
                let rows = section
                    .rows
                    .iter()
                    .enumerate()
                    .skip(skipped)
                    .map(move |(i, row)| BodyLine::Row(section, offset + i, row));
                head.into_iter().chain(rows)
            })
    }

    /// Move [`Self::top`] so the selected row shows among the body's first
    /// `height` lines.
    ///
    /// A move down stops at the first top that shows the row, so the row lands
    /// on the body's last line. A move up sets the top to the row, so the row
    /// shows under its section's head.
    pub(crate) fn scroll_into_view(&mut self, height: usize) {
        self.top = self.top.min(self.selected);
        while self.top < self.selected && !self.shows_selected(height) {
            self.top += 1;
        }
    }

    /// Whether the selected row is among the body's first `height` lines.
    fn shows_selected(&self, height: usize) -> bool {
        self.body_lines()
            .take(height)
            .any(|line| matches!(line, BodyLine::Row(_, index, _) if index == self.selected))
    }
}

impl GitStatusModel {
    /// Read the panel's data off `repo`, on whatever thread calls.
    ///
    /// `fs` answers whether an unstaged file still exists, which tells a
    /// deletion from an edit. `languages` aligns each conflicted file as the
    /// conflict view does, so the open count matches the chunks the view
    /// shows. `resolved` notes the staged files the conflict view resolved,
    /// keyed by absolute path.
    ///
    /// This reads every changed file's content, and the three stages of every
    /// conflicted file, so it costs a full diff of the tree.
    pub(crate) fn read(
        repo: &dyn GitRepo,
        fs: &dyn FsHost,
        languages: &LanguageRegistry,
        rebase: Option<&RebaseProgress>,
        resolved: &HashMap<PathBuf, Note>,
    ) -> Self {
        let branch = repo.head_branch();
        let (measured, upstream) = measure(repo, branch.as_deref());
        let divergence = match rebase {
            Some(rebase) => Divergence::Rebase {
                done: rebase.done,
                total: rebase.total,
                onto: commit_name(repo, &rebase.onto),
            },
            None => measured,
        };

        // The repository's own workdir, because every `ChangedFile` path joins
        // it, so a line count's path joined to it compares equal.
        let root = repo.workdir().unwrap_or_default();
        let sections = read_sections(
            repo,
            fs,
            languages,
            &root,
            rebase.map_or(&[][..], |rebase| &rebase.conflicts),
            resolved,
        );
        let op = match (rebase, repo.merge_state()) {
            (Some(rebase), _) => Some(rebase_banner(rebase)),
            (None, Some(merge)) => {
                let conflicted = sections
                    .iter()
                    .find(|section| section.kind == SectionKind::Conflicted)
                    .map_or(0, |section| section.rows.len());
                Some(merge_banner(&merge, branch.as_deref(), conflicted))
            },
            (None, None) => None,
        };

        Self {
            root,
            branch,
            divergence,
            upstream,
            op,
            sections,
        }
    }
}

impl OpKind {
    /// The text of the banner's tag chip.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Merge => "MERGE",
            Self::Rebase => "REB",
        }
    }

    /// The scope of the banner, whose background tints the banner and whose
    /// foreground colors its bar and its chip.
    pub(crate) fn scope(self) -> &'static str {
        match self {
            Self::Merge => scope::VCS_MERGE_BANNER,
            Self::Rebase => scope::VCS_REBASE_BANNER,
        }
    }
}

impl SectionKind {
    /// The title at the head of the section.
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Conflicted => "conflicted",
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::Untracked => "untracked",
        }
    }

    /// The keys the section's head names for its rows.
    pub(crate) fn keys(self) -> &'static str {
        match self {
            Self::Conflicted => "Enter merge \u{b7} o t file",
            Self::Staged => "u unstage",
            Self::Unstaged => "s stage \u{b7} Enter diff",
            Self::Untracked => "s add",
        }
    }

    /// The scope of the section's head and of its rows' bars.
    pub(crate) fn scope(self) -> &'static str {
        match self {
            Self::Conflicted => scope::VCS_STATUS_CONFLICTED,
            Self::Staged => scope::VCS_STATUS_STAGED,
            Self::Unstaged => scope::VCS_STATUS_UNSTAGED,
            Self::Untracked => scope::VCS_STATUS_UNTRACKED,
        }
    }
}

impl RowKind {
    /// The letter the row leads with, as git status names the change.
    pub(crate) fn letter(self) -> char {
        match self {
            Self::Unmerged => 'U',
            Self::Modified => 'M',
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Untracked => '?',
        }
    }

    /// The scope the kind letter paints in.
    pub(crate) fn scope(self) -> &'static str {
        let [added, modified, deleted, renamed, untracked, unmerged] = scope::VCS_STATUS_KIND;
        match self {
            Self::Unmerged => unmerged,
            Self::Modified => modified,
            Self::Added => added,
            Self::Deleted => deleted,
            Self::Renamed => renamed,
            Self::Untracked => untracked,
        }
    }
}

impl RebaseProgress {
    /// The progress of `active`, as its plan's entries applied and the next
    /// entries it holds.
    pub(crate) fn of(active: &ActiveRebase) -> Self {
        Self {
            done: active.total.saturating_sub(active.remaining.len()),
            total: active.total,
            onto: active.onto.clone(),
            head: active.current_head.clone(),
            message: active.last_message.clone(),
            next: active
                .remaining
                .iter()
                .take(NEXT_SHOWN)
                .map(|entry| (entry.op, entry.commit.sha.clone()))
                .collect(),
            conflicts: paused_conflicts(active),
        }
    }
}

/// The files of `active`'s conflict pause, each with the note of the side its
/// resolution takes.
///
/// A file skipped by [`ConflictResolution::SkipEntry`] reads as still waiting,
/// since that resolution picks no side of the file.
fn paused_conflicts(active: &ActiveRebase) -> Vec<(ConflictedFile, Option<Note>)> {
    let Some(RebasePause::Conflict {
        files, resolutions, ..
    }) = &active.pause
    else {
        return Vec::new();
    };
    files
        .iter()
        .map(|file| {
            let note = match resolutions.get(&file.path) {
                Some(ConflictResolution::TakeOurs) => Some(Note::TookOurs),
                Some(ConflictResolution::TakeTheirs) => Some(Note::TookTheirs),
                Some(ConflictResolution::SkipEntry) | None => None,
            };
            (file.clone(), note)
        })
        .collect()
}

/// How far HEAD has moved from the branch it follows, and the header's name
/// for that branch.
///
/// The upstream's own read is exact, where a name passed to
/// [`GitRepo::divergence`] resolves as `git rev-parse` resolves it, and a
/// local branch of the same name shadows the upstream.
fn measure(repo: &dyn GitRepo, branch: Option<&str>) -> (Divergence, String) {
    let Some(branch) = branch else {
        let divergence = repo.resolve_rev("HEAD").map_or(Divergence::Unknown, |sha| {
            Divergence::Detached(diff::short_sha(&sha))
        });
        return (divergence, String::new());
    };

    if let Some(upstream) = repo.upstream_name() {
        let divergence = between(repo.upstream_divergence(), &upstream);
        return (divergence, upstream);
    }

    let measures_main =
        branch != MAIN && repo.local_branches().iter().any(|(name, _)| name == MAIN);
    if !measures_main {
        return (Divergence::Unknown, String::new());
    }
    (
        between(repo.divergence("HEAD", MAIN), MAIN),
        format!("vs {MAIN}"),
    )
}

/// The divergence `counts` reports against the branch `name`.
fn between(counts: Option<(usize, usize)>, name: &str) -> Divergence {
    match counts {
        Some((0, 0)) => Divergence::Equal(name.to_string()),
        Some((ahead, behind)) => Divergence::AheadBehind(ahead, behind),
        None => Divergence::Unknown,
    }
}

/// The local branch at `sha`, or its short sha when no local branch points at
/// it.
fn commit_name(repo: &dyn GitRepo, sha: &str) -> String {
    repo.local_branches()
        .into_iter()
        .find(|(_, tip)| tip == sha)
        .map_or_else(|| diff::short_sha(sha), |(name, _)| name)
}

/// The changed files sorted into their sections, in board order, with the
/// empty sections left out.
fn read_sections(
    repo: &dyn GitRepo,
    fs: &dyn FsHost,
    languages: &LanguageRegistry,
    workdir: &Path,
    paused: &[(ConflictedFile, Option<Note>)],
    resolved: &HashMap<PathBuf, Note>,
) -> Vec<Section> {
    let stats = repo.change_line_stats();
    let lines = |side: &[(PathBuf, u32, u32)]| -> HashMap<PathBuf, (u32, u32)> {
        side.iter()
            .map(|(path, added, deleted)| (workdir.join(path), (*added, *deleted)))
            .collect()
    };
    let (staged_lines, unstaged_lines) = (lines(&stats.staged), lines(&stats.unstaged));

    let mut conflicted: Vec<FileRow> = repo
        .conflicted_paths()
        .into_iter()
        .map(|path| FileRow {
            note: repo
                .conflict_stages(&path)
                .map_or(Note::None, |stages| open_note(&stages, languages)),
            kind: RowKind::Unmerged,
            path,
            from_pause: false,
        })
        .collect();

    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    // A pause's file waiting for a side lists as conflicted, and a resolved
    // one as staged under the side it took, as the rebase board lists them.
    for (file, taken) in paused {
        let path = workdir.join(&file.path);
        match taken {
            Some(note) => staged.push(FileRow {
                path,
                kind: RowKind::Modified,
                note: *note,
                from_pause: true,
            }),
            None => conflicted.push(FileRow {
                note: open_note(file, languages),
                kind: RowKind::Unmerged,
                path,
                from_pause: true,
            }),
        }
    }
    for file in repo.changed_files() {
        if file.untracked {
            untracked.push(FileRow {
                path: file.path,
                kind: RowKind::Untracked,
                note: Note::None,
                from_pause: false,
            });
            continue;
        }
        if !file.staged {
            unstaged.push(FileRow {
                kind: unstaged_kind(&file, fs),
                note: lines_note(unstaged_lines.get(&file.path)),
                path: file.path,
                from_pause: false,
            });
            continue;
        }

        let kind = staged_kind(&file);
        // A file that both sides of the index change lists once, under staged,
        // so the unstaged line stats find its second row, as git status lists
        // it twice. A file deleted from the index but present on disk is
        // untracked there.
        if let Some(lines) = unstaged_lines.get(&file.path) {
            match kind {
                RowKind::Deleted => untracked.push(FileRow {
                    path: file.path.clone(),
                    kind: RowKind::Untracked,
                    note: Note::None,
                    from_pause: false,
                }),
                _ => unstaged.push(FileRow {
                    path: file.path.clone(),
                    kind: deletion_or_edit(&file.path, fs),
                    note: lines_note(Some(lines)),
                    from_pause: false,
                }),
            }
        }
        staged.push(FileRow {
            note: resolved
                .get(&file.path)
                .copied()
                .unwrap_or_else(|| lines_note(staged_lines.get(&file.path))),
            kind,
            path: file.path,
            from_pause: false,
        });
    }

    [
        (SectionKind::Conflicted, conflicted),
        (SectionKind::Staged, staged),
        (SectionKind::Unstaged, unstaged),
        (SectionKind::Untracked, untracked),
    ]
    .into_iter()
    .filter(|(_, rows)| !rows.is_empty())
    .map(|(kind, mut rows)| {
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        Section { kind, rows }
    })
    .collect()
}

/// The open note of a conflicted file, counted from its stages.
fn open_note(stages: &ConflictedFile, languages: &LanguageRegistry) -> Note {
    let doc = MergeDoc::from_stages(stages, languages.for_path(&stages.path).as_ref());
    Note::Open {
        open: doc
            .chunks
            .iter()
            .filter(|chunk| chunk.auto.is_none())
            .count(),
        total: doc.chunks.len(),
    }
}

/// The note for a side's line counts, or no note when the side changes no
/// line, as a pure move or a mode change does.
fn lines_note(lines: Option<&(u32, u32)>) -> Note {
    match lines {
        Some(&(added, deleted)) if added > 0 || deleted > 0 => Note::Lines { added, deleted },
        _ => Note::None,
    }
}

/// The kind of a staged file's change, which the index records against HEAD.
fn staged_kind(file: &ChangedFile) -> RowKind {
    match file.staged_kind {
        Some(StagedKind::Added) => RowKind::Added,
        Some(StagedKind::Deleted) => RowKind::Deleted,
        Some(StagedKind::Renamed) => RowKind::Renamed,
        Some(StagedKind::Modified) | None => RowKind::Modified,
    }
}

/// The kind of an unstaged change to a tracked file.
fn unstaged_kind(file: &ChangedFile, fs: &dyn FsHost) -> RowKind {
    match file.renamed_from {
        Some(_) => RowKind::Renamed,
        None => deletion_or_edit(&file.path, fs),
    }
}

/// [`RowKind::Deleted`] when the working tree no longer holds `path`, and an
/// edit otherwise.
///
/// A file that the host fails to stat counts as an edit, since its row still
/// names the file.
fn deletion_or_edit(path: &Path, fs: &dyn FsHost) -> RowKind {
    match fs.metadata(path) {
        Ok(None) => RowKind::Deleted,
        Ok(Some(_)) | Err(_) => RowKind::Modified,
    }
}

/// The banner of the merge in progress, with `conflicted` files left to
/// resolve.
fn merge_banner(merge: &MergeState, branch: Option<&str>, conflicted: usize) -> OpBanner {
    let theirs = merge
        .head_name
        .clone()
        .unwrap_or_else(|| diff::short_sha(&merge.head_sha));
    let files = if conflicted == 1 { "file" } else { "files" };

    OpBanner {
        kind: OpKind::Merge,
        head: format!("{theirs} \u{2192} {}", branch.unwrap_or("HEAD")),
        // The merge commit takes HEAD and the one head the merge brings in.
        message: "2 parents".to_string(),
        detail: format!("{conflicted} {files} left to resolve"),
        keys: vec![
            ("C", "commit", scope::VCS_STATUS_STAGED),
            ("a", "abort", scope::VCS_STATUS_CONFLICTED),
        ],
    }
}

/// The banner of the rebase that stoat runs.
///
/// The detail names the next entry's op and sha, then only the ops of the
/// entries after it, which keeps the line short enough for the panel.
fn rebase_banner(rebase: &RebaseProgress) -> OpBanner {
    let detail = match rebase.next.split_first() {
        Some(((op, sha), rest)) => {
            let mut detail = format!("next: {} {}", op_name(*op), diff::short_sha(sha));
            for (op, _) in rest {
                detail.push_str(", ");
                detail.push_str(op_name(*op));
            }
            detail
        },
        None => String::new(),
    };

    OpBanner {
        kind: OpKind::Rebase,
        head: format!(
            "{}/{} \u{b7} {}",
            rebase.done,
            rebase.total,
            diff::short_sha(&rebase.head)
        ),
        message: rebase
            .message
            .as_deref()
            .and_then(|message| message.lines().next())
            .unwrap_or_default()
            .to_string(),
        detail,
        // The skip takes `S`, since the panel's `s` stages the selected row.
        // No plan key shows, because no plan exists while a rebase runs.
        keys: vec![
            ("C", "continue", scope::VCS_STATUS_STAGED),
            ("S", "skip", scope::UI_DIAGNOSTIC_WARNING),
            ("a", "abort", scope::VCS_STATUS_CONFLICTED),
        ],
    }
}

/// The name a rebase plan gives `op`.
fn op_name(op: RebaseTodoOp) -> &'static str {
    match op {
        RebaseTodoOp::Pick => "pick",
        RebaseTodoOp::Squash => "squash",
        RebaseTodoOp::Fixup => "fixup",
        RebaseTodoOp::Drop => "drop",
        RebaseTodoOp::Reword => "reword",
        RebaseTodoOp::Edit => "edit",
    }
}

#[cfg(test)]
mod tests;
