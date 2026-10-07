//! The operation log (spec §4) and the transactions that write it (spec §5.2).
//!
//! Every change to refs, `HEAD`, the index, the stash, or working-tree files
//! goes through a [`Transaction`]. It holds `.nexus/lock`, applies the changes
//! with atomic writes, and records one `op` object describing the state they
//! leave behind. `OPLOG` points at the newest op and is written last, so after
//! a crash the refs and `OPLOG` disagree, and the next transaction notices and
//! records a `recovered` op before doing anything else.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::content::Sink;
use crate::error::{Error, IoResultExt as _, Result};
use crate::fsutil;
use crate::hash::ObjectId;
use crate::index::{Index, IndexEntry};
use crate::object::{ObjectKind, Op, View};
use crate::path::RepoPath;
use crate::refs::{Head, strip_line_end};
use crate::repo::Repo;
use crate::time::Timestamp;
use crate::tree;
use crate::worktree::Version;

/// The command recorded when the state changed outside nexus.
pub const RECOVERED: &str = "recovered";

/// Lock files this process holds, so an interrupt handler can remove them
/// before the process exits (see [`remove_held_locks`]).
static HELD_LOCKS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn held_locks() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    HELD_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Removes every lock file this process holds. For a Ctrl+C or termination
/// handler to call just before exiting, since exiting that way skips the
/// destructors that normally release locks. Whatever the interrupted command
/// had written stays consistent: every write is atomic, and the next command
/// records a `recovered` op if the refs and the op log disagree.
pub fn remove_held_locks() {
    for path in held_locks().iter() {
        let _ = fs::remove_file(path);
    }
}

/// Exclusive access to a repository for one mutating command, released when
/// dropped.
///
/// On Windows the lock file is opened delete-on-close, so the system removes
/// it even if the process is killed, and no other program can hold it open
/// in a way that blocks its removal.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    _file: File,
}

impl Lock {
    pub fn acquire(path: &Path, command: &str) -> Result<Self> {
        match open_new(path) {
            Ok(mut file) => {
                // Only for the "who holds it" message; failing to write it is harmless.
                let _ = writeln!(file, "pid {}\ncommand {command}", std::process::id());
                let _ = file.flush();
                held_locks().push(path.to_path_buf());
                Ok(Self {
                    path: path.to_path_buf(),
                    _file: file,
                })
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let holder = fs::read_to_string(path)
                    .ok()
                    .map(|text| text.lines().collect::<Vec<_>>().join(", "))
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| "unknown process".to_owned());
                Err(Error::Locked {
                    path: path.to_path_buf(),
                    holder,
                })
            }
            Err(err) => Err(err).at(path),
        }
    }
}

#[cfg(windows)]
fn open_new(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4; // read, write, delete
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(path)
}

#[cfg(not(windows))]
fn open_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

impl Drop for Lock {
    fn drop(&mut self) {
        held_locks().retain(|held| *held != self.path);
        // On Windows, closing the file (right after this) deletes it.
        if cfg!(not(windows)) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// The newest op and its ID, or `None` before the first op is recorded.
pub fn latest(repo: &Repo) -> Result<Option<(ObjectId, Op)>> {
    let path = repo.oplog_path();
    let Some(bytes) = fsutil::read_optional(&path)? else {
        return Ok(None);
    };
    let id = std::str::from_utf8(&bytes)
        .ok()
        .and_then(strip_line_end)
        .and_then(ObjectId::parse_hex)
        .ok_or_else(|| Error::Corrupt {
            path,
            reason: "OPLOG must hold one op hash".to_owned(),
        })?;
    Ok(Some((id, read_op(repo, &id)?)))
}

pub fn read_op(repo: &Repo, id: &ObjectId) -> Result<Op> {
    let body = repo.odb().read_kind(id, ObjectKind::Op)?;
    Op::decode(&body).map_err(|reason| Error::CorruptObject { id: *id, reason })
}

/// The state an op records: `HEAD`, every ref, the index as a tree, and the
/// stash list as a blob. With [`Sink::HashOnly`] no objects are written.
pub fn current_view(repo: &Repo, index: &Index, sink: Sink) -> Result<View> {
    let stash = fsutil::read_optional(&repo.stash_path())?.unwrap_or_default();
    Ok(View {
        head: repo.refs().head()?,
        refs: repo.refs().list()?,
        index: tree::write_index_tree(index, sink)?,
        stash: sink.put(ObjectKind::Blob, &stash)?,
    })
}

fn record(repo: &Repo, op: &Op) -> Result<ObjectId> {
    let id = repo.odb().write(ObjectKind::Op, &op.encode())?;
    fsutil::write_file(&repo.oplog_path(), format!("{id}\n").as_bytes())?;
    Ok(id)
}

/// A set of changes to apply under the repository lock, recorded as one op.
pub struct Transaction<'r> {
    repo: &'r Repo,
    _lock: Lock,
    command: String,
    time: Timestamp,
    /// The newest op when the transaction began (after any recovery).
    latest: Option<ObjectId>,
    /// The state that op records, whose objects are all stored. `None` only
    /// before a repository's first op.
    view: Option<View>,
    /// The index as loaded under the lock.
    base_index: Index,
    /// The index file's modification time when it was loaded.
    base_index_mtime: Option<i64>,
    /// Its replacement, from [`Self::set_index`].
    new_index: Option<Index>,
    /// Ref updates in order: `Some` sets the ref, `None` deletes it.
    refs: Vec<(String, Option<ObjectId>)>,
    head: Option<Head>,
    /// The tree of working files this transaction overwrites or deletes.
    saved: Option<ObjectId>,
    /// Whether working files are written or deleted, which is a change to
    /// record even when nothing else changes.
    worktree_changed: bool,
}

impl<'r> Transaction<'r> {
    /// Takes the lock and loads the index. If the repository's state no longer
    /// matches the newest op, records a `recovered` op first.
    pub fn begin(repo: &'r Repo, command: impl Into<String>, time: Timestamp) -> Result<Self> {
        let command = command.into();
        let lock = Lock::acquire(&repo.lock_path(), &command)?;
        let base_index_mtime = fs::metadata(repo.index_path())
            .ok()
            .and_then(|meta| crate::platform::mtime_ns(&meta));
        let index = repo.load_index()?;
        let (latest, view) = match latest(repo)? {
            None => (None, None),
            Some((id, op)) => {
                let view = current_view(repo, &index, Sink::HashOnly)?;
                if view == op.view {
                    (Some(id), Some(view))
                } else {
                    // A crash interrupted a transaction, or something edited
                    // `.nexus/` by hand. Record what's there now, writing any
                    // objects the view needs.
                    let view = current_view(repo, &index, Sink::Store(repo.odb()))?;
                    let recovered = Op {
                        parent: Some(id),
                        time,
                        command: RECOVERED.to_owned(),
                        view: view.clone(),
                        saved: None,
                        undo_of: None,
                    };
                    (Some(record(repo, &recovered)?), Some(view))
                }
            }
        };
        Ok(Self {
            repo,
            _lock: lock,
            command,
            time,
            latest,
            view,
            base_index: index,
            base_index_mtime,
            new_index: None,
            refs: Vec::new(),
            head: None,
            saved: None,
            worktree_changed: false,
        })
    }

    /// The index: as read under the lock, or as replaced by [`Self::set_index`].
    pub fn index(&self) -> &Index {
        self.new_index.as_ref().unwrap_or(&self.base_index)
    }

    pub fn set_index(&mut self, index: Index) {
        self.new_index = Some(index);
    }

    /// The ID of the tree the current index describes, with its tree objects
    /// stored. Free when the index is the one the newest op already recorded;
    /// otherwise only the trees that op's index didn't have are written.
    pub fn index_tree(&self) -> Result<ObjectId> {
        let sink = Sink::Store(self.repo.odb());
        match (&self.view, &self.new_index) {
            (Some(view), None) => Ok(view.index),
            (Some(_), Some(index)) => {
                let known = tree::tree_ids(&self.base_index)?;
                tree::write_index_tree_skipping(index, sink, &known)
            }
            (None, _) => tree::write_index_tree(self.index(), sink),
        }
    }

    pub fn set_ref(&mut self, name: &str, id: ObjectId) {
        self.refs.push((name.to_owned(), Some(id)));
    }

    pub fn delete_ref(&mut self, name: &str) {
        self.refs.push((name.to_owned(), None));
    }

    /// Records the working files this transaction is about to overwrite or
    /// delete, as the op's `saved` tree (spec §5.2), so nothing is lost even
    /// when the change is deliberate. Their content must already be stored.
    /// Call it before touching the files.
    pub fn save_working_files(&mut self, files: &[(RepoPath, Version)]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let mut saved = Index::default();
        for (path, version) in files {
            saved.insert(
                path.clone(),
                IndexEntry {
                    kind: version.kind,
                    id: version.id,
                    size: 0,
                    mtime_ns: 0,
                },
            );
        }
        self.saved = Some(tree::write_index_tree(
            &saved,
            Sink::Store(self.repo.odb()),
        )?);
        Ok(())
    }

    pub fn set_head(&mut self, head: Head) {
        self.head = Some(head);
    }

    /// Notes that working files are about to be written or deleted. Call it
    /// before touching them.
    pub fn changing_worktree(&mut self) {
        self.worktree_changed = true;
    }

    /// Ends a transaction whose working-tree changes failed partway: it
    /// records an op for the state as it was, plus `saved`, so the content
    /// of files already overwritten stays reachable (for undo, and from gc),
    /// then returns `err` with a note saying so.
    pub fn abandon(self, err: Error) -> Error {
        if self.saved.is_none() && !self.worktree_changed {
            return err;
        }
        let saved = self.saved.is_some();
        match self.finish() {
            Ok(_) if saved => Error::Invalid(format!(
                "{err}\nSome files had already changed. Their previous content is saved in the op \
                 log, and `nexus status` shows what differs."
            )),
            Ok(_) => Error::Invalid(format!(
                "{err}\nSome files had already changed; `nexus status` shows what differs."
            )),
            Err(_) => err,
        }
    }

    /// Applies the changes, then records an op unless the state is exactly
    /// what the newest op already describes. Returns the new op's ID.
    pub fn finish(mut self) -> Result<Option<ObjectId>> {
        let repo = self.repo;
        if let Some(index) = &mut self.new_index {
            smudge_carried_over(index, &self.base_index, self.base_index_mtime);
            index.save(&repo.index_path())?;
        }
        for (name, id) in &self.refs {
            match id {
                Some(id) => repo.refs().set(name, *id)?,
                None => repo.refs().delete(name)?,
            }
        }
        if let Some(head) = &self.head {
            repo.refs().set_head(head)?;
        }

        let index = self.index_tree()?;
        let stash = if let Some(view) = &self.view {
            view.stash
        } else {
            let stash = fsutil::read_optional(&repo.stash_path())?.unwrap_or_default();
            repo.odb().write(ObjectKind::Blob, &stash)?
        };
        let view = View {
            head: repo.refs().head()?,
            refs: repo.refs().list()?,
            index,
            stash,
        };
        // Changed working files count as a change even when the view is the
        // same (`nexus restore` changes nothing else).
        if self.view.as_ref() == Some(&view) && self.saved.is_none() && !self.worktree_changed {
            return Ok(None);
        }
        let op = Op {
            parent: self.latest,
            time: self.time,
            command: self.command,
            view,
            saved: self.saved,
            undo_of: None,
        };
        record(repo, &op).map(Some)
    }
}

/// Before a new index is saved, forgets the cached time of entries carried
/// over unchanged from the loaded index that weren't older than that index
/// file. They were racily clean (the file could have changed within the
/// timestamp granularity without its size or time changing), and once a
/// newer index file exists the fast path would trust them for good. Entries
/// this transaction set were just checked, so they keep their times.
fn smudge_carried_over(index: &mut Index, base: &Index, base_mtime: Option<i64>) {
    let Some(base_mtime) = base_mtime else {
        return;
    };
    let racy: Vec<(RepoPath, IndexEntry)> = index
        .iter()
        .filter(|(path, entry)| {
            entry.mtime_ns != 0 && entry.mtime_ns >= base_mtime && base.get(path) == Some(*entry)
        })
        .map(|(path, entry)| (path.clone(), *entry))
        .collect();
    for (path, entry) in racy {
        index.insert(
            path,
            IndexEntry {
                mtime_ns: 0,
                ..entry
            },
        );
    }
}

/// The command line as recorded in an op: `nexus` and the arguments, each
/// quoted when needed so the whole thing stays on one line.
pub fn command_line<S: AsRef<str>>(args: &[S]) -> String {
    let mut line = String::from("nexus");
    for arg in args {
        line.push(' ');
        line.push_str(&quote(arg.as_ref()));
    }
    line
}

fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && c != '"' && c != '\\');
    if plain {
        return arg.to_owned();
    }
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(quoted, "\\u{{{:x}}}", u32::from(c));
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_stay_on_one_line() {
        assert_eq!(command_line(&["add", "."]), "nexus add .");
        assert_eq!(
            command_line(&["commit", "-m", "two words\nand a \"quote\""]),
            r#"nexus commit -m "two words\nand a \"quote\"""#
        );
        assert_eq!(command_line(&["add", ""]), r#"nexus add """#);
        assert_eq!(command_line(&["add", r"C:\x"]), r#"nexus add "C:\\x""#);
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let lock = Lock::acquire(&path, "nexus add .").unwrap();
        let err = Lock::acquire(&path, "nexus commit").unwrap_err();
        assert!(err.to_string().contains("command nexus add ."), "{err}");
        assert!(held_locks().contains(&path));
        drop(lock);
        assert!(!path.exists());
        assert!(!held_locks().contains(&path));
        Lock::acquire(&path, "nexus commit").unwrap();
    }

    /// A reader that doesn't allow deletion can't keep the lock alive on
    /// Windows: with delete-on-close, it can't open the file at all.
    #[cfg(windows)]
    #[test]
    fn readers_cant_block_lock_removal_on_windows() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let lock = Lock::acquire(&path, "nexus add .").unwrap();
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .open(&path);
        assert!(
            reader.is_err(),
            "a reader without FILE_SHARE_DELETE must be refused"
        );
        drop(lock);
        assert!(!path.exists());
    }
}
