//! The working tree compared with the index and with trees: what `status`
//! reports, what `diff` shows, and what `checkout` and `restore` may safely
//! change. Also writing and removing working files.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use crate::config;
use crate::content::{self, Sink};
use crate::error::{Error, IoResultExt as _, Result};
use crate::fsutil::{self, Existing};
use crate::hash::ObjectId;
use crate::index::{FileKind, Index, IndexEntry};
use crate::odb::ObjectStore;
use crate::path::{self, RepoPath};
use crate::platform;
use crate::repo::Repo;
use crate::tree;
use crate::walk::{self, CaseFound, Lookup, OnDisk};

/// A file's kind and content, as a tree or the index records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub kind: FileKind,
    pub id: ObjectId,
}

/// Every file in a tree (`None`: the empty tree of an unborn branch).
pub fn tree_files(
    store: &ObjectStore,
    tree: Option<ObjectId>,
) -> Result<BTreeMap<RepoPath, Version>> {
    let Some(tree) = tree else {
        return Ok(BTreeMap::new());
    };
    Ok(tree::flatten(store, &tree)?
        .into_iter()
        .map(|file| {
            (
                file.path,
                Version {
                    kind: file.kind,
                    id: file.id,
                },
            )
        })
        .collect())
}

/// Every file in the index.
pub fn index_files(index: &Index) -> BTreeMap<RepoPath, Version> {
    index
        .iter()
        .map(|(path, entry)| {
            (
                path.clone(),
                Version {
                    kind: entry.kind,
                    id: entry.id,
                },
            )
        })
        .collect()
}

/// The tree the commit HEAD points at, or `None` on an unborn branch.
pub fn head_tree(repo: &Repo) -> Result<Option<ObjectId>> {
    let head = repo.refs().head()?;
    match repo.refs().resolve(&head)? {
        Some(commit) => Ok(Some(crate::graph::read_commit(repo.odb(), &commit)?.tree)),
        None => Ok(None),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: RepoPath,
    pub kind: ChangeKind,
}

/// How a tracked file on disk compares with its index entry.
#[derive(Clone, Debug)]
pub enum Working {
    /// Same content and kind. `refreshed` is the entry with updated size and
    /// time when those changed (or were smudged) but the content didn't.
    Clean {
        refreshed: Option<IndexEntry>,
    },
    Modified(WorkingFile),
    Missing,
}

/// A changed file on disk.
#[derive(Clone, Debug)]
pub struct WorkingFile {
    pub fs_path: PathBuf,
    /// The kind it would be stored as.
    pub kind: FileKind,
    /// What its content would be stored as. `None` when that wasn't worth
    /// computing (its size alone shows it changed) or the file couldn't be
    /// read (another program holds it, or it's still being written); a
    /// caller that needs it hashes the file itself and reports the error.
    pub id: Option<ObjectId>,
    pub meta: Metadata,
}

/// Compares working files with index entries: the size-and-time fast path
/// where it's safe, hashing (without storing) otherwise.
pub struct Checker<'r> {
    repo: &'r Repo,
    lookup: Lookup,
    index_mtime: Option<i64>,
    trust_exec_bit: bool,
    /// Whether the filesystem treats names differing only in case as one.
    ignore_case: bool,
}

impl<'r> Checker<'r> {
    pub fn new(repo: &'r Repo) -> Result<Self> {
        Ok(Self {
            repo,
            lookup: Lookup::default(),
            index_mtime: fs::metadata(repo.index_path())
                .ok()
                .and_then(|meta| platform::mtime_ns(&meta)),
            trust_exec_bit: platform::executable_bits_exist()
                && config::file_mode(&repo.config_path())?,
            ignore_case: platform::ignores_case(repo.root()),
        })
    }

    /// Finds `path` on disk the way staging does: exact names, real directories.
    pub fn find(&self, path: &RepoPath) -> Result<OnDisk> {
        self.lookup.find(self.repo.root(), path)
    }

    /// The kind to record for a file on disk with metadata `meta`.
    pub fn kind_of(&self, meta: &Metadata, recorded: FileKind) -> FileKind {
        match self
            .trust_exec_bit
            .then(|| platform::executable_bit(meta))
            .flatten()
        {
            Some(true) => FileKind::Exec,
            Some(false) => FileKind::File,
            None => recorded,
        }
    }

    /// Whether the cached size and time prove a file unchanged.
    fn unchanged_by_stat(&self, meta: &Metadata, entry: &IndexEntry) -> bool {
        // A cached time of 0 means "unknown", even for a file dated 1970.
        entry.mtime_ns != 0
            && entry.size == meta.len()
            && platform::mtime_ns(meta) == Some(entry.mtime_ns)
            && self
                .index_mtime
                .is_some_and(|index_mtime| entry.mtime_ns < index_mtime)
    }

    /// Compares tracked files with their index entries, hashing in parallel
    /// where the fast path can't decide. `found` supplies files already
    /// located by a walk; the rest are looked up. A file that can't be read
    /// counts as modified, as in Git, so one locked file doesn't stop
    /// `status`.
    pub fn check_all(
        &self,
        entries: Vec<(RepoPath, IndexEntry)>,
        mut found: BTreeMap<RepoPath, (PathBuf, Metadata)>,
    ) -> Result<Vec<(RepoPath, Working)>> {
        let mut results = Vec::with_capacity(entries.len());
        let mut to_hash = Vec::new();
        for (path, entry) in entries {
            let located = match found.remove(&path) {
                Some(file) => Some(file),
                None => match self.find(&path)? {
                    OnDisk::File { fs_path, meta } => Some((fs_path, meta)),
                    _ => None,
                },
            };
            let Some((fs_path, meta)) = located else {
                results.push((path, Working::Missing));
                continue;
            };
            let kind = self.kind_of(&meta, entry.kind);
            if self.unchanged_by_stat(&meta, &entry) {
                let working = if kind == entry.kind {
                    Working::Clean { refreshed: None }
                } else {
                    let id = Some(entry.id);
                    Working::Modified(WorkingFile {
                        fs_path,
                        kind,
                        id,
                        meta,
                    })
                };
                results.push((path, working));
                continue;
            }
            // A different size proves a change without reading the file.
            // (An entry with no cached time, from `restore --staged`, has no
            // cached size either.)
            if entry.mtime_ns != 0 && entry.size != meta.len() {
                let working = Working::Modified(WorkingFile {
                    fs_path,
                    kind,
                    id: None,
                    meta,
                });
                results.push((path, working));
                continue;
            }
            to_hash.push((path, entry, kind, meta, fs_path));
        }
        let files = to_hash
            .iter()
            .enumerate()
            .map(|(i, (.., meta, fs_path))| (i, fs_path.clone(), meta.len()))
            .collect();
        let mut hashed = content::store_files(Sink::HashOnly, files);
        hashed.sort_by_key(|(i, _)| *i);
        for ((path, entry, kind, meta, fs_path), (_, stored)) in to_hash.into_iter().zip(hashed) {
            let working = match stored {
                Ok(stored) if stored.id == entry.id && kind == entry.kind => {
                    let refreshed = IndexEntry {
                        size: stored.size,
                        mtime_ns: stored.mtime_ns.unwrap_or(0),
                        ..entry
                    };
                    Working::Clean {
                        refreshed: (refreshed != entry).then_some(refreshed),
                    }
                }
                Ok(stored) => Working::Modified(WorkingFile {
                    fs_path,
                    kind,
                    id: Some(stored.id),
                    meta,
                }),
                // Deleted since it was found.
                Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                    Working::Missing
                }
                Err(_) => Working::Modified(WorkingFile {
                    fs_path,
                    kind,
                    id: None,
                    meta,
                }),
            };
            results.push((path, working));
        }
        results.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(results)
    }
}

/// What `nexus status` reports.
pub struct Status {
    /// HEAD's tree against the index.
    pub staged: Vec<Change>,
    /// The index against the working tree.
    pub unstaged: Vec<Change>,
    /// Untracked, unignored paths. A directory holding nothing tracked is
    /// listed once, with a trailing `/`.
    pub untracked: Vec<String>,
    /// Index entries whose cached size and time can be updated.
    pub refreshed: Vec<(RepoPath, IndexEntry)>,
    pub warnings: Vec<String>,
}

pub fn status(repo: &Repo, index: &Index) -> Result<Status> {
    let checker = Checker::new(repo)?;
    let walked = walk::walk(repo.root(), &[RepoPath::root()])?;
    let mut found = BTreeMap::new();
    let mut untracked = Vec::new();
    for file in walked.files {
        if index.get(&file.path).is_some() {
            found.insert(file.path, (file.fs_path, file.meta));
        } else {
            untracked.push(file.path);
        }
    }
    let entries: Vec<(RepoPath, IndexEntry)> = index.iter().map(|(p, e)| (p.clone(), *e)).collect();
    let mut unstaged = Vec::new();
    let mut refreshed = Vec::new();
    for (path, working) in checker.check_all(entries, found)? {
        match working {
            Working::Clean {
                refreshed: Some(entry),
            } => refreshed.push((path, entry)),
            Working::Clean { refreshed: None } => {}
            Working::Modified(_) => unstaged.push(Change {
                path,
                kind: ChangeKind::Modified,
            }),
            Working::Missing => unstaged.push(Change {
                path,
                kind: ChangeKind::Deleted,
            }),
        }
    }
    let staged = tree::changed_files(repo.odb(), head_tree(repo)?, index)?
        .into_iter()
        .map(|change| Change {
            kind: match (change.old, change.new) {
                (None, _) => ChangeKind::Added,
                (_, None) => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            },
            path: change.path,
        })
        .collect();
    Ok(Status {
        staged,
        unstaged,
        untracked: collapse_untracked(index, &untracked),
        refreshed,
        warnings: walked.warnings,
    })
}

/// Lists each untracked file, except that a directory containing no tracked
/// files is listed once as `dir/`, as Git does.
fn collapse_untracked(index: &Index, untracked: &[RepoPath]) -> Vec<String> {
    let mut tracked_dirs: HashSet<&str> = HashSet::new();
    for (path, _) in index.iter() {
        let text = path.as_str();
        tracked_dirs.extend(text.match_indices('/').map(|(end, _)| &text[..end]));
    }
    let mut listed = BTreeSet::new();
    for path in untracked {
        let text = path.as_str();
        let collapsed = text
            .match_indices('/')
            .map(|(end, _)| &text[..end])
            .find(|dir| !tracked_dirs.contains(dir))
            .map_or_else(|| text.to_owned(), |dir| format!("{dir}/"));
        listed.insert(collapsed);
    }
    listed.into_iter().collect()
}

/// Writes a file's content into the working tree atomically, with the
/// executable bit set where it means something. Returns the new metadata.
pub fn write_working_file(
    store: &ObjectStore,
    fs_path: &Path,
    version: Version,
) -> Result<Metadata> {
    let dir = fs_path
        .parent()
        .expect("working files have a parent directory");
    fs::create_dir_all(dir).at(dir)?;
    fsutil::write_atomic(fs_path, Existing::Replace, |file| {
        content::write_content(store, &version.id, file, fs_path).map_err(io::Error::other)?;
        Ok(())
    })?;
    platform::set_file_mode(fs_path, version.kind == FileKind::Exec).at(fs_path)?;
    fs::metadata(fs_path).at(fs_path)
}

/// Removes a working file (if it's there) and then any parent directories
/// this leaves empty, up to but not including `root`.
pub fn remove_working_file(root: &Path, fs_path: &Path) -> Result<()> {
    // Retried like renames, for the brief locks antivirus scanners and
    // indexers take on Windows.
    let mut delay = std::time::Duration::from_millis(1);
    for attempt in 0.. {
        match fs::remove_file(fs_path) {
            Ok(()) => break,
            Err(err) if err.kind() == io::ErrorKind::NotFound => break,
            Err(err) if attempt < 10 && platform::is_transient_lock(&err) => {
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(err) => return Err(err).at(fs_path),
        }
    }
    let mut dir = fs_path.parent();
    while let Some(current) = dir {
        if current == root || !current.starts_with(root) || fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
    Ok(())
}

/// Changes that make the working tree and index match a target set of files.
#[derive(Debug, Default)]
pub struct Plan {
    /// Files to write, with their new content.
    pub writes: Vec<(RepoPath, Version)>,
    /// Tracked files to delete.
    pub deletes: Vec<RepoPath>,
    /// What the files being overwritten or deleted held, for the op's
    /// `saved` tree.
    pub saved: Vec<(RepoPath, Version)>,
}

/// Plans switching from the current index to `target` (checkout) with Git's
/// two-way rules. A path whose HEAD and target versions are the same keeps
/// whatever the index and working tree have, staged or not. A path the
/// switch changes is updated only if it holds no uncommitted work: its index
/// entry must match HEAD (or already match the target), and its working file
/// must match the index or be gone. An untracked file or directory in the
/// way, or a name this system can't use, also refuses the switch.
pub fn plan_switch(
    repo: &Repo,
    index: &Index,
    head: &BTreeMap<RepoPath, Version>,
    target: &BTreeMap<RepoPath, Version>,
) -> Result<Plan> {
    let current = index_files(index);
    let checker = Checker::new(repo)?;
    let mut plan = Plan::default();
    let mut problems = Vec::new();

    let paths: BTreeSet<&RepoPath> = head
        .keys()
        .chain(current.keys())
        .chain(target.keys())
        .collect();
    let mut changing = Vec::new();
    let mut tracked = Vec::new();
    for path in paths {
        let (in_head, in_index, in_target) = (head.get(path), current.get(path), target.get(path));
        if in_head == in_target || in_index == in_target {
            continue;
        }
        if in_index != in_head {
            problems.push(format!("{path} (staged changes)"));
            continue;
        }
        changing.push((path, in_target.copied()));
        if let Some(entry) = index.get(path) {
            tracked.push((path.clone(), *entry));
        }
    }
    // Tracked files found on disk: clean ones are overwritten, modified ones
    // refuse the switch. Either way they aren't something untracked in the way.
    let mut on_disk = HashSet::new();
    for (path, working) in checker.check_all(tracked, BTreeMap::new())? {
        match working {
            Working::Clean { .. } => {
                plan.saved.push((path.clone(), current[&path]));
                on_disk.insert(path);
            }
            Working::Modified(_) => {
                problems.push(format!("{path} (changes not staged)"));
                on_disk.insert(path);
            }
            // Deleted locally: its content is in the index, so nothing is lost.
            Working::Missing => {}
        }
    }
    if checker.ignore_case {
        for (a, b) in path::case_collisions(target.keys()) {
            problems.push(format!(
                "{b} (the target also has {a}, and this filesystem can't hold names that differ \
                 only in letter case)"
            ));
        }
    }

    let deleting: HashSet<&RepoPath> = changing
        .iter()
        .filter(|(_, version)| version.is_none())
        .map(|(path, _)| *path)
        .collect();
    for &(path, version) in &changing {
        if in_repo_dir(path) {
            problems.push(format!("{path} (the name can't be used on this system)"));
            continue;
        }
        let Some(version) = version else {
            plan.deletes.push(path.clone());
            continue;
        };
        if path.to_fs_path(repo.root()).is_none() {
            problems.push(format!("{path} (the name can't be used on this system)"));
        } else if !on_disk.contains(path)
            && let Some(problem) = blocked(&checker, repo, path, &deleting)?
        {
            problems.push(problem);
        }
        plan.writes.push((path.clone(), version));
    }
    if !problems.is_empty() {
        return Err(Error::Invalid(refusal(
            "checking out",
            &problems,
            "Commit or discard those changes first (`nexus commit`, or `nexus restore <path>`), \
             and move untracked files out of the way.",
        )));
    }
    Ok(plan)
}

/// Why creating the file `path` would destroy something untracked, or write
/// somewhere it shouldn't, if it would: something already on disk under its
/// name, or under a name the filesystem treats as the same (other letter
/// case, a Windows 8.3 short name), or anything but a real directory where
/// one of its directories must go. Tracked files in `deleting`, which are
/// deleted first, don't count, nor does a directory that only they fill.
pub(crate) fn blocked(
    checker: &Checker,
    repo: &Repo,
    path: &RepoPath,
    deleting: &HashSet<&RepoPath>,
) -> Result<Option<String>> {
    let root = repo.root();
    let found = checker
        .lookup
        .find_ignoring_case(root, path, checker.ignore_case)?;
    let (spelled, whole, what) = match found {
        CaseFound::Free { missing, .. } => {
            return Ok(short_name_alias(root, path, missing)?
                .map(|alias| format!("{path} (something on disk already answers to {alias})")));
        }
        CaseFound::Found {
            spelled,
            whole,
            what,
        } => (spelled, whole, what),
    };
    Ok(match what {
        OnDisk::File { .. } if deleting.contains(&spelled) => None,
        OnDisk::Dir if whole => {
            // Only allowed if everything inside is tracked and being deleted.
            let walked = walk::walk(root, std::slice::from_ref(&spelled))?;
            let kept = walked
                .files
                .iter()
                .any(|file| !deleting.contains(&file.path));
            (kept || dir_has_ignored(repo, &spelled, deleting)?).then(|| {
                if spelled == *path {
                    format!("{path} (an untracked directory is in the way)")
                } else {
                    format!("{path} (the untracked directory {spelled} is in the way)")
                }
            })
        }
        _ if !whole => Some(format!(
            "{path} (the untracked file {spelled} is in the way)"
        )),
        _ if spelled != *path => Some(format!(
            "{path} (an untracked {spelled} differs only in letter case)"
        )),
        _ => Some(format!("{path} (an untracked file is in the way)")),
    })
}

/// Whether a path lies in a directory that would be the repository's own
/// `.nexus` (see [`path::is_repo_dir_name`]). Such a path is never written or
/// deleted, whatever a tree or index says.
pub fn in_repo_dir(path: &RepoPath) -> bool {
    path.components().any(path::is_repo_dir_name)
}

/// The first `missing + 1` names of `path`, if the filesystem finds
/// something there although no directory listing showed it: a Windows 8.3
/// short name (`VERYLO~1.TXT`) for a file or directory with a long name.
fn short_name_alias(root: &Path, path: &RepoPath, missing: usize) -> Result<Option<RepoPath>> {
    let prefix =
        RepoPath::from_names(path.components().take(missing + 1)).map_err(Error::Invalid)?;
    let exists = prefix
        .to_fs_path(root)
        .is_some_and(|fs_path| fs::symlink_metadata(fs_path).is_ok());
    Ok(exists.then_some(prefix))
}

/// Whether a directory holds files the walk doesn't report (ignored ones,
/// symlinks), which deleting its tracked files would leave behind.
fn dir_has_ignored(repo: &Repo, path: &RepoPath, deleting: &HashSet<&RepoPath>) -> Result<bool> {
    let Some(dir) = path.to_fs_path(repo.root()) else {
        return Ok(true);
    };
    let mut pending = vec![dir];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).at(&dir)? {
            let entry = entry.at(&dir)?;
            let file_type = entry.file_type().at(&dir)?;
            if file_type.is_dir() {
                pending.push(entry.path());
                continue;
            }
            let tracked_and_deleted = repo
                .repo_path(&entry.path())
                .is_ok_and(|path| deleting.contains(&path));
            if !tracked_and_deleted {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The message for a refused operation: what's at risk, then `advice`.
pub fn refusal(action: &str, problems: &[String], advice: &str) -> String {
    const SHOWN: usize = 20;
    let mut message = format!("{action} would lose work in these files, so nothing was changed:");
    for problem in problems.iter().take(SHOWN) {
        message.push_str("\n  ");
        message.push_str(problem);
    }
    if problems.len() > SHOWN {
        let _ = write!(message, "\n  ... and {} more", problems.len() - SHOWN);
    }
    message.push('\n');
    message.push_str(advice);
    message
}

/// Carries out a plan: deletions first (so a file can replace a directory and
/// the reverse), then writes, updating `index` to match. Only files reached
/// through real directories are deleted, and a write must reach its file the
/// same way, so a link in the working tree can't redirect either one outside
/// the repository; nothing in `.nexus` is ever touched. The plan was checked
/// already; the checks here guard against something changing in between.
/// `done` counts the files deleted or written, so a caller can tell whether
/// a failure left anything changed.
pub fn apply(repo: &Repo, index: &mut Index, plan: &Plan, done: &mut usize) -> Result<()> {
    let root = repo.root();
    let ignore_case = platform::ignores_case(root);
    let lookup = Lookup::default();
    for path in &plan.deletes {
        if in_repo_dir(path) {
            return Err(Error::Invalid(format!("refusing to delete {path}")));
        }
        if let OnDisk::File { fs_path, .. } = lookup.find(root, path)? {
            remove_working_file(root, &fs_path)?;
            *done += 1;
        }
        index.remove(path);
    }
    // Deleting changed the directories, so look them up afresh. Directory
    // listings are cached and don't show what this loop creates, so the
    // paths it created are remembered.
    let mut lookup = Lookup::default();
    let mut created: HashSet<RepoPath> = HashSet::new();
    for (path, version) in &plan.writes {
        if in_repo_dir(path) {
            return Err(Error::Invalid(format!("refusing to write {path}")));
        }
        let fs_path = if let OnDisk::File { fs_path, .. } = lookup.find(root, path)? {
            fs_path
        } else {
            match lookup.find_ignoring_case(root, path, ignore_case)? {
                CaseFound::Free { missing, spelled } => {
                    let names: Vec<&str> = path.components().collect();
                    let prefix = RepoPath::from_names(names[..=missing].iter().copied())
                        .map_err(Error::Invalid)?;
                    if !created.contains(&prefix)
                        && let Some(alias) = short_name_alias(root, path, missing)?
                    {
                        return Err(Error::Invalid(format!(
                            "can't write {path}: something on disk already answers to {alias}"
                        )));
                    }
                    // Directories spelled differently on disk (left by a case
                    // rename, kept alive by untracked files) get the tree's
                    // spelling, so the index and the disk agree.
                    if spelled
                        .iter()
                        .zip(&names)
                        .any(|(on_disk, name)| on_disk != name)
                    {
                        respell_dirs(root, &names, &spelled)?;
                        lookup = Lookup::default();
                    }
                }
                // A directory the deletions emptied of files.
                CaseFound::Found {
                    spelled,
                    whole: true,
                    what: OnDisk::Dir,
                } => {
                    if let Some(dir) = spelled.to_fs_path(root) {
                        remove_empty_dirs(&dir)?;
                    }
                }
                CaseFound::Found { spelled, .. } => {
                    return Err(Error::Invalid(format!(
                        "can't write {path}: {spelled} is in the way"
                    )));
                }
            }
            path.to_fs_path(root)
                .ok_or_else(|| Error::Invalid(format!("{path} can't be created on this system")))?
        };
        let meta = write_working_file(repo.odb(), &fs_path, *version)?;
        *done += 1;
        let mut prefix = RepoPath::root();
        for name in path.components() {
            prefix = prefix.child(name).map_err(Error::Invalid)?;
            created.insert(prefix.clone());
        }
        index.insert(
            path.clone(),
            IndexEntry {
                kind: version.kind,
                id: version.id,
                size: meta.len(),
                mtime_ns: platform::mtime_ns(&meta).unwrap_or(0),
            },
        );
    }
    Ok(())
}

/// Renames directories whose on-disk spelling (`spelled`) differs from the
/// tree's (`names`) only in letter case, from the top down. Only called on a
/// filesystem that ignores case, where both spellings are the same directory.
fn respell_dirs(root: &Path, names: &[&str], spelled: &[String]) -> Result<()> {
    let mut parent = root.to_path_buf();
    for (name, on_disk) in names.iter().zip(spelled) {
        if *on_disk != *name {
            let from = parent.join(on_disk);
            let to = parent.join(name);
            fs::rename(&from, &to).at(&from)?;
        }
        parent.push(name);
    }
    Ok(())
}

/// Removes `dir` and the directories inside it, failing (before removing
/// anything) if it holds anything but directories.
fn remove_empty_dirs(dir: &Path) -> Result<()> {
    let mut dirs = vec![dir.to_path_buf()];
    let mut i = 0;
    while let Some(current) = dirs.get(i).cloned() {
        for entry in fs::read_dir(&current).at(&current)? {
            let entry = entry.at(&current)?;
            if entry.file_type().at(&current)?.is_dir() {
                dirs.push(entry.path());
            } else {
                return Err(Error::Invalid(format!(
                    "can't replace the directory {}: {} is in it",
                    dir.display(),
                    entry.path().display()
                )));
            }
        }
        i += 1;
    }
    for dir in dirs.iter().rev() {
        fs::remove_dir(dir).at(dir)?;
    }
    Ok(())
}
