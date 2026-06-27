//! Persistence codec for the on-disk index.
//!
//! Each [`FileShard`] serializes independently so a single changed file
//! reindexes without rewriting the whole index. A [`Manifest`] ties the
//! shards together, listing every covered file with its content hash and
//! stamping the [`SCHEMA_VERSION`] the index was written at.

use crate::FileShard;
use serde::{Deserialize, Serialize};

/// The on-disk format version.
///
/// Bump it whenever the persisted shard or manifest layout changes. The
/// loader compares a manifest's stamp against this and discards a stale
/// index rather than misreading bytes from an older layout.
pub const SCHEMA_VERSION: u32 = 1;

/// The index manifest, recording the schema version it was written at and
/// an entry for every file the index covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub files: Vec<FileEntry>,
}

/// One file's manifest entry, pairing its workspace-relative path with the
/// content hash that produced its shard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub rel_path: String,
    pub content_hash: [u8; 32],
}

/// Serialize a [`FileShard`] to its on-disk bytes.
pub fn encode_shard(shard: &FileShard) -> Vec<u8> {
    postcard::to_allocvec(shard).expect("a FileShard of plain data serializes infallibly")
}

/// Deserialize a [`FileShard`] from bytes, returning an error when they are
/// not a shard of the current layout.
pub fn decode_shard(bytes: &[u8]) -> postcard::Result<FileShard> {
    postcard::from_bytes(bytes)
}

/// Serialize a [`Manifest`] to its on-disk bytes.
pub fn encode_manifest(manifest: &Manifest) -> Vec<u8> {
    postcard::to_allocvec(manifest).expect("a Manifest of plain data serializes infallibly")
}

/// Deserialize a [`Manifest`] from bytes.
///
/// This does not check [`Manifest::schema_version`] against
/// [`SCHEMA_VERSION`]. It round-trips the stored version so the caller can
/// compare and decide whether to trust or rebuild the index.
pub fn decode_manifest(bytes: &[u8]) -> postcard::Result<Manifest> {
    postcard::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        decode_manifest, decode_shard, encode_manifest, encode_shard, FileEntry, Manifest,
        SCHEMA_VERSION,
    };
    use crate::{Confidence, Edge, EdgeKind, FileId, FileShard, Symbol, SymbolKey, Target};
    use stoat_language::{RefKind, SymbolKind};

    fn sample_shard() -> FileShard {
        FileShard {
            content_hash: [3u8; 32],
            symbols: vec![Symbol {
                key: SymbolKey([1u8; 16]),
                file: FileId(2),
                name: "foo".to_string(),
                kind: SymbolKind::Function,
                container: vec!["m".to_string()],
                def_range: 0..20,
                name_range: 3..6,
                body_hash: [5u8; 32],
            }],
            edges: vec![Edge {
                from: SymbolKey([1u8; 16]),
                to: Target::Unresolved {
                    name: "bar".to_string(),
                    kind: RefKind::Call,
                },
                kind: EdgeKind::Calls,
                site_range: 10..13,
                confidence: Confidence::NameMatch,
            }],
        }
    }

    #[test]
    fn shard_round_trips() {
        let shard = sample_shard();
        assert_eq!(decode_shard(&encode_shard(&shard)).unwrap(), shard);
    }

    #[test]
    fn manifest_round_trips() {
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            files: vec![FileEntry {
                rel_path: "src/a.rs".to_string(),
                content_hash: [7u8; 32],
            }],
        };
        assert_eq!(
            decode_manifest(&encode_manifest(&manifest)).unwrap(),
            manifest
        );
    }

    #[test]
    fn foreign_schema_version_decodes_observably() {
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION + 1,
            files: vec![],
        };
        let decoded = decode_manifest(&encode_manifest(&manifest)).unwrap();
        assert_ne!(decoded.schema_version, SCHEMA_VERSION);
        assert_eq!(decoded.schema_version, SCHEMA_VERSION + 1);
    }
}
