//! Staging: bringing the index in line with the working tree for the given
//! paths, which is what `nexus add` does.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::content::{self, Sink, StoredFile};
use crate::error::{Error, IoResultExt as _, Result};
use crate::index::{FileKind, Index, IndexEntry, MAX_PATH_LEN};
use crate::path::{self, RepoPath};
use crate::platform;
use crate::repo::{DIR_NAME, IGNORE_FILE, Repo};
use crate::walk::{self, FoundFile, Lookup, OnDisk};

#[derive(Clone, Copy, Debug)]
pub struct StageOptions {
    /// Mark every staged file executable (`nexus add --exec`).
    pub exec: bool,
    /// Whether the filesystem's executable bit means anything. False on
    /// Windows, and on Unix filesystems that report every file as executable
    /// (FAT, or Windows drives under WSL), where `[core] filemode = false`.
    pub trust_exec_bit: bool,
}

impl Default for StageOptions {
    fn default() -> Self {
        Self {
            exec: false,
            trust_exec_bit: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct StageReport {
    /// Paths that were added or whose content or kind changed.
    pub staged: Vec<RepoPath>,
    /// Paths removed from the index because they no longer exist.
    pub removed: Vec<RepoPath>,
    /// Whether the index needs saving. True when anything was staged or
    /// removed, and also when only cached file sizes or times were refreshed.
    pub index_updated: bool,
    pub warnings: Vec<String>,
}

impl StageReport {
    pub fn changed(&self) -> bool {
        !self.staged.is_empty() || !self.removed.is_empty()
    }
}

/// Updates `index` so that, inside each scope, it matches the working tree:
/// new and modified files are hashed and stored, and files that no longer
/// exist are removed. Ignore rules only stop untracked files from being added;
/// a file that's already tracked stays tracked.
///
/// Nothing is changed if a named path doesn't exist.
pub fn stage(
    repo: &Repo,
    index: &mut Index,
    scopes: &[RepoPath],
    options: StageOptions,
) -> Result<StageReport> {
    let root = repo.root();
    let index_mtime = std::fs::metadata(repo.index_path())
        .ok()
        .and_then(|meta| platform::mtime_ns(&meta));
    let walked = walk::walk(root, scopes)?;
    let mut report = StageReport {
        warnings: walked.warnings,
        ..StageReport::default()
    };
    let lookup = Lookup::default();
    let (files, missing) = with_tracked_files(
        repo,
        &lookup,
        index,
        scopes,
        walked.files,
        &mut report.warnings,
    )?;
    check_named_paths(
        repo,
        &lookup,
        scopes,
        &files,
        &missing,
        &mut report.warnings,
    )?;

    // Decide what needs hashing.
    let mut to_hash = Vec::new();
    let mut chmods = Vec::new();
    for file in files {
        if file.path.as_str().len() > MAX_PATH_LEN {
            report.warnings.push(format!(
                "skipping {}: the path is longer than {MAX_PATH_LEN} bytes",
                file.path
            ));
            continue;
        }
        let existing = index.get(&file.path).copied();
        let kind = file_kind(&file, existing, options, &mut chmods);
        let mtime_ns = platform::mtime_ns(&file.meta);
        let unchanged = existing.is_some_and(|entry| {
            // A cached time of 0 means "unknown", even for a file dated 1970.
            entry.mtime_ns != 0
                && entry.size == file.meta.len()
                && mtime_ns == Some(entry.mtime_ns)
                // Racy timestamps: a file modified in the same instant the
                // index was written might have changed after it was hashed.
                // (Saving the index also smudges such entries.)
                && index_mtime.is_some_and(|index_mtime| entry.mtime_ns < index_mtime)
        });
        match existing {
            Some(entry) if unchanged && entry.kind == kind => {}
            Some(entry) if unchanged => {
                index.insert(file.path.clone(), IndexEntry { kind, ..entry });
                report.index_updated = true;
                report.staged.push(file.path);
            }
            _ => to_hash.push((file, kind)),
        }
    }

    let hashed = hash_files(repo, to_hash)?;
    // Only now that every file was read, so a failed add changes no modes.
    for path in chmods {
        platform::make_executable(&path).at(&path)?;
    }
    let verified: HashSet<RepoPath> = hashed.iter().map(|(path, ..)| path.clone()).collect();
    smudge_racy_entries(index, index_mtime, &verified, &mut report);
    for (path, kind, file) in hashed {
        let entry = IndexEntry {
            kind,
            id: file.id,
            size: file.size,
            mtime_ns: file.mtime_ns.unwrap_or(0),
        };
        let old = index.get(&path).copied();
        if old != Some(entry) {
            report.index_updated = true;
            let content_changed =
                old.is_none_or(|old| old.id != entry.id || old.kind != entry.kind);
            index.insert(path.clone(), entry);
            if content_changed {
                report.staged.push(path);
            }
        }
    }
    for path in missing {
        if index.remove(&path).is_some() {
            report.index_updated = true;
            report.removed.push(path);
        }
    }
    report.staged.sort();
    report.removed.sort();
    portability_warnings(index, &mut report);
    Ok(report)
}

/// Forgets the cached modification time of entries that weren't checked in
/// this run and were modified no earlier than the index was last written.
/// Those are "racily clean": the file may have changed after it was hashed,
/// within the filesystem's timestamp granularity, and still have the same
/// size and time. Once the index is rewritten with a newer timestamp, the
/// fast path would trust them for good, so they're marked for rehashing.
/// (Entries checked in this run were just hashed, so they're trustworthy.)
fn smudge_racy_entries(
    index: &mut Index,
    index_mtime: Option<i64>,
    verified: &HashSet<RepoPath>,
    report: &mut StageReport,
) {
    let Some(index_mtime) = index_mtime else {
        return;
    };
    let racy: Vec<(RepoPath, IndexEntry)> = index
        .iter()
        .filter(|(path, entry)| {
            entry.mtime_ns != 0 && entry.mtime_ns >= index_mtime && !verified.contains(*path)
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
        report.index_updated = true;
    }
}

/// Adds the tracked files the walk didn't return. Those are either ignored
/// (still tracked, so they're restaged) or gone (returned separately, to be
/// removed from the index). A tracked path only counts as present if it's
/// spelled exactly that way on disk and reached through real directories.
fn with_tracked_files(
    repo: &Repo,
    lookup: &Lookup,
    index: &Index,
    scopes: &[RepoPath],
    mut files: Vec<FoundFile>,
    warnings: &mut Vec<String>,
) -> Result<(Vec<FoundFile>, Vec<RepoPath>)> {
    let found: HashSet<RepoPath> = files.iter().map(|f| f.path.clone()).collect();
    let mut seen = HashSet::new();
    let mut missing = Vec::new();
    for scope in scopes {
        for tracked in index.within(scope) {
            if found.contains(tracked) || !seen.insert(tracked.clone()) {
                continue;
            }
            if tracked.to_fs_path(repo.root()).is_none() {
                warnings.push(format!(
                    "leaving {tracked} as it is: the name can't be used on this system"
                ));
                continue;
            }
            match lookup.find(repo.root(), tracked)? {
                OnDisk::File { fs_path, meta } => files.push(FoundFile {
                    path: tracked.clone(),
                    fs_path,
                    meta,
                }),
                _ => missing.push(tracked.clone()),
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    missing.sort();
    Ok((files, missing))
}

/// Explains every path the user named that matched nothing. A path that
/// doesn't exist is an error, reported before anything changes.
fn check_named_paths(
    repo: &Repo,
    lookup: &Lookup,
    scopes: &[RepoPath],
    files: &[FoundFile],
    missing: &[RepoPath],
    warnings: &mut Vec<String>,
) -> Result<()> {
    let named: HashSet<&str> = scopes
        .iter()
        .filter(|s| !s.is_root())
        .map(RepoPath::as_str)
        .collect();
    let mut matched: HashSet<&str> = HashSet::new();
    for path in files.iter().map(|f| &f.path).chain(missing) {
        let text = path.as_str();
        let prefixes = text
            .match_indices('/')
            .map(|(end, _)| &text[..end])
            .chain([text]);
        matched.extend(prefixes.filter(|prefix| named.contains(prefix)));
    }
    for scope in scopes
        .iter()
        .filter(|s| !s.is_root() && !matched.contains(s.as_str()))
    {
        let in_nested_repo = scope.components().any(|name| name == DIR_NAME);
        match lookup.find(repo.root(), scope)? {
            OnDisk::File { .. } if in_nested_repo => {
                warnings.push(format!(
                    "{scope} is inside a {DIR_NAME} directory, so it wasn't staged"
                ));
            }
            OnDisk::File { .. } => warnings.push(format!(
                "{scope} is ignored by a {IGNORE_FILE} rule, so it wasn't staged"
            )),
            OnDisk::Dir => warnings.push(format!(
                "{scope}/ has nothing to stage: it's empty, or everything in it is ignored"
            )),
            OnDisk::Symlink => {
                warnings.push(format!("{scope} is a symlink; symlinks aren't supported"));
            }
            OnDisk::Other => {
                warnings.push(format!("{scope} isn't a regular file, so it wasn't staged"));
            }
            OnDisk::Missing {
                case_match: Some(spelled),
            } => {
                return Err(Error::Invalid(format!(
                    "{scope} doesn't match any files; did you mean {spelled}?"
                )));
            }
            OnDisk::Missing { case_match: None } => {
                return Err(Error::Invalid(format!("{scope} doesn't match any files")));
            }
        }
    }
    Ok(())
}

/// Stores the files' content (see [`content::store_files`]).
fn hash_files(
    repo: &Repo,
    to_hash: Vec<(FoundFile, FileKind)>,
) -> Result<Vec<(RepoPath, FileKind, StoredFile)>> {
    let files = to_hash
        .into_iter()
        .map(|(file, kind)| ((file.path, kind), file.fs_path, file.meta.len()))
        .collect();
    content::store_files(Sink::Store(repo.odb()), files)
        .into_iter()
        .map(|((path, kind), stored)| stored.map(|stored| (path, kind, stored)))
        .collect()
}
/// Warns about staged paths that would cause trouble on other operating systems.
fn portability_warnings(index: &Index, report: &mut StageReport) {
    // Only staged paths are warned about, so skip the work when there are none.
    if report.staged.is_empty() {
        return;
    }
    for staged in &report.staged {
        if let Some(problem) = path::windows_problem(staged) {
            report.warnings.push(format!(
                "{staged} can't be checked out on Windows: {problem}"
            ));
        }
    }
    for (a, b) in path::case_collisions(index.iter().map(|(path, _)| path)) {
        let involves_staged = [&a, &b]
            .into_iter()
            .filter_map(|p| RepoPath::parse(p).ok())
            .any(|colliding| {
                report
                    .staged
                    .iter()
                    .any(|staged| colliding.contains(staged))
            });
        if involves_staged {
            report.warnings.push(format!(
                "{a} and {b} differ only in letter case; on Windows and macOS they'd be the same path"
            ));
        }
    }
}

/// The kind to record for a file. Where the executable bit is meaningful, the
/// filesystem decides; elsewhere (Windows, or `[core] filemode = false`) the
/// kind already in the index stays. `--exec` marks the file executable
/// everywhere, and sets the bit where it's meaningful.
fn file_kind(
    file: &FoundFile,
    existing: Option<IndexEntry>,
    options: StageOptions,
    chmods: &mut Vec<PathBuf>,
) -> FileKind {
    let on_disk = if options.trust_exec_bit {
        platform::executable_bit(&file.meta)
    } else {
        None
    };
    if options.exec {
        if on_disk == Some(false) {
            chmods.push(file.fs_path.clone());
        }
        return FileKind::Exec;
    }
    match on_disk {
        Some(true) => FileKind::Exec,
        Some(false) => FileKind::File,
        None => existing.map_or(FileKind::File, |entry| entry.kind),
    }
}
