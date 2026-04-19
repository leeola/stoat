use super::snapshot::WorkspaceSnapshot;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;

pub const META_FILENAME: &str = "dump.ron";

pub const META_PATH_IN_ARCHIVE: &str = ".stoat/dump.ron";

/// Metadata describing one dump: when it was captured, which repo it
/// came from, which workspace fields were omitted from the capture
/// (runtime-only state like PTYs and async sessions), and a
/// [`WorkspaceSnapshot`] carrying the subset of workspace state that is
/// currently serializable. Written as `.stoat/dump.ron` inside the
/// archive so the archive is self-describing without needing the Stoat
/// build that produced it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DumpMeta {
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub name: String,
    pub stoat_version: String,
    pub git_root: PathBuf,
    /// Names of workspace fields that were intentionally skipped because
    /// they hold runtime-only state that cannot be serialized in the
    /// current build. Consumers of the dump should treat these as
    /// "reset to default on load".
    pub dropped_fields: Vec<String>,
    #[serde(default)]
    pub(crate) workspace: WorkspaceSnapshot,
}

impl DumpMeta {
    pub fn to_ron(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
    }

    pub fn from_ron(input: &str) -> Result<Self, ron::error::SpannedError> {
        ron::from_str(input)
    }
}
