use crate::input_view::InputView;
use std::path::PathBuf;

/// The screen a reader writes a commit message in, for the changes the index
/// holds.
///
/// The message is an [`InputView`] editor, so it takes the modal keys a buffer
/// takes. Ctrl-s commits the index with its text, and the screen stays up
/// until that commit lands, so a failed commit keeps the message.
pub(crate) struct CommitMessageEditor {
    /// The working tree of the repository the commit goes into.
    pub workdir: PathBuf,
    /// The staged paths, relative to [`Self::workdir`], which the screen lists
    /// above the message.
    pub staged: Vec<PathBuf>,
    pub input: InputView,
}
