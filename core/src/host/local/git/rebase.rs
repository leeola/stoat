//! libgit2 rebase + cherry-pick plumbing. Extracted from the
//! [`super::LocalGitRepo`] trait impl so the mutation-heavy logic lives
//! apart from the flat trait-method surface.

use crate::host::{
    git::{
        CherryPickOutcome, ConflictSnafu, ConflictedFile, GitApplyError, RebaseBackendSnafu,
        RebaseError, RebaseTodo, RebaseTodoOp,
    },
    local::git::tree::read_blob,
};
use git2::{
    build::TreeUpdateBuilder, Commit, FileMode, ObjectType, Oid, Repository, Tree, TreeEntry,
};
use std::{collections::BTreeMap, path::PathBuf};

/// Atomic rebase: replays every `todo` entry onto `onto` inside one
/// method call. Cannot pause for user input; `Reword` and `Edit` decay
/// to `Pick`. The interactive stepper in `action_handlers::rebase`
/// handles pause-aware rebasing.
pub(super) fn run_rebase(
    repo: &Repository,
    onto: &str,
    todo: &[RebaseTodo],
) -> Result<String, RebaseError> {
    let onto_oid = Oid::from_str(onto).map_err(rebase_backend)?;
    let mut current_id = onto_oid;

    let mut last_commit: Option<Oid> = None;

    for entry in todo {
        match entry.op {
            RebaseTodoOp::Drop => continue,
            RebaseTodoOp::Pick | RebaseTodoOp::Reword | RebaseTodoOp::Edit => {
                // `run_rebase` is the atomic fast path; it cannot
                // pause for user input. Reword/Edit degrade to Pick
                // here. The stepper in `action_handlers` handles true
                // reword/edit interactions.
                let commit = pick_onto(repo, &entry.sha, current_id)?;
                current_id = commit;
                last_commit = Some(commit);
            },
            RebaseTodoOp::Squash | RebaseTodoOp::Fixup => {
                let prev = last_commit.ok_or_else(|| {
                    RebaseBackendSnafu {
                        reason: "squash/fixup without a preceding pick",
                    }
                    .build()
                })?;
                let prev_commit = repo.find_commit(prev).map_err(rebase_backend)?;
                let entry_oid = Oid::from_str(&entry.sha).map_err(rebase_backend)?;
                let entry_commit = repo.find_commit(entry_oid).map_err(rebase_backend)?;

                let mut index = repo
                    .cherrypick_commit(&entry_commit, &prev_commit, 0, None)
                    .map_err(rebase_backend)?;
                if index.has_conflicts() {
                    return ConflictSnafu {
                        at_sha: entry.sha.clone(),
                    }
                    .fail();
                }
                let merged_tree_id = index.write_tree_to(repo).map_err(rebase_backend)?;
                let merged_tree = repo.find_tree(merged_tree_id).map_err(rebase_backend)?;

                let prev_parents: Vec<_> = prev_commit.parents().collect();
                let prev_parent_refs: Vec<_> = prev_parents.iter().collect();
                let combined_message = match entry.op {
                    RebaseTodoOp::Squash => format!(
                        "{}\n\n{}",
                        prev_commit.message().unwrap_or("").trim_end(),
                        entry.message.trim_end()
                    ),
                    _ => prev_commit.message().unwrap_or("").to_string(),
                };
                let folded = repo
                    .commit(
                        None,
                        &prev_commit.author(),
                        &prev_commit.committer(),
                        &combined_message,
                        &merged_tree,
                        &prev_parent_refs,
                    )
                    .map_err(rebase_backend)?;
                current_id = folded;
                last_commit = Some(folded);
            },
        }
    }

    repo.reference("HEAD", current_id, true, "run_rebase")
        .map_err(rebase_backend)?;
    Ok(current_id.to_string())
}

/// Cherry-pick a single commit onto another, returning either a clean
/// merged tree (ready for commit creation) or the list of conflicted
/// paths for the stepper to surface.
pub(super) fn cherry_pick_tree(
    repo: &Repository,
    source_sha: &str,
    onto_sha: &str,
) -> Result<CherryPickOutcome, GitApplyError> {
    let source_oid = Oid::from_str(source_sha).map_err(super::err_msg)?;
    let onto_oid = Oid::from_str(onto_sha).map_err(super::err_msg)?;
    let source = repo.find_commit(source_oid).map_err(super::err_msg)?;
    let onto = repo.find_commit(onto_oid).map_err(super::err_msg)?;
    if let Some(tree) = changed_path_tree(repo, &source, &onto) {
        return Ok(clean_pick(&source, tree));
    }

    let mut index = repo
        .cherrypick_commit(&source, &onto, 0, None)
        .map_err(super::err_msg)?;

    if index.has_conflicts() {
        let mut by_path: BTreeMap<PathBuf, ConflictedFile> = BTreeMap::new();
        for conflict in index.conflicts().map_err(super::err_msg)? {
            let conflict = conflict.map_err(super::err_msg)?;
            let pick_path = conflict
                .ancestor
                .as_ref()
                .map(|e| e.path.clone())
                .or_else(|| conflict.our.as_ref().map(|e| e.path.clone()))
                .or_else(|| conflict.their.as_ref().map(|e| e.path.clone()))
                .unwrap_or_default();
            let path = PathBuf::from(std::str::from_utf8(&pick_path).unwrap_or(""));
            let ancestor = conflict
                .ancestor
                .as_ref()
                .and_then(|e| read_blob(repo, e.id));
            let ours = conflict.our.as_ref().and_then(|e| read_blob(repo, e.id));
            let theirs = conflict.their.as_ref().and_then(|e| read_blob(repo, e.id));
            by_path.insert(
                path.clone(),
                ConflictedFile {
                    path,
                    ancestor,
                    ours,
                    theirs,
                },
            );
        }
        return Ok(CherryPickOutcome::Conflict {
            files: by_path.into_values().collect(),
        });
    }

    let tree_oid = index.write_tree_to(repo).map_err(super::err_msg)?;
    Ok(clean_pick(&source, tree_oid))
}

/// The tree a clean pick of `source` onto `onto` produces, built from the
/// paths `source` changed, or `None` when the three-way merge has to decide.
///
/// The merge walks all three trees and rebuilds the result through an index,
/// so its cost follows the size of the tree. This build reads only the subtrees
/// `source` changed, so its cost follows the size of the change.
///
/// It answers only when `onto` holds each changed path as the parent of
/// `source` had it, or holds a modified path as `source` had it. Every other
/// shape takes the merge, which detects renames. A rename whose destination
/// `onto` holds, or whose source `onto` deleted, is a conflict in the merge,
/// and a build path by path writes it clean. A root commit or a merge commit
/// has no single parent to diff against, so it takes the merge too.
pub(super) fn changed_path_tree(
    repo: &Repository,
    source: &Commit<'_>,
    onto: &Commit<'_>,
) -> Option<Oid> {
    if source.parent_count() != 1 {
        return None;
    }
    let parent = source.parent(0).ok()?.tree().ok()?;
    let onto = onto.tree().ok()?;

    let mut updates = TreeUpdateBuilder::new();
    queue_changes(
        repo,
        &parent,
        &source.tree().ok()?,
        Some(&onto),
        b"",
        &mut updates,
    )?;
    // libgit2 refuses an update that puts a path under a name `onto` holds as a
    // file, and that refusal sends the pick to the merge.
    updates.create_updated(repo, &onto).ok()
}

/// Queue into `updates` the changes `source` makes to `parent` under `prefix`,
/// or return `None` when one of them needs the merge.
///
/// An entry compares as its oid and mode, so a subtree both sides share is
/// skipped unread, and an absent entry equals an absent entry. `onto` is `None`
/// where `onto` holds no tree at `prefix`.
///
/// An addition or a removal that `onto` already carries is a rename candidate
/// in the merge, so only a modification settles as already there.
fn queue_changes(
    repo: &Repository,
    parent: &Tree<'_>,
    source: &Tree<'_>,
    onto: Option<&Tree<'_>>,
    prefix: &[u8],
    updates: &mut TreeUpdateBuilder,
) -> Option<()> {
    let kept = parent.iter().map(|before| {
        let after = source.get_name_bytes(before.name_bytes());
        (Some(before), after)
    });
    let added = source
        .iter()
        .filter(|after| parent.get_name_bytes(after.name_bytes()).is_none())
        .map(|after| (None, Some(after)));

    for (before, after) in kept.chain(added) {
        if entry_key(before.as_ref()) == entry_key(after.as_ref()) {
            continue;
        }
        let name = match (&before, &after) {
            (Some(entry), _) | (None, Some(entry)) => entry.name_bytes(),
            (None, None) => continue,
        };
        let path = [prefix, name].concat();
        let target = onto.and_then(|onto| onto.get_name_bytes(name));

        let before_tree = before.as_ref().is_some_and(is_tree);
        if before_tree != after.as_ref().is_some_and(is_tree) {
            return None;
        }
        if before_tree {
            let target = match target {
                Some(entry) if is_tree(&entry) => Some(repo.find_tree(entry.id()).ok()?),
                _ => None,
            };
            let before = repo.find_tree(before?.id()).ok()?;
            let after = repo.find_tree(after?.id()).ok()?;
            let mut prefix = path;
            prefix.push(b'/');
            queue_changes(repo, &before, &after, target.as_ref(), &prefix, updates)?;
            continue;
        }

        if target.as_ref().is_some_and(is_tree) {
            return None;
        }
        let target = entry_key(target.as_ref());
        if target == entry_key(before.as_ref()) {
            match after {
                Some(after) => updates.upsert(path, after.id(), file_mode(after.filemode())?),
                None => updates.remove(path),
            };
        } else if target != entry_key(after.as_ref()) || before.is_none() || after.is_none() {
            return None;
        }
    }
    Some(())
}

/// A tree entry as [`queue_changes`] compares it.
fn entry_key(entry: Option<&TreeEntry<'_>>) -> Option<(Oid, i32)> {
    entry.map(|entry| (entry.id(), entry.filemode()))
}

fn is_tree(entry: &TreeEntry<'_>) -> bool {
    entry.kind() == Some(ObjectType::Tree)
}

/// The mode `raw` names, or `None` for a mode a tree update does not write,
/// which sends the pick to the merge.
fn file_mode(raw: i32) -> Option<FileMode> {
    [
        FileMode::Blob,
        FileMode::BlobExecutable,
        FileMode::BlobGroupWritable,
        FileMode::Link,
        FileMode::Commit,
    ]
    .into_iter()
    .find(|mode| i32::from(*mode) == raw)
}

/// The outcome of a clean pick of `source` whose picked tree is `tree`.
fn clean_pick(source: &Commit<'_>, tree: Oid) -> CherryPickOutcome {
    let author = source.author();
    CherryPickOutcome::Clean {
        tree: tree.to_string(),
        message: source.message().unwrap_or("").to_string(),
        author_name: author.name().unwrap_or("").to_string(),
        author_email: author.email().unwrap_or("").to_string(),
        author_time: source.time().seconds(),
    }
}

/// Cherry-pick `sha` onto `onto`. Returns the new commit's oid.
fn pick_onto(repo: &Repository, sha: &str, onto: Oid) -> Result<Oid, RebaseError> {
    let entry_oid = Oid::from_str(sha).map_err(rebase_backend)?;
    let entry_commit = repo.find_commit(entry_oid).map_err(rebase_backend)?;
    let onto_commit = repo.find_commit(onto).map_err(rebase_backend)?;

    let tree_id = match changed_path_tree(repo, &entry_commit, &onto_commit) {
        Some(tree_id) => tree_id,
        None => {
            let mut index = repo
                .cherrypick_commit(&entry_commit, &onto_commit, 0, None)
                .map_err(rebase_backend)?;
            if index.has_conflicts() {
                return ConflictSnafu {
                    at_sha: sha.to_string(),
                }
                .fail();
            }
            index.write_tree_to(repo).map_err(rebase_backend)?
        },
    };
    let tree = repo.find_tree(tree_id).map_err(rebase_backend)?;
    let author = entry_commit.author();
    let committer = entry_commit.committer();
    let msg = entry_commit.message().unwrap_or("").to_string();
    repo.commit(None, &author, &committer, &msg, &tree, &[&onto_commit])
        .map_err(rebase_backend)
}

fn rebase_backend(e: git2::Error) -> RebaseError {
    RebaseBackendSnafu {
        reason: e.message().to_string(),
    }
    .build()
}
