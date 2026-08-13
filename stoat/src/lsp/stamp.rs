//! The buffer signature a request carries so its reply lands on the right
//! text.

use crate::{app::Stoat, buffer::BufferId, host::OffsetEncoding};

/// What a reply needs to be applied to the document its request was about.
///
/// The buffer, the version it held when the request went out, and the offset
/// encoding the server it went to negotiated. A reply is applied against all
/// three or against nothing.
///
/// A reply names positions in the text the server was told about, so if the
/// buffer has moved since, those positions name different text and applying the
/// reply shifts every edit by whatever was typed. The stamp is what lets the
/// reply be recognized as stale and dropped instead.
///
/// The version compared is the buffer's own, not the version last synced to the
/// server. The synced one only advances when the sync pump runs, so a reply
/// landing between an edit and the next sync would compare equal.
///
/// Two servers on one buffer can negotiate different encodings, and an edit
/// routed back to the one that produced it has to be read in that one's units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DocumentStamp {
    buffer_id: BufferId,
    version: u64,
    encoding: OffsetEncoding,
}

impl DocumentStamp {
    /// Stamp `buffer_id` at the version it holds now. `None` when there is no
    /// such buffer, in which case there is nothing to make a request about.
    pub(crate) fn take(
        stoat: &Stoat,
        buffer_id: BufferId,
        encoding: OffsetEncoding,
    ) -> Option<Self> {
        let version = stoat
            .active_workspace()
            .buffers
            .get(buffer_id)?
            .read()
            .expect("buffer lock")
            .version();
        Some(Self {
            buffer_id,
            version,
            encoding,
        })
    }

    /// Whether the buffer still holds the version this was taken at. A buffer
    /// that has since closed counts as changed, there being nothing to apply to.
    pub(crate) fn is_current(&self, stoat: &Stoat) -> bool {
        Self::take(stoat, self.buffer_id, self.encoding) == Some(*self)
    }

    /// The encoding the server this request went to reads positions in.
    pub(crate) fn encoding(&self) -> OffsetEncoding {
        self.encoding
    }
}
