//! A file's content as objects (spec §4): one blob below 8 MiB, otherwise a
//! `chunked` list of content-defined chunks, each stored as a blob. Large
//! files are streamed, so memory use stays flat whatever their size.

use std::fs::{File, Metadata};
use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use fastcdc::v2020::StreamCDC;
use rayon::prelude::*;

use crate::error::{Error, IoResultExt as _, Result};
use crate::hash::ObjectId;
use crate::object::{self, Chunk, ChunkList, ObjectKind};
pub use crate::object::{CHUNK_AVG, CHUNK_MAX, CHUNK_MIN, CHUNKED_THRESHOLD};
use crate::odb::ObjectStore;
use crate::platform;

/// How many bytes of chunks are hashed and compressed in parallel before the
/// next ones are read. This is what bounds memory while adding a huge file.
const BATCH_BYTES: usize = 16 * 1024 * 1024;

/// Where objects go: into the store, or nowhere (to compute IDs only, as
/// `nexus hash-object` does).
#[derive(Clone, Copy)]
pub enum Sink<'a> {
    Store(&'a ObjectStore),
    HashOnly,
}

impl Sink<'_> {
    pub fn put(self, kind: ObjectKind, body: &[u8]) -> Result<ObjectId> {
        match self {
            Self::Store(store) => store.write(kind, body),
            Self::HashOnly => Ok(object::id_of(kind, body)),
        }
    }
}

/// What reading a file produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredFile {
    pub id: ObjectId,
    pub size: u64,
    /// The modification time the file had while it was read.
    pub mtime_ns: Option<i64>,
}

/// Reads the file at `path` into objects. If the file changes while it's being
/// read, it's read once more; if it changes again, that's an error.
pub fn store_file(sink: Sink, path: &Path) -> Result<StoredFile> {
    Ok(store_file_limited(sink, path, true)?.expect("chunking is allowed"))
}

/// [`store_file`], except that with `allow_chunked` false a file that has
/// reached the chunking threshold is left alone (`None`): chunking spreads
/// work over the thread pool, so it mustn't start on a pool thread.
fn store_file_limited(sink: Sink, path: &Path, allow_chunked: bool) -> Result<Option<StoredFile>> {
    for _ in 0..2 {
        // One open per attempt: metadata comes from the open handle, before
        // and after reading, so a change while reading is noticed.
        let mut file = File::open(path).at(path)?;
        let before = file.metadata().at(path)?;
        if before.len() >= CHUNKED_THRESHOLD && !allow_chunked {
            return Ok(None);
        }
        let (id, size) = if before.len() < CHUNKED_THRESHOLD {
            let mut data = Vec::with_capacity(usize::try_from(before.len()).unwrap_or(0));
            file.read_to_end(&mut data).at(path)?;
            (sink.put(ObjectKind::Blob, &data)?, data.len() as u64)
        } else {
            store_chunked(sink, &file, path)?
        };
        let after = file.metadata().at(path)?;
        if size == after.len() && unchanged(&before, &after) {
            return Ok(Some(StoredFile {
                id,
                size,
                mtime_ns: platform::mtime_ns(&after),
            }));
        }
    }
    Err(Error::Invalid(format!(
        "{} kept changing while it was being read; try again once it's finished being written",
        path.display()
    )))
}

/// How many bytes of small files may be held in memory at once while they're
/// read in parallel. Without a limit, 16 threads each reading a file just
/// under the 8 MiB chunking threshold would hold well over 100 MB.
const SMALL_FILE_BUDGET: u64 = 16 * 1024 * 1024;

/// Stores (or, with [`Sink::HashOnly`], hashes) many files, each given as a
/// tag to return with it, its path, and its expected size, and returns each
/// file's result (not in the order given). Small files are read in parallel
/// within a memory budget. Large files go one at a time afterwards, on the
/// calling thread: each already spreads its chunks over the thread pool,
/// which must not be busy waiting on the budget then. So do small files that
/// grew past the threshold before they were read.
pub fn store_files<T: Send>(
    sink: Sink,
    files: Vec<(T, PathBuf, u64)>,
) -> Vec<(T, Result<StoredFile>)> {
    let (mut large, small): (Vec<_>, Vec<_>) = files
        .into_iter()
        .partition(|(_, _, size)| *size >= CHUNKED_THRESHOLD);
    let budget = ByteBudget {
        available: Mutex::new(SMALL_FILE_BUDGET),
        returned: Condvar::new(),
    };
    let read: Vec<(T, PathBuf, u64, Option<Result<StoredFile>>)> = small
        .into_par_iter()
        .map(|(tag, path, size)| {
            let _bytes = budget.borrow(size);
            let result = store_file_limited(sink, &path, false).transpose();
            (tag, path, size, result)
        })
        .collect();
    let mut stored = Vec::with_capacity(read.len() + large.len());
    for (tag, path, size, result) in read {
        match result {
            Some(result) => stored.push((tag, result)),
            None => large.push((tag, path, size)),
        }
    }
    for (tag, path, _) in large {
        let result = store_file(sink, &path);
        stored.push((tag, result));
    }
    stored
}

/// A count of bytes that threads borrow before reading a file and return
/// afterwards. Holders never wait for anything, so waiting can't deadlock.
struct ByteBudget {
    available: Mutex<u64>,
    returned: Condvar,
}

impl ByteBudget {
    fn borrow(&self, bytes: u64) -> BorrowedBytes<'_> {
        let bytes = bytes.min(SMALL_FILE_BUDGET);
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *available < bytes {
            available = self
                .returned
                .wait(available)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *available -= bytes;
        BorrowedBytes {
            budget: self,
            bytes,
        }
    }
}

struct BorrowedBytes<'a> {
    budget: &'a ByteBudget,
    bytes: u64,
}

impl Drop for BorrowedBytes<'_> {
    fn drop(&mut self) {
        let mut available = self
            .budget
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available += self.bytes;
        self.budget.returned.notify_all();
    }
}

fn unchanged(before: &Metadata, after: &Metadata) -> bool {
    before.len() == after.len() && before.modified().ok() == after.modified().ok()
}

/// Chunks a large file and stores the chunks. Finding chunk boundaries (on
/// this thread) overlaps with hashing and compressing the previous batch (on
/// a second one, which spreads each batch across the thread pool). The
/// rendezvous channel keeps at most two batches in memory.
fn store_chunked(sink: Sink, file: &File, path: &Path) -> Result<(ObjectId, u64)> {
    let list = std::thread::scope(|scope| -> Result<ChunkList> {
        let (batches, received) = std::sync::mpsc::sync_channel::<Vec<Vec<u8>>>(0);
        let storer = scope.spawn(move || -> Result<ChunkList> {
            let mut list = ChunkList::default();
            for mut batch in received {
                store_batch(sink, &mut batch, &mut list)?;
            }
            Ok(list)
        });

        let mut read_error = None;
        let mut batch = Vec::new();
        let mut batch_bytes = 0;
        for chunk in StreamCDC::new(file, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX) {
            match chunk {
                Ok(chunk) => {
                    batch_bytes += chunk.data.len();
                    batch.push(chunk.data);
                    if batch_bytes >= BATCH_BYTES {
                        batch_bytes = 0;
                        // A send only fails if the storer stopped on an error,
                        // which `join` reports below.
                        if batches.send(std::mem::take(&mut batch)).is_err() {
                            break;
                        }
                    }
                }
                Err(err) => {
                    read_error = Some(std::io::Error::from(err));
                    break;
                }
            }
        }
        if read_error.is_none() && !batch.is_empty() {
            let _ = batches.send(batch);
        }
        drop(batches);
        let list = match storer.join() {
            Ok(result) => result?,
            Err(panic) => std::panic::resume_unwind(panic),
        };
        match read_error {
            Some(err) => Err(err).at(path),
            None => Ok(list),
        }
    })?;
    let id = sink.put(ObjectKind::Chunked, &list.encode())?;
    Ok((id, list.size))
}

fn store_batch(sink: Sink, batch: &mut Vec<Vec<u8>>, list: &mut ChunkList) -> Result<()> {
    let ids = batch
        .par_iter()
        .map(|data| sink.put(ObjectKind::Blob, data))
        .collect::<Result<Vec<_>>>()?;
    for (data, id) in batch.drain(..).zip(ids) {
        let len = data.len() as u64;
        list.size += len;
        list.chunks.push(Chunk { id, len });
    }
    Ok(())
}

/// The chunks a file on disk would be split into, without storing anything.
/// Used to summarize how a large file changed.
pub fn chunk_list_of_file(path: &Path) -> Result<ChunkList> {
    let file = File::open(path).at(path)?;
    let mut list = ChunkList::default();
    for chunk in StreamCDC::new(file, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX) {
        let chunk = chunk.map_err(std::io::Error::from).at(path)?;
        let len = chunk.data.len() as u64;
        list.size += len;
        list.chunks.push(Chunk {
            id: object::id_of(ObjectKind::Blob, &chunk.data),
            len,
        });
    }
    Ok(list)
}

/// Writes the content of a `blob` or `chunked` object to `out`, checking each
/// chunk's length. Returns how many bytes were written. `out_path` names the
/// destination in error messages.
pub fn write_content(
    store: &ObjectStore,
    id: &ObjectId,
    out: &mut impl Write,
    out_path: &Path,
) -> Result<u64> {
    let object = store.read(id)?;
    match object.kind {
        ObjectKind::Blob => {
            out.write_all(&object.body).at(out_path)?;
            Ok(object.body.len() as u64)
        }
        ObjectKind::Chunked => {
            let list = ChunkList::decode(&object.body)
                .map_err(|reason| Error::CorruptObject { id: *id, reason })?;
            for chunk in &list.chunks {
                let data = store.read_kind(&chunk.id, ObjectKind::Blob)?;
                if data.len() as u64 != chunk.len {
                    return Err(Error::CorruptObject {
                        id: *id,
                        reason: format!("chunk {} has the wrong length", chunk.id),
                    });
                }
                out.write_all(&data).at(out_path)?;
            }
            Ok(list.size)
        }
        other => Err(Error::Invalid(format!(
            "{id} is a {other} object, not file content"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Deterministic pseudo-random bytes (xorshift64*), so tests don't need a
    /// random-number crate and every run sees the same data.
    pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed.max(1);
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            out.extend_from_slice(&state.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// The chunk boundaries of a fixed 20 MiB input are part of the on-disk
    /// format: if this test fails, large files would get different IDs.
    #[test]
    fn golden_chunk_boundaries() {
        let data = pseudo_random(20 * 1024 * 1024, 0x4e45_5855_5356_4353);
        let boundaries: Vec<(u64, usize)> =
            StreamCDC::new(&data[..], CHUNK_MIN, CHUNK_AVG, CHUNK_MAX)
                .map(|chunk| {
                    let chunk = chunk.unwrap();
                    (chunk.offset, chunk.length)
                })
                .collect();
        assert_eq!(boundaries, GOLDEN_BOUNDARIES);
    }

    /// `(offset, length)` of each chunk, recorded from fastcdc 5.0.0.
    const GOLDEN_BOUNDARIES: &[(u64, usize)] = &[
        (0, 1_625_690),
        (1_625_690, 1_971_462),
        (3_597_152, 1_179_783),
        (4_776_935, 953_890),
        (5_730_825, 1_153_565),
        (6_884_390, 1_175_354),
        (8_059_744, 816_019),
        (8_875_763, 609_329),
        (9_485_092, 3_184_777),
        (12_669_869, 1_587_909),
        (14_257_778, 1_407_224),
        (15_665_002, 1_391_428),
        (17_056_430, 1_668_410),
        (18_724_840, 1_689_051),
        (20_413_891, 557_629),
    ];

    #[test]
    fn small_and_large_files_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(&dir.path().join("objects"));
        for (name, len) in [("empty", 0), ("small", 1000), ("chunked", 9 * 1024 * 1024)] {
            let data = pseudo_random(len, 7);
            let path = dir.path().join(name);
            fs::write(&path, &data).unwrap();
            let stored = store_file(Sink::Store(&store), &path).unwrap();
            assert_eq!(stored.size, len as u64);
            assert_eq!(store_file(Sink::HashOnly, &path).unwrap().id, stored.id);

            let expected_kind = if len as u64 >= CHUNKED_THRESHOLD {
                ObjectKind::Chunked
            } else {
                ObjectKind::Blob
            };
            assert_eq!(store.read(&stored.id).unwrap().kind, expected_kind);

            let mut out = Vec::new();
            write_content(&store, &stored.id, &mut out, Path::new("out")).unwrap();
            assert!(out == data, "{name} didn't round-trip");
        }
    }
}
