use crate::agent_ipc::BridgeOutcome;
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
    /// The staged paths, relative to [`Self::workdir`], which the template
    /// lists.
    pub(crate) staged: Vec<PathBuf>,
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

/// The text git writes to COMMIT_EDITMSG before it opens the editor.
///
/// A non-empty `message` comes first, then a blank line and git's comment
/// block, which names `branch` and lists `staged`. An empty `message` leaves the
/// first line blank, where the reader types.
pub(crate) fn template(message: &str, branch: Option<&str>, staged: &[PathBuf]) -> String {
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
    for path in staged {
        let _ = writeln!(text, "#\t{}", path.display());
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
    use super::{cleanup, template};
    use std::path::PathBuf;

    #[test]
    fn the_template_names_the_branch_and_lists_the_staged_paths() {
        assert_eq!(
            template("", Some("main"), &["a.rs".into(), "b.rs".into()]),
            "\n# Please enter the commit message for your changes. Lines starting\n\
             # with '#' will be ignored, and an empty message aborts the commit.\n\
             #\n# On branch main\n# Changes to be committed:\n#\ta.rs\n#\tb.rs\n#\n",
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
                cleanup(&template("", Some("main"), &[PathBuf::from("a.rs")])),
                cleanup("feat: x\n\n\n# note\nbody  \n\n"),
                cleanup("\n  \nfeat: x"),
            ],
            ["", "feat: x\n\nbody", "feat: x"],
        );
    }
}
