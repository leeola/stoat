use crate::{agent_ipc::BridgeOutcome, host::StagedKind};
use std::{fmt::Write, path::PathBuf};
use tokio::sync::mpsc::UnboundedReceiver;

/// The repository's commit message file and the commit it feeds.
///
/// A COMMIT_EDITMSG buffer carries one from the moment the file opens to the
/// moment the commit lands. A commit the repository refuses writes the message
/// back and opens the file again with the same draft.
pub(crate) struct CommitDraft {
    /// The working tree of the repository the commit goes into.
    pub(crate) workdir: PathBuf,
    /// The repository's COMMIT_EDITMSG.
    pub(crate) path: PathBuf,
    /// The staged changes, which the template lists.
    pub(crate) staged: Vec<StagedChange>,
    /// The branch HEAD is on, which the template names, or `None` for a
    /// detached HEAD.
    pub(crate) branch: Option<String>,
    /// The message amends HEAD with the index, in place of a commit on top of
    /// HEAD.
    pub(crate) amend: bool,
}

/// The commit that a held COMMIT_EDITMSG buffer lands when it leaves the
/// editor.
///
/// The editor bridge sends how the buffer left. A clean close commits what the
/// file holds, and a close over unsaved edits commits nothing, as git does when
/// its editor exits.
pub(crate) struct CommitWait {
    pub(crate) draft: CommitDraft,
    pub(crate) outcome: UnboundedReceiver<BridgeOutcome>,
}

/// One staged change as git lists it under `Changes to be committed:`.
pub(crate) struct StagedChange {
    pub(crate) kind: StagedKind,
    /// The changed path, relative to the working tree.
    pub(crate) path: PathBuf,
    /// The path a rename moved from, relative to the working tree.
    pub(crate) from: Option<PathBuf>,
}

/// The text git writes to COMMIT_EDITMSG before it opens the editor.
///
/// A non-empty `message` comes first, then a blank line and git's comment
/// block, which names `branch` and lists each of `staged` under git's label for
/// its kind. An empty `message` leaves the first line blank, where the reader
/// types.
pub(crate) fn template(message: &str, branch: Option<&str>, staged: &[StagedChange]) -> String {
    let mut text = String::new();
    if !message.is_empty() {
        text.push_str(message);
        text.push('\n');
    }
    text.push_str(
        "\n# Please enter the commit message for your changes. Lines starting\n\
         # with '#' will be ignored, and an empty message aborts the commit.\n\
         #\n",
    );
    match branch {
        Some(branch) => {
            let _ = writeln!(text, "# On branch {branch}");
        },
        None => text.push_str("# Not currently on any branch.\n"),
    }
    text.push_str("# Changes to be committed:\n");
    for change in staged {
        let label = match change.kind {
            StagedKind::Added => "new file:",
            StagedKind::Modified => "modified:",
            StagedKind::Deleted => "deleted:",
            StagedKind::Renamed => "renamed:",
        };
        // Git pads the label to twelve columns, so the paths line up.
        let _ = write!(text, "#\t{label:<12}");
        if let Some(from) = &change.from {
            let _ = write!(text, "{} -> ", from.display());
        }
        let _ = writeln!(text, "{}", change.path.display());
    }
    text.push_str("#\n");
    text
}

/// The commit message that `text` holds, cleaned as git's `--cleanup=strip`
/// cleans it.
///
/// Comment lines go, each line loses its trailing whitespace, runs of blank
/// lines collapse to one, and the blank lines at both ends go. The result has
/// no trailing newline, so a text of comments alone cleans to nothing.
pub(crate) fn cleanup(text: &str) -> String {
    let mut message = String::new();
    let mut blank_before = false;
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let line = line.trim_end();
        if line.is_empty() {
            blank_before = !message.is_empty();
            continue;
        }

        if blank_before {
            message.push_str("\n\n");
        } else if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(line);
        blank_before = false;
    }
    message
}

#[cfg(test)]
mod tests {
    use super::{cleanup, template, StagedChange};
    use crate::host::StagedKind;
    use std::path::Path;
    use stoat_language::LanguageRegistry;

    fn change(kind: StagedKind, path: &str) -> StagedChange {
        StagedChange {
            kind,
            path: path.into(),
            from: None,
        }
    }

    #[test]
    fn the_template_names_the_branch_and_labels_each_staged_change() {
        let staged = [
            change(StagedKind::Modified, "a.rs"),
            change(StagedKind::Added, "b.rs"),
            change(StagedKind::Deleted, "c.rs"),
            StagedChange {
                from: Some("d.rs".into()),
                ..change(StagedKind::Renamed, "e.rs")
            },
        ];
        assert_eq!(
            template("", Some("main"), &staged),
            "\n# Please enter the commit message for your changes. Lines starting\n\
             # with '#' will be ignored, and an empty message aborts the commit.\n\
             #\n# On branch main\n# Changes to be committed:\n\
             #\tmodified:   a.rs\n#\tnew file:   b.rs\n#\tdeleted:    c.rs\n\
             #\trenamed:    d.rs -> e.rs\n#\n",
        );
    }

    /// The git-commit grammar reads the whole template with no error, so its
    /// comment block paints as comments and each staged path as a path.
    #[test]
    fn the_template_highlights_as_a_git_commit_message() {
        let text = template(
            "feat: x",
            Some("main"),
            &[change(StagedKind::Modified, "a.rs")],
        );
        let registry = LanguageRegistry::standard();
        let lang = registry
            .for_path(Path::new("COMMIT_EDITMSG"))
            .expect("the git-commit language");
        let tree = stoat_language::parse(&lang, &text, None).expect("a tree");
        let spans = stoat_language::extract_highlights(&lang, &tree, &text);
        let captured = |fragment: &str| {
            let start = text.find(fragment)?;
            let span = spans
                .iter()
                .rev()
                .find(|span| span.byte_range == (start..start + fragment.len()))?;
            let names = lang.highlight_capture_names();
            names.get(span.capture_index as usize).copied()
        };
        assert_eq!(
            (
                tree.root_node().has_error(),
                ["feat: x", "main", "a.rs"].map(captured)
            ),
            (
                false,
                [Some("title.markup"), Some("constant"), Some("string")]
            ),
        );
    }

    #[test]
    fn a_message_sits_above_a_blank_line_and_a_detached_head_has_no_branch() {
        assert_eq!(
            template("two", None, &[]),
            "two\n\n# Please enter the commit message for your changes. Lines starting\n\
             # with '#' will be ignored, and an empty message aborts the commit.\n\
             #\n# Not currently on any branch.\n# Changes to be committed:\n#\n",
        );
    }

    #[test]
    fn cleanup_drops_comments_trailing_space_and_extra_blank_lines() {
        assert_eq!(
            [
                cleanup(&template(
                    "",
                    Some("main"),
                    &[change(StagedKind::Modified, "a.rs")]
                )),
                cleanup("feat: x\n\n\n# note\nbody  \n\n"),
                cleanup("\n  \nfeat: x"),
            ],
            ["", "feat: x\n\nbody", "feat: x"],
        );
    }
}
