//! Minimal RelPath implementation for fuzzy matching compatibility.
//!
//! This is a simplified version of Zed's RelPath type, containing only the functionality
//! needed to work with the fuzzy crate's PathMatchCandidate.

use std::{
    borrow::{Borrow, Cow, ToOwned},
    fmt,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};

/// A file system path that is guaranteed to be relative.
///
/// This is a minimal implementation compatible with fuzzy::PathMatchCandidate.
/// Paths are stored as strings and normalized to use '/' separators.
#[repr(transparent)]
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(str);

impl ToOwned for RelPath {
    type Owned = RelPathBuf;

    fn to_owned(&self) -> Self::Owned {
        RelPathBuf(self.0.to_string())
    }
}

/// An owned representation of a relative path.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPathBuf(String);

impl RelPath {
    /// Creates an empty [`RelPath`].
    pub fn empty() -> &'static Self {
        unsafe { &*("" as *const str as *const RelPath) }
    }

    /// Creates a RelPath from a Path.
    ///
    /// Returns an error if the path is absolute.
    /// Converts backslashes to forward slashes for consistency.
    pub fn from_path(path: &Path) -> Result<Cow<Self>, &'static str> {
        let path_str = path.to_str().ok_or("non-UTF8 path")?;

        // Check if absolute
        if path.is_absolute() {
            return Err("absolute paths not allowed");
        }

        // Normalize separators to forward slashes
        if path_str.contains('\\') {
            let normalized = path_str.replace('\\', "/");
            Ok(Cow::Owned(RelPathBuf(normalized)))
        } else {
            Ok(Cow::Borrowed(unsafe {
                &*(path_str as *const str as *const RelPath)
            }))
        }
    }

    /// Returns the path as a Unix-style string (forward slashes).
    pub fn as_unix_str(&self) -> &str {
        &self.0
    }

    /// Returns individual path components as an iterator.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Converts to a PathBuf.
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }
}

impl RelPathBuf {
    /// Creates a RelPathBuf from a String.
    pub fn from_string(s: String) -> Self {
        RelPathBuf(s)
    }

    /// Returns a reference to the RelPath.
    pub fn as_rel_path(&self) -> &RelPath {
        unsafe { &*(self.0.as_str() as *const str as *const RelPath) }
    }
}

impl Deref for RelPathBuf {
    type Target = RelPath;

    fn deref(&self) -> &Self::Target {
        self.as_rel_path()
    }
}

impl From<&RelPath> for Arc<RelPath> {
    fn from(rel_path: &RelPath) -> Self {
        let bytes: Arc<str> = Arc::from(&rel_path.0);
        unsafe { Arc::from_raw(Arc::into_raw(bytes) as *const RelPath) }
    }
}

impl From<RelPathBuf> for Arc<RelPath> {
    fn from(buf: RelPathBuf) -> Self {
        let bytes: Arc<str> = Arc::from(buf.0);
        unsafe { Arc::from_raw(Arc::into_raw(bytes) as *const RelPath) }
    }
}

impl AsRef<RelPath> for RelPathBuf {
    fn as_ref(&self) -> &RelPath {
        self.as_rel_path()
    }
}

impl Borrow<RelPath> for RelPathBuf {
    fn borrow(&self) -> &RelPath {
        self.as_rel_path()
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.0)
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", &self.0)
    }
}

impl fmt::Debug for RelPathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", &self.0)
    }
}
