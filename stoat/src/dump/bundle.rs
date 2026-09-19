use super::{BundleFormatSnafu, DumpError, ReadDumpSnafu};
use crate::host::FsHost;
use snafu::ResultExt;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const MAGIC: &[u8; 4] = b"SDMP";
const VERSION: u8 = 1;

/// Serialize a dump bundle of the files in `paths`, read from under `root`
/// through `fs`.
///
/// The bundle is a framed header, the RON metadata, then a path and content
/// entry per file. Each file is read into one scratch buffer and appended, so
/// the peak is the bundle plus one file rather than two copies of the tree.
/// Sorted `paths` keep the output reproducible byte for byte across runs.
pub(crate) fn serialize(
    meta_ron: &str,
    root: &Path,
    paths: &[PathBuf],
    fs: &dyn FsHost,
) -> Result<Vec<u8>, DumpError> {
    let meta_bytes = meta_ron.as_bytes();
    if meta_bytes.len() > u32::MAX as usize {
        return BundleFormatSnafu {
            reason: "dump metadata too large".to_string(),
        }
        .fail();
    }
    if paths.len() > u32::MAX as usize {
        return BundleFormatSnafu {
            reason: "dump contains too many entries".to_string(),
        }
        .fail();
    }

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&(meta_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(meta_bytes);
    out.extend_from_slice(&(paths.len() as u32).to_le_bytes());

    let mut content = Vec::new();
    for rel in paths {
        let path_str = rel.to_str().ok_or_else(|| {
            BundleFormatSnafu {
                reason: format!("non-UTF-8 path in dump: {}", rel.display()),
            }
            .build()
        })?;
        let path_bytes = path_str.as_bytes();
        if path_bytes.len() > u16::MAX as usize {
            return BundleFormatSnafu {
                reason: format!("dump entry path exceeds 64KB: {path_str}"),
            }
            .fail();
        }

        let path = root.join(rel);
        fs.read(&path, &mut content)
            .with_context(|_| ReadDumpSnafu { path: path.clone() })?;

        out.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(path_bytes);
        out.extend_from_slice(&(content.len() as u64).to_le_bytes());
        out.extend_from_slice(&content);
    }

    Ok(out)
}

/// Deserialize a bundle written by [`serialize`]. Returns the metadata
/// RON string and the file map. Errors as [`DumpError::BundleFormat`]
/// on truncation, bad magic, or unknown version.
pub(crate) fn deserialize(bytes: &[u8]) -> Result<(String, BTreeMap<PathBuf, Vec<u8>>), DumpError> {
    let mut cur = Cursor::new(bytes);
    let magic = cur.take(4)?;
    if magic != MAGIC {
        return BundleFormatSnafu {
            reason: format!("bad dump magic: expected {:?}, got {magic:?}", MAGIC),
        }
        .fail();
    }
    let version = cur.take(1)?[0];
    if version != VERSION {
        return BundleFormatSnafu {
            reason: format!("unsupported dump version: {version}"),
        }
        .fail();
    }

    let meta_len = cur.read_u32()? as usize;
    let meta_bytes = cur.take(meta_len)?;
    let meta_ron = std::str::from_utf8(meta_bytes)
        .map_err(|e| {
            BundleFormatSnafu {
                reason: format!("dump metadata not UTF-8: {e}"),
            }
            .build()
        })?
        .to_string();

    let entry_count = cur.read_u32()? as usize;
    let mut entries = BTreeMap::new();
    for _ in 0..entry_count {
        let path_len = cur.read_u16()? as usize;
        let path_bytes = cur.take(path_len)?;
        let path_str = std::str::from_utf8(path_bytes).map_err(|e| {
            BundleFormatSnafu {
                reason: format!("dump entry path not UTF-8: {e}"),
            }
            .build()
        })?;
        let content_len = cur.read_u64()? as usize;
        let content = cur.take(content_len)?.to_vec();
        entries.insert(PathBuf::from(path_str), content);
    }

    if cur.pos != bytes.len() {
        return BundleFormatSnafu {
            reason: format!(
                "trailing bytes after dump payload: {} unread",
                bytes.len() - cur.pos
            ),
        }
        .fail();
    }

    Ok((meta_ron, entries))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DumpError> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            BundleFormatSnafu {
                reason: "dump payload length overflow".to_string(),
            }
            .build()
        })?;
        if end > self.bytes.len() {
            return BundleFormatSnafu {
                reason: format!(
                    "dump truncated: wanted {n} bytes at offset {}, have {}",
                    self.pos,
                    self.bytes.len() - self.pos
                ),
            }
            .fail();
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u16(&mut self) -> Result<u16, DumpError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, DumpError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64(&mut self) -> Result<u64, DumpError> {
        let bytes = self.take(8)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeFs;

    /// A bundle of `files`, each a relative path and its content, read from a
    /// fake tree rooted at `/ws`.
    fn bundle(meta: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let fs = FakeFs::new();
        for (path, content) in files {
            fs.insert_file(Path::new("/ws").join(path), content);
        }
        let paths: Vec<PathBuf> = files.iter().map(|(path, _)| PathBuf::from(path)).collect();
        serialize(meta, Path::new("/ws"), &paths, &fs).unwrap()
    }

    #[test]
    fn roundtrip_empty_entries() {
        let (meta, entries) = deserialize(&bundle("meta", &[])).unwrap();
        assert_eq!(meta, "meta");
        assert!(entries.is_empty());
    }

    /// Both files go through one scratch buffer, so bytes the first read left
    /// behind show up in the second file's content.
    #[test]
    fn roundtrip_with_entries() {
        let files: [(&str, &[u8]); 2] = [("a.rs", b"alpha"), ("sub/b.rs", b"beta\n\x00\xff")];
        let (meta, entries) = deserialize(&bundle("(name: \"x\")", &files)).unwrap();
        assert_eq!(meta, "(name: \"x\")");

        let expected: BTreeMap<PathBuf, Vec<u8>> = files
            .iter()
            .map(|(path, content)| (PathBuf::from(path), content.to_vec()))
            .collect();
        assert_eq!(entries, expected);
    }

    #[test]
    fn deserialize_rejects_bad_magic() {
        let mut bytes = bundle("m", &[]);
        bytes[0] = b'X';
        let err = deserialize(&bytes).unwrap_err();
        assert!(format!("{err}").contains("bad dump magic"));
    }

    #[test]
    fn deserialize_rejects_unknown_version() {
        let mut bytes = bundle("m", &[]);
        bytes[4] = 99;
        let err = deserialize(&bytes).unwrap_err();
        assert!(format!("{err}").contains("unsupported dump version"));
    }

    #[test]
    fn deserialize_rejects_truncated() {
        let bytes = bundle("m", &[]);
        let err = deserialize(&bytes[..bytes.len() - 1]).unwrap_err();
        assert!(format!("{err}").contains("truncated"));
    }

    #[test]
    fn deserialize_rejects_trailing_bytes() {
        let mut bytes = bundle("m", &[]);
        bytes.push(0);
        let err = deserialize(&bytes).unwrap_err();
        assert!(format!("{err}").contains("trailing bytes"));
    }
}
