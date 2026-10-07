//! Atomic file writes (spec §4 invariants): write a temp file in the target's
//! directory, then rename it into place, so readers see the old file or the new
//! one and never a partial write.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, IoResultExt as _, Result};
use crate::platform;

/// Temp files start with this, so directory listings can skip them.
pub const TEMP_PREFIX: &str = ".tmp-";

/// What to do when the target already exists.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    Replace,
    /// Leave it alone and succeed. Objects are immutable, so an existing file
    /// already has exactly the content being written.
    Keep,
}

/// How many times a rename blocked by a transient Windows lock is retried.
/// The delays double from 1 ms, about 1 s in total.
const RENAME_RETRIES: u32 = 10;

/// Makes temp file names unique within this process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically creates or replaces `path` with whatever `write` produces.
///
/// The temp file is created and renamed with `std::fs`, which gives Windows
/// paths the `\\?\` prefix they need beyond 260 characters. (`tempfile`'s
/// rename doesn't, so a repository in a deep directory couldn't store objects.)
pub fn write_atomic(
    path: &Path,
    existing: Existing,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Invalid(format!("{} has no parent directory", path.display())))?;
    let (temp, mut file) = create_temp(dir)?;
    let written = write(&mut file);
    drop(file);
    if let Err(err) = written {
        let _ = fs::remove_file(&temp);
        return Err(err).at(path);
    }

    let mut delay = Duration::from_millis(1);
    let mut attempt = 0;
    let mut cleared_read_only = false;
    loop {
        // An object that already exists has exactly this content, so for
        // `Keep` replacing it is harmless, and finding it is success.
        if existing == Existing::Keep && path.exists() {
            let _ = fs::remove_file(&temp);
            return Ok(());
        }
        match fs::rename(&temp, path) {
            Ok(()) => return Ok(()),
            // Windows won't replace a read-only file; clear the attribute, as
            // Git for Windows does.
            Err(_) if !cleared_read_only && platform::clear_read_only(path) => {
                cleared_read_only = true;
            }
            Err(err) if attempt < RENAME_RETRIES && platform::is_transient_lock(&err) => {
                thread::sleep(delay);
                delay *= 2;
                attempt += 1;
            }
            Err(err) => {
                let _ = fs::remove_file(&temp);
                return Err(err).at(path);
            }
        }
    }
}

/// Creates a new, uniquely named temp file in `dir`.
fn create_temp(dir: &Path) -> Result<(PathBuf, File)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    loop {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!("{TEMP_PREFIX}{}-{nanos:x}-{n}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err).at(dir),
        }
    }
}

/// Atomically creates or replaces `path` with `contents`.
pub fn write_file(path: &Path, contents: &[u8]) -> Result<()> {
    write_atomic(path, Existing::Replace, |file| file.write_all(contents))
}

/// Reads a file, treating "not found" as `None`.
pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).at(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_or_keeps_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        write_file(&path, b"one").unwrap();
        write_file(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");

        write_atomic(&path, Existing::Keep, |file| file.write_all(b"three")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["file"]);
    }
}
