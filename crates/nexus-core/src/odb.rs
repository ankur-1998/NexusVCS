//! The object store (spec §4). Each object lives at `objects/<2>/<62>` as one
//! encoding byte followed by its canonical bytes: zstd-compressed, or raw when
//! compression would save under 5%.

use std::fs;
use std::io::{self, BufRead as _, BufReader, Read, Write as _};
use std::path::{Path, PathBuf};

use crate::error::{Error, IoResultExt as _, Result};
use crate::fsutil::{self, Existing};
use crate::hash::{Hasher, ObjectId};
use crate::object::{self, ObjectKind};

const RAW: u8 = 0x00;
const ZSTD: u8 = 0x01;
const ZSTD_LEVEL: i32 = 3;

#[derive(Clone, Debug)]
pub struct ObjectStore {
    /// The `objects` directory.
    dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    pub kind: ObjectKind,
    pub body: Vec<u8>,
}

impl ObjectStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// Where the object with this ID is (or would be) stored.
    pub fn path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_string();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    pub fn contains(&self, id: &ObjectId) -> bool {
        self.path(id).is_file()
    }

    /// Every stored object whose hex ID starts with `prefix` (lowercase hex,
    /// at least 2 characters). Only the one fan-out directory is listed.
    pub fn find_by_prefix(&self, prefix: &str) -> Result<Vec<ObjectId>> {
        let Some((fanout, rest)) = prefix.split_at_checked(2) else {
            return Ok(Vec::new());
        };
        let dir = self.dir.join(fanout);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).at(&dir),
        };
        let mut found = Vec::new();
        for entry in entries {
            let name = entry.at(&dir)?.file_name();
            if let Some(name) = name.to_str()
                && name.starts_with(rest)
                && let Some(id) = ObjectId::parse_hex(&format!("{fanout}{name}"))
            {
                found.push(id);
            }
        }
        found.sort();
        Ok(found)
    }

    /// Stores an object and returns its ID. Objects are immutable, so one that
    /// already exists isn't written again.
    pub fn write(&self, kind: ObjectKind, body: &[u8]) -> Result<ObjectId> {
        let header = object::header(kind, body.len());
        let mut hasher = Hasher::default();
        hasher.update(&header);
        hasher.update(body);
        let id = hasher.finish();
        let path = self.path(&id);
        if path.is_file() {
            return Ok(id);
        }

        let compressed = compress(&header, body).at(&path)?;
        let raw_len = header.len() + body.len();
        let store_raw = compressed.len() * 100 > raw_len * 95;
        let dir = path.parent().expect("object paths have a parent");
        fs::create_dir_all(dir).at(dir)?;
        fsutil::write_atomic(&path, Existing::Keep, |file| {
            if store_raw {
                file.write_all(&[RAW])?;
                file.write_all(&header)?;
                file.write_all(body)
            } else {
                file.write_all(&[ZSTD])?;
                file.write_all(&compressed)
            }
        })?;
        Ok(id)
    }

    /// Reads an object, verifying that its content still matches its ID.
    ///
    /// The header is decoded first and the body's declared size checked
    /// against a limit for its type, so a damaged or hostile file can't make
    /// decompression allocate gigabytes before the hash check rejects it.
    pub fn read(&self, id: &ObjectId) -> Result<Object> {
        let path = self.path(id);
        let stored = match fs::read(&path) {
            Ok(stored) => stored,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(Error::MissingObject(*id));
            }
            Err(err) => return Err(err).at(&path),
        };
        let corrupt = |reason: String| Error::CorruptObject { id: *id, reason };
        if stored.first() == Some(&RAW) {
            // Already the canonical bytes: check them in place, then drop the
            // encoding byte and header with one move instead of copying.
            let mut hasher = Hasher::default();
            hasher.update(&stored[1..]);
            if hasher.finish() != *id {
                return Err(corrupt("its content doesn't match its hash".to_owned()));
            }
            let (kind, header_len) = object::parse_header(&stored[1..]).map_err(corrupt)?;
            let mut body = stored;
            body.drain(..=header_len);
            return Ok(Object { kind, body });
        }
        let (canonical, header_len, kind) = match stored.split_first() {
            Some((&ZSTD, rest)) => {
                let decoder = zstd::stream::read::Decoder::with_buffer(rest)
                    .map_err(|err| corrupt(format!("can't decompress it: {err}")))?;
                decode(decoder).map_err(corrupt)?
            }
            Some((other, _)) => return Err(corrupt(format!("unknown encoding byte {other:#04x}"))),
            None => return Err(corrupt("the file is empty".to_owned())),
        };
        let mut hasher = Hasher::default();
        hasher.update(&canonical);
        if hasher.finish() != *id {
            return Err(corrupt("its content doesn't match its hash".to_owned()));
        }
        let mut body = canonical;
        body.drain(..header_len);
        Ok(Object { kind, body })
    }

    /// Reads an object that must be of `kind`.
    pub fn read_kind(&self, id: &ObjectId, kind: ObjectKind) -> Result<Vec<u8>> {
        let object = self.read(id)?;
        if object.kind == kind {
            Ok(object.body)
        } else {
            Err(Error::Invalid(format!(
                "{id} is a {} object, not a {kind}",
                object.kind
            )))
        }
    }
}

/// The largest body each type of object can legitimately have. Blobs are
/// smaller than the chunking threshold (chunks are smaller still); trees,
/// chunk lists, commits, and ops are text and stay far below the generous cap.
fn max_body_len(kind: ObjectKind) -> usize {
    const MAX_TEXT_OBJECT: usize = 256 * 1024 * 1024;
    match kind {
        ObjectKind::Blob => usize::try_from(object::CHUNKED_THRESHOLD).unwrap_or(usize::MAX),
        _ => MAX_TEXT_OBJECT,
    }
}

/// Reads canonical bytes from `reader`: the header first, then exactly the
/// declared number of body bytes, failing on anything over the type's limit
/// or anything after the body. Returns the bytes, the header's length, and the
/// type.
fn decode(reader: impl Read) -> Result<(Vec<u8>, usize, ObjectKind), String> {
    let mut reader = BufReader::new(reader);
    let mut canonical = Vec::new();
    // `take` bounds the header search, so a file of non-NUL bytes can't make
    // this read forever.
    (&mut reader)
        .take(object::MAX_HEADER_LEN as u64 + 1)
        .read_until(0, &mut canonical)
        .map_err(|err| format!("can't decompress it: {err}"))?;
    if canonical.last() != Some(&0) {
        return Err("the header has no terminating NUL".to_owned());
    }
    let header_len = canonical.len();
    let (kind, body_len) = object::parse_header_text(&canonical[..header_len - 1])?;
    if body_len > max_body_len(kind) {
        return Err(format!(
            "its header declares an implausible {body_len}-byte {kind}"
        ));
    }
    canonical.reserve_exact(body_len);
    let read = (&mut reader)
        .take(body_len as u64)
        .read_to_end(&mut canonical)
        .map_err(|err| format!("can't decompress it: {err}"))?;
    if read != body_len {
        return Err(format!(
            "the header declares {body_len} bytes but the body has {read}"
        ));
    }
    let mut extra = [0; 1];
    let trailing = reader
        .read(&mut extra)
        .map_err(|err| format!("can't decompress it: {err}"))?;
    if trailing != 0 {
        return Err(format!(
            "the body is longer than the {body_len} bytes its header declares"
        ));
    }
    Ok((canonical, header_len, kind))
}

fn compress(header: &[u8], body: &[u8]) -> io::Result<Vec<u8>> {
    let total = header.len() + body.len();
    let mut encoder = zstd::stream::Encoder::new(Vec::with_capacity(total / 2 + 64), ZSTD_LEVEL)?;
    encoder.include_contentsize(true)?;
    encoder.set_pledged_src_size(Some(total as u64))?;
    encoder.write_all(header)?;
    encoder.write_all(body)?;
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn write_then_read() {
        let (_dir, store) = store();
        let body = b"hello\n".repeat(100);
        let id = store.write(ObjectKind::Blob, &body).unwrap();
        assert_eq!(id, object::id_of(ObjectKind::Blob, &body));
        assert_eq!(
            store.read(&id).unwrap(),
            Object {
                kind: ObjectKind::Blob,
                body: body.clone()
            }
        );
        // Repetitive text compresses, so it's stored with zstd.
        assert_eq!(fs::read(store.path(&id)).unwrap()[0], ZSTD);
        // Writing it again is a no-op.
        assert_eq!(store.write(ObjectKind::Blob, &body).unwrap(), id);
    }

    #[test]
    fn tiny_objects_are_stored_raw() {
        let (_dir, store) = store();
        let id = store.write(ObjectKind::Blob, b"x").unwrap();
        let stored = fs::read(store.path(&id)).unwrap();
        assert_eq!(stored, b"\x00blob 1\0x");
    }

    #[test]
    fn detects_corruption_and_missing_objects() {
        let (_dir, store) = store();
        let id = store
            .write(ObjectKind::Blob, b"some content that is long enough")
            .unwrap();
        let path = store.path(&id);
        let mut stored = fs::read(&path).unwrap();
        let last = stored.len() - 1;
        stored[last] ^= 0xff;
        fs::write(&path, &stored).unwrap();
        assert!(matches!(store.read(&id), Err(Error::CorruptObject { .. })));

        let absent = ObjectId::from_bytes([0; 32]);
        assert!(matches!(store.read(&absent), Err(Error::MissingObject(_))));
    }

    /// Plants an object file with arbitrary stored bytes under some ID.
    fn plant(store: &ObjectStore, stored: &[u8]) -> ObjectId {
        let id = ObjectId::from_bytes([0xab; 32]);
        let path = store.path(&id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, stored).unwrap();
        id
    }

    #[test]
    fn refuses_decompression_bombs_before_allocating() {
        let (_dir, store) = store();
        // A tiny zstd frame that expands to gigabytes of zeros, claiming to be
        // a blob far over the limit.
        let mut payload = b"blob 4000000000\0".to_vec();
        payload.resize(payload.len() + 64 * 1024 * 1024, 0);
        let mut stored = vec![ZSTD];
        stored.extend(zstd::bulk::compress(&payload, 3).unwrap());
        assert!(stored.len() < 100_000);
        let id = plant(&store, &stored);
        let err = store.read(&id).unwrap_err().to_string();
        assert!(err.contains("implausible"), "{err}");
    }

    #[test]
    fn rejects_bodies_that_dont_match_their_header() {
        let (_dir, store) = store();
        for canonical in [&b"blob 3\0abcd"[..], b"blob 5\0abc", b"no header at all"] {
            let mut stored = vec![ZSTD];
            stored.extend(zstd::bulk::compress(canonical, 3).unwrap());
            let id = plant(&store, &stored);
            assert!(
                matches!(store.read(&id), Err(Error::CorruptObject { .. })),
                "{canonical:?}"
            );
        }
    }
}
