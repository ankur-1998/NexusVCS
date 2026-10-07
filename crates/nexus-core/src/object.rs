//! Object formats (spec §4). Every object's canonical bytes are
//! `<type> <bodyByteLength>\0<body>`, and its ID is the SHA-256 of those bytes.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;

use crate::hash::{Hasher, ObjectId};
use crate::path::{nfc, validate_name};
use crate::refs::{Head, is_valid_ref_name};
use crate::time::Timestamp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Blob,
    Chunked,
    Tree,
    Commit,
    Op,
}

impl ObjectKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Chunked => "chunked",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Op => "op",
        }
    }

    fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"blob" => Self::Blob,
            b"chunked" => Self::Chunked,
            b"tree" => Self::Tree,
            b"commit" => Self::Commit,
            b"op" => Self::Op,
            _ => return None,
        })
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The canonical header that precedes a body of `body_len` bytes.
pub fn header(kind: ObjectKind, body_len: usize) -> Vec<u8> {
    format!("{kind} {body_len}\0").into_bytes()
}

/// The ID an object with this kind and body has.
pub fn id_of(kind: ObjectKind, body: &[u8]) -> ObjectId {
    let mut hasher = Hasher::default();
    hasher.update(&header(kind, body.len()));
    hasher.update(body);
    hasher.finish()
}

/// The longest header there can be: the longest type name, a space, and a
/// 20-digit length.
pub const MAX_HEADER_LEN: usize = "chunked".len() + 1 + 20;

/// Parses the header text before the NUL, `<type> <length>`.
pub fn parse_header_text(header: &[u8]) -> Result<(ObjectKind, usize), String> {
    let (name, len) = header
        .split(|&b| b == b' ')
        .collect::<Vec<_>>()
        .try_into()
        .ok()
        .map(|[name, len]: [&[u8]; 2]| (name, len))
        .ok_or("the header isn't `<type> <length>`")?;
    let kind = ObjectKind::from_name(name).ok_or("unknown object type")?;
    let canonical_len =
        !len.is_empty() && len.iter().all(u8::is_ascii_digit) && (len.len() == 1 || len[0] != b'0');
    let declared: usize = std::str::from_utf8(len)
        .ok()
        .filter(|_| canonical_len)
        .and_then(|len| len.parse().ok())
        .ok_or("the header's length isn't a canonical decimal number")?;
    Ok((kind, declared))
}

/// Splits canonical bytes into their kind and the length of the header,
/// checking that the declared length matches the body.
pub fn parse_header(canonical: &[u8]) -> Result<(ObjectKind, usize), String> {
    let nul = canonical
        .iter()
        .position(|&b| b == 0)
        .ok_or("the header has no terminating NUL")?;
    let (kind, declared) = parse_header_text(&canonical[..nul])?;
    let body_len = canonical.len() - nul - 1;
    if declared != body_len {
        return Err(format!(
            "the header declares {declared} bytes but the body has {body_len}"
        ));
    }
    Ok((kind, nul + 1))
}

// ---------------------------------------------------------------------------
// Trees

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Exec,
    Tree,
}

impl EntryKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Exec => "exec",
            Self::Tree => "tree",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "file" => Self::File,
            "exec" => Self::Exec,
            "tree" => Self::Tree,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub id: ObjectId,
}

/// A directory: entries sorted by name in byte order, names unique.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// One `<file|exec|tree> <hash> <name>\n` line per entry. The entries must
    /// already be sorted by name.
    pub fn encode(&self) -> Vec<u8> {
        debug_assert!(self.entries.windows(2).all(|w| w[0].name < w[1].name));
        let mut body = String::new();
        for entry in &self.entries {
            let _ = writeln!(body, "{} {} {}", entry.kind.name(), entry.id, entry.name);
        }
        body.into_bytes()
    }

    pub fn decode(body: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(body).map_err(|_| "a tree isn't valid UTF-8")?;
        let mut entries: Vec<TreeEntry> = Vec::new();
        for line in lines(text)? {
            let mut fields = line.splitn(3, ' ');
            let (Some(kind), Some(id), Some(name)) = (fields.next(), fields.next(), fields.next())
            else {
                return Err(format!("malformed tree entry {line:?}"));
            };
            let kind =
                EntryKind::from_name(kind).ok_or_else(|| format!("unknown entry kind {kind:?}"))?;
            let id = ObjectId::parse_hex(id)
                .ok_or_else(|| format!("bad hash in tree entry {line:?}"))?;
            validate_name(name)?;
            if nfc(name) != name {
                return Err(format!("tree entry {name:?} isn't in Unicode NFC form"));
            }
            if entries
                .last()
                .is_some_and(|last| last.name.as_str() >= name)
            {
                return Err(format!("tree entries aren't sorted and unique at {name:?}"));
            }
            entries.push(TreeEntry {
                name: name.to_owned(),
                kind,
                id,
            });
        }
        Ok(Self { entries })
    }
}

/// Splits a body made of `\n`-terminated lines. Only `\n` ends a line: a `\r`
/// is data (a file name can end with one).
fn lines(text: &str) -> Result<impl Iterator<Item = &str>, String> {
    if !text.is_empty() && !text.ends_with('\n') {
        return Err("the last line isn't terminated by a newline".to_owned());
    }
    Ok(text.split_terminator('\n'))
}

// ---------------------------------------------------------------------------
// Chunked files

/// Files this size or larger are stored as chunks. Part of the format.
pub const CHUNKED_THRESHOLD: u64 = 8 * 1024 * 1024;
/// `FastCDC` 2020 parameters (normalization level 1, seed 0). Part of the
/// format: changing them changes the ID of every large file.
pub const CHUNK_MIN: usize = 256 * 1024;
pub const CHUNK_AVG: usize = 1024 * 1024;
pub const CHUNK_MAX: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub id: ObjectId,
    pub len: u64,
}

/// A large file's content: its size, then its chunks in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChunkList {
    pub size: u64,
    pub chunks: Vec<Chunk>,
}

impl ChunkList {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = format!("size {}\n", self.size);
        for chunk in &self.chunks {
            let _ = writeln!(body, "{} {}", chunk.id, chunk.len);
        }
        body.into_bytes()
    }

    pub fn decode(body: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(body).map_err(|_| "a chunk list isn't valid UTF-8")?;
        let mut lines = lines(text)?;
        let size = lines
            .next()
            .and_then(|line| line.strip_prefix("size "))
            .and_then(parse_u64)
            .ok_or("a chunk list must start with `size <bytes>`")?;
        let mut chunks = Vec::new();
        for line in lines {
            let chunk = line
                .split_once(' ')
                .and_then(|(id, len)| {
                    Some(Chunk {
                        id: ObjectId::parse_hex(id)?,
                        len: parse_u64(len)?,
                    })
                })
                .ok_or_else(|| format!("malformed chunk line {line:?}"))?;
            chunks.push(chunk);
        }
        let total = chunks
            .iter()
            .try_fold(0_u64, |sum, chunk| sum.checked_add(chunk.len));
        if total != Some(size) {
            return Err(format!(
                "the chunks don't add up to the declared size of {size} bytes"
            ));
        }
        // Only lists that chunking could have produced are canonical.
        if size < CHUNKED_THRESHOLD {
            return Err(format!(
                "a chunk list for {size} bytes, below the chunking threshold"
            ));
        }
        let (last, rest) = chunks
            .split_last()
            .ok_or("a chunk list needs at least one chunk")?;
        let max = CHUNK_MAX as u64;
        let min = CHUNK_MIN as u64;
        if rest.iter().any(|c| c.len < min || c.len > max) || last.len == 0 || last.len > max {
            return Err("a chunk's length is outside what chunking produces".to_owned());
        }
        Ok(Self { size, chunks })
    }
}

/// Parses a canonical decimal number: no sign, no leading zeros.
fn parse_u64(text: &str) -> Option<u64> {
    let value: u64 = text.parse().ok()?;
    (value.to_string() == text).then_some(value)
}

// ---------------------------------------------------------------------------
// Commits

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: Timestamp,
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} <{}> {}", self.name, self.email, self.time)
    }
}

impl Signature {
    fn parse(text: &str) -> Option<Self> {
        let (who, when) = text.rsplit_once("> ")?;
        let (name, email) = who.rsplit_once(" <")?;
        Some(Self {
            name: name.to_owned(),
            email: email.to_owned(),
            time: Timestamp::parse(when)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: ObjectId,
    /// None for a root commit, two for a merge. The first is the branch the
    /// commit was made on.
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    /// Ends with a newline.
    pub message: String,
}

impl Commit {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = format!("tree {}\n", self.tree);
        for parent in &self.parents {
            let _ = writeln!(body, "parent {parent}");
        }
        let _ = write!(body, "author {}\n\n{}", self.author, self.message);
        body.into_bytes()
    }

    pub fn decode(body: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(body).map_err(|_| "a commit isn't valid UTF-8")?;
        let (headers, message) = text
            .split_once("\n\n")
            .ok_or("a commit needs a blank line before its message")?;
        let mut lines = headers.split('\n');
        let tree = lines
            .next()
            .and_then(|line| line.strip_prefix("tree "))
            .and_then(ObjectId::parse_hex)
            .ok_or("a commit must start with `tree <hash>`")?;
        let mut parents = Vec::new();
        let mut author = None;
        for line in lines {
            if author.is_some() {
                return Err(format!("unexpected line after the author: {line:?}"));
            }
            if let Some(parent) = line.strip_prefix("parent ") {
                parents.push(ObjectId::parse_hex(parent).ok_or("bad parent hash")?);
            } else if let Some(signature) = line.strip_prefix("author ") {
                author = Some(Signature::parse(signature).ok_or("malformed author line")?);
            } else {
                return Err(format!("unexpected commit header {line:?}"));
            }
        }
        Ok(Self {
            tree,
            parents,
            author: author.ok_or("a commit needs an author")?,
            message: message.to_owned(),
        })
    }

    /// The first line of the message.
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Operations

/// The repository state an operation leaves behind (spec §4, operation log).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub head: Head,
    pub refs: BTreeMap<String, ObjectId>,
    /// The index as a tree, without its stat data.
    pub index: ObjectId,
    /// The stash list as a blob.
    pub stash: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    pub parent: Option<ObjectId>,
    pub time: Timestamp,
    /// The command line that made the change, on one line.
    pub command: String,
    pub view: View,
    /// Working-tree files this operation overwrote or deleted.
    pub saved: Option<ObjectId>,
    /// Set on undo and redo operations.
    pub undo_of: Option<ObjectId>,
}

impl Op {
    pub fn encode(&self) -> Vec<u8> {
        debug_assert!(!self.command.contains('\n'));
        let mut body = String::new();
        if let Some(parent) = self.parent {
            let _ = writeln!(body, "parent {parent}");
        }
        let _ = writeln!(body, "time {}", self.time);
        let _ = writeln!(body, "command {}", self.command);
        let _ = writeln!(body, "head {}", self.view.head.encode());
        for (name, id) in &self.view.refs {
            let _ = writeln!(body, "ref {name} {id}");
        }
        let _ = writeln!(body, "index {}", self.view.index);
        let _ = writeln!(body, "stash {}", self.view.stash);
        if let Some(saved) = self.saved {
            let _ = writeln!(body, "saved {saved}");
        }
        if let Some(undo_of) = self.undo_of {
            let _ = writeln!(body, "undo-of {undo_of}");
        }
        body.into_bytes()
    }

    pub fn decode(body: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(body).map_err(|_| "an op isn't valid UTF-8")?;
        let mut lines = lines(text)?.peekable();
        let hash =
            |text: &str| ObjectId::parse_hex(text).ok_or_else(|| format!("bad hash {text:?}"));

        let parent = match lines.peek().and_then(|line| line.strip_prefix("parent ")) {
            Some(parent) => {
                let parent = hash(parent)?;
                lines.next();
                Some(parent)
            }
            None => None,
        };
        let mut field = |name: &str| {
            lines
                .next()
                .and_then(|line| line.strip_prefix(name)?.strip_prefix(' '))
                .ok_or_else(|| format!("expected an `{name}` line"))
        };
        let time = Timestamp::parse(field("time")?).ok_or("bad op time")?;
        let command = field("command")?.to_owned();
        let head = Head::decode(field("head")?).ok_or("bad op head")?;

        let mut refs = BTreeMap::new();
        while let Some(rest) = lines.peek().and_then(|line| line.strip_prefix("ref ")) {
            let (name, id) = rest.rsplit_once(' ').ok_or("malformed ref line")?;
            if !is_valid_ref_name(name) {
                return Err(format!("invalid ref name {name:?}"));
            }
            if refs
                .last_key_value()
                .is_some_and(|(last, _): (&String, _)| last.as_str() >= name)
            {
                return Err(format!("refs aren't sorted and unique at {name}"));
            }
            refs.insert(name.to_owned(), hash(id)?);
            lines.next();
        }
        let mut field = |name: &str| {
            lines
                .next()
                .and_then(|line| line.strip_prefix(name)?.strip_prefix(' '))
                .ok_or_else(|| format!("expected an `{name}` line"))
        };
        let index = hash(field("index")?)?;
        let stash = hash(field("stash")?)?;

        let mut saved = None;
        let mut undo_of = None;
        for line in lines {
            if let Some(id) = line
                .strip_prefix("saved ")
                .filter(|_| saved.is_none() && undo_of.is_none())
            {
                saved = Some(hash(id)?);
            } else if let Some(id) = line.strip_prefix("undo-of ").filter(|_| undo_of.is_none()) {
                undo_of = Some(hash(id)?);
            } else {
                return Err(format!("unexpected op line {line:?}"));
            }
        }
        Ok(Self {
            parent,
            time,
            command,
            view: View {
                head,
                refs,
                index,
                stash,
            },
            saved,
            undo_of,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> ObjectId {
        ObjectId::from_bytes([byte; 32])
    }

    fn time() -> Timestamp {
        Timestamp::parse("1759737600 +0530").unwrap()
    }

    #[test]
    fn golden_ids() {
        // SHA-256 of the canonical bytes, computed independently of this code
        // (.NET's SHA256). These pin the header format across every OS.
        assert_eq!(
            id_of(ObjectKind::Blob, b"hello\n").to_string(),
            "2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4"
        );
        assert_eq!(
            id_of(ObjectKind::Blob, b"").to_string(),
            "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
        );
        assert_eq!(
            id_of(ObjectKind::Tree, b"").to_string(),
            "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
        );
    }

    #[test]
    fn header_round_trip_and_rejections() {
        let mut canonical = header(ObjectKind::Tree, 3);
        canonical.extend_from_slice(b"abc");
        assert_eq!(parse_header(&canonical), Ok((ObjectKind::Tree, 7)));
        for bad in [
            &b"tree 3abc"[..],
            b"tree 03\0abc",
            b"tree 4\0abc",
            b"twig 3\0abc",
            b"tree  3\0abc",
        ] {
            assert!(parse_header(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tree_round_trip() {
        let tree = Tree {
            entries: vec![
                TreeEntry {
                    name: "a b.txt".into(),
                    kind: EntryKind::File,
                    id: id(1),
                },
                TreeEntry {
                    name: "run.sh".into(),
                    kind: EntryKind::Exec,
                    id: id(2),
                },
                TreeEntry {
                    name: "src".into(),
                    kind: EntryKind::Tree,
                    id: id(3),
                },
            ],
        };
        let body = tree.encode();
        assert!(String::from_utf8_lossy(&body).starts_with(&format!("file {} a b.txt\n", id(1))));
        assert_eq!(Tree::decode(&body).unwrap(), tree);
        assert_eq!(Tree::decode(b"").unwrap(), Tree::default());

        let unsorted = format!("file {} b\nfile {} a\n", id(1), id(2));
        assert!(Tree::decode(unsorted.as_bytes()).is_err());
        let duplicate = format!("file {} a\nfile {} a\n", id(1), id(2));
        assert!(Tree::decode(duplicate.as_bytes()).is_err());
    }

    #[test]
    fn chunk_list_round_trip() {
        let mib = 1024 * 1024;
        let list = ChunkList {
            size: 9 * mib,
            chunks: vec![
                Chunk {
                    id: id(1),
                    len: 4 * mib,
                },
                Chunk {
                    id: id(2),
                    len: 4 * mib,
                },
                Chunk {
                    id: id(3),
                    len: mib,
                },
            ],
        };
        assert_eq!(ChunkList::decode(&list.encode()).unwrap(), list);

        // Only lists chunking could have produced are accepted.
        let encode = |size: u64, lens: &[u64]| {
            ChunkList {
                size,
                chunks: lens.iter().map(|&len| Chunk { id: id(1), len }).collect(),
            }
            .encode()
        };
        let rejected = [
            encode(9 * mib + 1, &[4 * mib, 4 * mib, mib]), // doesn't add up
            encode(30, &[10, 20]),                         // below the threshold
            encode(9 * mib, &[mib / 8, 4 * mib, 4 * mib, mib - mib / 8]), // chunk below the minimum
            encode(9 * mib, &[5 * mib, 4 * mib]),          // chunk above the maximum
            encode(8 * mib, &[]),                          // no chunks
        ];
        for body in rejected {
            assert!(
                ChunkList::decode(&body).is_err(),
                "{}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    #[test]
    fn decoders_keep_carriage_returns_and_reject_crlf_lines() {
        let tree = Tree {
            entries: vec![
                TreeEntry {
                    name: "Icon".into(),
                    kind: EntryKind::File,
                    id: id(1),
                },
                TreeEntry {
                    name: "Icon\r".into(),
                    kind: EntryKind::File,
                    id: id(2),
                },
            ],
        };
        assert_eq!(Tree::decode(&tree.encode()).unwrap(), tree);
        let crlf = format!("tree {}\r\nauthor A <a@b.c> 0 +0000\r\n\r\nmsg\r\n", id(1));
        assert!(Commit::decode(crlf.as_bytes()).is_err());
        let nfd_name = format!("file {} cafe\u{301}\n", id(1));
        assert!(Tree::decode(nfd_name.as_bytes()).is_err());
    }

    #[test]
    fn commit_format_is_exact() {
        let commit = Commit {
            tree: id(1),
            parents: vec![id(2)],
            author: Signature {
                name: "Ada Lovelace".into(),
                email: "ada@example.com".into(),
                time: time(),
            },
            message: "Add login form\n\nWith validation.\n".into(),
        };
        let expected = format!(
            "tree {}\nparent {}\nauthor Ada Lovelace <ada@example.com> 1759737600 +0530\n\nAdd login form\n\nWith validation.\n",
            id(1),
            id(2)
        );
        assert_eq!(String::from_utf8(commit.encode()).unwrap(), expected);
        assert_eq!(Commit::decode(expected.as_bytes()).unwrap(), commit);
        assert_eq!(commit.subject(), "Add login form");
    }

    #[test]
    fn op_round_trip() {
        let op = Op {
            parent: Some(id(9)),
            time: time(),
            command: "nexus commit -m \"first commit\"".into(),
            view: View {
                head: Head::Branch("refs/heads/main".into()),
                refs: BTreeMap::from([
                    ("refs/heads/main".into(), id(1)),
                    ("refs/tags/v1".into(), id(2)),
                ]),
                index: id(3),
                stash: id(4),
            },
            saved: Some(id(5)),
            undo_of: None,
        };
        let text = String::from_utf8(op.encode()).unwrap();
        assert!(text.starts_with(&format!(
            "parent {}\ntime 1759737600 +0530\ncommand nexus commit",
            id(9)
        )));
        assert!(text.contains("head ref: refs/heads/main\n"));
        assert_eq!(Op::decode(text.as_bytes()).unwrap(), op);

        let first = Op {
            parent: None,
            saved: None,
            view: View {
                refs: BTreeMap::new(),
                ..op.view.clone()
            },
            ..op.clone()
        };
        assert_eq!(Op::decode(&first.encode()).unwrap(), first);
    }
}
