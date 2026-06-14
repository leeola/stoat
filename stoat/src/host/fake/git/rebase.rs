//! Deterministic cherry-pick / rewrite / rebase replay for
//! [`super::FakeGitRepo`]. Real 3-way merging is unnecessary for the
//! UI-level tests that use this fake; the implementations below
//! overlay trees directly and mint synthetic shas.

use super::{FakeCommit, FakeRepoState, RecordedRebase};
use crate::host::git::{
    BackendSnafu, CherryPickOutcome, ConflictSnafu, ConflictedFile, GitApplyError,
    RebaseBackendSnafu, RebaseError, RebaseTodo, RebaseTodoOp, RewriteResult,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
};

pub(super) fn rewrite_commit(
    state: &mut FakeRepoState,
    sha: &str,
    tree: &BTreeMap<PathBuf, String>,
    message: Option<&str>,
    descendants: &[String],
) -> Result<RewriteResult, GitApplyError> {
    if let Some(c) = &state.conflict_at
        && (c == sha || descendants.iter().any(|d| d == c))
    {
        return BackendSnafu {
            reason: format!("simulated cherry-pick conflict at {c}"),
        }
        .fail();
    }
    let Some(target) = state.commits.get(sha).cloned() else {
        return BackendSnafu {
            reason: format!("unknown sha: {sha}"),
        }
        .fail();
    };

    state.synth_counter += 1;
    let new_target_sha = format!(
        "rewritten-{}-{}",
        &sha[..sha.len().min(6)],
        state.synth_counter
    );
    let new_target = FakeCommit {
        parent: target.parent.clone(),
        tree: tree.clone(),
        message: message
            .map(str::to_string)
            .unwrap_or(target.message.clone()),
        author_name: target.author_name.clone(),
        author_email: target.author_email.clone(),
        time: target.time,
    };
    state.commits.insert(new_target_sha.clone(), new_target);

    let mut mapping: HashMap<String, String> = HashMap::new();
    mapping.insert(sha.to_string(), new_target_sha.clone());
    let mut current = new_target_sha.clone();

    for desc_sha in descendants {
        let Some(desc) = state.commits.get(desc_sha).cloned() else {
            return BackendSnafu {
                reason: format!("unknown descendant sha: {desc_sha}"),
            }
            .fail();
        };
        state.synth_counter += 1;
        let new_sha = format!(
            "rewritten-{}-{}",
            &desc_sha[..desc_sha.len().min(6)],
            state.synth_counter
        );
        let new_commit = FakeCommit {
            parent: Some(current.clone()),
            tree: desc.tree.clone(),
            message: desc.message.clone(),
            author_name: desc.author_name.clone(),
            author_email: desc.author_email.clone(),
            time: desc.time,
        };
        state.commits.insert(new_sha.clone(), new_commit);
        mapping.insert(desc_sha.clone(), new_sha.clone());
        current = new_sha;
    }

    state.head = Some(current.clone());
    Ok(RewriteResult {
        new_head: current,
        mapping,
    })
}

pub(super) fn run_rebase(
    state: &mut FakeRepoState,
    onto: &str,
    todo: &[RebaseTodo],
) -> Result<String, RebaseError> {
    if let Some(c) = &state.conflict_at
        && todo.iter().any(|t| &t.sha == c)
    {
        return ConflictSnafu { at_sha: c.clone() }.fail();
    }
    if !state.commits.contains_key(onto) && !onto.is_empty() {
        return RebaseBackendSnafu {
            reason: format!("unknown onto sha: {onto}"),
        }
        .fail();
    }

    let mut current = onto.to_string();
    let mut last_commit: Option<String> = None;
    let mut last_message: Option<String> = None;

    for entry in todo {
        match entry.op {
            RebaseTodoOp::Drop => continue,
            RebaseTodoOp::Pick | RebaseTodoOp::Reword | RebaseTodoOp::Edit => {
                let Some(src) = state.commits.get(&entry.sha).cloned() else {
                    return RebaseBackendSnafu {
                        reason: format!("unknown sha in rebase: {}", entry.sha),
                    }
                    .fail();
                };
                state.synth_counter += 1;
                let new_sha = format!(
                    "rebased-{}-{}",
                    &entry.sha[..entry.sha.len().min(6)],
                    state.synth_counter
                );
                let new_commit = FakeCommit {
                    parent: Some(current.clone()),
                    tree: src.tree.clone(),
                    message: src.message.clone(),
                    author_name: src.author_name.clone(),
                    author_email: src.author_email.clone(),
                    time: src.time,
                };
                state.commits.insert(new_sha.clone(), new_commit);
                last_message = Some(src.message);
                current = new_sha.clone();
                last_commit = Some(new_sha);
            },
            RebaseTodoOp::Squash | RebaseTodoOp::Fixup => {
                let Some(prev_sha) = last_commit.clone() else {
                    return RebaseBackendSnafu {
                        reason: "squash/fixup without preceding pick",
                    }
                    .fail();
                };
                let Some(prev) = state.commits.get(&prev_sha).cloned() else {
                    return RebaseBackendSnafu {
                        reason: "previous commit missing",
                    }
                    .fail();
                };
                let Some(src) = state.commits.get(&entry.sha).cloned() else {
                    return RebaseBackendSnafu {
                        reason: format!("unknown sha in rebase: {}", entry.sha),
                    }
                    .fail();
                };
                let mut merged_tree = prev.tree.clone();
                for (path, content) in &src.tree {
                    merged_tree.insert(path.clone(), content.clone());
                }
                let combined_message = match entry.op {
                    RebaseTodoOp::Squash => {
                        let base = last_message.clone().unwrap_or_default();
                        format!("{}\n\n{}", base.trim_end(), src.message.trim_end())
                    },
                    _ => last_message.clone().unwrap_or(prev.message.clone()),
                };

                state.synth_counter += 1;
                let new_sha = format!(
                    "rebased-{}-{}",
                    &entry.sha[..entry.sha.len().min(6)],
                    state.synth_counter
                );
                let new_commit = FakeCommit {
                    parent: prev.parent.clone(),
                    tree: merged_tree,
                    message: combined_message.clone(),
                    author_name: prev.author_name.clone(),
                    author_email: prev.author_email.clone(),
                    time: prev.time,
                };
                state.commits.insert(new_sha.clone(), new_commit);
                state.commits.remove(&prev_sha);
                current = new_sha.clone();
                last_commit = Some(new_sha);
                last_message = Some(combined_message);
            },
        }
    }

    state.head = Some(current.clone());
    state.applied_rebases.push(RecordedRebase {
        onto: onto.to_string(),
        todo: todo.to_vec(),
        new_head: current.clone(),
    });
    Ok(current)
}

pub(super) fn cherry_pick_tree(
    state: &FakeRepoState,
    source_sha: &str,
    onto_sha: &str,
) -> Result<CherryPickOutcome, GitApplyError> {
    if let Some(conflict_sha) = &state.conflict_at
        && conflict_sha == source_sha
    {
        let source = state.commits.get(source_sha).cloned();
        let onto = state.commits.get(onto_sha).cloned();
        let ancestor_tree = source
            .as_ref()
            .and_then(|c| c.parent.as_ref())
            .and_then(|p| state.commits.get(p))
            .map(|c| c.tree.clone())
            .unwrap_or_default();
        // Produce a conflict on every path that differs between
        // ours and theirs; surface stages from each side.
        let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
        let ours_tree = onto.as_ref().map(|c| &c.tree);
        let theirs_tree = source.as_ref().map(|c| &c.tree);
        if let Some(t) = ours_tree {
            paths.extend(t.keys().cloned());
        }
        if let Some(t) = theirs_tree {
            paths.extend(t.keys().cloned());
        }
        let files: Vec<ConflictedFile> = paths
            .into_iter()
            .map(|p| ConflictedFile {
                ancestor: ancestor_tree.get(&p).cloned(),
                ours: ours_tree.and_then(|t| t.get(&p).cloned()),
                theirs: theirs_tree.and_then(|t| t.get(&p).cloned()),
                path: p,
            })
            .collect();
        return Ok(CherryPickOutcome::Conflict { files });
    }
    let Some(source) = state.commits.get(source_sha).cloned() else {
        return BackendSnafu {
            reason: format!("unknown source sha: {source_sha}"),
        }
        .fail();
    };
    let Some(onto) = state.commits.get(onto_sha).cloned() else {
        return BackendSnafu {
            reason: format!("unknown onto sha: {onto_sha}"),
        }
        .fail();
    };
    // Deterministic merge: start from onto's tree, then overlay the
    // diff introduced by source against its parent. Sufficient for
    // snapshot/regression tests without implementing real 3-way merge
    // in the fake.
    let source_parent_tree = source
        .parent
        .as_ref()
        .and_then(|p| state.commits.get(p))
        .map(|c| c.tree.clone())
        .unwrap_or_default();
    let mut tree = onto.tree.clone();
    for (path, content) in &source.tree {
        if source_parent_tree.get(path) != Some(content) {
            tree.insert(path.clone(), content.clone());
        }
    }
    for path in source_parent_tree.keys() {
        if !source.tree.contains_key(path) {
            tree.remove(path);
        }
    }
    Ok(CherryPickOutcome::Clean {
        tree,
        message: source.message.clone(),
        author_name: source.author_name.clone(),
        author_email: source.author_email.clone(),
        author_time: source.time,
    })
}
