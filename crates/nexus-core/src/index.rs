//! The staging area (spec §4): a binary file of entries sorted by path,
//! little-endian, ending in a SHA-256 checksum of everything before it.
//!
//! ```text
//! header   magic "NXIX" | version u32 = 1 | entry_count u32
//! entry    path_len u16 | path | kind u8 (0 file, 1 exec) | hash [32] | size u64 | mtime_ns i64
//! trailer  SHA-256 of all preceding bytes [32]
//! ```

use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};
use crate::fsutil;
use crate::hash::ObjectId;
use crate::object::EntryKind;
use crate::path::RepoPath;

const MAGIC: &[u8; 4] = b"NXIX";
const VERSION: u32 = 1;
const CHECKSUM_LEN: usize = 32;

/// The longest path the index can store, in bytes.
pub const MAX_PATH_LEN: usize = u16::MAX as usize;

/// Files modified this recently when the index is written get their stat
/// data smudged (see [`Index::save`]). Two seconds covers filesystems with
/// coarse timestamps, such as FAT's.
pub const RACY_WINDOW_NS: i64 = 2_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Exec,
}

impl From<FileKind> for EntryKind {
    fn from(kind: FileKind) -> Self {
        match kind {
            FileKind::File => Self::File,
            FileKind::Exec => Self::Exec,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub kind: FileKind,
    pub id: ObjectId,
    pub size: u64,
    /// Zero when the filesystem didn't report a modification time, which
    /// forces the content to be rehashed next time.
    pub mtime_ns: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    entries: BTreeMap<RepoPath, IndexEntry>,
}

impl Index {
    /// Loads the index, treating a missing file as an empty index.
    pub fn load(path: &Path) -> Result<Self> {
        match fsutil::read_optional(path)? {
            None => Ok(Self::default()),
            Some(bytes) => Self::decode(&bytes).map_err(|reason| Error::Corrupt {
                path: path.to_path_buf(),
                reason: format!("the index is corrupt: {reason}"),
            }),
        }
    }

    /// Writes the index. Entries for files modified within [`RACY_WINDOW_NS`]
    /// of now are written with a zero modification time ("smudged"), so the
    /// next command rehashes them instead of trusting their size and time.
    ///
    /// Without this, a same-size edit made within the filesystem's timestamp
    /// granularity of staging would look unchanged forever once a later write
    /// gave the index a newer timestamp (the "racy timestamp" problem).
    pub fn save(&self, path: &Path) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|now| i64::try_from(now.as_nanos()).ok())
            .unwrap_or(i64::MAX);
        fsutil::write_file(
            path,
            &self.encode_with(Some(now.saturating_sub(RACY_WINDOW_NS))),
        )
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, path: &RepoPath) -> Option<&IndexEntry> {
        self.entries.get(path)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RepoPath, &IndexEntry)> {
        self.entries.iter()
    }

    /// Adds or replaces an entry. A file can't also be a directory, so this
    /// removes any entry for an ancestor of `path` and any entry inside it.
    pub fn insert(&mut self, path: RepoPath, entry: IndexEntry) {
        debug_assert!(!path.is_root());
        let text = path.as_str();
        for (end, _) in text.match_indices('/') {
            self.entries.remove(&text[..end]);
        }
        let inside: Vec<RepoPath> = self
            .within(&path)
            .filter(|other| **other != path)
            .cloned()
            .collect();
        for other in inside {
            self.entries.remove(&other);
        }
        self.entries.insert(path, entry);
    }

    pub fn remove(&mut self, path: &RepoPath) -> Option<IndexEntry> {
        self.entries.remove(path)
    }

    /// The paths of entries at or inside `scope`, in order.
    pub fn within<'a>(&'a self, scope: &'a RepoPath) -> impl Iterator<Item = &'a RepoPath> + 'a {
        // Everything inside `scope` starts with its text, and sorted paths that
        // share a prefix are contiguous, so scan from `scope` to the first path
        // without that prefix. ("a-b" and "ab" share the prefix but aren't
        // inside "a", hence the filter.)
        self.entries
            .range::<RepoPath, _>((Bound::Included(scope), Bound::Unbounded))
            .map(|(path, _)| path)
            .take_while(move |path| path.as_str().starts_with(scope.as_str()))
            .filter(move |path| scope.contains(path))
    }

    pub fn encode(&self) -> Vec<u8> {
        self.encode_with(None)
    }

    /// Encodes the index, writing a zero modification time for entries
    /// modified at or after `racy_after`.
    fn encode_with(&self, racy_after: Option<i64>) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.entries.len() * 64 + CHECKSUM_LEN);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        let count = u32::try_from(self.entries.len()).expect("fewer than 2^32 index entries");
        out.extend_from_slice(&count.to_le_bytes());
        for (path, entry) in &self.entries {
            let path = path.as_str().as_bytes();
            let len =
                u16::try_from(path.len()).expect("index paths are at most MAX_PATH_LEN bytes");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(path);
            out.push(match entry.kind {
                FileKind::File => 0,
                FileKind::Exec => 1,
            });
            out.extend_from_slice(entry.id.as_bytes());
            out.extend_from_slice(&entry.size.to_le_bytes());
            let racy = racy_after.is_some_and(|after| entry.mtime_ns >= after);
            let mtime_ns = if racy { 0 } else { entry.mtime_ns };
            out.extend_from_slice(&mtime_ns.to_le_bytes());
        }
        let checksum = Sha256::digest(&out);
        out.extend_from_slice(&checksum);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let body_len = bytes
            .len()
            .checked_sub(CHECKSUM_LEN)
            .ok_or("too short to hold a checksum")?;
        let (body, checksum) = bytes.split_at(body_len);
        if Sha256::digest(body).as_slice() != checksum {
            return Err("the checksum doesn't match".to_owned());
        }
        let mut reader = Reader(body);
        if reader.take(4)? != MAGIC {
            return Err("not an index file".to_owned());
        }
        let version = u32::from_le_bytes(reader.array()?);
        if version != VERSION {
            return Err(format!("unsupported index version {version}"));
        }
        let count = u32::from_le_bytes(reader.array()?);
        let mut entries = BTreeMap::new();
        let mut previous: Option<RepoPath> = None;
        let mut files: HashSet<String> = HashSet::new();
        for _ in 0..count {
            let len = usize::from(u16::from_le_bytes(reader.array()?));
            let text =
                std::str::from_utf8(reader.take(len)?).map_err(|_| "a path isn't valid UTF-8")?;
            let path = RepoPath::parse(text)?;
            if previous.as_ref().is_some_and(|previous| *previous >= path) {
                return Err(format!("entries aren't sorted and unique at {path}"));
            }
            // A path can't be both a file and a directory. Sorting doesn't put
            // `a` next to `a/b` (`a-b` can sit between them), so check every
            // ancestor against the files seen so far.
            for (end, _) in text.match_indices('/') {
                if files.contains(&text[..end]) {
                    return Err(format!(
                        "{} is listed both as a file and as a directory. Delete .nexus/index and \
                         run `nexus add .` to rebuild it",
                        &text[..end]
                    ));
                }
            }
            files.insert(text.to_owned());
            let kind = match reader.take(1)?[0] {
                0 => FileKind::File,
                1 => FileKind::Exec,
                other => return Err(format!("unknown entry kind {other}")),
            };
            let id = ObjectId::from_bytes(reader.array()?);
            let size = u64::from_le_bytes(reader.array()?);
            let mtime_ns = i64::from_le_bytes(reader.array()?);
            previous = Some(path.clone());
            entries.insert(
                path,
                IndexEntry {
                    kind,
                    id,
                    size,
                    mtime_ns,
                },
            );
        }
        if !reader.0.is_empty() {
            return Err("unexpected bytes after the last entry".to_owned());
        }
        Ok(Self { entries })
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.0.len() < len {
            return Err("truncated".to_owned());
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        Ok(self
            .take(N)?
            .try_into()
            .expect("take returns exactly N bytes"))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn path(text: &str) -> RepoPath {
        RepoPath::parse(text).unwrap()
    }

    fn entry(byte: u8) -> IndexEntry {
        IndexEntry {
            kind: if byte.is_multiple_of(2) {
                FileKind::File
            } else {
                FileKind::Exec
            },
            id: ObjectId::from_bytes([byte; 32]),
            size: u64::from(byte) * 1000,
            mtime_ns: i64::from(byte) * -7,
        }
    }

    #[test]
    fn exact_layout() {
        let mut index = Index::default();
        index.insert(path("a"), entry(1));
        let bytes = index.encode();
        assert_eq!(&bytes[..12], b"NXIX\x01\x00\x00\x00\x01\x00\x00\x00");
        assert_eq!(&bytes[12..15], b"\x01\x00a");
        assert_eq!(bytes[15], 1);
        assert_eq!(bytes.len(), 12 + 2 + 1 + 1 + 32 + 8 + 8 + 32);
    }

    #[test]
    fn rejects_damage() {
        let mut index = Index::default();
        index.insert(path("src/main.rs"), entry(2));
        let mut bytes = index.encode();
        bytes[20] ^= 1;
        assert_eq!(
            Index::decode(&bytes),
            Err("the checksum doesn't match".to_owned())
        );
        assert!(Index::decode(b"short").is_err());
    }

    #[test]
    fn rejects_a_path_listed_as_file_and_directory() {
        // Insert would prevent this, so build the entries directly. `a-b`
        // sorts between `a` and `a/b`, so the check can't rely on adjacency.
        let index = Index {
            entries: BTreeMap::from([
                (path("a"), entry(1)),
                (path("a-b"), entry(2)),
                (path("a/b"), entry(3)),
            ]),
        };
        let err = Index::decode(&index.encode()).unwrap_err();
        assert!(
            err.contains("a is listed both as a file and as a directory"),
            "{err}"
        );
    }

    #[test]
    fn saving_smudges_recently_modified_entries() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index");
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap();
        let mut index = Index::default();
        index.insert(
            path("old"),
            IndexEntry {
                mtime_ns: now - 3 * RACY_WINDOW_NS,
                ..entry(1)
            },
        );
        index.insert(
            path("fresh"),
            IndexEntry {
                mtime_ns: now,
                ..entry(2)
            },
        );
        index.save(&file).unwrap();
        let loaded = Index::load(&file).unwrap();
        assert_eq!(
            loaded.get(&path("old")).unwrap().mtime_ns,
            now - 3 * RACY_WINDOW_NS
        );
        assert_eq!(
            loaded.get(&path("fresh")).unwrap().mtime_ns,
            0,
            "recent entries must be rehashed next time"
        );
        assert_eq!(loaded.get(&path("fresh")).unwrap().id, entry(2).id);
    }

    #[test]
    fn insert_replaces_files_that_became_directories_and_back() {
        let mut index = Index::default();
        index.insert(path("a"), entry(1));
        index.insert(path("a/b"), entry(2));
        assert_eq!(
            index.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["a/b"]
        );
        index.insert(path("a/c"), entry(3));
        index.insert(path("ab"), entry(4));
        index.insert(path("a"), entry(5));
        assert_eq!(
            index.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["a", "ab"]
        );
    }

    #[test]
    fn within_a_scope() {
        let mut index = Index::default();
        for p in ["a", "a-b", "a/x", "a/y/z", "b"] {
            index.insert(path(p), entry(1));
        }
        // Inserting "a/x" removed the file "a"; re-add it as a sibling name instead.
        let scope = path("a");
        assert_eq!(
            index
                .within(&scope)
                .map(RepoPath::as_str)
                .collect::<Vec<_>>(),
            ["a/x", "a/y/z"]
        );
        let root = RepoPath::root();
        assert_eq!(index.within(&root).count(), 4);
    }

    proptest! {
        #[test]
        fn encode_decode_round_trip(
            files in proptest::collection::btree_map("[a-z]{1,3}(/[a-zé]{1,3}){0,2}", any::<(bool, [u8; 32], u64, i64)>(), 0..40)
        ) {
            let mut index = Index::default();
            for (p, (exec, id, size, mtime_ns)) in files {
                let kind = if exec { FileKind::Exec } else { FileKind::File };
                index.insert(path(&p), IndexEntry { kind, id: ObjectId::from_bytes(id), size, mtime_ns });
            }
            prop_assert_eq!(Index::decode(&index.encode()).unwrap(), index);
        }
    }
}
