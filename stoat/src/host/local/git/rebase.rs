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
use git2::Repository;
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
    let onto_oid = git2::Oid::from_str(onto).map_err(rebase_backend)?;
    let mut current_id = onto_oid;

    let mut last_commit: Option<git2::Oid> = None;

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
                let entry_oid = git2::Oid::from_str(&entry.sha).map_err(rebase_backend)?;
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
    let source_oid = git2::Oid::from_str(source_sha).map_err(super::err_msg)?;
    let onto_oid = git2::Oid::from_str(onto_sha).map_err(super::err_msg)?;
    let source = repo.find_commit(source_oid).map_err(super::err_msg)?;
    let onto = repo.find_commit(onto_oid).map_err(super::err_msg)?;

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
    let tree = repo.find_tree(tree_oid).map_err(super::err_msg)?;
    let mut out: BTreeMap<PathBuf, String> = BTreeMap::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return git2::TreeWalkResult::Ok;
        }
        let name = match entry.name() {
            Some(n) => n,
            None => return git2::TreeWalkResult::Ok,
        };
        let rel = if dir.is_empty() {
            PathBuf::from(name)
        } else {
            PathBuf::from(dir).join(name)
        };
        if let Ok(blob) = entry.to_object(repo).and_then(|o| o.peel_to_blob()) {
            if let Ok(text) = std::str::from_utf8(blob.content()) {
                out.insert(rel, text.to_string());
            }
        }
        git2::TreeWalkResult::Ok
    })
    .map_err(super::err_msg)?;

    let author = source.author();
    Ok(CherryPickOutcome::Clean {
        tree: out,
        message: source.message().unwrap_or("").to_string(),
        author_name: author.name().unwrap_or("").to_string(),
        author_email: author.email().unwrap_or("").to_string(),
        author_time: source.time().seconds(),
    })
}

/// Cherry-pick `sha` onto `onto`. Returns the new commit's oid.
fn pick_onto(repo: &Repository, sha: &str, onto: git2::Oid) -> Result<git2::Oid, RebaseError> {
    let entry_oid = git2::Oid::from_str(sha).map_err(rebase_backend)?;
    let entry_commit = repo.find_commit(entry_oid).map_err(rebase_backend)?;
    let onto_commit = repo.find_commit(onto).map_err(rebase_backend)?;

    let mut index = repo
        .cherrypick_commit(&entry_commit, &onto_commit, 0, None)
        .map_err(rebase_backend)?;
    if index.has_conflicts() {
        return ConflictSnafu {
            at_sha: sha.to_string(),
        }
        .fail();
    }
    let tree_id = index.write_tree_to(repo).map_err(rebase_backend)?;
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
